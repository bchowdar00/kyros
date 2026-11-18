//! What the on-chain strategy record is written from.
//!
//! Every deployment gets one record: opened once (the program's hash and
//! its budget), then sealed once per round it placed an order in (won or
//! lost, the round's result in cents, and the SHA-256 of that round's
//! journal). This module keeps the journals and the list of what is still
//! to be written; the recorder process (`chain/recorder`) reads
//! `GET /api/records/pending`, sends each item to the contract and
//! reports the transaction with `POST /api/records/ack`. Anyone can read
//! a record and the exact journal bytes behind a seal.
//!
//! Nothing here is a Gemini price. An `Opened` event carries ids, a hash
//! and a budget; a `Sealed` event carries ids, won, a result in cents and
//! a hash.
//!
//! The record is kept in memory, like the exchange it describes.

pub mod journal;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::automation::runtime::RuntimeHandle;
use crate::engine::events::{ExecutionEvent, ExecutionSink};
use crate::engine::{Exchange, MarketStatus};
use crate::types::{Address, MarketId, SeriesId};
use journal::{Journal, hex32};

/// Journals kept for reading. Older ones keep their seal (hash, result,
/// transaction) and lose their bytes.
pub const JOURNALS_KEPT: usize = 5_000;
/// Items one `pending` answer carries at most.
pub const PENDING_LIMIT: usize = 200;
/// How often resolved rounds are looked for.
pub const SWEEP_EVERY: Duration = Duration::from_secs(1);
/// The header the recorder proves itself with.
pub const TOKEN_HEADER: &str = "x-recorder-token";
/// Where the server reads the recorder's token.
pub const TOKEN_ENV: &str = "RECORDER_TOKEN";

/// A deployment's id on chain: SHA-256 of
/// `kyros.record.v1\n<sub-account>\n<deployment id>`. The sub-account is
/// in it because two users may deploy the same document.
pub fn record_id(account: Address, deployment: &str) -> [u8; 32] {
    Sha256::digest(format!("kyros.record.v1\n{account}\n{deployment}").as_bytes()).into()
}

