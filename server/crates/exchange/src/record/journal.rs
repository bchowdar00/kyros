//! The journal of one deployment's round: one line per order event, in
//! the engine's order, as bytes that never change for the same events.
//!
//! Each line is one JSON object with its keys in alphabetical order and
//! no whitespace, ended by a newline:
//!
//! ```text
//! {"client_order_id":"t","event":"fill","filled":5,"order":2,"price":600,"remaining":0,"round":1,"seq":0}
//! ```
//!
//! `event` is `fill`, `partial`, `reject` (with a `reason`), `cancel_ack`,
//! `expiry` or `replaced`; `price` is tenths of a cent on the Up book;
//! `order` is the engine's order id; `seq` counts the round's lines from
//! zero. There is no clock time in a line, so replaying the same events
//! gives the same bytes and the same SHA-256.

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::engine::events::{ExecKind, ExecReject, ExecutionEvent};

/// `0x` and 64 lowercase hex digits.
pub fn hex32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(66);
    out.push_str("0x");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn reason(reject: ExecReject) -> &'static str {
    match reject {
        ExecReject::ZeroQuantity => "zero_quantity",
        ExecReject::UnknownAccount => "unknown_account",
        ExecReject::InsufficientBalance => "insufficient_balance",
        ExecReject::NoLiquidity => "no_liquidity",
        ExecReject::SeriesMismatch => "series_mismatch",
        ExecReject::PostOnlyWouldCross => "post_only_would_cross",
        ExecReject::ReduceOnlyNoPosition => "reduce_only_no_position",
        ExecReject::MarketClosed => "market_closed",
        ExecReject::VenueRefused => "venue_refused",
        ExecReject::VenueUnreachable => "venue_unreachable",
    }
}

/// One deployment's journal for one round.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Journal {
    bytes: Vec<u8>,
    lines: u32,
    fills: u32,
}

