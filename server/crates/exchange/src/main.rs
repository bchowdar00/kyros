use std::sync::{Arc, Mutex};

use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::{delete, get, post},
};
use futures::StreamExt;
use tower_http::cors::CorsLayer;

use exchange::api::{self, AppState, SeriesChannels, broadcast_book};
use exchange::automation::lane_bridge::spawn_lane_bridge;
use exchange::automation::runtime::{runtime_channel, spawn_manager};
use exchange::automation::series_signals::spawn_series_publisher;
use exchange::automation::signal_bus::SignalBus;
use exchange::automation::signals::SignalStore;
use exchange::automation::sme::task::spawn_platform;
use exchange::automation::delta_signal::spawn_delta_publisher;
use exchange::automation::spot_signal::spawn_spot_publisher;
use exchange::automation::stats_signal::spawn_stats_publisher;
use exchange::automation::venue::{EngineClock, EngineVenue, SeriesMap};
use exchange::engine::{Exchange, RoundClock};
use exchange::gemini::audit::{RoundAudit, spawn_audit};
use exchange::gemini::driver::{DriverParts, GeminiUpDriver, RestSource, SeriesLink};
use exchange::gemini::index_feed::spawn_gemini_index_feed;
use exchange::gemini::mirror_task::spawn_mirror;
use exchange::gemini::strikes::{GeminiClock, IndexTape, StrikeBook, spawn_reference_listener};
use exchange::gemini::{BTC_INDEX_ID, BTC_INDEX_STREAM, CONTRACT_STATUS_STREAM, GeminiEnv, GeminiState, PublicSocket, gemini_series_for};
use exchange::record::{self, RecordBook};
use exchange::pricing::feed::{PriceFeed, spawn_tick_flusher};
use exchange::registry::{self, PriceSource};
use exchange::scheduler;
use exchange::series::SERIES;
use exchange::{signals, signals_retention};
use exchange::templates::TemplateCatalogue;
use exchange::types::Tenths;

const STARTING_BALANCE: Tenths = Tenths(1_000_000); // $1000

