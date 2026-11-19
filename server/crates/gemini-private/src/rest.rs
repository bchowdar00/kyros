//! Private REST client for Gemini's prediction-market API.
//!
//! Only five read/cancel paths are reachable; there is deliberately no
//! place-order or terms-accept path here. Every request carries the standard
//! Gemini private headers with an empty body.

use crate::auth::{ApiKey, Nonce};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::time::Duration;

/// Active orders path.
pub const PATH_ACTIVE: &str = "/v1/prediction-markets/orders/active";
/// Order history path.
pub const PATH_HISTORY: &str = "/v1/prediction-markets/orders/history";
/// Positions path.
pub const PATH_POSITIONS: &str = "/v1/prediction-markets/positions";
/// Cancel-order path.
pub const PATH_CANCEL: &str = "/v1/prediction-markets/order/cancel";
/// Terms-status path.
pub const PATH_TERMS_STATUS: &str = "/v1/prediction-markets/terms/status";

/// True only for the five allowlisted paths.
pub fn is_allowed(path: &str) -> bool {
    path == PATH_ACTIVE
        || path == PATH_HISTORY
        || path == PATH_POSITIONS
        || path == PATH_CANCEL
        || path == PATH_TERMS_STATUS
}

/// Errors from the private REST client. No variant stores a header value.
#[derive(Debug, thiserror::Error)]
pub enum RestError {
    /// The requested path is not on the allowlist (checked before any network).
    #[error("path not allowed: {0}")]
    NotAllowed(String),
    /// A transport-level failure (connect, timeout, read).
    #[error("transport error: {0}")]
    Transport(String),
    /// A non-2xx HTTP response. `body` is the first 300 characters.
    #[error("http status {status}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// First 300 characters of the response body.
        body: String,
    },
    /// The response body could not be decoded.
    #[error("decode error: {0}")]
    Decode(String),
}

/// REST order status (`open` / `filled` / `cancelled`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestOrderStatus {
    /// `open`.
    Open,
    /// `filled`.
    Filled,
    /// `cancelled`.
    Cancelled,
    /// Any other status value.
    Other(String),
}

/// An order returned by the active/history endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestOrder {
    /// `orderId`.
    pub order_id: u64,
    /// `clientOrderId`.
    pub client_order_id: Option<String>,
    /// `status`.
    pub status: RestOrderStatus,
    /// `symbol`.
    pub symbol: String,
    /// `quantity` (zero when absent).
    pub quantity: Decimal,
    /// `filledQuantity` (zero when absent).
    pub filled_quantity: Decimal,
    /// `remainingQuantity` (zero when absent).
    pub remaining_quantity: Decimal,
    /// `price`.
    pub price: Option<Decimal>,
    /// `avgExecutionPrice`.
    pub avg_execution_price: Option<Decimal>,
}

/// A position returned by the positions endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestPosition {
    /// `symbol`.
    pub symbol: String,
    /// `outcome` (`yes` / `no`), carried verbatim.
    pub outcome: Option<String>,
    /// `totalQuantity` (zero when absent).
    pub total_quantity: Decimal,
    /// `avgPrice`.
    pub avg_price: Option<Decimal>,
}

/// A private REST client bound to a base URL and key.
pub struct PrivateRest {
    base: String,
    key: ApiKey,
    client: reqwest::Client,
    nonce: Nonce,
    /// One request at a time: Gemini wants nonces to arrive in order, and
    /// two requests in flight together can overtake each other.
    turn: tokio::sync::Mutex<()>,
}

impl PrivateRest {
    /// Build a client (10 second timeout, no redirects) against `base`.
    pub fn new(base: impl Into<String>, key: ApiKey) -> PrivateRest {
        PrivateRest::with_nonce(base, key, Nonce::new())
    }

    /// As [`PrivateRest::new`], drawing nonces from `nonce`. Whatever else
    /// signs with the same key (the order socket) must share it.
    pub fn with_nonce(base: impl Into<String>, key: ApiKey, nonce: Nonce) -> PrivateRest {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client builds");
        PrivateRest {
            base: base.into(),
            key,
            client,
            nonce,
            turn: tokio::sync::Mutex::new(()),
        }
    }

    /// Currently open orders, optionally filtered by `symbol`.
    pub async fn active_orders(&self, symbol: &str) -> Result<Vec<RestOrder>, RestError> {
        let body = self
            .send(
                reqwest::Method::POST,
                PATH_ACTIVE,
                // The default page is 50 orders; 100 is the most.
                serde_json::json!({ "symbol": symbol, "limit": 100 }),
            )
            .await?;
        let parsed: OrdersBody = decode(&body)?;
        Ok(parsed.orders.into_iter().map(RestOrder::from).collect())
    }

