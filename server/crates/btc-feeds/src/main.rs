use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use btc_feeds::CaptureStream;
use btc_feeds::capture::{CaptureError, CaptureOptions, run_capture};
use btc_feeds::latency::analyze;
use btc_feeds::live::{
    LiveControl, LiveDiagnostics, LiveError, LiveOptions, ProcessClock, run_live,
};
use btc_feeds::normalization::analyze as analyze_normalization;
use btc_feeds::replay::{KrakenBookDepths, NormalizeOptions, normalize, replay};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

const DEFAULT_LIVE_MAX_EVENTS: u64 = 10_000_000;
const DEFAULT_LIVE_MAX_BYTES: u64 = 2_147_483_648;
/// `EX_CONFIG` (sysexits.h): a configuration refusal, not a fault. The capture
/// binary exits with it when a rights-gated venue is not enabled by
/// `SIGNALS_ENABLED_VENUES` (R7).
const EXIT_FATAL: i32 = 78;

/// The process exit code for a capture error: a venue-gate refusal is
/// [`EXIT_FATAL`] (78), everything else is the default failure code (1).
fn capture_exit_code(error: &btc_feeds::capture::CaptureError) -> i32 {
    match error {
        btc_feeds::capture::CaptureError::VenueRefused(_)
        // A refuse-closed feed with no key, or a key the venue rejects, is a
        // configuration refusal, not a fault: the supervisor must not restart it,
        // so a missing or rejected Pyth Hermes credential also maps to exit 78,
        // the same contract as a venue gate.
        | btc_feeds::capture::CaptureError::PythHermesAuth(_)
        | btc_feeds::capture::CaptureError::PythHermesTerminal(_) => EXIT_FATAL,
        _ => 1,
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "btc-feeds",
    about = "Bounded BTC feed capture, inline normalization, and replay"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Normalize a fixed live feed inline and write source events to private JSONL.
    Run {
        #[arg(long, value_enum)]
        stream: StreamArg,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        publisher_epoch: String,
        #[arg(long, default_value_t = 30)]
        duration_seconds: u64,
        #[arg(long, default_value_t = DEFAULT_LIVE_MAX_EVENTS)]
        max_events: u64,
        #[arg(long, default_value_t = DEFAULT_LIVE_MAX_BYTES)]
        max_bytes: u64,
        #[arg(long, default_value_t = 100)]
        kraken_btc_book_depth: u16,
        /// Drop one eligible Coinbase match and require REST recovery proof.
        #[arg(long)]
        induce_coinbase_trade_gap: bool,
        /// Deliberately recycle each live socket at this interval.
        #[arg(long)]
        induced_reconnect_seconds: Option<u64>,
        /// Observe-only Kraken public-trade reconciliation cadence, in seconds
        /// (kraken-core only). Omitted disables the maintenance job.
        #[arg(long)]
        kraken_trade_reconciliation_seconds: Option<u64>,
    },
    /// Capture one fixed provider WebSocket stream to a new private JSONL file.
    Capture {
        #[arg(long, value_enum)]
        stream: StreamArg,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 30)]
        duration_seconds: u64,
        #[arg(long, default_value_t = 100_000)]
        max_records: u64,
        #[arg(long, default_value_t = 2_147_483_648_u64)]
        max_bytes: u64,
        #[arg(long, default_value_t = 100)]
        kraken_btc_book_depth: u16,
        /// Exact BTC Up/Down slug for a self-contained Polymarket Gamma + WS journal.
        #[arg(long)]
        polymarket_slug: Option<String>,
        /// Exact KXBTC15M ticker; credentials are read only from the process environment.
        #[arg(long)]
        kalshi_market_ticker: Option<String>,
        /// Path to the SportsDataIO NBA request-matrix control file (that stream
        /// only). The file names operations and parameters from the compiled
        /// allowlist; the credential is read ONLY from SPORTSDATAIO_NBA_API_KEY
        /// and is never a command-line argument.
        #[arg(long)]
        control_file: Option<PathBuf>,
        /// Shorten the proactive connection lifetime, in seconds, to exercise
        /// the make-before-break rotation without waiting for the venue's own
        /// session limit. Refused with exit 78 on a stream that has no
        /// connection lifetime of its own: it can only shorten one, never
        /// create one.
        #[arg(long)]
        connection_lifetime_seconds: Option<u64>,
    },
    /// Re-run a private capture through the same deterministic adapter state machine.
    Replay {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        publisher_epoch: String,
        /// Optional assertion; replay always uses the depth recorded in the capture.
        #[arg(long)]
        kraken_btc_book_depth: Option<u16>,
    },
    /// Normalize raw provider messages from stdin into `SourceEvent` JSONL on
    /// stdout. The language-neutral entry point: one raw message per line in,
    /// one normalized event per line out, one JSON feed notice per line on
    /// stderr.
    Normalize {
        #[arg(long, value_enum)]
        stream: StreamArg,
        #[arg(long)]
        publisher_epoch: String,
        /// Kraken BTC/USD book subscription depth. Read only for the two Kraken
        /// streams; must match the depth the messages were subscribed at.
        #[arg(long, default_value_t = 10)]
        kraken_btc_book_depth: u16,
        /// Kraken USDT/USD book subscription depth (kraken-core only).
        #[arg(long, default_value_t = 10)]
        kraken_usdt_book_depth: u16,
        /// Path to a prediction-market discovery journal, ingested as the
        /// Configuration record before any message. Required for
        /// kalshi-market and polymarket-market.
        #[arg(long)]
        configuration: Option<PathBuf>,
        /// Path to a file holding one message that was sent TO the venue,
        /// replayed as an outbound record after the connection opens and
        /// before any stdin line. Repeatable; replayed in the order given.
        ///
        /// Kalshi and Polymarket refuse market data until they have seen the
        /// exact subscription that was sent, so those two streams need one
        /// --subscription per subscription (Kalshi: orderbook_delta, ticker
        /// and trade; Polymarket: the market subscribe). Every other stream
        /// needs none.
        #[arg(long)]
        subscription: Vec<PathBuf>,
    },
    /// Summarize source-to-local-receipt timing from a private capture.
    Latency {
        #[arg(long)]
        input: PathBuf,
    },
    /// Time parsing and normalization before normalized JSON serialization.
    NormalizationLatency {
        #[arg(long)]
        input: PathBuf,
        #[arg(long, default_value = "benchmark")]
        publisher_epoch: String,
        /// Optional assertion; analysis always uses the depth recorded in the capture.
        #[arg(long)]
        kraken_btc_book_depth: Option<u16>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StreamArg {
    BinanceSpot,
    BinanceUsdmPublic,
    BinanceUsdmRest,
    BybitSpot,
    BybitLinear,
    BybitLinearRest,
    ChainlinkAggregator,
    CoinbaseCore,
    CoinbaseLevel3,
    CoinbaseMechanisms,
    KrakenCore,
    KrakenLevel3,
    KalshiMarket,
    KalshiIndex,
    NbaEspnDaily,
    NbaEspnGames,
    NbaEspnStatus,
    PolymarketMarket,
    PolymarketChainlinkTwap,
    PythHermesStream,
    PythHermesRest,
    /// Pinned so the CLI token matches the stream identity `sportsdataio_nba`;
    /// clap's derived name would otherwise be `sports-data-io-nba`.
    #[value(name = "sportsdataio-nba")]
    SportsDataIoNba,
}

impl From<StreamArg> for CaptureStream {
    fn from(value: StreamArg) -> Self {
        match value {
            StreamArg::BinanceSpot => Self::BinanceSpot,
            StreamArg::BybitSpot => Self::BybitSpot,
            StreamArg::BinanceUsdmPublic => Self::BinanceUsdmPublic,
            StreamArg::BinanceUsdmRest => Self::BinanceUsdmRest,
            StreamArg::BybitLinear => Self::BybitLinear,
            StreamArg::BybitLinearRest => Self::BybitLinearRest,
            StreamArg::ChainlinkAggregator => Self::ChainlinkAggregator,
            StreamArg::CoinbaseCore => Self::CoinbaseCore,
            StreamArg::CoinbaseLevel3 => Self::CoinbaseLevel3,
            StreamArg::CoinbaseMechanisms => Self::CoinbaseMechanisms,
            StreamArg::KrakenCore => Self::KrakenCore,
            StreamArg::KrakenLevel3 => Self::KrakenLevel3,
            StreamArg::KalshiMarket => Self::KalshiMarket,
            StreamArg::KalshiIndex => Self::KalshiIndex,
            StreamArg::NbaEspnDaily => Self::NbaEspnDaily,
            StreamArg::NbaEspnGames => Self::NbaEspnGames,
            StreamArg::NbaEspnStatus => Self::NbaEspnStatus,
            StreamArg::PolymarketMarket => Self::PolymarketMarket,
            StreamArg::PolymarketChainlinkTwap => Self::PolymarketChainlinkTwap,
            StreamArg::PythHermesStream => Self::PythHermesStream,
            StreamArg::PythHermesRest => Self::PythHermesRest,
            StreamArg::SportsDataIoNba => Self::SportsDataIoNba,
        }
    }
}

/// `EX_CONFIG`, the exit a venue refusal uses.
///
/// The same code `btc_signals::exit_code::ExitClass::Fatal` maps to, so one
/// venue refusal has one exit contract however it is reached: a supervisor that
/// knows not to restart a refused lane must not restart a refused capture
/// either. The constant is duplicated rather than shared because `btc-feeds`
/// sits below `btc-signals` in the dependency graph, and one integer is a
/// smaller cost than inverting that.
const EXIT_CONFIG: u8 = 78;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:?}");
            ExitCode::from(exit_code_for(&error))
        }
    }
}