/// A dollar amount written as a decimal, in whole cents, rounded down.
/// `None` when it is not a plain non-negative decimal or does not fit.
pub fn budget_cents(allocation: &str) -> Option<u64> {
    let (whole, fraction) = allocation.split_once('.').unwrap_or((allocation, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) {
        return None;
    }
    let mut cents = fraction.bytes().take(2).fold(0u64, |acc, b| acc * 10 + u64::from(b - b'0'));
    if fraction.len() == 1 {
        cents *= 10;
    }
    whole.parse::<u64>().ok()?.checked_mul(100)?.checked_add(cents)
}

/// Tenths of a cent to whole cents, toward zero.
pub fn pnl_cents(pnl_tenths: i64) -> i64 {
    pnl_tenths / 10
}

/// `0x` and 64 hex digits, as bytes.
fn parse32(s: &str) -> Option<[u8; 32]> {
    let hex = s.strip_prefix("0x")?;
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// What the record needs to know of a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    /// The sub-account the strategy runs in.
    pub account: Address,
    pub series: SeriesId,
    /// The runtime's id, `<strategy id>:<deploy epoch>`.
    pub deployment: String,
    pub strategy_id: String,
    /// SHA-256 of the strategy document, 64 hex digits.
    pub source_hash: String,
    /// The budget, as the envelope states it (dollars).
    pub allocation: String,
}

/// One thing still to be written on chain. The wire shape is the
/// recorder's (`chain/recorder/src/index.ts`, `validateRecord`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingItem {
    Open { deployment_id: String, program_hash: String, budget_cents: u64 },
    Seal { deployment_id: String, round_id: u64, won: bool, pnl_cents: i64, journal_hash: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SealView {
    pub round_id: u64,
    pub won: bool,
    pub pnl_cents: i64,
    pub journal_hash: String,
    /// The transaction that sealed it; `null` until the recorder reports it.
    pub tx_hash: Option<String>,
    /// Lines in the journal.
    pub lines: u32,
}

/// What `GET /api/records/{id}` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordView {
    /// The id on chain.
    pub deployment_id: String,
    pub account: Address,
    pub series: SeriesId,
    pub deployment: String,
    pub strategy_id: String,
    pub program_hash: String,
    pub budget_cents: u64,
    pub open_tx: Option<String>,
    /// Oldest round first.
    pub seals: Vec<SealView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckError {
    /// The id is not `0x` and 64 hex digits.
    BadId,
    /// The transaction hash is not `0x` and 64 hex digits.
    BadTxHash,
    /// No such record, or no such seal.
    NotFound,
    /// Already acknowledged with a different transaction.
    Conflict,
}

struct Seal {
    won: bool,
    pnl_cents: i64,
    hash: [u8; 32],
    lines: u32,
    /// `None` once more than [`JOURNALS_KEPT`] newer journals exist.
    journal: Option<Vec<u8>>,
    tx: Option<String>,
}

struct Record {
    id: [u8; 32],
    deployment: Deployment,
    budget_cents: u64,
    open_tx: Option<String>,
    seals: BTreeMap<u64, Seal>,
}

#[derive(Default)]
struct Inner {
    records: Vec<Record>,
    by_id: HashMap<[u8; 32], usize>,
    /// The record an account's events belong to now.
    by_account: HashMap<Address, usize>,
    /// Journals of rounds not yet resolved.
    live: HashMap<(Address, SeriesId, MarketId), Journal>,
    /// Seals the recorder has not reported, oldest first.
    unacked: VecDeque<([u8; 32], u64)>,
    /// Seals that still hold their journal bytes, oldest first.
    kept: VecDeque<([u8; 32], u64)>,
}

/// Every deployment's record. Cheap to clone.
#[derive(Clone, Default)]
pub struct RecordBook {
    inner: Arc<Mutex<Inner>>,
}

impl RecordBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// An order event from the engine. Called inside the engine's lock,
    /// so it only appends a line.
    pub fn on_event(&self, event: &ExecutionEvent) {
        // An event raised before a market was consulted belongs to no round.
        let Some(market) = event.market else { return };
        let mut inner = self.lock();
        inner.live.entry((event.account, event.series, market)).or_default().push(market.0, event);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("record book poisoned")
    }

    /// Whether `account` already has a record for `deployment`.
    pub fn knows(&self, account: Address, deployment: &str) -> bool {
        let inner = self.lock();
        inner.by_account.get(&account).is_some_and(|&i| inner.records[i].deployment.deployment == deployment)
    }

    /// Open a record for a deployment, once. A new deployment on the same
    /// sub-account gets a record of its own.
    pub fn note_deployment(&self, deployment: Deployment) {
        let id = record_id(deployment.account, &deployment.deployment);
        let mut inner = self.lock();
        if let Some(&known) = inner.by_id.get(&id) {
            inner.by_account.insert(deployment.account, known);
            return;
        }
        // The contract takes a 32-byte hash and whole cents. A deployment
        // that cannot be written as that is not recorded at all.
        let (Some(_), Some(budget_cents)) = (parse32(&format!("0x{}", deployment.source_hash)), budget_cents(&deployment.allocation)) else {
            tracing::warn!(account = %deployment.account, deployment = %deployment.deployment, "record: deployment has no writable hash or budget");
            return;
        };
        let index = inner.records.len();
        inner.by_id.insert(id, index);
        inner.by_account.insert(deployment.account, index);
        inner.records.push(Record { id, deployment, budget_cents, open_tx: None, seals: BTreeMap::new() });
    }

    /// Rounds with a journal that are not sealed yet.
    pub fn live_rounds(&self) -> Vec<(Address, SeriesId, MarketId)> {
        self.lock().live.keys().copied().collect()
    }

    /// A round with a journal has resolved. `pnl_tenths` is the account's
    /// result for it, when the engine has one. With no result, a journal
    /// without fills is sealed at zero and one with fills is dropped: a
    /// seal never carries a guessed number.
    pub fn settle(&self, account: Address, series: SeriesId, market: MarketId, pnl_tenths: Option<i64>) {
        let mut inner = self.lock();
        let Some(journal) = inner.live.remove(&(account, series, market)) else { return };
        let Some(&index) = inner.by_account.get(&account) else {
            tracing::debug!(%account, market = market.0, "record: a round's events have no deployment record");
            return;
        };
        let pnl_tenths = match pnl_tenths {
            Some(pnl) => pnl,
            None if !journal.has_fills() => 0,
            None => {
                tracing::warn!(%account, market = market.0, "record: a round with fills has no result; not sealed");
                return;
            }
        };
        let round = u64::from(market.0);
        let record = &mut inner.records[index];
        if record.seals.contains_key(&round) {
            return;
        }
        let id = record.id;
        record.seals.insert(
            round,
            Seal {
                won: pnl_tenths > 0,
                pnl_cents: pnl_cents(pnl_tenths),
                hash: journal.hash(),
                lines: journal.lines(),
                journal: Some(journal.bytes().to_vec()),
                tx: None,
            },
        );
        inner.unacked.push_back((id, round));
        inner.kept.push_back((id, round));
        while inner.kept.len() > JOURNALS_KEPT {
            let (old_id, old_round) = inner.kept.pop_front().expect("longer than the cap");
            let old = inner.by_id[&old_id];
            if let Some(seal) = inner.records[old].seals.get_mut(&old_round) {
                seal.journal = None;
            }
        }
    }

    /// Open a record for every deployment and seal every resolved round.
    pub fn sweep(&self, exchange: &Exchange) {
        for account in exchange.accounts_iter() {
            let Some(strategy) = &account.strategy else { continue };
            if self.knows(account.address, &strategy.deployment_id) {
                continue;
            }
            self.note_deployment(Deployment {
                account: account.address,
                series: strategy.series,
                deployment: strategy.deployment_id.clone(),
                strategy_id: strategy.strategy_id.clone(),
                source_hash: strategy.program.source_hash.clone(),
                allocation: strategy.envelope.allocation.as_str().to_owned(),
            });
        }
        for (account, series, market) in self.live_rounds() {
            let still_open = matches!(
                exchange.engine(series).current_market(),
                Some(current) if current.id == market && current.status == MarketStatus::Open
            );
            if still_open {
                continue;
            }
            let result = exchange.position(series, &account).last_market.filter(|r| r.market_id == market);
            self.settle(account, series, market, result.map(|r| r.pnl_tenths));
        }
    }

    /// What is still to be written: opens first, then seals, oldest first.
    pub fn pending(&self, limit: usize) -> Vec<PendingItem> {
        let inner = self.lock();
        let opens = inner.records.iter().filter(|r| r.open_tx.is_none()).map(|r| PendingItem::Open {
            deployment_id: hex32(&r.id),
            program_hash: format!("0x{}", r.deployment.source_hash.to_ascii_lowercase()),
            budget_cents: r.budget_cents,
        });
        let seals = inner.unacked.iter().filter_map(|(id, round)| {
            let seal = inner.records[inner.by_id[id]].seals.get(round)?;
            Some(PendingItem::Seal {
                deployment_id: hex32(id),
                round_id: *round,
                won: seal.won,
                pnl_cents: seal.pnl_cents,
                journal_hash: hex32(&seal.hash),
            })
        });
        opens.chain(seals).take(limit).collect()
    }

    /// The recorder wrote an item. `round_id` is `None` for an open.
    /// Acknowledging twice with the same transaction is fine.
    pub fn ack(&self, deployment_id: &str, round_id: Option<u64>, tx_hash: &str) -> Result<(), AckError> {
        let id = parse32(deployment_id).ok_or(AckError::BadId)?;
        let tx = hex32(&parse32(tx_hash).ok_or(AckError::BadTxHash)?);
        let mut inner = self.lock();
        let index = *inner.by_id.get(&id).ok_or(AckError::NotFound)?;
        let slot = match round_id {
            None => &mut inner.records[index].open_tx,
            Some(round) => &mut inner.records[index].seals.get_mut(&round).ok_or(AckError::NotFound)?.tx,
        };
        match slot {
            Some(known) if *known == tx => Ok(()),
            Some(_) => Err(AckError::Conflict),
            None => {
                *slot = Some(tx);
                if let Some(round) = round_id {
                    inner.unacked.retain(|item| *item != (id, round));
                }
                Ok(())
            }
        }
    }

    fn index_of(inner: &Inner, id: &str) -> Option<usize> {
        match id.len() {
            66 => inner.by_id.get(&parse32(id)?).copied(),
            42 => inner.by_account.get(&id.parse::<Address>().ok()?).copied(),
            _ => None,
        }
    }

    /// A record by its id on chain (`0x` and 64 hex) or by the
    /// sub-account's address (`0x` and 40 hex; its newest record).
    pub fn record(&self, id: &str) -> Option<RecordView> {
        let inner = self.lock();
        let record = &inner.records[Self::index_of(&inner, id)?];
        Some(RecordView {
            deployment_id: hex32(&record.id),
            account: record.deployment.account,
            series: record.deployment.series,
            deployment: record.deployment.deployment.clone(),
            strategy_id: record.deployment.strategy_id.clone(),
            program_hash: format!("0x{}", record.deployment.source_hash.to_ascii_lowercase()),
            budget_cents: record.budget_cents,
            open_tx: record.open_tx.clone(),
            seals: record
                .seals
                .iter()
                .map(|(round, seal)| SealView {
                    round_id: *round,
                    won: seal.won,
                    pnl_cents: seal.pnl_cents,
                    journal_hash: hex32(&seal.hash),
                    tx_hash: seal.tx.clone(),
                    lines: seal.lines,
                })
                .collect(),
        })
    }

    /// The exact bytes a seal's hash was taken over, while they are kept.
    pub fn journal(&self, id: &str, round_id: u64) -> Option<Vec<u8>> {
        let inner = self.lock();
        inner.records[Self::index_of(&inner, id)?].seals.get(&round_id)?.journal.clone()
    }
}

/// Put the engine's order events through `book`, then on to `runtime`.
/// This replaces `EngineVenue::install`; call it once at boot, before any
/// strategy order.
pub fn install_sink(exchange: &Arc<Mutex<Exchange>>, runtime: RuntimeHandle, book: RecordBook) {
    let sink = ExecutionSink::new(move |event| {
        book.on_event(&event);
        runtime.execution(crate::automation::venue::engine::convert(event));
    });
    exchange.lock().expect("exchange mutex poisoned").set_execution_sink(sink);
}

/// Look for resolved rounds every [`SWEEP_EVERY`], forever.
pub fn spawn_sealer(exchange: Arc<Mutex<Exchange>>, book: RecordBook) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            let exchange = exchange.lock().expect("exchange mutex poisoned");
            book.sweep(&exchange);
        }
    })
}