    /// Historical (filled or cancelled) orders.
    pub async fn order_history(
        &self,
        symbol: &str,
        limit: u32,
    ) -> Result<Vec<RestOrder>, RestError> {
        let body = self
            .send(
                reqwest::Method::POST,
                PATH_HISTORY,
                serde_json::json!({ "symbol": symbol, "limit": limit }),
            )
            .await?;
        let parsed: OrdersBody = decode(&body)?;
        Ok(parsed.orders.into_iter().map(RestOrder::from).collect())
    }

    /// Open positions for the account.
    pub async fn positions(&self) -> Result<Vec<RestPosition>, RestError> {
        let body = self
            .send(reqwest::Method::POST, PATH_POSITIONS, serde_json::json!({}))
            .await?;
        let parsed: PositionsBody = decode(&body)?;
        Ok(parsed
            .positions
            .into_iter()
            .map(RestPosition::from)
            .collect())
    }

    /// Cancel an order by id.
    pub async fn cancel(&self, order_id: u64) -> Result<(), RestError> {
        self.send(
            reqwest::Method::POST,
            PATH_CANCEL,
            serde_json::json!({ "orderId": order_id }),
        )
        .await?;
        Ok(())
    }

    /// Whether the account has accepted the latest prediction-market terms.
    pub async fn terms_accepted(&self) -> Result<bool, RestError> {
        let body = self
            .send(reqwest::Method::GET, PATH_TERMS_STATUS, serde_json::json!({}))
            .await?;
        let parsed: TermsBody = decode(&body)?;
        Ok(parsed.has_accepted_latest)
    }

    /// Sign and send a request: empty body, standard private headers, and a
    /// payload of `{"request": path, "nonce": <unix seconds>, ...extra}`.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &'static str,
        extra: serde_json::Value,
    ) -> Result<Vec<u8>, RestError> {
        if !is_allowed(path) {
            return Err(RestError::NotAllowed(path.to_string()));
        }
        let _turn = self.turn.lock().await;
        let nonce = self.nonce.next();
        // More than a request a second, kept up, would carry the nonce
        // away from the clock; the request waits here instead, holding
        // its turn, so the ones behind it wait too.
        let wait = crate::auth::nonce_wait(nonce, crate::auth::unix_seconds());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        let mut payload = serde_json::Map::new();
        payload.insert("request".into(), serde_json::Value::String(path.to_string()));
        payload.insert("nonce".into(), serde_json::Value::Number(nonce.into()));
        if let serde_json::Value::Object(map) = extra {
            for (k, v) in map {
                payload.insert(k, v);
            }
        }
        let headers = self.key.rest_headers(&serde_json::Value::Object(payload));

        let resp = self
            .client
            .request(method, format!("{}{}", self.base, path))
            .header("Content-Type", "text/plain")
            .header("Content-Length", "0")
            .header("Cache-Control", "no-cache")
            .header("X-GEMINI-APIKEY", headers.apikey)
            .header("X-GEMINI-PAYLOAD", headers.payload)
            .header("X-GEMINI-SIGNATURE", headers.signature)
            .send()
            .await
            .map_err(|e| RestError::Transport(e.to_string()))?;

        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| RestError::Transport(e.to_string()))?;
        if !matches!(status, 200..=299) {
            let body: String = String::from_utf8_lossy(&bytes).chars().take(300).collect();
            return Err(RestError::Http { status, body });
        }
        Ok(bytes.to_vec())
    }
}

fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, RestError> {
    serde_json::from_slice(body).map_err(|e| RestError::Decode(e.to_string()))
}

fn dec_or_zero(value: Option<String>) -> Decimal {
    value
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| Decimal::from_str(s).ok())
        .unwrap_or(Decimal::ZERO)
}

fn opt_dec(value: Option<String>) -> Option<Decimal> {
    value
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| Decimal::from_str(s).ok())
}

#[derive(serde::Deserialize)]
struct OrdersBody {
    #[serde(default)]
    orders: Vec<RawRestOrder>,
}

