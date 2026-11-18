//! Deterministic process exit-code taxonomy shared by the `btc-signals` binaries.
//!
//! A process supervisor (systemd unit or container runtime) must decide whether
//! to restart a stopped signal process without parsing log text. This module
//! maps every classified outcome to one of four stable, documented exit codes:
//!
//! | Code | Class | Meaning | Supervisor action |
//! | ---- | ----- | ------- | ----------------- |
//! | 0    | `Success`     | Clean, voluntary completion | none / stay stopped |
//! | 75   | `Retryable`   | Transient provider/network failure | restart with backoff |
//! | 78   | `Fatal`       | Config, contract, lineage, corruption, or invariant error | do NOT restart; needs operator action |
//! | 130  | `Interrupted` | Stopped by an operating-system signal (SIGINT or SIGTERM) | do NOT auto-restart |
//!
//! The codes reuse well-known conventions: 75 is `EX_TEMPFAIL` and 78 is
//! `EX_CONFIG` from `sysexits`, and 130 is the shell convention for a process
//! terminated by SIGINT (`128 + 2`). SIGINT (Ctrl-C) and SIGTERM (systemd's
//! default `KillSignal`, and what `docker stop` sends) are treated identically:
//! both mean "interrupted, shut down gracefully", both let the run write its
//! terminal record, and both report exit 130. The graceful-stop wiring lives in
//! `btc_feeds::live`'s shutdown future, so every consumer of that path — both
//! binaries included — handles the two signals the same way. The mapping is consumed by the templates in
//! [`docs/deployment`](../../../docs/deployment); see its `README.md` for the
//! supervision contract.
//!
//! Classification is expressed as exhaustive matches over the existing error
//! enums (no wildcard arms) so that a new error variant fails to compile until a
//! human assigns it a class. The only outcomes that are `Retryable` are live
//! provider/network failures; everything durability-, contract-, or
//! config-related is `Fatal` because it is not safe to restart blindly.

use std::fmt;
use std::process::ExitCode;

use btc_feeds::coinbase_rest::CoinbaseRestError;
use btc_feeds::live::LiveError;
use btc_feeds::prediction_live::PredictionLiveError;

/// Clean, voluntary completion.
pub const EXIT_SUCCESS: u8 = 0;
/// Transient failure; a supervisor should restart with backoff.
pub const EXIT_RETRYABLE: u8 = 75;
/// Permanent failure; a supervisor must not restart without operator action.
pub const EXIT_FATAL: u8 = 78;
/// Stopped by an operating-system signal.
pub const EXIT_INTERRUPTED: u8 = 130;

/// The supervision class of a finished process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitClass {
    /// The run finished its work and exited cleanly.
    Success,
    /// A transient provider/network failure; restarting with backoff is safe.
    Retryable,
    /// A configuration, contract, lineage, corruption, or invariant failure that
    /// a blind restart cannot fix.
    Fatal,
    /// The run was stopped by an operating-system signal.
    Interrupted,
}

impl ExitClass {
    /// The stable numeric exit code for this class.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            ExitClass::Success => EXIT_SUCCESS,
            ExitClass::Retryable => EXIT_RETRYABLE,
            ExitClass::Fatal => EXIT_FATAL,
            ExitClass::Interrupted => EXIT_INTERRUPTED,
        }
    }

    /// The process [`ExitCode`] a `main` should return for this class.
    #[must_use]
    pub fn exit_code(self) -> ExitCode {
        ExitCode::from(self.code())
    }

    /// Combine two classes, keeping the one that most restricts restarting.
    ///
    /// When several things fail in one run, the safest supervisor decision wins:
    /// a single do-not-restart cause suppresses restart even if another cause
    /// looked transient. Ordering, least to most restrictive: `Success`,
    /// `Interrupted`, `Retryable`, `Fatal`.
    #[must_use]
    pub fn worst(self, other: ExitClass) -> ExitClass {
        fn rank(class: ExitClass) -> u8 {
            match class {
                ExitClass::Success => 0,
                ExitClass::Interrupted => 1,
                ExitClass::Retryable => 2,
                ExitClass::Fatal => 3,
            }
        }
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
}