impl Journal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append the line for `event`, which happened in round `round`.
    pub fn push(&mut self, round: u32, event: &ExecutionEvent) {
        let (name, why) = match event.kind {
            ExecKind::Fill => ("fill", None),
            ExecKind::Partial => ("partial", None),
            ExecKind::Reject(reject) => ("reject", Some(reason(reject))),
            ExecKind::CancelAck => ("cancel_ack", None),
            ExecKind::Expiry => ("expiry", None),
            ExecKind::Replaced => ("replaced", None),
        };
        // A sorted map, so the key order does not depend on how
        // `serde_json` is built.
        let mut line: BTreeMap<&str, Value> = BTreeMap::new();
        line.insert("client_order_id", Value::from(&*event.client_order_id));
        line.insert("event", Value::from(name));
        line.insert("filled", Value::from(event.filled_qty.0));
        line.insert("order", event.venue_order_id.map_or(Value::Null, |id| Value::from(id.0)));
        line.insert("price", event.price.map_or(Value::Null, |p| Value::from(p.tenths())));
        if let Some(why) = why {
            line.insert("reason", Value::from(why));
        }
        line.insert("remaining", Value::from(event.remaining_qty.0));
        line.insert("round", Value::from(round));
        line.insert("seq", Value::from(self.lines));
        serde_json::to_writer(&mut self.bytes, &line).expect("a map of plain values serialises");
        self.bytes.push(b'\n');
        self.lines += 1;
        if matches!(event.kind, ExecKind::Fill | ExecKind::Partial) {
            self.fills += 1;
        }
    }

    /// The exact bytes the hash is taken over.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn lines(&self) -> u32 {
        self.lines
    }

    pub fn is_empty(&self) -> bool {
        self.lines == 0
    }

    /// Whether any line is a fill, whole or partial.
    pub fn has_fills(&self) -> bool {
        self.fills > 0
    }

    /// SHA-256 of [`Journal::bytes`].
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(&self.bytes).into()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ticks::PriceTick;
    use crate::types::{Address, MarketId, OrderId, Qty, SeriesId};

    pub(crate) fn event(account: u8, cid: &str, kind: ExecKind, filled: u32, remaining: u32, price: Option<u16>) -> ExecutionEvent {
        ExecutionEvent {
            series: SeriesId(0),
            market: Some(MarketId(1)),
            account: Address([account; 20]),
            client_order_id: cid.into(),
            venue_order_id: if matches!(kind, ExecKind::Reject(_)) { None } else { Some(OrderId(2)) },
            kind,
            filled_qty: Qty(filled),
            remaining_qty: Qty(remaining),
            price: price.map(|p| PriceTick::from_tenths(p).unwrap()),
        }
    }

    fn of(events: &[ExecutionEvent]) -> Journal {
        let mut j = Journal::new();
        for e in events {
            j.push(1, e);
        }
        j
    }

    #[test]
    fn every_kind_of_event_is_one_line_with_sorted_keys() {
        let j = of(&[
            event(7, "t", ExecKind::Fill, 5, 0, Some(600)),
            event(7, "m", ExecKind::Partial, 2, 3, Some(550)),
            event(7, "r", ExecKind::Reject(ExecReject::InsufficientBalance), 0, 0, None),
            event(7, "c", ExecKind::CancelAck, 0, 0, None),
            event(7, "e", ExecKind::Expiry, 0, 0, None),
            event(7, "x", ExecKind::Replaced, 0, 0, None),
        ]);
        let text = std::str::from_utf8(j.bytes()).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"client_order_id":"t","event":"fill","filled":5,"order":2,"price":600,"remaining":0,"round":1,"seq":0}"#, "\n",
                r#"{"client_order_id":"m","event":"partial","filled":2,"order":2,"price":550,"remaining":3,"round":1,"seq":1}"#, "\n",
                r#"{"client_order_id":"r","event":"reject","filled":0,"order":null,"price":null,"reason":"insufficient_balance","remaining":0,"round":1,"seq":2}"#, "\n",
                r#"{"client_order_id":"c","event":"cancel_ack","filled":0,"order":2,"price":null,"remaining":0,"round":1,"seq":3}"#, "\n",
                r#"{"client_order_id":"e","event":"expiry","filled":0,"order":2,"price":null,"remaining":0,"round":1,"seq":4}"#, "\n",
                r#"{"client_order_id":"x","event":"replaced","filled":0,"order":2,"price":null,"remaining":0,"round":1,"seq":5}"#, "\n",
            )
        );
        assert_eq!(j.lines(), 6);
        assert!(j.has_fills());
    }

    #[test]
    fn every_reject_reason_has_a_name() {
        use ExecReject::*;
        let names: Vec<String> = [
            ZeroQuantity, UnknownAccount, InsufficientBalance, NoLiquidity, SeriesMismatch, PostOnlyWouldCross, ReduceOnlyNoPosition, MarketClosed,
            VenueRefused, VenueUnreachable,
        ]
        .into_iter()
        .map(|r| {
            let j = of(&[event(1, "a", ExecKind::Reject(r), 0, 0, None)]);
            let v: serde_json::Value = serde_json::from_slice(j.bytes()).unwrap();
            v["reason"].as_str().unwrap().to_owned()
        })
        .collect();
        assert_eq!(
            names,
            ["zero_quantity", "unknown_account", "insufficient_balance", "no_liquidity", "series_mismatch", "post_only_would_cross", "reduce_only_no_position", "market_closed", "venue_refused", "venue_unreachable"]
        );
    }

    #[test]
    fn the_same_events_always_give_the_same_bytes_and_hash() {
        let events = [event(7, "t", ExecKind::Fill, 5, 0, Some(600)), event(7, "c", ExecKind::CancelAck, 0, 0, None)];
        let (a, b) = (of(&events), of(&events));
        assert_eq!(a.bytes(), b.bytes());
        assert_eq!(a.hash(), b.hash());
        // The hash is SHA-256 of the bytes, nothing else.
        assert_eq!(a.hash().as_slice(), Sha256::digest(a.bytes()).as_slice());
    }

    #[test]
    fn a_changed_fill_changes_the_hash() {
        let base = of(&[event(7, "t", ExecKind::Fill, 5, 0, Some(600))]);
        for other in [
            event(7, "t", ExecKind::Fill, 4, 0, Some(600)),
            event(7, "t", ExecKind::Fill, 5, 0, Some(610)),
            event(7, "t", ExecKind::Partial, 5, 0, Some(600)),
            event(7, "u", ExecKind::Fill, 5, 0, Some(600)),
        ] {
            assert_ne!(of(&[other]).hash(), base.hash());
        }
        // So does the order of two events.
        let (x, y) = (event(7, "a", ExecKind::Fill, 1, 0, Some(500)), event(7, "b", ExecKind::Fill, 1, 0, Some(500)));
        assert_ne!(of(&[x.clone(), y.clone()]).hash(), of(&[y, x]).hash());
    }

    #[test]
    fn a_round_with_no_orders_has_an_empty_journal() {
        let j = Journal::new();
        assert!(j.is_empty());
        assert!(j.bytes().is_empty());
        assert!(!j.has_fills());
        let cancels = of(&[event(7, "c", ExecKind::CancelAck, 0, 0, None)]);
        assert!(!cancels.is_empty());
        assert!(!cancels.has_fills(), "a cancel is not a fill");
    }

    #[test]
    fn a_client_order_id_cannot_break_out_of_its_line() {
        let j = of(&[event(7, "a\"\n{\"x\":1}", ExecKind::CancelAck, 0, 0, None)]);
        let text = std::str::from_utf8(j.bytes()).unwrap();
        assert_eq!(text.matches('\n').count(), 1, "one event, one line");
        let v: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(v["client_order_id"], "a\"\n{\"x\":1}");
    }

    #[test]
    fn a_hash_prints_as_bytes32() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0xab;
        bytes[31] = 0x01;
        assert_eq!(hex32(&bytes), format!("0xab{}01", "00".repeat(30)));
    }
}
