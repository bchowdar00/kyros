//! Kraken spot level-3 (order-by-order) BTC/USD book adapter. A checksum
//! mismatch discards the book and demands resubscription rather than repairing
//! uncertain state.

use std::collections::{BTreeMap, HashMap};

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use signals_core::SourceEvent;

use crate::adapter::{GuardAction, RecordGuard, validate_publisher_epoch, validate_timestamp};
use crate::decimal::{decimal_string, update_checksum};
use crate::{
    AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, Provider, RawRecord,
    RecoveryAction,
};

const SYMBOL: &str = "BTC/USD";
const SOURCE_ID: &str = "kraken.spot.btc-usd.book-orders";
const SOURCE_VERSION: &str = "1.0.0";
const MAX_ROWS_PER_MESSAGE: usize = 100_000;
const MAX_ORDER_ID_BYTES: usize = 64;

/// Deterministic, checksum-gated Kraken BTC/USD per-order book state.
pub struct KrakenLevel3Adapter {
    publisher_epoch: String,
    guard: RecordGuard,
    next_sequence: u64,
    book: Level3Book,
}

impl KrakenLevel3Adapter {
    /// Construct a Kraken level-3 BTC/USD adapter.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes. `depth` must equal the depth the
    /// `level3` subscription was made at, since the order-book checksum is
    /// computed over exactly that many levels. Either violation returns
    /// [`AdapterError::InvalidRecord`].
    pub fn new(publisher_epoch: impl Into<String>, depth: u16) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(Provider::KrakenSpot),
            next_sequence: 0,
            book: Level3Book::new(depth)?,
        })
    }

    /// Whether the local order book currently holds a checksum-verified
    /// snapshot. `false` after construction, after a reconnect, and after a
    /// checksum mismatch discarded the book.
    pub fn is_ready(&self) -> bool {
        self.book.ready
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let envelope: Level3Envelope = serde_json::from_str(payload).map_err(payload_error)?;
        if envelope.method.as_deref() == Some("subscribe") {
            return if envelope.success.unwrap_or(false) {
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::SubscriptionAccepted,
                    "Kraken level-three subscription acknowledged",
                )))
            } else {
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::SubscriptionRejected,
                    "Kraken level-three subscription rejected",
                )))
            };
        }
        match envelope.channel.as_deref() {
            Some("heartbeat") => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::Heartbeat,
                "Kraken heartbeat observed",
            ))),
            Some("status") => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::VenueStatus,
                "Kraken engine status observed",
            ))),
            Some("level3") => self.level3(record, envelope),
            Some(_) => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                "retained unknown Kraken level-three channel",
            ))),
            None => Err(AdapterError::InvalidPayload(
                "Kraken level-three message has neither method nor channel".to_owned(),
            )),
        }
    }

    fn level3(
        &mut self,
        record: &RawRecord,
        envelope: Level3Envelope,
    ) -> Result<IngestOutcome, AdapterError> {
        let message_type = envelope
            .message_type
            .as_deref()
            .ok_or_else(|| AdapterError::InvalidPayload("level3 type is missing".to_owned()))?;
        let is_snapshot = match message_type {
            "snapshot" => true,
            "update" => false,
            _ => {
                return Err(AdapterError::InvalidPayload(
                    "level3 type must be snapshot or update".to_owned(),
                ));
            }
        };
        let data = envelope
            .data
            .ok_or_else(|| AdapterError::InvalidPayload("level3 data is missing".to_owned()))?;
        let mut messages: Vec<Level3Message> =
            serde_json::from_str(data.get()).map_err(payload_error)?;
        if messages.len() != 1 {
            return Err(AdapterError::InvalidPayload(format!(
                "level3 message must contain one symbol; received {}",
                messages.len()
            )));
        }
        let message = messages
            .pop()
            .ok_or_else(|| AdapterError::InvalidPayload("level3 data is empty".to_owned()))?;
        if message.symbol != SYMBOL {
            return Err(AdapterError::InvalidPayload(
                "level3 message is not BTC/USD".to_owned(),
            ));
        }
        validate_timestamp(&message.timestamp, "message timestamp")?;
        let row_count = message
            .bids
            .len()
            .checked_add(message.asks.len())
            .ok_or_else(|| {
                AdapterError::InvalidPayload("level3 row count overflowed".to_owned())
            })?;
        if is_snapshot && row_count == 0 {
            self.book.clear();
            return Err(AdapterError::InvalidPayload(
                "Kraken level-three snapshot must contain at least one order".to_owned(),
            ));
        }
        if row_count > MAX_ROWS_PER_MESSAGE {
            return Err(AdapterError::InvalidPayload(format!(
                "level3 message has {row_count} rows; maximum is {MAX_ROWS_PER_MESSAGE}"
            )));
        }

        if !is_snapshot && !self.book.ready {
            return Ok(self.resubscribe_notice(
                FeedNoticeCode::AwaitingSnapshot,
                "Kraken level-three update ignored before a valid snapshot",
            ));
        }

        if is_snapshot {
            self.book.clear();
        }
        let apply_result = (|| {
            let mut changes = Vec::with_capacity(row_count);
            if is_snapshot {
                for order in message.bids {
                    changes.push(self.book.add_snapshot(Side::Bid, order)?);
                }
                for order in message.asks {
                    changes.push(self.book.add_snapshot(Side::Ask, order)?);
                }
            } else {
                for event in message.bids {
                    changes.push(self.book.apply_event(Side::Bid, event)?);
                }
                for event in message.asks {
                    changes.push(self.book.apply_event(Side::Ask, event)?);
                }
            }
            changes.extend(self.book.truncate()?);
            Ok::<_, BookApplyError>(changes)
        })();
        let changes = match apply_result {
            Ok(changes) => changes,
            Err(BookApplyError::InvalidPayload(error)) => {
                // A partially applied provider message is never retained.
                self.book.clear();
                return Err(error);
            }
            Err(BookApplyError::Inconsistent(detail)) => {
                self.book.clear();
                return Ok(self.resubscribe_notice(
                    FeedNoticeCode::StateInconsistent,
                    format!("Kraken level-three state invalidated: {detail}"),
                ));
            }
        };
        let calculated_checksum = self.book.checksum();
        if calculated_checksum != message.checksum {
            self.book.clear();
            return Ok(self.resubscribe_notice(
                FeedNoticeCode::ChecksumMismatch,
                format!(
                    "Kraken level-three checksum mismatch: expected {}, calculated {calculated_checksum}",
                    message.checksum
                ),
            ));
        }
        self.book.ready = true;

        let update_count = u32::try_from(changes.len()).map_err(|_| {
            AdapterError::InvalidPayload("level3 update_count exceeds u32".to_owned())
        })?;
        let update_group = format!(
            "{}:{}:{}",
            record.stream.as_str(),
            record.connection_epoch,
            record.ordinal
        );
        let update_kind = if is_snapshot { "snapshot" } else { "delta" };
        let mut events = Vec::with_capacity(changes.len());
        for (index, change) in changes.into_iter().enumerate() {
            let update_index = u32::try_from(index).map_err(|_| {
                AdapterError::InvalidPayload("level3 update_index exceeds u32".to_owned())
            })?;
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(SYMBOL)),
                ("configured_depth".to_owned(), json!(self.book.depth)),
                ("update_group".to_owned(), json!(&update_group)),
                ("update_index".to_owned(), json!(update_index)),
                ("update_count".to_owned(), json!(update_count)),
                ("update_kind".to_owned(), json!(update_kind)),
                ("event_kind".to_owned(), json!(change.event_kind)),
                ("side".to_owned(), json!(change.side.as_str())),
                ("order_id".to_owned(), json!(change.order.order_id)),
                ("price".to_owned(), json!(change.order.price_text)),
                ("size".to_owned(), json!(change.order.quantity_text)),
                ("order_time".to_owned(), json!(change.order.timestamp)),
                ("book_checksum".to_owned(), json!(message.checksum)),
                ("event_time".to_owned(), json!(&message.timestamp)),
            ]);
            events.push(self.source_event(
                message.timestamp.clone(),
                record.received_at.clone(),
                values,
            )?);
        }
        Ok(IngestOutcome {
            events,
            notices: Vec::new(),
        })
    }

    fn resubscribe_notice(&self, code: FeedNoticeCode, detail: impl Into<String>) -> IngestOutcome {
        IngestOutcome::notice(FeedNotice::recovery(
            code,
            detail,
            RecoveryAction::ResubscribeKrakenLevel3 {
                symbol: SYMBOL.to_owned(),
                depth: self.book.depth,
            },
        ))
    }

    fn source_event(
        &mut self,
        event_time: String,
        received_at: String,
        values: BTreeMap<String, Value>,
    ) -> Result<SourceEvent, AdapterError> {
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(AdapterError::SequenceExhausted)?;
        Ok(SourceEvent {
            schema_version: 1,
            source_id: SOURCE_ID.to_owned(),
            source_version: SOURCE_VERSION.to_owned(),
            publisher_epoch: self.publisher_epoch.clone(),
            sequence,
            event_time,
            received_at,
            values,
        })
    }
}