/// Classify a live spot/auxiliary feed failure.
///
/// Only genuine provider/network transients are `Retryable`; malformed provider
/// payloads, credential/config problems, and internal invariants are `Fatal`.
#[must_use]
pub fn classify_live(error: &LiveError) -> ExitClass {
    match error {
        // Transient provider/network conditions: a fresh process can succeed.
        LiveError::Capture(_) | LiveError::ConnectionAttemptLimit | LiveError::NoConnection => {
            ExitClass::Retryable
        }
        // A Coinbase REST recovery failure is transient only for transport and
        // provider-availability conditions; a bad range, malformed response, or
        // adapter fault is a contract error and `Fatal`, exactly as
        // `LiveError::Adapter` is.
        LiveError::CoinbaseRest(rest) => classify_coinbase_rest(rest),
        // The only `KrakenRest` error surfaced as a `LiveError` is secure-HTTP
        // client construction at reconciler setup — an environment/config fault,
        // not a per-cycle transient (cycle failures are swallowed observe-only).
        LiveError::KrakenRest(_)
        // Programming/config, provider-contract, diagnostic, or invariant faults.
        | LiveError::InvalidOptions(_)
        | LiveError::Adapter(_)
        | LiveError::KrakenAuth(_)
        | LiveError::Signal(_)
        | LiveError::InvalidRecovery(_)
        | LiveError::DiagnosticIncomplete(_)
        | LiveError::CounterExhausted
        | LiveError::ClockRange
        | LiveError::Publication(_)
        | LiveError::TargetMismatch(_) => ExitClass::Fatal,
    }
}

/// Classify a Coinbase REST recovery failure.
///
/// Only transport (`Http`) and provider-availability (`RangeUnavailable`, where
/// the exchange had not yet exposed enough trades before the recovery deadline)
/// conditions are transient; a fresh process can succeed. `InvalidRange` is a
/// caller/config fault, `InvalidResponse` is a provider-contract fault, and
/// `Adapter` is a validation fault — all `Fatal`, matching how the same
/// `AdapterError` is classified through `LiveError::Adapter`.
#[must_use]
fn classify_coinbase_rest(error: &CoinbaseRestError) -> ExitClass {
    match error {
        CoinbaseRestError::Http | CoinbaseRestError::RangeUnavailable { .. } => {
            ExitClass::Retryable
        }
        CoinbaseRestError::InvalidRange
        | CoinbaseRestError::InvalidResponse
        | CoinbaseRestError::Adapter(_) => ExitClass::Fatal,
    }
}

/// Classify a live prediction-market feed failure.
#[must_use]
pub fn classify_prediction_live(error: &PredictionLiveError) -> ExitClass {
    match error {
        PredictionLiveError::Capture(_) | PredictionLiveError::ConnectionAttemptLimit => {
            ExitClass::Retryable
        }
        // Reuse the spot classifier for a nested live error.
        PredictionLiveError::Live(inner) => classify_live(inner),
        PredictionLiveError::InvalidOptions(_)
        | PredictionLiveError::InvalidDiscovery(_)
        | PredictionLiveError::Adapter(_)
        | PredictionLiveError::KalshiAuth(_)
        | PredictionLiveError::Signal(_)
        | PredictionLiveError::UnexpectedBinary
        | PredictionLiveError::DiagnosticIncomplete(_)
        | PredictionLiveError::ClockRange
        | PredictionLiveError::Counter
        | PredictionLiveError::Publication(_) => ExitClass::Fatal,
    }
}

/// An error carrying a pre-computed [`ExitClass`].
///
/// Aggregation sites collapse several typed failures into one operator message.
/// Wrapping that message in this type lets the message reach the user unchanged
/// while the already-known class travels to the process exit code. Its
/// [`Display`](fmt::Display) is exactly the wrapped message, so the printed
/// error text is identical to a plain string error.
#[derive(Debug)]
pub struct ClassifiedMessage {
    class: ExitClass,
    message: String,
}

impl ClassifiedMessage {
    /// Wrap `message` with an already-determined `class`.
    #[must_use]
    pub fn new(class: ExitClass, message: String) -> Self {
        Self { class, message }
    }

    /// The pre-computed class.
    #[must_use]
    pub fn class(&self) -> ExitClass {
        self.class
    }
}

impl fmt::Display for ClassifiedMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClassifiedMessage {}

/// Determine the [`ExitClass`] of a finished-with-error run.
///
/// A [`ClassifiedMessage`] anywhere in the chain reports its stored class
/// directly. Otherwise the first live feed error found in the cause chain is
/// classified; any other error is `Fatal`, because an unclassified failure is
/// not safe to restart automatically.
#[must_use]
pub fn exit_class_for(error: &anyhow::Error) -> ExitClass {
    for cause in error.chain() {
        if let Some(classified) = cause.downcast_ref::<ClassifiedMessage>() {
            return classified.class();
        }
        if let Some(live) = cause.downcast_ref::<LiveError>() {
            return classify_live(live);
        }
        if let Some(prediction) = cause.downcast_ref::<PredictionLiveError>() {
            return classify_prediction_live(prediction);
        }
        if let Some(poll) = cause.downcast_ref::<crate::lane::PollDriverError>() {
            return classify_poll_driver(poll);
        }
        if let Some(file_gen) = cause.downcast_ref::<crate::file_generation::FileGenerationError>()
        {
            return classify_file_generation(file_gen);
        }
    }
    ExitClass::Fatal
}