// ─────────────────────────────── Routes ────────────────────────────────

/// Whether a recorder request may proceed. Closed until a token is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// No token is configured: the recorder's routes answer `503`.
    Unconfigured,
    Unauthorized,
    Granted,
}

#[derive(Clone)]
pub struct RecordApi {
    pub book: RecordBook,
    token: Option<Arc<str>>,
}

impl RecordApi {
    /// An empty token is no token.
    pub fn new(book: RecordBook, token: Option<String>) -> Self {
        RecordApi { book, token: token.filter(|t| !t.is_empty()).map(Arc::from) }
    }

    pub fn access(&self, provided: Option<&str>) -> Access {
        match &self.token {
            None => Access::Unconfigured,
            Some(expected) if provided.is_some_and(|p| same_bytes(p.as_bytes(), expected.as_bytes())) => Access::Granted,
            Some(_) => Access::Unauthorized,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AckRequest {
    pub deployment_id: String,
    #[serde(default)]
    pub round_id: Option<u64>,
    pub tx_hash: String,
}

fn refuse(status: StatusCode, code: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": code }))).into_response()
}

/// Compares every byte whatever the first difference; only the length leaks.
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// The refusal for a recorder request that may not proceed, if any.
fn refusal(api: &RecordApi, headers: &HeaderMap) -> Option<Response> {
    let provided = headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok());
    match api.access(provided) {
        Access::Granted => None,
        Access::Unauthorized => Some(refuse(StatusCode::UNAUTHORIZED, "recorder_unauthorized")),
        Access::Unconfigured => Some(refuse(StatusCode::SERVICE_UNAVAILABLE, "recorder_unconfigured")),
    }
}

pub async fn pending_handler(State(api): State<RecordApi>, headers: HeaderMap) -> Response {
    match refusal(&api, &headers) {
        None => Json(api.book.pending(PENDING_LIMIT)).into_response(),
        Some(refused) => refused,
    }
}

pub async fn ack_handler(State(api): State<RecordApi>, headers: HeaderMap, Json(req): Json<AckRequest>) -> Response {
    if let Some(refused) = refusal(&api, &headers) {
        return refused;
    }
    match api.book.ack(&req.deployment_id, req.round_id, &req.tx_hash) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(AckError::BadId) => refuse(StatusCode::BAD_REQUEST, "bad_deployment_id"),
        Err(AckError::BadTxHash) => refuse(StatusCode::BAD_REQUEST, "bad_tx_hash"),
        Err(AckError::NotFound) => refuse(StatusCode::NOT_FOUND, "record_not_found"),
        Err(AckError::Conflict) => refuse(StatusCode::CONFLICT, "already_acknowledged"),
    }
}

pub async fn record_handler(State(api): State<RecordApi>, Path(id): Path<String>) -> Response {
    match api.book.record(&id) {
        Some(view) => Json(view).into_response(),
        None => refuse(StatusCode::NOT_FOUND, "record_not_found"),
    }
}

pub async fn journal_handler(State(api): State<RecordApi>, Path((id, round_id)): Path<(String, u64)>) -> Response {
    match api.book.journal(&id, round_id) {
        Some(bytes) => ([(header::CONTENT_TYPE, "application/x-ndjson")], bytes).into_response(),
        None => refuse(StatusCode::NOT_FOUND, "journal_not_found"),
    }
}

/// The record routes. The recorder's two (`pending`, `ack`) need
/// `RECORDER_TOKEN`; the two reads are public.
pub fn routes<S: Clone + Send + Sync + 'static>(book: RecordBook) -> Router<S> {
    let token = std::env::var(TOKEN_ENV).ok();
    Router::new()
        .route("/api/records/pending", get(pending_handler))
        .route("/api/records/ack", post(ack_handler))
        .route("/api/records/{id}", get(record_handler))
        .route("/api/records/{id}/{round_id}/journal", get(journal_handler))
        .with_state(RecordApi::new(book, token))
}

