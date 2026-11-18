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
use exchange::automation::nba_signals::spawn_nba_publishers;
use exchange::automation::runtime::{runtime_channel, spawn_manager};
use exchange::automation::series_signals::spawn_series_publisher;
use exchange::automation::signal_bus::SignalBus;
use exchange::automation::signals::SignalStore;
use exchange::automation::sme::task::spawn_platform;
use exchange::automation::delta_signal::spawn_delta_publisher;
use exchange::automation::spot_signal::spawn_spot_publisher;
use exchange::automation::stats_signal::spawn_stats_publisher;
use exchange::automation::venue::{EngineClock, EngineVenue, SeriesMap};
use exchange::bots::{market_maker, traders};
use exchange::engine::{Exchange, RoundClock};
use exchange::gemini::audit::{RoundAudit, spawn_audit};
use exchange::gemini::driver::{DriverParts, GeminiUpDriver, RestSource, SeriesLink};
use exchange::gemini::index_feed::spawn_gemini_index_feed;
use exchange::gemini::strikes::{GeminiClock, IndexTape, StrikeBook, spawn_reference_listener};
use exchange::gemini::{BTC_INDEX_ID, BTC_INDEX_STREAM, CONTRACT_STATUS_STREAM, GeminiEnv, GeminiState, PublicSocket, gemini_series_for};
use exchange::house;
use exchange::pricing::feed::{PriceFeed, spawn_tick_flusher};
use exchange::pricing::mark::{BtcMark, SharedMark};
use exchange::registry::{self, PriceSource};
use exchange::scheduler;
use exchange::series::SERIES;
use exchange::{signals, signals_retention};
use exchange::sim::nba::{NbaDriver, NbaMark, Pacing, spawn_world};
use exchange::templates::TemplateCatalogue;
use exchange::types::Tenths;

const STARTING_BALANCE: Tenths = Tenths(1_000_000); // $1000

/// Default trader-cohort size per series (table order), used when
/// `TRADER_BOT_COUNT_<SLUG>` (slug upper-cased, `-` → `_`, e.g.
/// `TRADER_BOT_COUNT_BTC_15M`) isn't set. The cohorts together are capped
/// at the number of names in `crates/exchange/info/names.txt` (703);
/// later series shrink first.
const DEFAULT_TRADER_BOT_COUNTS: &[usize] = &[350, 250, 100];

/// Seed of the simulated NBA season (matchups and every game), so the
/// schedule is reproducible across restarts. Override with `NBA_SEED`.
const NBA_SEASON_SEED: u64 = 0x005E_A50A_B0B0;

/// Fixed seed for the bot population so personalities (archetype, anchor,
/// size, bankroll) are reproducible across server restarts.
const TRADER_BOT_POPULATION_SEED: u64 = 0xB075_EED5_F00D;

async fn health() -> &'static str {
    "ok"
}