/// Classify a file-generation layout failure.
///
/// A transient I/O fault (an `ENOSPC`/`EIO` while sealing or opening a
/// generation) is `Retryable` (75): the disk or filesystem may recover, and a
/// supervisor should restart the lane rather than hold it down under
/// `RestartPreventExitStatus=78`. Every deterministic layout fault — a refused
/// epoch reuse, a `RunMeta` mismatch, an out-of-range rotation period, or a
/// missing/unowned lane root — is `Fatal` (78) and needs operator action, never
/// a blind restart.
#[must_use]
pub fn classify_file_generation(error: &crate::file_generation::FileGenerationError) -> ExitClass {
    use crate::file_generation::FileGenerationError;
    match error {
        FileGenerationError::Io(_) => ExitClass::Retryable,
        FileGenerationError::LaneRootNotDirectory { .. }
        | FileGenerationError::LaneRootNotOwned { .. }
        | FileGenerationError::GenerationExists { .. }
        | FileGenerationError::MetaMismatch { .. }
        | FileGenerationError::RotationPeriodTooShort { .. } => ExitClass::Fatal,
    }
}

/// Classify an oracle-lane poll-driver failure.
///
/// A whole-run endpoint outage is the transient `NoConnection` shape a supervisor
/// should restart (75); any other poll-driver failure — a decoded start-up target
/// mismatch, an internal counter/clock error — is `Fatal` (78), never restarted
/// blindly.
#[must_use]
pub fn classify_poll_driver(error: &crate::lane::PollDriverError) -> ExitClass {
    match error {
        crate::lane::PollDriverError::Transient(_) => ExitClass::Retryable,
        crate::lane::PollDriverError::Poll(_) => ExitClass::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use btc_feeds::capture::CaptureError;
    use btc_feeds::coinbase_rest::CoinbaseRestError;
    use btc_feeds::kraken_auth::KrakenAuthError;
    use btc_feeds::{AdapterError, KalshiAuthError};

    use super::*;

    #[test]
    fn codes_are_the_documented_constants() {
        assert_eq!(ExitClass::Success.code(), 0);
        assert_eq!(ExitClass::Retryable.code(), 75);
        assert_eq!(ExitClass::Fatal.code(), 78);
        assert_eq!(ExitClass::Interrupted.code(), 130);
    }

    #[test]
    fn worst_lets_the_most_restrictive_class_win() {
        assert_eq!(
            ExitClass::Retryable.worst(ExitClass::Fatal),
            ExitClass::Fatal
        );
        assert_eq!(
            ExitClass::Fatal.worst(ExitClass::Retryable),
            ExitClass::Fatal
        );
        assert_eq!(
            ExitClass::Success.worst(ExitClass::Retryable),
            ExitClass::Retryable
        );
        assert_eq!(
            ExitClass::Interrupted.worst(ExitClass::Retryable),
            ExitClass::Retryable
        );
        assert_eq!(
            ExitClass::Success.worst(ExitClass::Success),
            ExitClass::Success
        );
    }

    /// Every `LiveError` variant maps to an intended class. Adding a variant
    /// forces both `classify_live`'s match and this list to be updated.
    #[test]
    fn every_live_error_variant_maps() {
        let cases: Vec<(LiveError, ExitClass)> = vec![
            (LiveError::InvalidOptions(String::new()), ExitClass::Fatal),
            (LiveError::TargetMismatch(String::new()), ExitClass::Fatal),
            (
                LiveError::Capture(CaptureError::CounterExhausted),
                ExitClass::Retryable,
            ),
            (
                LiveError::Adapter(AdapterError::SequenceExhausted),
                ExitClass::Fatal,
            ),
            (
                LiveError::CoinbaseRest(CoinbaseRestError::InvalidRange),
                ExitClass::Fatal,
            ),
            (
                LiveError::CoinbaseRest(CoinbaseRestError::Http),
                ExitClass::Retryable,
            ),
            (
                LiveError::KrakenAuth(KrakenAuthError::InvalidCredentials),
                ExitClass::Fatal,
            ),
            (
                LiveError::Signal(std::io::Error::other("signal")),
                ExitClass::Fatal,
            ),
            (LiveError::InvalidRecovery(String::new()), ExitClass::Fatal),
            (LiveError::ConnectionAttemptLimit, ExitClass::Retryable),
            (LiveError::NoConnection, ExitClass::Retryable),
            (
                LiveError::DiagnosticIncomplete(String::new()),
                ExitClass::Fatal,
            ),
            (LiveError::CounterExhausted, ExitClass::Fatal),
            (LiveError::ClockRange, ExitClass::Fatal),
            (LiveError::Publication(String::new()), ExitClass::Fatal),
        ];
        for (error, expected) in cases {
            assert_eq!(classify_live(&error), expected, "classifying {error}");
        }
    }

    /// Every `CoinbaseRestError` variant maps to an intended class through
    /// `LiveError::CoinbaseRest`. Adding a variant forces both the
    /// `classify_coinbase_rest` match and this list to be updated, and pins the
    /// contract that a deterministic range/response/adapter fault is `Fatal`
    /// (no restart loop) while transport and availability transients retry.
    #[test]
    fn every_coinbase_rest_error_variant_maps() {
        let cases: Vec<(CoinbaseRestError, ExitClass)> = vec![
            (CoinbaseRestError::Http, ExitClass::Retryable),
            (
                CoinbaseRestError::RangeUnavailable {
                    expected: 2,
                    recovered: 1,
                },
                ExitClass::Retryable,
            ),
            (CoinbaseRestError::InvalidRange, ExitClass::Fatal),
            (CoinbaseRestError::InvalidResponse, ExitClass::Fatal),
            (
                CoinbaseRestError::Adapter(AdapterError::SequenceExhausted),
                ExitClass::Fatal,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(
                classify_live(&LiveError::CoinbaseRest(error)),
                expected,
                "classifying coinbase rest"
            );
        }
    }

    /// Every `PredictionLiveError` variant maps to an intended class.
    #[test]
    fn every_prediction_live_error_variant_maps() {
        let cases: Vec<(PredictionLiveError, ExitClass)> = vec![
            (
                PredictionLiveError::InvalidOptions(String::new()),
                ExitClass::Fatal,
            ),
            (
                PredictionLiveError::InvalidDiscovery(String::new()),
                ExitClass::Fatal,
            ),
            (
                PredictionLiveError::Capture(CaptureError::CounterExhausted),
                ExitClass::Retryable,
            ),
            (
                PredictionLiveError::Adapter(AdapterError::SequenceExhausted),
                ExitClass::Fatal,
            ),
            (
                PredictionLiveError::Live(LiveError::NoConnection),
                ExitClass::Retryable,
            ),
            (
                PredictionLiveError::Live(LiveError::ClockRange),
                ExitClass::Fatal,
            ),
            (
                PredictionLiveError::KalshiAuth(KalshiAuthError::InvalidCredentials),
                ExitClass::Fatal,
            ),
            (
                PredictionLiveError::Signal(std::io::Error::other("signal")),
                ExitClass::Fatal,
            ),
            (
                PredictionLiveError::ConnectionAttemptLimit,
                ExitClass::Retryable,
            ),
            (PredictionLiveError::UnexpectedBinary, ExitClass::Fatal),
            (
                PredictionLiveError::DiagnosticIncomplete(String::new()),
                ExitClass::Fatal,
            ),
            (PredictionLiveError::ClockRange, ExitClass::Fatal),
            (PredictionLiveError::Counter, ExitClass::Fatal),
            (
                PredictionLiveError::Publication(String::new()),
                ExitClass::Fatal,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(
                classify_prediction_live(&error),
                expected,
                "classifying {error}"
            );
        }
    }

    #[test]
    fn classified_message_round_trips_through_anyhow() {
        let error = anyhow::Error::new(ClassifiedMessage::new(
            ExitClass::Retryable,
            "provider dropped".to_owned(),
        ));
        assert_eq!(error.to_string(), "provider dropped");
        assert_eq!(exit_class_for(&error), ExitClass::Retryable);
    }

    #[test]
    fn live_error_in_the_chain_is_classified() {
        let error = anyhow::Error::new(LiveError::NoConnection).context("inline feed run failed");
        assert_eq!(exit_class_for(&error), ExitClass::Retryable);
    }

    #[test]
    fn unclassified_error_defaults_to_fatal() {
        let error = anyhow::anyhow!("some validation failure");
        assert_eq!(exit_class_for(&error), ExitClass::Fatal);
    }

    #[test]
    fn file_generation_io_is_retryable_and_generation_exists_is_fatal() {
        use crate::file_generation::FileGenerationError;

        let io = FileGenerationError::Io(std::io::Error::other("enospc"));
        assert_eq!(classify_file_generation(&io), ExitClass::Retryable);

        let exists = FileGenerationError::GenerationExists {
            path: std::path::PathBuf::from("/lane/gen-live-spot-1"),
        };
        assert_eq!(classify_file_generation(&exists), ExitClass::Fatal);

        // And a transient I/O fault classifies as Retryable through the anyhow
        // chain that main() actually inspects.
        let wrapped = anyhow::Error::new(FileGenerationError::Io(std::io::Error::other("eio")))
            .context("sealing generation failed");
        assert_eq!(exit_class_for(&wrapped), ExitClass::Retryable);
    }
}