#[cfg(test)]
mod tests {
    use super::journal::tests::event;
    use super::*;
    use crate::engine::events::{ExecKind, ExecReject};
    use crate::engine::sub_accounts::test_support::named;
    use crate::engine::{PlaceOpts, TradeSource};
    use crate::ticks::PriceTick;
    use crate::types::{DEFAULT_SERIES, Qty, Side, Tenths};

    const ACCOUNT: Address = Address([7; 20]);
    const TX_A: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TX_B: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn deployment(account: Address, epoch: u64) -> Deployment {
        Deployment {
            account,
            series: SeriesId(0),
            deployment: format!("Strategy_0123456789ab:{epoch}"),
            strategy_id: "Strategy_0123456789ab".into(),
            source_hash: "0123456789ab".to_owned() + &"cd".repeat(26),
            allocation: "100".into(),
        }
    }

    fn id_of(account: Address, epoch: u64) -> String {
        hex32(&record_id(account, &format!("Strategy_0123456789ab:{epoch}")))
    }

    /// A book with one deployment and a two-line journal (one fill) in round 1.
    fn book_with_a_round() -> RecordBook {
        let book = RecordBook::new();
        book.note_deployment(deployment(ACCOUNT, 1));
        book.on_event(&event(7, "t", ExecKind::Fill, 5, 0, Some(600)));
        book.on_event(&event(7, "c", ExecKind::CancelAck, 0, 0, None));
        book
    }

