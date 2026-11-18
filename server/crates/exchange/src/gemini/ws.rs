//! Gemini's public market-data socket: one connection, many streams,
//! kept alive forever.
//!
//! [`PublicSocket::spawn`] starts a task that connects with
//! `?snapshot=-1` (so the first depth frame of every book is the whole
//! book), subscribes to whatever has been asked for, and broadcasts
//! [`WsEvent`]s. When the connection drops, goes silent, or a depth
//! stream skips an update, it broadcasts [`WsEvent::Disconnected`],
//! waits 2, 4, 8 … 30 seconds, reconnects, subscribes to everything
//! again and broadcasts [`WsEvent::Connected`]. A consumer that keeps a
//! book must throw it away on `Disconnected`: the next depth event for
//! each symbol is a fresh snapshot.
//!
//! This socket carries no credentials and can only name public streams.

use std::collections::BTreeSet;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::env::GeminiEnv;
use super::types::{DepthStep, DepthTracker, Parsed, WsEvent, parse_ws_message};

/// First wait before reconnecting; doubles each failed attempt.
pub const BACKOFF_BASE: Duration = Duration::from_secs(2);
/// The longest wait between attempts.
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// With an index-price stream subscribed (about one message a second),
/// this much silence means the connection is dead.
pub const IDLE_LIMIT: Duration = Duration::from_secs(5);
const EVENT_BUFFER: usize = 4096;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait before reconnect attempt `attempt` (0 is the first).
pub fn backoff_delay(attempt: u32, base: Duration, max: Duration) -> Duration {
    1u32.checked_shl(attempt)
        .and_then(|factor| base.checked_mul(factor))
        .map_or(max, |wait| wait.min(max))
}

/// The stream names that belong on a public, keyless socket.
pub fn is_public_stream(stream: &str) -> bool {
    !stream.to_ascii_lowercase().contains("account")
        && (stream == "contractStatus" || stream.contains('@'))
}

/// Everything about a connection that a test wants to shorten.
#[derive(Debug, Clone)]
pub struct SocketConfig {
    /// Without the query string; `?snapshot=-1` is added.
    pub url: String,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// Applies only while an `@indexPrice` stream is subscribed.
    pub idle_limit: Duration,
}

impl SocketConfig {
    pub fn for_env(env: &GeminiEnv) -> Self {
        Self {
            url: env.ws_url.to_owned(),
            backoff_base: BACKOFF_BASE,
            backoff_max: BACKOFF_MAX,
            idle_limit: IDLE_LIMIT,
        }
    }
}

#[derive(Debug)]
enum Command {
    Subscribe(Vec<String>),
    Unsubscribe(Vec<String>),
}

/// A handle on the socket task. Cheap to clone; the task ends when the
/// last handle is dropped.
#[derive(Debug, Clone)]
pub struct PublicSocket {
    commands: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<WsEvent>,
}

impl PublicSocket {
    pub fn spawn(env: GeminiEnv) -> (Self, broadcast::Receiver<WsEvent>) {
        Self::spawn_with(SocketConfig::for_env(&env))
    }

    pub fn spawn_with(config: SocketConfig) -> (Self, broadcast::Receiver<WsEvent>) {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = broadcast::channel(EVENT_BUFFER);
        tokio::spawn(run(config, command_rx, events.clone()));
        (Self { commands, events }, event_rx)
    }

    /// Add streams, e.g. `"GEMI-BTC05M2611181720-UP@depth@100ms"`,
    /// `"GEMI-BTC05M2611181720-UP@trade"`, `"contractStatus"`,
    /// `"lukka:LS-LRRR-XBTUSD@indexPrice"`. Names that are not public
    /// streams are dropped with a warning. Remembered across reconnects.
    pub fn subscribe(&self, streams: Vec<String>) {
        let _ = self.commands.send(Command::Subscribe(streams));
    }

    pub fn unsubscribe(&self, streams: Vec<String>) {
        let _ = self.commands.send(Command::Unsubscribe(streams));
    }