/// The exit class for a failed run.
///
/// Only a venue refusal is special-cased; everything else is a plain failure.
/// Returns the raw code rather than an `ExitCode` so the mapping is assertable
/// in a test — `ExitCode` is opaque and cannot be compared.
fn exit_code_for(error: &anyhow::Error) -> u8 {
    if error.chain().any(|cause| {
        matches!(
            cause.downcast_ref(),
            Some(
                CaptureError::VenueNotEnabled { .. }
                    | CaptureError::VenueRefused(_)
                    | CaptureError::RotationNotSupported { .. }
                    | CaptureError::PythHermesAuth(_)
                    | CaptureError::PythHermesTerminal(_)
            )
        )
    }) {
        return EXIT_CONFIG;
    }
    EXIT_FAILURE
}

/// The generic failure exit, matching `ExitCode::FAILURE`.
const EXIT_FAILURE: u8 = 1;

async fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Run {
            stream,
            output,
            publisher_epoch,
            duration_seconds,
            max_events,
            max_bytes,
            kraken_btc_book_depth,
            induce_coinbase_trade_gap,
            induced_reconnect_seconds,
            kraken_trade_reconciliation_seconds,
        } => {
            anyhow::ensure!(max_events > 0, "max_events must be greater than zero");
            anyhow::ensure!(max_bytes > 0, "max_bytes must be greater than zero");
            ensure_live_stream_supported(stream.into())?;
            // R7: a rights-gated venue (Binance, Bybit) refuses before any socket
            // opens unless `SIGNALS_ENABLED_VENUES` lists it — the same gate the
            // capture path and the venue-lane shell apply, mapped to exit-78 by
            // `exit_code_for`. An ungated stream always admits.
            btc_feeds::capture::ensure_live_venue_enabled(stream.into())?;
            let mut output_options = OpenOptions::new();
            output_options.create_new(true).write(true);
            #[cfg(unix)]
            {
                output_options.mode(0o600);
            }
            let output_file = output_options.open(&output).with_context(|| {
                format!(
                    "failed to create live output {}; existing files are never overwritten",
                    output.display()
                )
            })?;
            let mut writer =
                BoundedJsonlWriter::new(BufWriter::new(output_file), max_events, max_bytes)?;
            let run_result = run_live(
                LiveOptions {
                    stream: stream.into(),
                    duration: Duration::from_secs(duration_seconds),
                    kraken_btc_book_depth,
                    publisher_epoch,
                    diagnostics: LiveDiagnostics {
                        induce_coinbase_trade_gap_once: induce_coinbase_trade_gap,
                        induced_reconnect_interval: induced_reconnect_seconds
                            .map(Duration::from_secs),
                    },
                    shutdown: None,
                    kraken_trade_reconciliation: kraken_trade_reconciliation_seconds
                        .map(Duration::from_secs),
                },
                ProcessClock::start(),
                |timed| {
                    for event in timed.outcome.events {
                        writer.write(&event)?;
                    }
                    Ok(LiveControl::Continue)
                },
            )
            .await;
            let finalize_result = writer
                .writer
                .flush()
                .and_then(|()| writer.writer.get_ref().sync_data());
            let report = match (run_result, finalize_result) {
                (Ok(report), Ok(())) => report,
                (Err(run_error), Ok(())) => {
                    return Err(run_error).context("inline feed run failed");
                }
                (Ok(_), Err(finalize_error)) => {
                    return Err(finalize_error)
                        .context("failed to finalize live normalized output");
                }
                (Err(run_error), Err(finalize_error)) => anyhow::bail!(
                    "inline feed run failed ({run_error}) and output finalization failed ({finalize_error})"
                ),
            };
            // Observe-only escalation-log summary: a Kraken reconciliation
            // divergence is a candidate silent trade loss for human review. Each
            // divergence is already emitted to stderr as it happens (so a killed
            // run keeps its evidence); this end-of-run pass re-lists the retained
            // escalations in the same format. The machine-readable report (which
            // carries every entry) goes to stdout.
            if let Some(reconciliation) = &report.kraken_trade_reconciliation {
                for escalation in &reconciliation.escalations {
                    eprintln!("{}", escalation.log_notice());
                }
            }
            println!(
                "{}",
                serde_json::to_string(&report).context("report serialization failed")?
            );
        }
        Command::Capture {
            stream,
            output,
            duration_seconds,
            max_records,
            max_bytes,
            kraken_btc_book_depth,
            polymarket_slug,
            kalshi_market_ticker,
            control_file,
            connection_lifetime_seconds,
        } => {
            let report = match run_capture(CaptureOptions {
                stream: stream.into(),
                output,
                duration: Duration::from_secs(duration_seconds),
                max_records,
                max_bytes,
                kraken_btc_book_depth,
                polymarket_slug,
                kalshi_market_ticker,
                sportsdataio_control_file: control_file,
                connection_lifetime_override: connection_lifetime_seconds.map(Duration::from_secs),
            })
            .await
            {
                Ok(report) => report,
                Err(error) => {
                    // A rights-gated venue whose token is absent from the
                    // allow-list (R7) is a configuration refusal, not a fault:
                    // exit 78 (EX_CONFIG) with the one-line reason, the same code
                    // a lane binary maps it onto.
                    if capture_exit_code(&error) == EXIT_FATAL {
                        eprintln!("{error}");
                        std::process::exit(EXIT_FATAL);
                    }
                    return Err(error).context("feed capture failed");
                }
            };
            println!(
                "{}",
                serde_json::to_string(&report_json(&report))
                    .context("report serialization failed")?
            );
        }
        Command::Replay {
            input,
            output,
            publisher_epoch,
            kraken_btc_book_depth,
        } => {
            let input_file = File::open(&input)
                .with_context(|| format!("failed to open capture {}", input.display()))?;
            let mut output_options = OpenOptions::new();
            output_options.create_new(true).write(true);
            #[cfg(unix)]
            {
                output_options.mode(0o600);
            }
            let output_file = output_options.open(&output).with_context(|| {
                format!(
                    "failed to create normalized output {}; existing files are never overwritten",
                    output.display()
                )
            })?;
            let mut reader = BufReader::new(input_file);
            let mut writer = BufWriter::new(output_file);
            let report = replay(
                &mut reader,
                &mut writer,
                &publisher_epoch,
                kraken_btc_book_depth,
            )
            .context("capture replay failed")?;
            println!(
                "{}",
                serde_json::to_string(&report).context("report serialization failed")?
            );
        }
        Command::Normalize {
            stream,
            publisher_epoch,
            kraken_btc_book_depth,
            kraken_usdt_book_depth,
            configuration,
            subscription,
        } => {
            let configuration = match configuration {
                Some(path) => Some(std::fs::read_to_string(&path).with_context(|| {
                    format!("failed to read configuration journal {}", path.display())
                })?),
                None => None,
            };
            let subscriptions = subscription
                .iter()
                .map(|path| {
                    std::fs::read_to_string(path).with_context(|| {
                        format!("failed to read subscription message {}", path.display())
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut reader = stdin.lock();
            let mut events = BufWriter::new(stdout.lock());
            let mut notices = BufWriter::new(stderr.lock());
            normalize(
                &mut reader,
                &mut events,
                &mut notices,
                &NormalizeOptions {
                    stream: stream.into(),
                    publisher_epoch,
                    kraken_depths: KrakenBookDepths {
                        btc: kraken_btc_book_depth,
                        usdt: kraken_usdt_book_depth,
                    },
                    configuration,
                    subscriptions,
                },
            )
            .context("normalization failed")?;
            events
                .flush()
                .context("failed to flush normalized output")?;
        }
        Command::Latency { input } => {
            let input_file = File::open(&input)
                .with_context(|| format!("failed to open capture {}", input.display()))?;
            let mut reader = BufReader::new(input_file);
            let report = analyze(&mut reader).context("latency analysis failed")?;
            println!(
                "{}",
                serde_json::to_string(&report).context("report serialization failed")?
            );
        }
        Command::NormalizationLatency {
            input,
            publisher_epoch,
            kraken_btc_book_depth,
        } => {
            let input_file = File::open(&input)
                .with_context(|| format!("failed to open capture {}", input.display()))?;
            let mut reader = BufReader::new(input_file);
            let report =
                analyze_normalization(&mut reader, &publisher_epoch, kraken_btc_book_depth)
                    .context("normalization latency analysis failed")?;
            println!(
                "{}",
                serde_json::to_string(&report).context("report serialization failed")?
            );
        }
    }
    Ok(())
}

struct BoundedJsonlWriter<W> {
    writer: W,
    maximum_events: u64,
    maximum_bytes: u64,
    events_written: u64,
    bytes_written: u64,
    scratch: Vec<u8>,
}

impl<W: Write> BoundedJsonlWriter<W> {
    fn new(writer: W, maximum_events: u64, maximum_bytes: u64) -> Result<Self> {
        anyhow::ensure!(maximum_events > 0, "max_events must be greater than zero");
        anyhow::ensure!(maximum_bytes > 0, "max_bytes must be greater than zero");
        Ok(Self {
            writer,
            maximum_events,
            maximum_bytes,
            events_written: 0,
            bytes_written: 0,
            scratch: Vec::with_capacity(1024),
        })
    }

    fn write<T: Serialize>(&mut self, value: &T) -> Result<(), LiveError> {
        if self.events_written >= self.maximum_events {
            return Err(LiveError::Publication(format!(
                "diagnostic output reached its {} event limit",
                self.maximum_events
            )));
        }

        self.scratch.clear();
        serde_json::to_writer(&mut self.scratch, value)
            .map_err(|error| LiveError::Publication(error.to_string()))?;
        self.scratch.push(b'\n');
        let line_bytes = u64::try_from(self.scratch.len())
            .map_err(|_| LiveError::Publication("output byte count overflowed".to_owned()))?;
        let next_bytes = self
            .bytes_written
            .checked_add(line_bytes)
            .ok_or_else(|| LiveError::Publication("output byte count overflowed".to_owned()))?;
        if next_bytes > self.maximum_bytes {
            return Err(LiveError::Publication(format!(
                "diagnostic output reached its {} byte limit",
                self.maximum_bytes
            )));
        }

        self.writer
            .write_all(&self.scratch)
            .map_err(|error| LiveError::Publication(error.to_string()))?;
        self.events_written = self
            .events_written
            .checked_add(1)
            .ok_or_else(|| LiveError::Publication("output event count overflowed".to_owned()))?;
        self.bytes_written = next_bytes;
        Ok(())
    }
}

/// Reject `run` for streams that have no inline live-runner path. These are
/// captured with `capture` and normalized with `replay`; their live adoption
/// lands in a later slice.
///
/// The Binance and Bybit venue streams are NOT refused here: the venue-lane
/// shell (`btc-signals::lane_runtime`) drives them through `run_live`, and this
/// binary can too. What keeps a rights-gated venue off the wire is the R7 gate
/// ([`ensure_live_venue_enabled`], `SIGNALS_ENABLED_VENUES`) the caller applies
/// next — not a blanket refusal — so an operator who lists the venue can run it
/// and one who does not is refused with exit-78 before any socket opens.
fn ensure_live_stream_supported(stream: CaptureStream) -> Result<()> {
    anyhow::ensure!(
        !matches!(
            stream,
            // Poll-only: the Chainlink oracle feed opens no WebSocket and is
            // driven by the oracle lane's poll driver, not `run_live`.
            CaptureStream::ChainlinkAggregator
                // Capture-only NBA/SportsDataIO sources: no live-runner path.
                | CaptureStream::NbaEspnDaily
                | CaptureStream::NbaEspnGames
                | CaptureStream::NbaEspnStatus
                | CaptureStream::SportsDataIoNba
                // Documented-not-observed (ruling R1): the index socket's
                // handshake, message names and field spellings are settled by
                // the wave-A capture on the Tokyo host. Until the adapter is
                // finalized against those frames the live runner refuses the
                // stream outright, so no signal is ever computed from a shape
                // nobody has seen. Capture it with `capture` and normalize with
                // `replay`.
                | CaptureStream::KalshiIndex
                // Refuse-closed keyed oracle feed (R1/R7): the Pyth Hermes SSE
                // and REST anchor are driven by the oracle lane's poll/SSE seam,
                // not `run_live`, and are adopted only once a key exists AND an
                // authenticated hour has been captured. Capture with `capture`.
                | CaptureStream::PythHermesStream
                | CaptureStream::PythHermesRest
        ),
        "the {} stream has no live-runner path yet; capture it with `capture` and normalize with `replay`",
        stream.as_str()
    );
    Ok(())
}

fn report_json(report: &btc_feeds::capture::CaptureReport) -> serde_json::Value {
    serde_json::json!({
        "provider": report.provider.as_str(),
        "records": report.records,
        "bytes_written": report.bytes_written,
        "connection_attempts": report.connection_attempts,
        "successful_connections": report.successful_connections,
        "read_idle_timeouts": report.read_idle_timeouts,
        "non_success_responses": report.non_success_responses,
        "served_error_responses": report.served_error_responses,
        "interrupted": report.interrupted
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        BoundedJsonlWriter, EXIT_CONFIG, EXIT_FAILURE, EXIT_FATAL, capture_exit_code,
        ensure_live_stream_supported, exit_code_for, report_json,
    };
    use btc_feeds::CaptureStream;
    use btc_feeds::capture::{CaptureError, CaptureReport};

    #[test]
    fn a_venue_gate_refusal_maps_to_exit_78_everything_else_to_one() {
        assert_eq!(
            capture_exit_code(&CaptureError::VenueRefused("bybit not enabled".to_owned())),
            EXIT_FATAL
        );
        assert_eq!(EXIT_FATAL, 78);
        assert_eq!(
            capture_exit_code(&CaptureError::InvalidOptions("bad".to_owned())),
            1
        );
    }

    /// A venue the allow-list does not list is a configuration decision, so it
    /// exits 78 (`EX_CONFIG`) — the same code the lane gate uses, so a
    /// supervisor that knows not to restart a refused lane does not restart a
    /// refused capture either (ruling R7).
    #[test]
    fn a_venue_refusal_exits_78_and_every_other_failure_exits_1() {
        let refusal = anyhow::Error::new(btc_feeds::capture::CaptureError::VenueNotEnabled {
            stream: "binance_usdm_public",
            token: "binance",
            env: "SIGNALS_ENABLED_VENUES",
        })
        .context("feed capture failed");
        assert_eq!(exit_code_for(&refusal), EXIT_CONFIG);
        assert_eq!(EXIT_CONFIG, 78);
        // The refusal is found through the context chain, not only at the root.
        assert!(refusal.to_string().contains("feed capture failed"));
        assert!(
            refusal
                .chain()
                .any(|cause| cause.to_string().contains("RIGHTS-LEDGER"))
        );

        // Arming rotation on a stream that has none is the same class of
        // mistake: operator configuration, not a transient fault.
        let rotation = anyhow::Error::new(btc_feeds::capture::CaptureError::RotationNotSupported {
            stream: "kalshi_market",
        })
        .context("feed capture failed");
        assert_eq!(exit_code_for(&rotation), EXIT_CONFIG);

        // Every other failure keeps the plain failure exit: only a refusal is a
        // configuration decision.
        let other = anyhow::Error::new(btc_feeds::capture::CaptureError::NoConnection)
            .context("feed capture failed");
        assert_eq!(exit_code_for(&other), EXIT_FAILURE);
        assert_eq!(
            exit_code_for(&anyhow::anyhow!("some unrelated failure")),
            EXIT_FAILURE
        );
    }

    #[test]
    fn live_run_refuses_only_streams_with_no_live_path() {
        // The Binance and Bybit venue streams now have a live-runner path (the
        // lane shell drives them, and so does this binary); the R7 gate, not a
        // blanket refusal, keeps a rights-gated venue off the wire.
        for stream in [
            CaptureStream::BinanceSpot,
            CaptureStream::BinanceUsdmPublic,
            CaptureStream::BinanceUsdmRest,
            CaptureStream::BybitSpot,
            CaptureStream::BybitLinear,
            CaptureStream::BybitLinearRest,
            CaptureStream::CoinbaseCore,
            CaptureStream::KalshiMarket,
        ] {
            assert!(
                ensure_live_stream_supported(stream).is_ok(),
                "{stream:?} has a live-runner path"
            );
        }
        // Streams with genuinely no inline live path stay refused: the poll-only
        // Chainlink oracle feed, the capture-only NBA/SportsDataIO sources, and
        // the documented-not-observed Kalshi index socket (ruling R1).
        for stream in [
            CaptureStream::ChainlinkAggregator,
            CaptureStream::NbaEspnStatus,
            CaptureStream::NbaEspnDaily,
            CaptureStream::NbaEspnGames,
            CaptureStream::SportsDataIoNba,
            CaptureStream::KalshiIndex,
            // Refuse-closed keyed oracle feed (R1/R7): both Pyth Hermes streams
            // are driven by the oracle lane's poll/SSE seam, not `run_live`.
            CaptureStream::PythHermesStream,
            CaptureStream::PythHermesRest,
        ] {
            assert!(
                ensure_live_stream_supported(stream).is_err(),
                "{stream:?} has no live-runner path"
            );
        }
    }

    /// A credentialed run's report must not read an auth or rate-limit refusal as
    /// a transport problem: a recorded non-2xx answer and a read that produced no
    /// response at all are separate fields with separate names.
    #[test]
    fn the_capture_report_separates_non_success_answers_from_read_idle_timeouts() {
        let report = CaptureReport {
            provider: CaptureStream::SportsDataIoNba.provider(),
            records: 8,
            bytes_written: 4_096,
            connection_attempts: 1,
            successful_connections: 1,
            read_idle_timeouts: 2,
            non_success_responses: 3,
            served_error_responses: 5,
            interrupted: false,
        };
        let rendered = report_json(&report);
        assert_eq!(rendered["read_idle_timeouts"], json!(2));
        assert_eq!(rendered["non_success_responses"], json!(3));
        // A served error (HTTP 200 + retCode/empty list) is its own counter, so
        // an hour of rate-limiting is distinguishable from a healthy hour.
        assert_eq!(rendered["served_error_responses"], json!(5));
    }

    #[test]
    fn diagnostic_writer_stops_before_a_partial_limit_record() {
        let mut writer = BoundedJsonlWriter::new(Vec::new(), 2, 10).expect("valid limits");
        writer.write(&json!(1)).expect("first line fits");
        writer.write(&json!(2)).expect("second line fits");
        assert!(writer.write(&json!(3)).is_err());
        assert_eq!(writer.writer, b"1\n2\n");

        let mut byte_limited = BoundedJsonlWriter::new(Vec::new(), 10, 3).expect("valid limits");
        byte_limited.write(&json!(1)).expect("first line fits");
        assert!(byte_limited.write(&json!(2)).is_err());
        assert_eq!(byte_limited.writer, b"1\n");
    }
}