impl FeedAdapter for KrakenLevel3Adapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "Kraken level-three captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                self.book.clear();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Kraken level-three WebSocket connected; book awaits a checksum-valid snapshot",
                )))
            }
            GuardAction::Disconnected => {
                self.book.clear();
                Ok(IngestOutcome::notice(FeedNotice::recovery(
                    FeedNoticeCode::Disconnected,
                    "Kraken level-three WebSocket disconnected",
                    RecoveryAction::Reconnect,
                )))
            }
            GuardAction::Outbound => Ok(IngestOutcome::default()),
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "Kraken level-three captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

struct Level3Book {
    depth: u16,
    ready: bool,
    bids: BTreeMap<Decimal, Vec<StoredOrder>>,
    asks: BTreeMap<Decimal, Vec<StoredOrder>>,
    locations: HashMap<String, (Side, Decimal)>,
}

impl Level3Book {
    fn new(depth: u16) -> Result<Self, AdapterError> {
        if !matches!(depth, 10 | 100 | 1000) {
            return Err(AdapterError::InvalidRecord(
                "Kraken level-three depth must be 10, 100, or 1000".to_owned(),
            ));
        }
        Ok(Self {
            depth,
            ready: false,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            locations: HashMap::new(),
        })
    }

    fn clear(&mut self) {
        self.ready = false;
        self.bids.clear();
        self.asks.clear();
        self.locations.clear();
    }