#[derive(serde::Deserialize)]
struct RawRestOrder {
    #[serde(rename = "orderId")]
    order_id: u64,
    #[serde(rename = "clientOrderId")]
    client_order_id: Option<String>,
    status: Option<String>,
    symbol: String,
    quantity: Option<String>,
    #[serde(rename = "filledQuantity")]
    filled_quantity: Option<String>,
    #[serde(rename = "remainingQuantity")]
    remaining_quantity: Option<String>,
    price: Option<String>,
    #[serde(rename = "avgExecutionPrice")]
    avg_execution_price: Option<String>,
}

impl From<RawRestOrder> for RestOrder {
    fn from(raw: RawRestOrder) -> RestOrder {
        let status = match raw.status.as_deref() {
            Some("open") => RestOrderStatus::Open,
            Some("filled") => RestOrderStatus::Filled,
            Some("cancelled") => RestOrderStatus::Cancelled,
            Some(other) => RestOrderStatus::Other(other.to_string()),
            None => RestOrderStatus::Other(String::new()),
        };
        RestOrder {
            order_id: raw.order_id,
            client_order_id: raw.client_order_id.filter(|s| !s.is_empty()),
            status,
            symbol: raw.symbol,
            quantity: dec_or_zero(raw.quantity),
            filled_quantity: dec_or_zero(raw.filled_quantity),
            remaining_quantity: dec_or_zero(raw.remaining_quantity),
            price: opt_dec(raw.price),
            avg_execution_price: opt_dec(raw.avg_execution_price),
        }
    }
}

#[derive(serde::Deserialize)]
struct PositionsBody {
    #[serde(default)]
    positions: Vec<RawRestPosition>,
}

#[derive(serde::Deserialize)]
struct RawRestPosition {
    symbol: String,
    outcome: Option<String>,
    #[serde(rename = "totalQuantity")]
    total_quantity: Option<String>,
    #[serde(rename = "avgPrice")]
    avg_price: Option<String>,
}

impl From<RawRestPosition> for RestPosition {
    fn from(raw: RawRestPosition) -> RestPosition {
        RestPosition {
            symbol: raw.symbol,
            outcome: raw.outcome.filter(|s| !s.is_empty()),
            total_quantity: dec_or_zero(raw.total_quantity),
            avg_price: opt_dec(raw.avg_price),
        }
    }
}

#[derive(serde::Deserialize)]
struct TermsBody {
    #[serde(rename = "hasAcceptedLatest")]
    has_accepted_latest: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{sign, unix_seconds};
    use base64::Engine;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const SECRET: &str = "kyros-test-secret";

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn b64_decode(s: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode(s).unwrap()
    }

    fn http_response(status_line: &str, body: &str) -> String {
        format!(
            "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Accept one connection, capture the request head, reply with `response`.
    async fn serve_once(
        response: String,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        });
        (addr, handle)
    }

    fn split_request(raw: &str) -> (String, HashMap<String, String>) {
        let head = raw.split("\r\n\r\n").next().unwrap_or("");
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or("").to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        (request_line, headers)
    }

    fn key() -> ApiKey {
        ApiKey::new("account-kyros-demo", SECRET).unwrap()
    }

    #[test]
    fn allowlist() {
        for p in [
            PATH_ACTIVE,
            PATH_HISTORY,
            PATH_POSITIONS,
            PATH_CANCEL,
            PATH_TERMS_STATUS,
        ] {
            assert!(is_allowed(p), "{p} should be allowed");
        }
        for p in [
            "/v1/prediction-markets/order",
            "/v1/prediction-markets/terms/accept",
            "/v1/balances",
        ] {
            assert!(!is_allowed(p), "{p} should be denied");
        }
    }