    /// A new receiver. Events sent before this call are not replayed.
    pub fn events(&self) -> broadcast::Receiver<WsEvent> {
        self.events.subscribe()
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Why a connection's loop ended.
enum SessionEnd {
    /// The connection is gone or can no longer be trusted. Reconnect.
    Dropped,
    /// Every [`PublicSocket`] handle is gone. Stop.
    NoHandles,
}

async fn run(
    config: SocketConfig,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: broadcast::Sender<WsEvent>,
) {
    // The slash matters: without a path the request line is malformed.
    let url = format!("{}/?snapshot=-1", config.url.trim_end_matches('/'));
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    let mut next_request_id: u64 = 1;
    let mut attempt: u32 = 0;
    loop {
        match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(&url)).await {
            Ok(Ok((socket, _))) => {
                attempt = 0;
                let end =
                    session(socket, &config, &mut commands, &events, &mut wanted, &mut next_request_id)
                        .await;
                let _ = events.send(WsEvent::Disconnected);
                if matches!(end, SessionEnd::NoHandles) {
                    return;
                }
            }
            Ok(Err(error)) => tracing::warn!(%error, "gemini socket: connect failed"),
            Err(_) => tracing::warn!("gemini socket: connect timed out"),
        }

        let wait = backoff_delay(attempt, config.backoff_base, config.backoff_max);
        attempt = attempt.saturating_add(1);
        tracing::info!(wait_ms = wait.as_millis() as u64, "gemini socket: reconnecting");
        let sleep = tokio::time::sleep(wait);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = &mut sleep => break,
                command = commands.recv() => match command {
                    // Asked for while down: remembered, sent on connect.
                    Some(command) => { apply(&mut wanted, command); }
                    None => return,
                },
            }
        }
    }
}

/// Update the wanted set; returns what changed so it can be sent.
fn apply(wanted: &mut BTreeSet<String>, command: Command) -> (&'static str, Vec<String>) {
    match command {
        Command::Subscribe(streams) => {
            let mut added = Vec::new();
            for stream in streams {
                if !is_public_stream(&stream) {
                    tracing::warn!(%stream, "gemini socket: not a public stream; dropped");
                } else if wanted.insert(stream.clone()) {
                    added.push(stream);
                }
            }
            ("SUBSCRIBE", added)
        }
        Command::Unsubscribe(streams) => {
            ("UNSUBSCRIBE", streams.into_iter().filter(|s| wanted.remove(s)).collect())
        }
    }
}

fn request(id: &mut u64, method: &str, params: &[String]) -> Message {
    let text = serde_json::json!({ "id": id.to_string(), "method": method, "params": params });
    *id += 1;
    Message::Text(text.to_string().into())
}