    fn add_snapshot(&mut self, side: Side, row: Level3Order) -> Result<Change, BookApplyError> {
        if row.event.is_some() {
            return Err(BookApplyError::invalid(
                "level3 snapshot order unexpectedly contains an event".to_owned(),
            ));
        }
        let order = validate_order(row).map_err(BookApplyError::InvalidPayload)?;
        if order.quantity.is_zero() {
            return Err(BookApplyError::invalid(
                "level3 snapshot order quantity must be positive".to_owned(),
            ));
        }
        self.insert(side, order.clone())?;
        Ok(Change {
            event_kind: "snapshot",
            side,
            order,
        })
    }

    fn apply_event(&mut self, side: Side, row: Level3Order) -> Result<Change, BookApplyError> {
        let event = match row.event.as_deref() {
            Some("add") => "add",
            Some("modify") => "modify",
            Some("delete") => "delete",
            Some(_) => {
                return Err(BookApplyError::invalid(
                    "level3 event must be add, modify, or delete".to_owned(),
                ));
            }
            None => {
                return Err(BookApplyError::invalid(
                    "level3 update order is missing event".to_owned(),
                ));
            }
        };
        let order = validate_order(row).map_err(BookApplyError::InvalidPayload)?;
        if event != "delete" && order.quantity.is_zero() {
            return Err(BookApplyError::invalid(
                "level3 add or modify quantity must be positive".to_owned(),
            ));
        }
        match event {
            "add" => self.insert(side, order.clone())?,
            "modify" => self.modify(side, &order)?,
            "delete" => self.delete(side, &order)?,
            _ => {
                return Err(BookApplyError::invalid(
                    "level3 event is outside the validated set".to_owned(),
                ));
            }
        }
        Ok(Change {
            event_kind: event,
            side,
            order,
        })
    }