async fn health() -> &'static str {
    "ok"
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.next().await {
        match msg {
            Message::Text(text) => {
                if socket.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            Message::Binary(data) => {
                if socket.send(Message::Binary(data)).await.is_err() {
                    break;
                }
            }
            Message::Ping(payload) => {
                if socket.send(Message::Pong(payload)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    // BTC signal producers (spot, prediction and venue lanes from the
    // signals prototype). Each runs on its own thread and runtime, in
    // parallel with everything below; see `signals.rs` for lane selection.
    // Their in-process message stream is created here, before the registry
    // exists, and buffers until the lane bridge attaches further down.
    let (live_lanes, lane_messages): (Vec<&'static str>, Option<_>) = match signals::spawn_signal_producers() {
        Some(mut producers) => {
            let lanes = producers.lanes().to_vec();
            let messages = producers.take_messages();
            signals::install_shutdown(producers);
            // Prune sealed generations so the lanes never fill the disk.
            signals_retention::spawn(signals_retention::Config::from_env(signals::lane_root()));
            (lanes, messages)
        }
        None => (Vec::new(), None),
    };

    // The live BTC price is the index Gemini settles on, from Gemini's
    // public socket: the one price the strikes and results are measured
    // in. Recorded in the registry.
    let price_source = PriceSource::GeminiIndex;
    // `production` unless GEMINI_ENV says `sandbox`; anything else stops
    // the boot here.
    let gemini_env = GeminiEnv::from_env();

    // The venue registry: every series and signal, derived from the series
    // table and the signal definition files, written to REGISTRY_PATH and
    // served at /api/registry.
    let registry = match registry::generate_and_write(&registry::Inputs {
        price_source,
        live_lanes: &live_lanes,
        definitions_dir: None,
        copy_dir: None,
    }) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "registry: generation failed");
            std::process::exit(78);
        }
    };

    // The public strategy templates, stamped per series and compiled
    // against that registry. A template that does not compile clean is a
    // boot failure, never a broken card.
    let templates = match TemplateCatalogue::load(&registry, price_source) {
        Ok(c) => {
            tracing::info!(templates = c.len(), documents = c.document_count(), "strategy templates compiled");
            Arc::new(c)
        }
        Err(e) => {
            tracing::error!(error = %e, "strategy templates: load failed");
            std::process::exit(78);
        }
    };

    // Every series in the table gets an engine, a scheduler and a trades
    // batcher; they all share one ledger of balances.
    let engine = Arc::new(Mutex::new(Exchange::new(STARTING_BALANCE, SERIES)));
    // The strategy runtime's inbox, and the engine's execution sink that
    // reports order fates into it. Installed before anything can place a
    // client-tagged order so no strategy fill is ever unreported.
    let (runtime, runtime_rx) = runtime_channel();
    // Every order event goes to the on-chain record's journal first and
    // then to the runtime. The sealer looks for resolved rounds.
    let records = RecordBook::new();
    record::install_sink(&engine, runtime.clone(), records.clone());
    record::spawn_sealer(engine.clone(), records.clone());
    let price = PriceFeed::new();
    // Charts get one tick per interval; this emits a burst's trailing price.
    spawn_tick_flusher(price.clone());
    let channels = SeriesChannels::for_all_series();

    // Gemini's public socket, keyless: every contract's strike and status,
    // and the index price once a second. The listener keeps the strike
    // book, the index tape (the price at each second, for a result Gemini
    // is late with) and the offset between this host's clock and Gemini's.
    // The index also feeds the chart, the strike distance and fair value.
    let (gemini_socket, gemini_events) = PublicSocket::spawn(gemini_env);
    gemini_socket.subscribe(vec![CONTRACT_STATUS_STREAM.to_string(), BTC_INDEX_STREAM.to_string()]);
    let strikes = StrikeBook::new();
    let index_tape = IndexTape::new();
    let gemini_clock = GeminiClock::system();
    spawn_reference_listener(gemini_events, strikes.clone(), index_tape.clone(), gemini_clock.clone(), BTC_INDEX_ID);
    spawn_gemini_index_feed(gemini_socket.events(), price.clone(), BTC_INDEX_ID);
    // Every round's result is checked against Gemini's own, later.
    let round_audit = RoundAudit::new();
    spawn_audit(round_audit.clone(), RestSource(gemini_env));
    let mut gemini = GeminiState::new(gemini_env, gemini_clock.clone(), round_audit.clone());
    gemini.socket = Some(gemini_socket.clone());
    tracing::info!(env = gemini_env.name, index = BTC_INDEX_STREAM, "gemini: public socket started");
    // Signals platform: the runtime's matching engine, catalogued from the
    // generated registry (every signal starts disabled; a signal is enabled
    // when something in this process publishes it). Instruments are the
    // registry's series slugs with the `.current` suffix.
    let platform = {
        let series_map = SeriesMap::new(SERIES.iter().map(|s| (s.slug, s.id)));
        let clock = EngineClock::new(engine.clone(), series_map.clone());
        let handle = spawn_platform(SignalStore::from_registry(&registry), clock.clone());
        for spec in SERIES {
            handle.enable(&exchange::registry::stats_signal_id(spec.slug));
        }
        tracing::info!(signals = registry.signals.len(), "signals platform started");
        // The strategy runtime: deployments wake on platform activations
        // and trade through `EngineVenue` over the shared exchange.
        let venue = EngineVenue::new(engine.clone(), series_map);
        spawn_manager(runtime_rx, handle.clone(), venue, clock);
        handle
    };

    // The signal bus: the one path every publisher takes. It types each
    // frame against the registry and hands it to the platform first, then
    // keeps it as the latest frame and broadcasts it to subscribers. The
    // lane bridge drains the lanes' in-process message stream onto it.
    let bus = SignalBus::new(registry.clone(), Some(platform.clone()));
    if let Some(messages) = lane_messages {
        spawn_lane_bridge(bus.clone(), platform.clone(), messages);
        tracing::info!(lanes = ?live_lanes, "lane bridge started");
    }
    // The live BTC price → the registry's one external signal.
    spawn_spot_publisher(price.clone(), price_source, bus.clone());

    for ch in channels.iter() {
        let series = ch.series;
        // Round statistics → `<slug>.current.stats.v1` on the bus, and the
        // round's strike, window, price and trades → `<slug>.current.*.v1`.
        spawn_stats_publisher(engine.clone(), series, bus.clone());
        spawn_series_publisher(engine.clone(), series, bus.clone());
        // Spot against the round's strike → `<slug>.current.delta.v1`
        // (BTC series only; an NBA round has no strike).
        if series.is_btc() {
            spawn_delta_publisher(engine.clone(), price.clone(), series, bus.clone());
        }
        {
            let channels = ch.clone();
            let broadcast = move |engine: &exchange::engine::MarketEngine| broadcast_book(engine, &channels);
            if let Some(gemini_series) = gemini_series_for(series.slug()) {
                // A BTC series follows Gemini's windows: the round opens
                // on Gemini's strike, closes at Gemini's expiry (orders
                // are refused from that second, on Gemini's clock) and
                // resolves on Gemini's own result.
                let link = SeriesLink::new();
                gemini.follow(series, gemini_series, link.clone());
                {
                    let mut exchange = engine.lock().expect("engine mutex poisoned");
                    let market = exchange.engine_mut(series);
                    market.set_lock_at_close(true);
                    let clock = gemini_clock.clone();
                    market.set_clock(RoundClock::new(move || clock.now_ms() / 1000));
                }
                let driver = GeminiUpDriver::new(DriverParts {
                    series: gemini_series,
                    window_secs: series.duration_secs(),
                    source: RestSource(gemini_env),
                    strikes: strikes.clone(),
                    tape: index_tape.clone(),
                    clock: gemini_clock.clone(),
                    link: link.clone(),
                    audit: round_audit.clone(),
                    feed: Some(price.clone()),
                });
                scheduler::spawn_with_driver(engine.clone(), series, driver, runtime.clone(), broadcast);
                // The series' book is Gemini's own live book for the
                // open window, held by one account that is not a trader.
                let channels = ch.clone();
                spawn_mirror(engine.clone(), series, gemini_socket.clone(), link, move |engine| {
                    broadcast_book(engine, &channels)
                });
            } else {
                tracing::error!(series = %series, "no round driver for this series; it will never open a round");
            }
        }
        // Trades batcher: polls this series' engine for new fills and
        // pushes an enriched batch (with identities) onto its channel.
        api::spawn_trades_batcher(engine.clone(), series, ch.trades_tx.clone());
    }

    let state = AppState {
        exchange: engine,
        channels,
        price,
        runtime,
        registry,
        bus,
        templates,
        deploys: api::DeployKeys::default(),
        gemini: Some(gemini),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/ws", get(ws_handler))
        .route("/ws/price", get(api::price_ws))
        .route("/ws/book", get(api::book_ws))
        .route("/ws/trades", get(api::trades_ws))
        .route("/ws/positions", get(api::positions_ws))
        .route("/ws/accounts", get(api::accounts_ws))
        .route("/ws/signals", get(api::signals_ws))
        .route("/api/accounts", post(api::create_account))
        .route("/api/accounts/{address}", get(api::get_account))
        .route("/api/accounts/{address}/trades", get(api::get_user_trades))
        .route("/api/accounts/{address}/positions", get(api::get_positions))
        .route("/api/orders", post(api::place_order))
        .route("/api/orders/{id}", delete(api::cancel_order))
        .route("/api/positions/close", post(api::close_position))
        .route("/api/positions/flip", post(api::flip_position))
        .route("/api/book", get(api::get_book))
        .route("/api/book/mid-history", get(api::get_mid_history))
        .route("/api/trades", get(api::get_trades))
        .route("/api/market/stats", get(api::get_market_stats))
        .route("/api/series", get(api::list_series))
        .route("/api/rounds/audit", get(api::get_rounds_audit))
        .route("/api/registry", get(api::get_registry))
        .route("/api/signals", get(api::list_signals))
        .route("/api/signals/history", get(api::get_signals_history))
        .route("/api/markets/history", get(api::get_markets_history))
        .route("/api/price/history", get(api::price_history))
        .route("/api/strategy", post(api::compile_strategy))
        .route("/api/strategy/schema", get(api::get_strategy_schema))
        .route("/api/strategy/compile", post(api::compile_strategy))
        .route("/api/strategies/templates", get(api::list_templates))
        .route("/api/strategies/templates/{id}", get(api::get_template))
        .route(
            "/api/accounts/{address}/strategies",
            get(api::list_strategies).post(api::create_strategy),
        )
        .route(
            "/api/accounts/{address}/strategies/{id}",
            get(api::get_strategy)
                .patch(api::update_strategy)
                .delete(api::delete_strategy),
        )
        .route(
            "/api/accounts/{address}/strategies/{id}/fund",
            post(api::fund_strategy),
        )
        .route(
            "/api/accounts/{address}/templates/{template_id}/deploy",
            post(api::deploy_template),
        )
        .merge(api::writer_meter::routes())
        .merge(record::routes(records))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tracing::info!("server listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}
