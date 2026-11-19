//! WebSocket JSON shapes for Gemini's authenticated prediction-market socket.
//!
//! Outbound request builders return compact JSON text; inbound frames are
//! parsed with [`parse_inbound`]. All monetary decimals travel as JSON
//! strings and are carried here as [`rust_decimal::Decimal`].

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Order side, serialized as Gemini's `BUY` / `SELL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireSide {
    /// `BUY`.
    #[serde(rename = "BUY")]
    Buy,
    /// `SELL`.
    #[serde(rename = "SELL")]
    Sell,
}

/// Time in force, serialized as `GTC` / `IOC` / `FOK` / `MOC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireTif {
    /// `GTC`.
    #[serde(rename = "GTC")]
    Gtc,
    /// `IOC`.
    #[serde(rename = "IOC")]
    Ioc,
    /// `FOK`.
    #[serde(rename = "FOK")]
    Fok,
    /// `MOC`.
    #[serde(rename = "MOC")]
    Moc,
}

/// Event outcome, serialized as `YES` / `NO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireOutcome {
    /// `YES`.
    #[serde(rename = "YES")]
    Yes,
    /// `NO`.
    #[serde(rename = "NO")]
    No,
}

/// Parameters for an `order.place` request. Always a `LIMIT` order.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaceParams {
    /// Contract symbol.
    pub symbol: String,
    /// Order side.
    pub side: WireSide,
    /// Time in force.
    pub tif: WireTif,
    /// Limit price.
    pub price: Decimal,
    /// Order quantity.
    pub quantity: Decimal,
    /// Caller-assigned client order id.
    pub client_order_id: String,
    /// Event outcome being traded.
    pub outcome: WireOutcome,
}

/// Order status parsed from the `X` field of an order event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderStatus {
    /// `NEW`.
    New,
    /// `OPEN`.
    Open,
    /// `FILLED`.
    Filled,
    /// `PARTIALLY_FILLED`.
    PartiallyFilled,
    /// `CANCELED`.
    Canceled,
    /// `REJECTED`.
    Rejected,
    /// `MODIFIED`.
    Modified,
    /// Any other (forward-compatible) status value.
    Other(String),
}

/// A parsed `orderUpdate` event. Missing or empty fields are `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderUpdate {
    /// `s`: symbol.
    pub symbol: String,
    /// `i`: order id (survives values larger than 2^53 exactly).
    pub order_id: u64,
    /// `c`: client order id.
    pub client_order_id: Option<String>,
    /// `X`: status.
    pub status: OrderStatus,
    /// `S`: side.
    pub side: Option<WireSide>,
    /// `O`: event outcome.
    pub outcome: Option<WireOutcome>,
    /// `p`: order price.
    pub price: Option<Decimal>,
    /// `q`: original quantity.
    pub original_qty: Option<Decimal>,
    /// `z`: remaining quantity.
    pub remaining_qty: Option<Decimal>,
    /// `Z`: executed quantity (last or cumulative, per event).
    pub executed_qty: Option<Decimal>,
    /// `L`: last execution price.
    pub last_price: Option<Decimal>,
    /// `t`: trade id (survives values larger than 2^53 exactly).
    pub trade_id: Option<u64>,
    /// `r`: rejection/cancellation reason.
    pub reason: Option<String>,
    /// `E`: event time in nanoseconds.
    pub event_time_ns: Option<u64>,
}

/// A request/response reply frame. `result` is intentionally ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Matches the request id (accepted as a JSON string or number).
    pub id: u64,
    /// HTTP-style status code.
    pub status: u16,
    /// `error.code`, when present.
    pub error_code: Option<i64>,
    /// `error.msg`, when present.
    pub error_msg: Option<String>,
}

/// An inbound socket frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// A request/response reply.
    Response(Response),
    /// An `orderUpdate` event.
    OrderUpdate(OrderUpdate),
    /// Any other valid JSON frame.
    Other,
}

/// Request envelope shared by every outbound method.
#[derive(Serialize)]
struct Envelope<P: Serialize> {
    id: String,
    method: &'static str,
    params: P,
}