    #[test]
    fn an_id_is_a_hash_of_the_sub_account_and_the_deployment() {
        let expected = Sha256::digest(format!("kyros.record.v1\n{ACCOUNT}\nStrategy_ab:3").as_bytes());
        assert_eq!(record_id(ACCOUNT, "Strategy_ab:3").as_slice(), expected.as_slice());
        assert_ne!(record_id(ACCOUNT, "Strategy_ab:3"), record_id(ACCOUNT, "Strategy_ab:4"));
        assert_ne!(record_id(ACCOUNT, "Strategy_ab:3"), record_id(Address([8; 20]), "Strategy_ab:3"));
    }

    #[test]
    fn a_budget_is_whole_cents_rounded_down() {
        assert_eq!(budget_cents("100"), Some(10_000));
        assert_eq!(budget_cents("12.3"), Some(1_230));
        assert_eq!(budget_cents("12.349"), Some(1_234));
        assert_eq!(budget_cents("0.009"), Some(0));
        assert_eq!(budget_cents("0"), Some(0));
        for bad in ["", "-1", "1e3", "abc", "1.2.3", "184467440737095517"] {
            assert_eq!(budget_cents(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_result_is_whole_cents_toward_zero() {
        assert_eq!(pnl_cents(1_234), 123);
        assert_eq!(pnl_cents(-1_239), -123);
        assert_eq!(pnl_cents(9), 0);
        assert_eq!(pnl_cents(-9), 0);
    }

    #[test]
    fn a_deployment_is_opened_once() {
        let book = RecordBook::new();
        assert!(!book.knows(ACCOUNT, "Strategy_0123456789ab:1"));
        book.note_deployment(deployment(ACCOUNT, 1));
        book.note_deployment(deployment(ACCOUNT, 1));
        assert!(book.knows(ACCOUNT, "Strategy_0123456789ab:1"));
        assert_eq!(
            book.pending(10),
            vec![PendingItem::Open {
                deployment_id: id_of(ACCOUNT, 1),
                program_hash: format!("0x0123456789ab{}", "cd".repeat(26)),
                budget_cents: 10_000,
            }]
        );
        // A second deployment on the same sub-account is a second record.
        book.note_deployment(deployment(ACCOUNT, 2));
        assert_eq!(book.pending(10).len(), 2);
        assert_eq!(book.record(&ACCOUNT.to_string()).unwrap().deployment, "Strategy_0123456789ab:2");
        assert_eq!(book.record(&id_of(ACCOUNT, 1)).unwrap().deployment, "Strategy_0123456789ab:1");
    }

    #[test]
    fn a_deployment_whose_hash_or_budget_is_not_writable_gets_no_record() {
        let book = RecordBook::new();
        let mut bad_hash = deployment(ACCOUNT, 1);
        bad_hash.source_hash = "xyz".into();
        book.note_deployment(bad_hash);
        let mut bad_budget = deployment(Address([8; 20]), 1);
        bad_budget.allocation = "lots".into();
        book.note_deployment(bad_budget);
        assert!(book.pending(10).is_empty());
    }

    #[test]
    fn a_resolved_round_with_orders_is_sealed_with_its_result_and_journal_hash() {
        let book = book_with_a_round();
        assert_eq!(book.live_rounds(), vec![(ACCOUNT, SeriesId(0), MarketId(1))]);
        assert_eq!(book.pending(10).len(), 1, "nothing is sealed while the round is open");
        book.settle(ACCOUNT, SeriesId(0), MarketId(1), Some(1_234));
        assert!(book.live_rounds().is_empty());
        let journal = book.journal(&id_of(ACCOUNT, 1), 1).expect("kept");
        assert_eq!(std::str::from_utf8(&journal).unwrap().lines().count(), 2);
        let hash = format!("0x{}", Sha256::digest(&journal).iter().map(|b| format!("{b:02x}")).collect::<String>());
        assert_eq!(
            book.pending(10)[1],
            PendingItem::Seal { deployment_id: id_of(ACCOUNT, 1), round_id: 1, won: true, pnl_cents: 123, journal_hash: hash.clone() }
        );
        let view = book.record(&id_of(ACCOUNT, 1)).unwrap();
        assert_eq!(view.seals, vec![SealView { round_id: 1, won: true, pnl_cents: 123, journal_hash: hash, tx_hash: None, lines: 2 }]);
        assert_eq!((view.budget_cents, view.open_tx), (10_000, None));
    }

    #[test]
    fn a_losing_or_flat_round_is_not_won() {
        for (pnl, won, cents) in [(-500, false, -50), (0, false, 0), (9, true, 0)] {
            let book = book_with_a_round();
            book.settle(ACCOUNT, SeriesId(0), MarketId(1), Some(pnl));
            assert!(matches!(&book.pending(10)[1], PendingItem::Seal { won: w, pnl_cents: c, .. } if *w == won && *c == cents), "{pnl}");
        }
    }

    #[test]
    fn a_round_with_no_orders_is_not_sealed() {
        let book = RecordBook::new();
        book.note_deployment(deployment(ACCOUNT, 1));
        book.settle(ACCOUNT, SeriesId(0), MarketId(1), Some(0));
        assert_eq!(book.pending(10).len(), 1, "only the open");
        assert!(book.record(&id_of(ACCOUNT, 1)).unwrap().seals.is_empty());
    }

    #[test]
    fn a_round_of_orders_that_never_filled_is_sealed_at_zero_without_a_result() {
        let book = RecordBook::new();
        book.note_deployment(deployment(ACCOUNT, 1));
        book.on_event(&event(7, "r", ExecKind::Reject(ExecReject::NoLiquidity), 0, 0, None));
        book.settle(ACCOUNT, SeriesId(0), MarketId(1), None);
        assert!(matches!(&book.pending(10)[1], PendingItem::Seal { won: false, pnl_cents: 0, round_id: 1, .. }));
    }

    #[test]
    fn a_round_with_fills_and_no_result_is_dropped_not_guessed() {
        let book = book_with_a_round();
        book.settle(ACCOUNT, SeriesId(0), MarketId(1), None);
        assert_eq!(book.pending(10).len(), 1);
        assert!(book.live_rounds().is_empty());
    }

    #[test]
    fn a_deployment_that_trades_before_it_is_noted_keeps_its_first_events() {
        // The engine can report an order before the next sweep has opened
        // the deployment's record.
        let book = RecordBook::new();
        book.on_event(&event(7, "t", ExecKind::Fill, 5, 0, Some(600)));
        book.note_deployment(deployment(ACCOUNT, 1));
        book.settle(ACCOUNT, SeriesId(0), MarketId(1), Some(100));
        assert_eq!(book.record(&id_of(ACCOUNT, 1)).unwrap().seals[0].lines, 1);
    }

    #[test]
    fn events_with_no_record_by_the_time_the_round_resolves_are_dropped() {
        let book = RecordBook::new();
        book.on_event(&event(9, "t", ExecKind::Fill, 5, 0, Some(600)));
        book.settle(Address([9; 20]), SeriesId(0), MarketId(1), Some(100));
        assert!(book.live_rounds().is_empty());
        assert!(book.pending(10).is_empty());
    }

    #[test]
    fn an_event_with_no_round_is_not_journalled() {
        let book = RecordBook::new();
        book.note_deployment(deployment(ACCOUNT, 1));
        let mut no_round = event(7, "r", ExecKind::Reject(ExecReject::UnknownAccount), 0, 0, None);
        no_round.market = None;
        book.on_event(&no_round);
        assert!(book.live_rounds().is_empty());
    }

    #[test]
    fn a_redeployment_seals_under_its_own_record() {
        let book = book_with_a_round();
        book.settle(ACCOUNT, SeriesId(0), MarketId(1), Some(100));
        book.note_deployment(deployment(ACCOUNT, 2));
        let mut later = event(7, "t", ExecKind::Fill, 1, 0, Some(500));
        later.market = Some(MarketId(2));
        book.on_event(&later);
        book.settle(ACCOUNT, SeriesId(0), MarketId(2), Some(-100));
        assert_eq!(book.record(&id_of(ACCOUNT, 1)).unwrap().seals.len(), 1);
        let second = book.record(&id_of(ACCOUNT, 2)).unwrap();
        assert_eq!(second.seals.iter().map(|s| s.round_id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn pending_lists_opens_before_seals_and_stops_at_the_limit() {
        let book = book_with_a_round();
        book.settle(ACCOUNT, SeriesId(0), MarketId(1), Some(100));
        book.note_deployment(deployment(Address([8; 20]), 1));
        let kinds: Vec<&str> = book
            .pending(10)
            .iter()
            .map(|p| match p {
                PendingItem::Open { .. } => "open",
                PendingItem::Seal { .. } => "seal",
            })
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["open", "open", "seal"]);
        assert_eq!(book.pending(2).len(), 2);
    }

    #[test]
    fn an_acknowledged_item_leaves_pending_and_shows_its_transaction() {
        let book = book_with_a_round();
        book.settle(ACCOUNT, SeriesId(0), MarketId(1), Some(100));
        let id = id_of(ACCOUNT, 1);
        assert_eq!(book.ack(&id, None, TX_A), Ok(()));
        assert!(matches!(book.pending(10).as_slice(), [PendingItem::Seal { .. }]));
        assert_eq!(book.ack(&id, Some(1), TX_B), Ok(()));
        assert!(book.pending(10).is_empty());
        let view = book.record(&id).unwrap();
        assert_eq!(view.open_tx.as_deref(), Some(TX_A));
        assert_eq!(view.seals[0].tx_hash.as_deref(), Some(TX_B));
        // The same acknowledgement again is fine; a different one is not.
        assert_eq!(book.ack(&id, None, TX_A), Ok(()));
        assert_eq!(book.ack(&id, None, &TX_A.to_uppercase().replace("0X", "0x")), Ok(()));
        assert_eq!(book.ack(&id, None, TX_B), Err(AckError::Conflict));
        assert_eq!(book.ack(&id, Some(1), TX_A), Err(AckError::Conflict));
    }

    #[test]
    fn an_acknowledgement_must_name_a_real_item_and_a_real_transaction() {
        let book = book_with_a_round();
        let id = id_of(ACCOUNT, 1);
        assert_eq!(book.ack("nope", None, TX_A), Err(AckError::BadId));
        assert_eq!(book.ack(&ACCOUNT.to_string(), None, TX_A), Err(AckError::BadId), "an address is not an id here");
        assert_eq!(book.ack(&id_of(ACCOUNT, 9), None, TX_A), Err(AckError::NotFound));
        assert_eq!(book.ack(&id, Some(1), TX_A), Err(AckError::NotFound), "round 1 is not sealed yet");
        for bad in ["", "0x12", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", &format!("0x{}", "zz".repeat(32))] {
            assert_eq!(book.ack(&id, None, bad), Err(AckError::BadTxHash), "{bad:?}");
        }
        assert_eq!(book.pending(10).len(), 1, "nothing was acknowledged");
    }

    #[test]
    fn only_the_newest_journals_keep_their_bytes() {
        let book = RecordBook::new();
        book.note_deployment(deployment(ACCOUNT, 1));
        for round in 1..=(JOURNALS_KEPT as u32 + 2) {
            let mut e = event(7, "t", ExecKind::Fill, 1, 0, Some(500));
            e.market = Some(MarketId(round));
            book.on_event(&e);
            book.settle(ACCOUNT, SeriesId(0), MarketId(round), Some(10));
        }
        let id = id_of(ACCOUNT, 1);
        assert_eq!(book.journal(&id, 1), None);
        assert_eq!(book.journal(&id, 2), None);
        assert!(book.journal(&id, 3).is_some());
        // The seals themselves stay.
        assert_eq!(book.record(&id).unwrap().seals.len(), JOURNALS_KEPT + 2);
    }

    /// A real exchange: a user, a strategy sub-account, a counterparty.
    fn exchange_with_a_deployment() -> (Exchange, Address, RecordBook) {
        let mut x = Exchange::new(Tenths(1_000_000), crate::series::SERIES);
        let (parent, other) = (Address([1; 20]), Address([2; 20]));
        for a in [parent, other] {
            x.register_user(a);
            x.get_or_create_account(a);
        }
        let child = x.create_sub_account_in(parent, Tenths(100_000), DEFAULT_SERIES).unwrap();
        x.deploy_strategy_in(child, named("Test", DEFAULT_SERIES)).unwrap();
        x.open_first_market(DEFAULT_SERIES, 100_000.0, 0, 300);
        let book = RecordBook::new();
        let sink = book.clone();
        x.set_execution_sink(ExecutionSink::new(move |ev| sink.on_event(&ev)));
        (x, child, book)
    }

    #[test]
    fn a_sweep_opens_every_deployment_and_seals_its_resolved_round_from_the_engine() {
        let (mut x, child, book) = exchange_with_a_deployment();
        let other = Address([2; 20]);
        book.sweep(&x);
        let record = book.record(&child.to_string()).expect("the sweep found the deployment");
        assert_eq!(record.budget_cents, 10_000, "the test envelope's $100");
        assert_eq!(record.program_hash.len(), 66);
        // The strategy buys 10 Up at 40c from the other account; Up wins.
        let tick = PriceTick::from_tenths(400).unwrap();
        x.place_limit(DEFAULT_SERIES, other, Side::Ask, tick, Qty(10), TradeSource::Ui).unwrap();
        x.place_limit_with(DEFAULT_SERIES, child, Side::Bid, tick, Qty(10), TradeSource::Strategy, PlaceOpts::with_client_id("buy")).unwrap();
        book.sweep(&x);
        assert!(book.record(&child.to_string()).unwrap().seals.is_empty(), "the round is still open");
        x.resolve_and_open_next(DEFAULT_SERIES, 100_500.0, 100_500.0, 300, 300);
        book.sweep(&x);
        let seals = book.record(&child.to_string()).unwrap().seals;
        assert_eq!(seals.len(), 1);
        // Paid $4.00 for ten contracts that settle at $1 each: +$6.00.
        assert_eq!((seals[0].round_id, seals[0].won, seals[0].pnl_cents, seals[0].lines), (1, true, 600, 1));
        // A second sweep changes nothing.
        book.sweep(&x);
        assert_eq!(book.record(&child.to_string()).unwrap().seals.len(), 1);
        assert_eq!(book.pending(10).len(), 2);
    }

    #[test]
    fn the_installed_sink_feeds_the_record_and_then_the_runtime() {
        use crate::automation::runtime::{RuntimeCommand, runtime_channel};
        let (x, child, _unused) = exchange_with_a_deployment();
        let exchange = Arc::new(Mutex::new(x));
        let (runtime, mut inbox) = runtime_channel();
        let book = RecordBook::new();
        install_sink(&exchange, runtime, book.clone());
        let tick = PriceTick::from_tenths(400).unwrap();
        let opts = PlaceOpts { post_only: true, ..PlaceOpts::with_client_id("rest") };
        let placed = exchange.lock().unwrap().place_limit_with(DEFAULT_SERIES, child, Side::Bid, tick, Qty(1), TradeSource::Strategy, opts).unwrap();
        exchange.lock().unwrap().cancel(DEFAULT_SERIES, child, placed.order_id).unwrap();
        assert_eq!(book.live_rounds(), vec![(child, DEFAULT_SERIES, MarketId(1))], "the cancel was journalled");
        assert!(matches!(inbox.try_recv(), Ok(RuntimeCommand::Execution(_))), "and the runtime still hears it");
    }

    fn api(token: Option<&str>) -> RecordApi {
        RecordApi::new(book_with_a_round(), token.map(str::to_owned))
    }

    fn with_token(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(TOKEN_HEADER, token.parse().unwrap());
        headers
    }

    async fn body(response: Response) -> (StatusCode, Vec<u8>) {
        let status = response.status();
        (status, axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap().to_vec())
    }

    #[test]
    fn the_recorders_routes_are_closed_until_a_token_is_set() {
        assert_eq!(api(None).access(Some("anything")), Access::Unconfigured);
        assert_eq!(api(Some("")).access(Some("")), Access::Unconfigured, "an empty token is no token");
        let open = api(Some("s3cret"));
        assert_eq!(open.access(None), Access::Unauthorized);
        assert_eq!(open.access(Some("s3cre")), Access::Unauthorized);
        assert_eq!(open.access(Some("s3cret")), Access::Granted);
    }

    #[tokio::test]
    async fn pending_and_ack_need_the_token_and_the_reads_do_not() {
        let closed = api(None);
        assert_eq!(body(pending_handler(State(closed.clone()), HeaderMap::new()).await).await.0, StatusCode::SERVICE_UNAVAILABLE);
        let open = api(Some("s3cret"));
        assert_eq!(body(pending_handler(State(open.clone()), with_token("wrong")).await).await.0, StatusCode::UNAUTHORIZED);
        let ack = || AckRequest { deployment_id: id_of(ACCOUNT, 1), round_id: None, tx_hash: TX_A.into() };
        assert_eq!(body(ack_handler(State(open.clone()), HeaderMap::new(), Json(ack())).await).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(open.book.pending(10).len(), 1, "an unauthorised acknowledgement changes nothing");

        let (status, bytes) = body(pending_handler(State(open.clone()), with_token("s3cret")).await).await;
        assert_eq!(status, StatusCode::OK);
        let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(list[0]["kind"], "open");
        assert_eq!(list[0]["deployment_id"], id_of(ACCOUNT, 1));
        assert_eq!(list[0]["budget_cents"], 10_000);

        assert_eq!(body(ack_handler(State(open.clone()), with_token("s3cret"), Json(ack())).await).await.0, StatusCode::NO_CONTENT);
        let conflict = AckRequest { tx_hash: TX_B.into(), ..ack() };
        assert_eq!(body(ack_handler(State(open.clone()), with_token("s3cret"), Json(conflict)).await).await.0, StatusCode::CONFLICT);
        let missing = AckRequest { round_id: Some(5), ..ack() };
        assert_eq!(body(ack_handler(State(open.clone()), with_token("s3cret"), Json(missing)).await).await.0, StatusCode::NOT_FOUND);
        let bad = AckRequest { tx_hash: "0x12".into(), ..ack() };
        assert_eq!(body(ack_handler(State(open.clone()), with_token("s3cret"), Json(bad)).await).await.0, StatusCode::BAD_REQUEST);

        // Reading a record needs no token, even where the recorder's routes are closed.
        let (status, bytes) = body(record_handler(State(closed.clone()), Path(id_of(ACCOUNT, 1))).await).await;
        assert_eq!(status, StatusCode::OK);
        let view: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(view["account"], ACCOUNT.to_string());
        assert_eq!(body(record_handler(State(closed), Path("0x00".into())).await).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_journal_is_served_as_the_exact_bytes_that_were_hashed() {
        let api = api(None);
        api.book.settle(ACCOUNT, SeriesId(0), MarketId(1), Some(100));
        let response = journal_handler(State(api.clone()), Path((id_of(ACCOUNT, 1), 1))).await;
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/x-ndjson");
        let (status, bytes) = body(response).await;
        assert_eq!(status, StatusCode::OK);
        let hash = format!("0x{}", Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>());
        assert_eq!(api.book.record(&id_of(ACCOUNT, 1)).unwrap().seals[0].journal_hash, hash);
        assert_eq!(body(journal_handler(State(api), Path((id_of(ACCOUNT, 1), 2))).await).await.0, StatusCode::NOT_FOUND);
    }
}