    #[tokio::test]
    async fn active_orders_request_and_decode() {
        let body = r#"{"orders":[{"orderId":12345678901234567,"clientOrderId":"cli-9","status":"open","symbol":"GEMI-X","quantity":"10","filledQuantity":"4","remainingQuantity":"6","price":"0.48","avgExecutionPrice":null}]}"#;
        let (addr, handle) = serve_once(http_response("HTTP/1.1 200 OK", body)).await;
        let rest = PrivateRest::new(format!("http://{addr}"), key());

        let orders = rest.active_orders("GEMI-X").await.unwrap();
        assert_eq!(orders.len(), 1);
        let o = &orders[0];
        assert_eq!(o.order_id, 12345678901234567);
        assert_eq!(o.client_order_id.as_deref(), Some("cli-9"));
        assert_eq!(o.status, RestOrderStatus::Open);
        assert_eq!(o.symbol, "GEMI-X");
        assert_eq!(o.quantity, dec("10"));
        assert_eq!(o.filled_quantity, dec("4"));
        assert_eq!(o.remaining_quantity, dec("6"));
        assert_eq!(o.price, Some(dec("0.48")));
        assert_eq!(o.avg_execution_price, None);

        let req = handle.await.unwrap();
        let (request_line, headers) = split_request(&req);
        assert_eq!(
            request_line,
            "POST /v1/prediction-markets/orders/active HTTP/1.1"
        );
        for h in [
            "content-type",
            "content-length",
            "cache-control",
            "x-gemini-apikey",
            "x-gemini-payload",
            "x-gemini-signature",
        ] {
            assert!(headers.contains_key(h), "missing header {h}");
        }
        assert_eq!(headers["content-type"], "text/plain");
        assert_eq!(headers["content-length"], "0");
        assert_eq!(headers["cache-control"], "no-cache");
        assert_eq!(headers["x-gemini-apikey"], "account-kyros-demo");

        let payload_b64 = &headers["x-gemini-payload"];
        let decoded = b64_decode(payload_b64);
        let pv: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(pv["request"], PATH_ACTIVE);
        assert_eq!(pv["limit"], 100, "the default page is 50; ask for the most Gemini gives");
        assert!(pv["nonce"].is_number());
        let nonce = pv["nonce"].as_u64().unwrap();
        assert!(
            unix_seconds().abs_diff(nonce) <= 5,
            "nonce {nonce} not within 5s of now"
        );
        assert_eq!(pv["symbol"], "GEMI-X");
        assert_eq!(headers["x-gemini-signature"], sign(SECRET.as_bytes(), payload_b64));
    }

    #[tokio::test]
    async fn order_history_sends_limit() {
        let (addr, handle) = serve_once(http_response("HTTP/1.1 200 OK", r#"{"orders":[]}"#)).await;
        let rest = PrivateRest::new(format!("http://{addr}"), key());
        let orders = rest.order_history("GEMI-X", 25).await.unwrap();
        assert!(orders.is_empty());

        let req = handle.await.unwrap();
        let (request_line, headers) = split_request(&req);
        assert_eq!(
            request_line,
            "POST /v1/prediction-markets/orders/history HTTP/1.1"
        );
        let pv: serde_json::Value =
            serde_json::from_slice(&b64_decode(&headers["x-gemini-payload"])).unwrap();
        assert_eq!(pv["request"], PATH_HISTORY);
        assert_eq!(pv["limit"].as_u64(), Some(25));
    }

    #[tokio::test]
    async fn positions_decode() {
        let body = r#"{"positions":[{"symbol":"GEMI-X","outcome":"yes","totalQuantity":"5","avgPrice":"0.40"},{"symbol":"GEMI-Y"}]}"#;
        let (addr, handle) = serve_once(http_response("HTTP/1.1 200 OK", body)).await;
        let rest = PrivateRest::new(format!("http://{addr}"), key());

        let ps = rest.positions().await.unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].symbol, "GEMI-X");
        assert_eq!(ps[0].outcome.as_deref(), Some("yes"));
        assert_eq!(ps[0].total_quantity, dec("5"));
        assert_eq!(ps[0].avg_price, Some(dec("0.40")));
        // Missing decimal defaults to zero; missing outcome is None.
        assert_eq!(ps[1].total_quantity, dec("0"));
        assert_eq!(ps[1].outcome, None);
        assert_eq!(ps[1].avg_price, None);

        let req = handle.await.unwrap();
        let (request_line, _) = split_request(&req);
        assert_eq!(request_line, "POST /v1/prediction-markets/positions HTTP/1.1");
    }

    fn nonce_of(raw_request: &str) -> u64 {
        let (_, headers) = split_request(raw_request);
        let payload: serde_json::Value = serde_json::from_slice(&b64_decode(&headers["x-gemini-payload"])).unwrap();
        payload["nonce"].as_u64().unwrap()
    }

    #[tokio::test]
    async fn two_requests_in_one_second_carry_different_nonces() {
        let body = r#"{"positions":[]}"#;
        let (a, first) = serve_once(http_response("HTTP/1.1 200 OK", body)).await;
        let (b, second) = serve_once(http_response("HTTP/1.1 200 OK", body)).await;
        // One nonce source for everything signed with the key, as the
        // server wires it: REST reads and the socket's sign-in.
        let nonce = crate::auth::Nonce::new();
        PrivateRest::with_nonce(format!("http://{a}"), key(), nonce.clone()).positions().await.unwrap();
        PrivateRest::with_nonce(format!("http://{b}"), key(), nonce.clone()).positions().await.unwrap();
        let (n1, n2) = (nonce_of(&first.await.unwrap()), nonce_of(&second.await.unwrap()));
        assert!(n2 > n1, "{n1} then {n2}");
        assert!(nonce.next() > n2);
    }