    fn insert(&mut self, side: Side, order: StoredOrder) -> Result<(), BookApplyError> {
        if self.locations.contains_key(&order.order_id) {
            return Err(BookApplyError::Inconsistent(
                "add contains an order_id already present in the configured window",
            ));
        }
        self.locations
            .insert(order.order_id.clone(), (side, order.price));
        self.levels_mut(side)
            .entry(order.price)
            .or_default()
            .push(order);
        Ok(())
    }

    fn modify(&mut self, side: Side, replacement: &StoredOrder) -> Result<(), BookApplyError> {
        let (stored_side, stored_price) =
            self.locations.get(&replacement.order_id).copied().ok_or(
                BookApplyError::Inconsistent("modify references an order outside current state"),
            )?;
        if stored_side != side || stored_price != replacement.price {
            return Err(BookApplyError::Inconsistent(
                "modify changes the stored order side or price",
            ));
        }
        let level =
            self.levels_mut(side)
                .get_mut(&stored_price)
                .ok_or(BookApplyError::Inconsistent(
                    "stored price level is missing",
                ))?;
        let stored = level
            .iter_mut()
            .find(|order| order.order_id == replacement.order_id)
            .ok_or(BookApplyError::Inconsistent(
                "stored order is missing from its price queue",
            ))?;
        stored.quantity_text.clone_from(&replacement.quantity_text);
        stored.quantity = replacement.quantity;
        stored.timestamp.clone_from(&replacement.timestamp);
        Ok(())
    }

    fn delete(&mut self, side: Side, provider: &StoredOrder) -> Result<(), BookApplyError> {
        let (stored_side, stored_price) =
            self.locations
                .remove(&provider.order_id)
                .ok_or(BookApplyError::Inconsistent(
                    "delete references an order outside current state",
                ))?;
        if stored_side != side || stored_price != provider.price {
            return Err(BookApplyError::Inconsistent(
                "delete changes the stored order side or price",
            ));
        }
        let levels = self.levels_mut(side);
        let level = levels
            .get_mut(&stored_price)
            .ok_or(BookApplyError::Inconsistent(
                "stored delete price level is missing",
            ))?;
        let position = level
            .iter()
            .position(|order| order.order_id == provider.order_id)
            .ok_or(BookApplyError::Inconsistent(
                "stored delete order is missing from its price queue",
            ))?;
        level.remove(position);
        if level.is_empty() {
            levels.remove(&stored_price);
        }
        Ok(())
    }

    fn truncate(&mut self) -> Result<Vec<Change>, BookApplyError> {
        let mut removed = Vec::new();
        for side in [Side::Bid, Side::Ask] {
            while self.levels(side).len() > usize::from(self.depth) {
                let price = match side {
                    Side::Bid => self.levels(side).keys().next().copied(),
                    Side::Ask => self.levels(side).keys().next_back().copied(),
                }
                .ok_or(BookApplyError::Inconsistent(
                    "depth truncation could not identify an outer price",
                ))?;
                let orders =
                    self.levels_mut(side)
                        .remove(&price)
                        .ok_or(BookApplyError::Inconsistent(
                            "depth truncation could not remove the selected price",
                        ))?;
                for order in orders {
                    self.locations.remove(&order.order_id);
                    removed.push(Change {
                        event_kind: "depth_eviction",
                        side,
                        order,
                    });
                }
            }
        }
        Ok(removed)
    }