async fn session(
    socket: Socket,
    config: &SocketConfig,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &broadcast::Sender<WsEvent>,
    wanted: &mut BTreeSet<String>,
    next_request_id: &mut u64,
) -> SessionEnd {
    let (mut sink, mut stream) = socket.split();
    let mut depth = DepthTracker::new();

    // Take what is already queued, so one request names every stream.
    loop {
        match commands.try_recv() {
            Ok(command) => {
                apply(wanted, command);
            }
            Err(mpsc::error::TryRecvError::Empty) => break,
            Err(mpsc::error::TryRecvError::Disconnected) => return SessionEnd::NoHandles,
        }
    }
    if !wanted.is_empty() {
        let all: Vec<String> = wanted.iter().cloned().collect();
        if sink.send(request(next_request_id, "SUBSCRIBE", &all)).await.is_err() {
            return SessionEnd::Dropped;
        }
    }
    let _ = events.send(WsEvent::Connected);
    tracing::info!(streams = wanted.len(), "gemini socket: connected");

    let mut last_frame = Instant::now();
    loop {
        // Only an index stream is guaranteed to talk; without one,
        // silence proves nothing.
        let deadline = wanted
            .iter()
            .any(|s| s.ends_with("@indexPrice"))
            .then(|| last_frame + config.idle_limit);
        let silence = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    last_frame = Instant::now();
                    match parse_ws_message(text.as_str()) {
                        Some(Parsed::Event(event)) => { let _ = events.send(event); }
                        Some(Parsed::Depth(frame)) => {
                            let symbol = frame.symbol.clone();
                            match depth.accept(frame) {
                                DepthStep::Event(event) => { let _ = events.send(event); }
                                DepthStep::Stale => {}
                                DepthStep::Gap => {
                                    tracing::warn!(%symbol, "gemini socket: depth update missed; reopening");
                                    return SessionEnd::Dropped;
                                }
                            }
                        }
                        None => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None => return SessionEnd::Dropped,
                Some(Ok(_)) => last_frame = Instant::now(),
                Some(Err(error)) => {
                    tracing::warn!(%error, "gemini socket: read failed");
                    return SessionEnd::Dropped;
                }
            },
            command = commands.recv() => {
                let Some(command) = command else { return SessionEnd::NoHandles };
                let (method, changed) = apply(wanted, command);
                if changed.is_empty() {
                    continue;
                }
                if method == "UNSUBSCRIBE" {
                    for stream in &changed {
                        if let Some((symbol, rest)) = stream.split_once('@')
                            && rest.starts_with("depth")
                        {
                            depth.forget(&symbol.to_ascii_uppercase());
                        }
                    }
                }
                if sink.send(request(next_request_id, method, &changed)).await.is_err() {
                    return SessionEnd::Dropped;
                }
            },
            () = silence => {
                tracing::warn!(
                    idle_ms = config.idle_limit.as_millis() as u64,
                    "gemini socket: silent; reopening"
                );
                return SessionEnd::Dropped;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    const WAIT: Duration = Duration::from_secs(3);

    fn fixture_lines(name: &str) -> Vec<String> {
        let path = format!("{}/tests/fixtures/gemini/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap().lines().map(str::to_owned).collect()
    }

    fn config(url: String) -> SocketConfig {
        SocketConfig {
            url,
            backoff_base: Duration::from_millis(20),
            backoff_max: Duration::from_millis(80),
            idle_limit: Duration::from_millis(150),
        }
    }

    /// One accepted connection on the fake Gemini.
    struct Conn {
        ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        uri: String,
    }

    impl Conn {
        /// Read client frames until every one of `want` has been named in
        /// a message of `method`; returns all params seen.
        async fn expect_method(&mut self, method: &str, want: &[&str]) -> Vec<String> {
            let mut seen: Vec<String> = Vec::new();
            while !want.iter().all(|w| seen.iter().any(|s| s == w)) {
                let msg = tokio::time::timeout(WAIT, self.ws.next())
                    .await
                    .unwrap_or_else(|_| panic!("no {method} naming {want:?}; saw {seen:?}"))
                    .expect("client closed")
                    .expect("read");
                let Message::Text(text) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                assert!(v["id"].is_string(), "request id must be a string: {text}");
                if v["method"] == method {
                    for p in v["params"].as_array().unwrap() {
                        seen.push(p.as_str().unwrap().to_owned());
                    }
                }
            }
            seen
        }

        async fn send(&mut self, line: &str) {
            self.ws.send(Message::Text(line.to_owned().into())).await.unwrap();
        }
    }

    struct FakeGemini {
        listener: TcpListener,
        url: String,
    }

    impl FakeGemini {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            Self { listener, url }
        }

        async fn accept(&self) -> Conn {
            let (stream, _) = tokio::time::timeout(WAIT, self.listener.accept())
                .await
                .expect("the client never connected")
                .unwrap();
            let uri = Arc::new(Mutex::new(String::new()));
            let seen = uri.clone();
            let ws = tokio_tungstenite::accept_hdr_async(stream, move |req: &Request, resp: Response| {
                *seen.lock().unwrap() = req.uri().to_string();
                Ok(resp)
            })
            .await
            .unwrap();
            let uri = uri.lock().unwrap().clone();
            Conn { ws, uri }
        }
    }

    async fn next_event(rx: &mut broadcast::Receiver<WsEvent>) -> WsEvent {
        tokio::time::timeout(WAIT, rx.recv()).await.expect("no event in time").expect("channel closed")
    }

    #[test]
    fn reconnect_waits_double_from_two_seconds_up_to_thirty() {
        let waits: Vec<u64> =
            (0..7).map(|n| backoff_delay(n, BACKOFF_BASE, BACKOFF_MAX).as_secs()).collect();
        assert_eq!(waits, vec![2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(backoff_delay(u32::MAX, BACKOFF_BASE, BACKOFF_MAX), BACKOFF_MAX);
    }

    #[test]
    fn account_streams_are_not_public_streams() {
        for ok in [
            "GEMI-BTC05M2611181720-UP@depth@100ms",
            "GEMI-BTC05M2611181720-UP@trade",
            "GEMI-BTC05M2611181720-UP@bookTicker",
            "contractStatus",
            "lukka:LS-LRRR-XBTUSD@indexPrice",
        ] {
            assert!(is_public_stream(ok), "{ok}");
        }
        for refused in ["orders@account", "balances@account", "positions@account", "", "x@ACCOUNT"] {
            assert!(!is_public_stream(refused), "{refused}");
        }
    }

    #[tokio::test]
    async fn it_connects_with_a_full_snapshot_subscribes_and_passes_events_on() {
        let gemini = FakeGemini::start().await;
        let (socket, mut rx) = PublicSocket::spawn_with(config(gemini.url.clone()));
        socket.subscribe(vec!["GEMI-X-UP@trade".into(), "contractStatus".into()]);

        let mut conn = gemini.accept().await;
        assert_eq!(conn.uri, "/?snapshot=-1");
        conn.expect_method("SUBSCRIBE", &["GEMI-X-UP@trade", "contractStatus"]).await;
        assert_eq!(next_event(&mut rx).await, WsEvent::Connected);

        conn.send(&fixture_lines("ack.jsonl")[0]).await;
        conn.send(&fixture_lines("trade.jsonl")[0]).await;
        let WsEvent::Trade { symbol, .. } = next_event(&mut rx).await else {
            panic!("expected the trade");
        };
        assert_eq!(symbol, "GEMI-BTC05M2611181705-UP");
    }

    #[tokio::test]
    async fn a_dropped_connection_is_announced_then_reopened_with_every_stream() {
        let gemini = FakeGemini::start().await;
        let (socket, mut rx) = PublicSocket::spawn_with(config(gemini.url.clone()));
        socket.subscribe(vec!["a@trade".into()]);
        let mut first = gemini.accept().await;
        first.expect_method("SUBSCRIBE", &["a@trade"]).await;
        assert_eq!(next_event(&mut rx).await, WsEvent::Connected);
        socket.subscribe(vec!["b@depth@100ms".into()]);
        first.expect_method("SUBSCRIBE", &["b@depth@100ms"]).await;

        drop(first);
        assert_eq!(next_event(&mut rx).await, WsEvent::Disconnected);

        let mut second = gemini.accept().await;
        let seen = second.expect_method("SUBSCRIBE", &["a@trade", "b@depth@100ms"]).await;
        assert_eq!(seen.len(), 2, "each stream once: {seen:?}");
        assert_eq!(next_event(&mut rx).await, WsEvent::Connected);
    }

    #[tokio::test]
    async fn an_unsubscribed_stream_is_dropped_now_and_not_reopened_later() {
        let gemini = FakeGemini::start().await;
        let (socket, mut rx) = PublicSocket::spawn_with(config(gemini.url.clone()));
        socket.subscribe(vec!["a@trade".into(), "b@trade".into()]);
        let mut first = gemini.accept().await;
        first.expect_method("SUBSCRIBE", &["a@trade", "b@trade"]).await;
        assert_eq!(next_event(&mut rx).await, WsEvent::Connected);

        socket.unsubscribe(vec!["a@trade".into()]);
        first.expect_method("UNSUBSCRIBE", &["a@trade"]).await;
        drop(first);
        assert_eq!(next_event(&mut rx).await, WsEvent::Disconnected);

        let mut second = gemini.accept().await;
        let seen = second.expect_method("SUBSCRIBE", &["b@trade"]).await;
        assert_eq!(seen, vec!["b@trade".to_owned()]);
    }

    #[tokio::test]
    async fn a_skipped_depth_update_reopens_the_connection_for_a_new_snapshot() {
        let gemini = FakeGemini::start().await;
        let (socket, mut rx) = PublicSocket::spawn_with(config(gemini.url.clone()));
        socket.subscribe(vec!["s@depth@100ms".into()]);
        let mut first = gemini.accept().await;
        first.expect_method("SUBSCRIBE", &["s@depth@100ms"]).await;
        assert_eq!(next_event(&mut rx).await, WsEvent::Connected);

        first.send(r#"{"e":"depthUpdate","E":1795022187405302187,"s":"s","U":10,"u":10,"b":[["0.50","5.00"]],"a":[]}"#).await;
        let WsEvent::Depth { snapshot, .. } = next_event(&mut rx).await else { panic!("expected depth") };
        assert!(snapshot);
        // U 12 > last applied 10: an update was missed.
        first.send(r#"{"e":"depthUpdate","E":1795022187405302188,"s":"s","U":12,"u":13,"b":[["0.51","1.00"]],"a":[]}"#).await;
        assert_eq!(next_event(&mut rx).await, WsEvent::Disconnected);

        let mut second = gemini.accept().await;
        second.expect_method("SUBSCRIBE", &["s@depth@100ms"]).await;
        assert_eq!(next_event(&mut rx).await, WsEvent::Connected);
        second.send(r#"{"e":"depthUpdate","E":1795022187405302189,"s":"s","U":40,"u":40,"b":[["0.52","2.00"]],"a":[]}"#).await;
        let WsEvent::Depth { snapshot, .. } = next_event(&mut rx).await else { panic!("expected depth") };
        assert!(snapshot, "the first frame after a reconnect is a snapshot again");
    }

    #[tokio::test]
    async fn silence_on_an_index_stream_counts_as_a_drop() {
        let gemini = FakeGemini::start().await;
        let (socket, mut rx) = PublicSocket::spawn_with(config(gemini.url.clone()));
        socket.subscribe(vec!["lukka:LS-LRRR-XBTUSD@indexPrice".into()]);
        let mut first = gemini.accept().await;
        first.expect_method("SUBSCRIBE", &["lukka:LS-LRRR-XBTUSD@indexPrice"]).await;
        assert_eq!(next_event(&mut rx).await, WsEvent::Connected);
        first.send(&fixture_lines("index.jsonl")[0]).await;
        assert!(matches!(next_event(&mut rx).await, WsEvent::Index { .. }));
        // The fake says nothing more but keeps the connection open.
        assert_eq!(next_event(&mut rx).await, WsEvent::Disconnected);
        let _second = gemini.accept().await;
        drop(first);
    }

    #[tokio::test]
    async fn a_refused_stream_name_is_never_sent() {
        let gemini = FakeGemini::start().await;
        let (socket, _rx) = PublicSocket::spawn_with(config(gemini.url.clone()));
        socket.subscribe(vec!["orders@account".into(), "a@trade".into()]);
        let mut conn = gemini.accept().await;
        let seen = conn.expect_method("SUBSCRIBE", &["a@trade"]).await;
        assert_eq!(seen, vec!["a@trade".to_owned()]);
    }

    #[tokio::test]
    async fn it_keeps_trying_while_gemini_is_unreachable() {
        // Reserve a port, then close it: connections are refused.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let (socket, mut rx) = PublicSocket::spawn_with(config(format!("ws://{addr}")));
        socket.subscribe(vec!["a@trade".into()]);
        tokio::time::sleep(Duration::from_millis(120)).await;
        // Now Gemini comes up on the same port.
        let listener = TcpListener::bind(addr).await.unwrap();
        let gemini = FakeGemini { listener, url: format!("ws://{addr}") };
        let mut conn = gemini.accept().await;
        conn.expect_method("SUBSCRIBE", &["a@trade"]).await;
        assert_eq!(next_event(&mut rx).await, WsEvent::Connected);
    }
}