#[derive(Serialize)]
struct PlaceWire<'a> {
    symbol: &'a str,
    side: WireSide,
    #[serde(rename = "type")]
    order_type: &'static str,
    #[serde(rename = "timeInForce")]
    time_in_force: WireTif,
    price: String,
    quantity: String,
    #[serde(rename = "clientOrderId")]
    client_order_id: &'a str,
    #[serde(rename = "eventOutcome")]
    event_outcome: WireOutcome,
}

#[derive(Serialize)]
struct CancelWire {
    #[serde(rename = "orderId")]
    order_id: u64,
}

/// Build an `order.place` request. `id` is encoded as a JSON string; `price`
/// and `quantity` are encoded as plain decimal strings.
pub fn place_request(id: u64, p: &PlaceParams) -> String {
    let env = Envelope {
        id: id.to_string(),
        method: "order.place",
        params: PlaceWire {
            symbol: &p.symbol,
            side: p.side,
            order_type: "LIMIT",
            time_in_force: p.tif,
            price: p.price.to_string(),
            quantity: p.quantity.to_string(),
            client_order_id: &p.client_order_id,
            event_outcome: p.outcome,
        },
    };
    serde_json::to_string(&env).expect("order.place request serializes")
}

/// Build an `order.cancel` request. `orderId` is a JSON number.
pub fn cancel_request(id: u64, order_id: u64) -> String {
    let env = Envelope {
        id: id.to_string(),
        method: "order.cancel",
        params: CancelWire { order_id },
    };
    serde_json::to_string(&env).expect("order.cancel request serializes")
}

/// Build a `SUBSCRIBE` request whose params are the stream array.
pub fn subscribe_request(id: u64, streams: &[&str]) -> String {
    let env = Envelope {
        id: id.to_string(),
        method: "SUBSCRIBE",
        params: streams,
    };
    serde_json::to_string(&env).expect("SUBSCRIBE request serializes")
}

/// A non-empty, parseable decimal field, else `None`.
fn opt_decimal(value: &Option<String>) -> Option<Decimal> {
    value
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| Decimal::from_str(s).ok())
}

fn opt_side(value: &Option<String>) -> Option<WireSide> {
    match value.as_deref() {
        Some("BUY") => Some(WireSide::Buy),
        Some("SELL") => Some(WireSide::Sell),
        _ => None,
    }
}

fn opt_outcome(value: &Option<String>) -> Option<WireOutcome> {
    match value.as_deref() {
        Some("YES") => Some(WireOutcome::Yes),
        Some("NO") => Some(WireOutcome::No),
        _ => None,
    }
}

fn order_status(x: &str) -> OrderStatus {
    match x {
        "NEW" => OrderStatus::New,
        "OPEN" => OrderStatus::Open,
        "FILLED" => OrderStatus::Filled,
        "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
        "CANCELED" => OrderStatus::Canceled,
        "REJECTED" => OrderStatus::Rejected,
        "MODIFIED" => OrderStatus::Modified,
        other => OrderStatus::Other(other.to_string()),
    }
}

#[derive(Deserialize)]
struct RawOrderUpdate {
    s: Option<String>,
    i: Option<u64>,
    c: Option<String>,
    #[serde(rename = "X")]
    x: Option<String>,
    #[serde(rename = "S")]
    side: Option<String>,
    #[serde(rename = "O")]
    outcome: Option<String>,
    p: Option<String>,
    q: Option<String>,
    z: Option<String>,
    #[serde(rename = "Z")]
    executed: Option<String>,
    #[serde(rename = "L")]
    last_price: Option<String>,
    t: Option<u64>,
    r: Option<String>,
    #[serde(rename = "E")]
    event_time: Option<u64>,
}

/// Id that may arrive as a JSON string or number.
#[derive(Deserialize)]
#[serde(untagged)]
enum IdField {
    Num(u64),
    Str(String),
}

#[derive(Deserialize)]
struct RawError {
    code: Option<i64>,
    msg: Option<String>,
}

#[derive(Deserialize)]
struct RawResponse {
    id: IdField,
    status: u16,
    error: Option<RawError>,
}