    fn checksum(&self) -> u32 {
        let mut hasher = crc32fast::Hasher::new();
        for orders in self.asks.values().take(10) {
            for order in orders {
                update_checksum(&mut hasher, &order.price_text);
                update_checksum(&mut hasher, &order.quantity_text);
            }
        }
        for orders in self.bids.values().rev().take(10) {
            for order in orders {
                update_checksum(&mut hasher, &order.price_text);
                update_checksum(&mut hasher, &order.quantity_text);
            }
        }
        hasher.finalize()
    }

    fn levels(&self, side: Side) -> &BTreeMap<Decimal, Vec<StoredOrder>> {
        match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        }
    }

    fn levels_mut(&mut self, side: Side) -> &mut BTreeMap<Decimal, Vec<StoredOrder>> {
        match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        }
    }
}

#[derive(Debug)]
enum BookApplyError {
    InvalidPayload(AdapterError),
    Inconsistent(&'static str),
}

impl BookApplyError {
    fn invalid(detail: String) -> Self {
        Self::InvalidPayload(AdapterError::InvalidPayload(detail))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Bid,
    Ask,
}

impl Side {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Bid => "bid",
            Self::Ask => "ask",
        }
    }
}

#[derive(Clone)]
struct StoredOrder {
    order_id: String,
    price: Decimal,
    price_text: String,
    quantity_text: String,
    quantity: Decimal,
    timestamp: String,
}

struct Change {
    event_kind: &'static str,
    side: Side,
    order: StoredOrder,
}

fn validate_order(row: Level3Order) -> Result<StoredOrder, AdapterError> {
    if row.order_id.is_empty()
        || row.order_id.len() > MAX_ORDER_ID_BYTES
        || !row.order_id.is_ascii()
    {
        return Err(AdapterError::InvalidPayload(
            "level3 order_id is empty, non-ASCII, or outside bounds".to_owned(),
        ));
    }
    validate_timestamp(&row.timestamp, "order timestamp")?;
    let price_text = decimal_string(&row.limit_price, "limit_price")?;
    let quantity_text = decimal_string(&row.order_qty, "order_qty")?;
    let price = price_text.parse::<Decimal>().map_err(|_| {
        AdapterError::InvalidPayload("level3 limit_price is not representable".to_owned())
    })?;
    if price.is_zero() {
        return Err(AdapterError::InvalidPayload(
            "level3 limit_price must be positive".to_owned(),
        ));
    }
    let quantity = quantity_text.parse::<Decimal>().map_err(|_| {
        AdapterError::InvalidPayload("level3 order_qty is not representable".to_owned())
    })?;
    Ok(StoredOrder {
        order_id: row.order_id,
        price,
        price_text,
        quantity_text,
        quantity,
        timestamp: row.timestamp,
    })
}

fn payload_error(error: serde_json::Error) -> AdapterError {
    AdapterError::InvalidPayload(error.to_string())
}

#[derive(Deserialize)]
struct Level3Envelope {
    channel: Option<String>,
    #[serde(rename = "type")]
    message_type: Option<String>,
    data: Option<Box<RawValue>>,
    method: Option<String>,
    success: Option<bool>,
}

#[derive(Deserialize)]
struct Level3Message {
    symbol: String,
    #[serde(default)]
    bids: Vec<Level3Order>,
    #[serde(default)]
    asks: Vec<Level3Order>,
    checksum: u32,
    timestamp: String,
}

#[derive(Deserialize)]
struct Level3Order {
    event: Option<String>,
    order_id: String,
    limit_price: Box<RawValue>,
    order_qty: Box<RawValue>,
    timestamp: String,
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use crate::{
        CaptureStream, FeedAdapter, FeedNoticeCode, RawRecord, RecordKind, RecoveryAction,
    };

    use super::{KrakenLevel3Adapter, Level3Book, Side, StoredOrder};