/// Trader-cohort size per series in table order: the per-slug env
/// override or the default, then trimmed (later series first) so the
/// total never exceeds the number of unique names available.
fn cohort_sizes(available_names: usize) -> Vec<usize> {
    let mut counts: Vec<usize> = SERIES
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let key = format!(
                "TRADER_BOT_COUNT_{}",
                spec.slug.to_ascii_uppercase().replace('-', "_")
            );
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or_else(|| DEFAULT_TRADER_BOT_COUNTS.get(i).copied().unwrap_or(0))
        })
        .collect();
    let mut total: usize = counts.iter().sum();
    for c in counts.iter_mut().rev() {
        if total <= available_names {
            break;
        }
        let trim = (total - available_names).min(*c);
        *c -= trim;
        total -= trim;
    }
    counts
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

    // Every series in the table gets an engine, a scheduler, a market
    // maker, a trader cohort and a trades batcher; they all share one
    // ledger of balances.
    let engine = Arc::new(Mutex::new(Exchange::new(STARTING_BALANCE, SERIES)));
    // The strategy runtime's inbox, and the engine's execution sink that
    // reports order fates into it. Installed before anything can place a
    // client-tagged order so no strategy fill is ever unreported.
    let (runtime, runtime_rx) = runtime_channel();
    EngineVenue::install(&engine, runtime.clone());
    let price = PriceFeed::new();
    // Charts get one tick per interval; this emits a burst's trailing price.
    spawn_tick_flusher(price.clone());
    let channels = SeriesChannels::for_all_series();

    // Seed every series' trader cohort *before* anything else has a chance
    // to touch the engine. Each bot gets its own funded account (bound to
    // its series), archetype, anchor distance, size band and bankroll. The
    // async tasks below pick them up and start trading once their series'
    // first market opens. One shuffle of the name list, sliced into
    // disjoint runs, keeps handles unique across cohorts.
    // The house account: registered as a user under the templates' creator
    // name and credited for one deployment of every template side, which
    // it makes once the runtime is up (below).
    let seed_house = house::seeding_enabled();
    if seed_house {
        let mut engine_guard = engine.lock().expect("engine mutex poisoned");
        house::seed_house_account(&mut engine_guard, &templates);
    }

    let bot_population = {
        // Embedded at compile time so the binary runs from any working
        // directory (cargo run from the workspace root, the deploy image, …).
        let mut names = traders::parse_names(include_str!("../info/names.txt"));
        let counts = cohort_sizes(names.len());
        let mut rng = traders::seeded_rng(TRADER_BOT_POPULATION_SEED);
        traders::shuffle_names(&mut names, &mut rng);
        let mut population = Vec::new();
        let mut offset = 0;
        let mut engine_guard = engine.lock().expect("engine mutex poisoned");
        for (spec, &count) in SERIES.iter().zip(counts.iter()) {
            let slice = &names[offset..offset + count];
            offset += count;
            // Per-series population seed so cohorts don't mirror each other.
            let seed = TRADER_BOT_POPULATION_SEED ^ ((spec.id.0 as u64) << 48);
            let cohort = traders::build_population(&mut engine_guard, slice, count, seed, spec.id);
            tracing::info!(
                series = %spec.id,
                count = cohort.len(),
                available_names = names.len(),
                "trader-bot cohort seeded",
            );
            population.extend(cohort);
        }
        population
    };

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
    gemini.socket = Some(gemini_socket);
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

    // The simulated NBA season, if an NBA series is in the table: the
    // world plays one game per round and its feeds go on the bus. Started
    // before the schedulers so the sharp line has quoted the first
    // matchup by the time the NBA driver opens its first round.
    let nba_world = SERIES.iter().find(|s| s.underlying == exchange::registry::NBA_UNDERLYING).map(|spec| {
        let seed = std::env::var("NBA_SEED").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(NBA_SEASON_SEED);
        let pacing = Pacing::for_duration(spec.duration_secs);
        let world = spawn_world(seed, pacing);
        spawn_nba_publishers(world.clone(), bus.clone(), seed);
        tracing::info!(series = spec.slug, seed, ?pacing, "nba world started");
        world
    });

    // The bots' view of each series' round: one mark source per series.
    let btc_mark: SharedMark = BtcMark::new(price.clone());
    // The crowd's mark wobbles on the season seed, so a restart replays it.
    let nba_seed = std::env::var("NBA_SEED").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(NBA_SEASON_SEED);
    let nba_mark: SharedMark = NbaMark::seeded(bus.clone(), nba_seed);
    let mark_for = |series: exchange::types::SeriesId| -> SharedMark {
        if series.is_btc() { btc_mark.clone() } else { nba_mark.clone() }
    };

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
                    link,
                    audit: round_audit.clone(),
                    feed: Some(price.clone()),
                });
                scheduler::spawn_with_driver(engine.clone(), series, driver, runtime.clone(), broadcast);
            } else if let Some(world) = &nba_world {
                let driver = NbaDriver::new(world.clone(), bus.clone(), series.duration_secs()).with_feed(price.clone());
                scheduler::spawn_with_driver(engine.clone(), series, driver, runtime.clone(), broadcast);
            } else {
                tracing::error!(series = %series, "no round driver for this series; it will never open a round");
            }
        }
        {
            let channels = ch.clone();
            market_maker::spawn(engine.clone(), series, mark_for(series), move |engine| {
                broadcast_book(engine, &channels);
            });
        }
        // Trades batcher: polls this series' engine for new fills and
        // pushes an enriched batch (with identities) onto its channel.
        api::spawn_trades_batcher(engine.clone(), series, ch.trades_tx.clone());
    }

    // Spawn one tokio task per trader bot. Each derives its own RNG seed
    // from the population seed + index so behavior is reproducible, and
    // broadcasts on its own series' book channel.
    for (idx, bot) in bot_population.into_iter().enumerate() {
        let bot_channels = channels[bot.series.0 as usize].clone();
        let task_seed = TRADER_BOT_POPULATION_SEED ^ (idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        traders::spawn_bot(
            engine.clone(),
            mark_for(bot.series),
            bot,
            task_seed,
            move |engine| broadcast_book(engine, &bot_channels),
        );
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
        crowd: api::templates::Crowd::enabled_by_env(),
        gemini: Some(gemini),
    };

    // Seed the cards: the house runs both sides of every template on every
    // series it lists. A side that fails to start is logged, never fatal.
    if seed_house {
        let report = house::deploy_house_templates(&state).await;
        for (template, series, side, error) in &report.failed {
            tracing::error!(template, series, ?side, error, "house seeding: template side did not start");
        }
        tracing::info!(deployed = report.deployed, failed = report.failed.len(), house = %house::HOUSE_ADDRESS, "house seeding done");
    } else {
        tracing::info!("house seeding disabled ({}=0)", house::HOUSE_SEED_ENV);
    }

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
        .route("/api/holders", get(api::list_holders))
        .route("/api/leaderboard", get(api::get_leaderboard))
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
        .layer(CorsLayer::permissive())
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tracing::info!("server listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}