    #[tokio::test]
    async fn a_run_of_requests_cannot_push_the_nonce_far_ahead_of_the_clock() {
        let (addr, _request) = serve_once(http_response("HTTP/1.1 200 OK", r#"{"positions":[]}"#)).await;
        let nonce = crate::auth::Nonce::new();
        // Eight draws inside a second leave the next nonce eight seconds
        // ahead: three more than a request may carry.
        for _ in 0..8 {
            nonce.next();
        }
        let started = std::time::Instant::now();
        PrivateRest::with_nonce(format!("http://{addr}"), key(), nonce.clone()).positions().await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(1_900), "sent after {:?}", started.elapsed());
        assert!(nonce.next() <= crate::auth::unix_seconds() + crate::auth::MAX_NONCE_LEAD + 2);
    }

    #[tokio::test]
    async fn requests_go_out_one_at_a_time_so_nonces_arrive_in_order() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut nonces = Vec::new();
            let mut overlapped = false;
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                nonces.push(nonce_of(&String::from_utf8_lossy(&buf)));
                // While this request is unanswered, no other may arrive.
                if let Ok(Ok((mut early, _))) = tokio::time::timeout(Duration::from_millis(150), listener.accept()).await {
                    overlapped = true;
                    let _ = early.write_all(http_response("HTTP/1.1 200 OK", r#"{"positions":[]}"#).as_bytes()).await;
                }
                sock.write_all(http_response("HTTP/1.1 200 OK", r#"{"positions":[]}"#).as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
                if overlapped {
                    break;
                }
            }
            (nonces, overlapped)
        });
        let rest = std::sync::Arc::new(PrivateRest::new(format!("http://{addr}"), key()));
        let (a, b) = (rest.clone(), rest.clone());
        let (first, second) = tokio::join!(tokio::spawn(async move { a.positions().await }), tokio::spawn(async move { b.positions().await }));
        let (nonces, overlapped) = server.await.unwrap();
        assert!(!overlapped, "a second request was sent while the first was unanswered");
        first.unwrap().unwrap();
        second.unwrap().unwrap();
        assert!(nonces[1] > nonces[0], "{nonces:?}");
    }

    #[tokio::test]
    async fn cancel_sends_numeric_order_id() {
        let (addr, handle) =
            serve_once(http_response("HTTP/1.1 200 OK", r#"{"result":"ok"}"#)).await;
        let rest = PrivateRest::new(format!("http://{addr}"), key());
        rest.cancel(73797746498585286).await.unwrap();

        let req = handle.await.unwrap();
        let (request_line, headers) = split_request(&req);
        assert_eq!(request_line, "POST /v1/prediction-markets/order/cancel HTTP/1.1");
        let pv: serde_json::Value =
            serde_json::from_slice(&b64_decode(&headers["x-gemini-payload"])).unwrap();
        assert_eq!(pv["request"], PATH_CANCEL);
        assert!(pv["orderId"].is_number());
        assert_eq!(pv["orderId"].as_u64(), Some(73797746498585286));
    }

    #[tokio::test]
    async fn terms_accepted_is_a_get() {
        let body = r#"{"hasAcceptedLatest":true,"acceptedVersion":3,"latestVersion":3}"#;
        let (addr, handle) = serve_once(http_response("HTTP/1.1 200 OK", body)).await;
        let rest = PrivateRest::new(format!("http://{addr}"), key());
        assert!(rest.terms_accepted().await.unwrap());

        let req = handle.await.unwrap();
        let (request_line, _) = split_request(&req);
        assert_eq!(request_line, "GET /v1/prediction-markets/terms/status HTTP/1.1");
    }

    #[tokio::test]
    async fn http_400_maps_to_http_error() {
        let (addr, handle) =
            serve_once(http_response("HTTP/1.1 400 Bad Request", r#"{"error":"nope"}"#)).await;
        let rest = PrivateRest::new(format!("http://{addr}"), key());
        let err = rest.active_orders("GEMI-X").await.unwrap_err();
        match err {
            RestError::Http { status, .. } => assert_eq!(status, 400),
            other => panic!("expected Http 400, got {other:?}"),
        }
        let _ = handle.await;
    }
}