    fn record(ordinal: u64, payload: &str) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: crate::Provider::KrakenSpot,
            stream: CaptureStream::KrakenLevel3,
            connection_epoch: 1,
            ordinal,
            received_at: "2026-08-12T12:00:01Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::InboundText,
            payload: Some(payload.to_owned()),
        }
    }

    fn connected() -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: crate::Provider::KrakenSpot,
            stream: CaptureStream::KrakenLevel3,
            connection_epoch: 1,
            ordinal: 1,
            received_at: "2026-08-12T12:00:00Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::Connected,
            payload: None,
        }
    }

    fn ready_adapter() -> KrakenLevel3Adapter {
        let mut adapter = KrakenLevel3Adapter::new("test", 10).expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let outcome = adapter
            .ingest(&record(2, VALID_SNAPSHOT))
            .expect("checksum-valid snapshot is accepted");
        assert_eq!(outcome.events.len(), 2);
        assert!(adapter.is_ready());
        adapter
    }

    #[test]
    fn checksum_failure_clears_ready_state() {
        let mut adapter = KrakenLevel3Adapter::new("test", 10).expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let outcome = adapter
            .ingest(&record(
                2,
                r#"{"channel":"level3","type":"snapshot","data":[{"symbol":"BTC/USD","checksum":0,"timestamp":"2026-08-12T12:00:01Z","bids":[{"order_id":"ORDER-1","limit_price":"60000.0","order_qty":"1.0","timestamp":"2026-08-12T12:00:00Z"}],"asks":[]}]}"#,
            ))
            .expect("checksum mismatch is operational state");
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::ChecksumMismatch);
        assert!(!adapter.is_ready());
    }

    #[test]
    fn update_before_snapshot_fails_closed() {
        let mut adapter = KrakenLevel3Adapter::new("test", 10).expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let outcome = adapter
            .ingest(&record(
                2,
                r#"{"channel":"level3","type":"update","data":[{"symbol":"BTC/USD","checksum":0,"timestamp":"2026-08-12T12:00:01Z","bids":[],"asks":[]}]}"#,
            ))
            .expect("awaiting snapshot is operational state");
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::AwaitingSnapshot);
    }

    #[test]
    fn empty_snapshot_is_rejected_and_does_not_mark_the_book_ready() {
        let mut adapter = KrakenLevel3Adapter::new("test", 10).expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let empty = record(
            2,
            r#"{"channel":"level3","type":"snapshot","data":[{"symbol":"BTC/USD","checksum":0,"timestamp":"2026-08-12T12:00:01Z","bids":[],"asks":[]}]}"#,
        );
        assert!(adapter.ingest(&empty).is_err());
        assert!(!adapter.is_ready());
    }

    #[test]
    fn duplicate_add_invalidates_state_and_requests_resubscription() {
        let mut adapter = ready_adapter();
        let outcome = adapter
            .ingest(&record(
                3,
                r#"{"channel":"level3","type":"update","data":[{"symbol":"BTC/USD","checksum":0,"timestamp":"2026-08-13T12:00:02Z","bids":[{"event":"add","order_id":"BID-1","limit_price":"60000.0","order_qty":"1.5","timestamp":"2026-08-13T12:00:02Z"}],"asks":[]}] }"#,
            ))
            .expect("state inconsistency is recoverable operational state");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
        assert!(matches!(
            outcome.notices[0].recovery,
            Some(RecoveryAction::ResubscribeKrakenLevel3 { depth: 10, .. })
        ));
        assert!(!adapter.is_ready());

        let awaiting = adapter
            .ingest(&record(
                4,
                r#"{"channel":"level3","type":"update","data":[{"symbol":"BTC/USD","checksum":0,"timestamp":"2026-08-13T12:00:03Z","bids":[],"asks":[]}] }"#,
            ))
            .expect("updates remain gated until a new snapshot");
        assert_eq!(awaiting.notices[0].code, FeedNoticeCode::AwaitingSnapshot);
    }

    #[test]
    fn unknown_delete_invalidates_state_and_requests_resubscription() {
        let mut adapter = ready_adapter();
        let outcome = adapter
            .ingest(&record(
                3,
                r#"{"channel":"level3","type":"update","data":[{"symbol":"BTC/USD","checksum":0,"timestamp":"2026-08-13T12:00:02Z","bids":[{"event":"delete","order_id":"MISSING","limit_price":"60000.0","order_qty":"0","timestamp":"2026-08-13T12:00:02Z"}],"asks":[]}] }"#,
            ))
            .expect("state inconsistency is recoverable operational state");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
        assert!(matches!(
            outcome.notices[0].recovery,
            Some(RecoveryAction::ResubscribeKrakenLevel3 { depth: 10, .. })
        ));
        assert!(!adapter.is_ready());
    }

    #[test]
    fn official_level_three_queue_checksum_vector_matches() {
        let mut book = Level3Book::new(10).expect("book constructs");
        for (index, (price, quantity)) in [
            ("44939.4", "0.88968699"),
            ("44939.4", "0.45210000"),
            ("44939.4", "0.10000000"),
            ("44939.4", "0.14296323"),
            ("44939.4", "0.25000000"),
            ("44939.4", "0.10292988"),
            ("44939.4", "0.33880000"),
            ("44939.4", "1.28140860"),
            ("44937.1", "0.03346877"),
            ("44934.7", "0.35630000"),
            ("44930.2", "0.22734299"),
            ("44930.2", "0.01000000"),
            ("44930.2", "0.05550000"),
            ("44930.2", "0.70000000"),
            ("44930.2", "0.15000000"),
            ("44928.0", "0.00105240"),
            ("44919.6", "0.33870000"),
            ("44919.5", "0.07610000"),
            ("44912.0", "0.35630000"),
            ("44909.7", "0.06690000"),
            ("44901.9", "0.00088982"),
        ]
        .into_iter()
        .enumerate()
        {
            insert(&mut book, Side::Bid, index, price, quantity);
        }
        for (index, (price, quantity)) in [
            ("44939.5", "4.52308393"),
            ("44939.5", "0.00111261"),
            ("44939.5", "0.00100000"),
            ("44939.5", "0.01000000"),
            ("44950.0", "0.10334926"),
            ("44953.0", "0.00064537"),
            ("44955.0", "0.00250000"),
            ("44959.6", "0.35630000"),
            ("44959.6", "0.35630000"),
            ("44960.1", "0.00338072"),
            ("44960.2", "0.88967575"),
            ("44967.0", "3.14392283"),
            ("44978.5", "0.06778960"),
            ("44979.2", "0.35630000"),
        ]
        .into_iter()
        .enumerate()
        {
            insert(&mut book, Side::Ask, index + 100, price, quantity);
        }
        assert_eq!(book.checksum(), 1_063_832_831);
    }

    fn insert(book: &mut Level3Book, side: Side, index: usize, price: &str, quantity: &str) {
        book.insert(
            side,
            StoredOrder {
                order_id: format!("ORDER-{index}"),
                price: price.parse::<Decimal>().expect("price parses"),
                price_text: price.to_owned(),
                quantity_text: quantity.to_owned(),
                quantity: quantity.parse::<Decimal>().expect("quantity parses"),
                timestamp: "2024-01-08T12:26:39Z".to_owned(),
            },
        )
        .expect("order inserts");
    }

    const VALID_SNAPSHOT: &str = r#"{"channel":"level3","type":"snapshot","data":[{"symbol":"BTC/USD","checksum":2981471764,"timestamp":"2026-08-13T12:00:01Z","bids":[{"order_id":"BID-1","limit_price":"60000.0","order_qty":"1.0","timestamp":"2026-08-13T12:00:00Z"}],"asks":[{"order_id":"ASK-1","limit_price":"60001.0","order_qty":"2.0","timestamp":"2026-08-13T12:00:00Z"}]}]}"#;
}