fn order_update(value: serde_json::Value) -> Result<OrderUpdate, String> {
    let raw: RawOrderUpdate =
        serde_json::from_value(value).map_err(|e| format!("bad orderUpdate: {e}"))?;
    let symbol = raw
        .s
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "orderUpdate missing s".to_string())?;
    let order_id = raw.i.ok_or_else(|| "orderUpdate missing i".to_string())?;
    let x = raw
        .x
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "orderUpdate missing X".to_string())?;
    Ok(OrderUpdate {
        symbol,
        order_id,
        client_order_id: raw.c.filter(|s| !s.is_empty()),
        status: order_status(&x),
        side: opt_side(&raw.side),
        outcome: opt_outcome(&raw.outcome),
        price: opt_decimal(&raw.p),
        original_qty: opt_decimal(&raw.q),
        remaining_qty: opt_decimal(&raw.z),
        executed_qty: opt_decimal(&raw.executed),
        last_price: opt_decimal(&raw.last_price),
        trade_id: raw.t,
        reason: raw.r.filter(|s| !s.is_empty()),
        event_time_ns: raw.event_time,
    })
}

/// Classify an inbound socket frame.
pub fn parse_inbound(text: &str) -> Result<Inbound, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let obj = match value.as_object() {
        Some(obj) => obj,
        None => return Ok(Inbound::Other),
    };
    if let Some(event) = obj.get("e") {
        if event.as_str() == Some("orderUpdate") {
            return Ok(Inbound::OrderUpdate(order_update(value)?));
        }
        return Ok(Inbound::Other);
    }
    if obj.contains_key("id") && obj.contains_key("status") {
        let raw: RawResponse =
            serde_json::from_value(value).map_err(|e| format!("bad response: {e}"))?;
        let id = match raw.id {
            IdField::Num(n) => n,
            IdField::Str(s) => s.parse::<u64>().map_err(|e| format!("bad response id: {e}"))?,
        };
        let (error_code, error_msg) = match raw.error {
            Some(err) => (err.code, err.msg),
            None => (None, None),
        };
        return Ok(Inbound::Response(Response {
            id,
            status: raw.status,
            error_code,
            error_msg,
        }));
    }
    Ok(Inbound::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn place_request_shape() {
        let p = PlaceParams {
            symbol: "GEMI-X".into(),
            side: WireSide::Buy,
            tif: WireTif::Gtc,
            price: dec("0.48"),
            quantity: dec("10"),
            client_order_id: "cli-1".into(),
            outcome: WireOutcome::Yes,
        };
        let v: serde_json::Value = serde_json::from_str(&place_request(42, &p)).unwrap();
        assert_eq!(v["id"], serde_json::json!("42"));
        assert_eq!(v["method"], "order.place");
        let params = &v["params"];
        assert_eq!(params["symbol"], "GEMI-X");
        assert_eq!(params["side"], "BUY");
        assert_eq!(params["type"], "LIMIT");
        assert_eq!(params["timeInForce"], "GTC");
        assert_eq!(params["clientOrderId"], "cli-1");
        assert_eq!(params["eventOutcome"], "YES");
        // price/quantity must be JSON strings with no trailing zeros/exponent.
        assert!(params["price"].is_string());
        assert_eq!(params["price"], "0.48");
        assert!(params["quantity"].is_string());
        assert_eq!(params["quantity"], "10");
    }

    #[test]
    fn cancel_request_numeric_order_id() {
        let v: serde_json::Value =
            serde_json::from_str(&cancel_request(7, 73797746498585286)).unwrap();
        assert_eq!(v["method"], "order.cancel");
        let order_id = &v["params"]["orderId"];
        assert!(order_id.is_number());
        assert!(!order_id.is_string());
        assert_eq!(order_id.as_u64(), Some(73797746498585286));
    }

    #[test]
    fn subscribe_request_array_params() {
        let v: serde_json::Value =
            serde_json::from_str(&subscribe_request(9, &["orders@account", "balances@account"]))
                .unwrap();
        assert_eq!(v["method"], "SUBSCRIBE");
        assert_eq!(v["id"], serde_json::json!("9"));
        assert_eq!(
            v["params"],
            serde_json::json!(["orders@account", "balances@account"])
        );
    }

    #[test]
    fn parse_order_event_new() {
        let text = r#"{"e":"orderUpdate","E":1759291847686856569,"s":"GEMI-BTC05M2606011000-UP","i":73797746498585286,"c":"btc-5m-quote-001","S":"BUY","o":"LIMIT","X":"NEW","O":"YES","p":"0.48000","q":"10","z":"10","T":1759291847686856569}"#;
        let Inbound::OrderUpdate(u) = parse_inbound(text).unwrap() else {
            panic!("expected OrderUpdate");
        };
        assert_eq!(u.symbol, "GEMI-BTC05M2606011000-UP");
        assert_eq!(u.order_id, 73797746498585286);
        assert_eq!(u.client_order_id.as_deref(), Some("btc-5m-quote-001"));
        assert_eq!(u.status, OrderStatus::New);
        assert_eq!(u.side, Some(WireSide::Buy));
        assert_eq!(u.outcome, Some(WireOutcome::Yes));
        assert_eq!(u.price, Some(dec("0.48000")));
        assert_eq!(u.original_qty, Some(dec("10")));
        assert_eq!(u.remaining_qty, Some(dec("10")));
        assert_eq!(u.executed_qty, None);
        assert_eq!(u.last_price, None);
        assert_eq!(u.trade_id, None);
        assert_eq!(u.event_time_ns, Some(1759291847686856569));
    }

    #[test]
    fn parse_order_event_canceled() {
        let text = r#"{"e":"orderUpdate","E":1759291847731455006,"s":"GEMI-BTC05M2606011000-UP","i":73797746498585286,"c":"btc-5m-quote-001","X":"CANCELED","T":1759291847731455006}"#;
        let Inbound::OrderUpdate(u) = parse_inbound(text).unwrap() else {
            panic!("expected OrderUpdate");
        };
        assert_eq!(u.order_id, 73797746498585286);
        assert_eq!(u.status, OrderStatus::Canceled);
        assert_eq!(u.side, None);
        assert_eq!(u.outcome, None);
        assert_eq!(u.price, None);
        assert_eq!(u.original_qty, None);
        assert_eq!(u.event_time_ns, Some(1759291847731455006));
    }

    #[test]
    fn parse_response_id_string_and_number() {
        let Inbound::Response(r) =
            parse_inbound(r#"{"id":"7","status":200,"result":{"ok":true}}"#).unwrap()
        else {
            panic!("expected Response");
        };
        assert_eq!(r.id, 7);
        assert_eq!(r.status, 200);
        assert_eq!(r.error_code, None);

        let Inbound::Response(r2) = parse_inbound(r#"{"id":7,"status":200}"#).unwrap() else {
            panic!("expected Response");
        };
        assert_eq!(r2.id, 7);
    }

    #[test]
    fn parse_error_response() {
        let text = r#"{"id":"1","status":401,"error":{"code":-1002,"msg":"Authentication required"}}"#;
        let Inbound::Response(r) = parse_inbound(text).unwrap() else {
            panic!("expected Response");
        };
        assert_eq!(r.id, 1);
        assert_eq!(r.status, 401);
        assert_eq!(r.error_code, Some(-1002));
        assert_eq!(r.error_msg.as_deref(), Some("Authentication required"));
    }

    #[test]
    fn parse_depth_update_is_other() {
        let text = r#"{"e":"depthUpdate","E":123,"s":"GEMI-X","b":[["0.5","10"]],"a":[]}"#;
        assert_eq!(parse_inbound(text).unwrap(), Inbound::Other);
    }

    #[test]
    fn parse_order_update_missing_order_id_is_err() {
        let text = r#"{"e":"orderUpdate","E":1,"s":"GEMI-X","X":"NEW"}"#;
        assert!(parse_inbound(text).is_err());
    }

    #[test]
    fn parse_invalid_json_is_err() {
        assert!(parse_inbound("{not json").is_err());
    }
}
