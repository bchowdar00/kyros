//! Shared file-generation layout for in-process generation rotation (C5).
//!
//! A *file generation* is one `gen-live-<lane>-<unix>` directory holding
//! `output.jsonl`, `journal.jsonl` and `RUN-META.json`, with a lane-level
//! `CURRENT` pointer naming the newest one. Today this layout is owned by
//! `docs/deployment/production-btc/live-run.sh` (lines ~226-299): the script
//! creates the directory, writes `RUN-META.json` and publishes `CURRENT`, and
//! the producer binary fills `output.jsonl`/`journal.jsonl`. C5 moves that
//! layout ownership into the binary so a lane can seal one generation and open
//! the next without exiting, keeping every processor and ring in memory.
//!
//! This module owns the *layout only* — naming, the `RUN-META.json` schema and
//! byte formatting, the atomic `CURRENT` write, the same-epoch refusal, and the
//! drift-free rotation deadlines. It does not open output/journal writers, does
//! not touch the checkpoint, and does not know about processors; the three
//! runtimes drive it. The word "generation" here always means a *file*
//! generation; it is never the Kalshi `market_generation` (contract rollover),
//! which is a wholly separate concept.
//!
//! `RUN-META.json` reproduces live-run.sh's ten fields byte-for-byte (same key
//! set, same order, same two-space indentation, same bare/quoted rendering) and
//! appends three rotation fields: `rotated_from` (previous directory name, or
//! `null` for the first generation of a process), `rotation_seconds` (the
//! rotation period), and `sealed_utc` (`null` while open, set by
//! [`OpenFileGeneration::seal`]).

use std::fs::DirBuilder;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

/// The generation output file name (mirrors live-run.sh's `output.jsonl`).
pub const OUTPUT_FILE_NAME: &str = "output.jsonl";
/// The generation journal file name (mirrors live-run.sh's `journal.jsonl`).
pub const JOURNAL_FILE_NAME: &str = "journal.jsonl";
/// The generation run-metadata file name.
pub const RUN_META_FILE_NAME: &str = "RUN-META.json";
/// The lane-level pointer naming the newest generation directory.
pub const CURRENT_FILE_NAME: &str = "CURRENT";

/// Operator metadata for one file generation.
///
/// These are exactly the fields live-run.sh writes into `RUN-META.json`, in the
/// same order. The struct carries only the operator-supplied values; the three
/// rotation fields (`rotated_from`, `rotation_seconds`, `sealed_utc`) are added
/// by [`FileGenerationLayout::open_next`]/[`OpenFileGeneration::seal`] and are
/// not part of `RunMeta`. `duration_seconds` keeps live-run.sh's field name; in
/// rotation mode it is the optional total run bound (`--duration-seconds`),
/// while the rotation period is supplied to
/// [`FileGenerationLayout::open_next_with_rotation`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunMeta {
    /// Lane identity (`spot`, `binance`, `bybit`, `oracle`, `polymarket`, `kalshi`).
    pub lane: String,
    /// Publisher epoch for this process (`live-<lane>-<unix>`).
    pub publisher_epoch: String,
    /// Generation directory name (`gen-<publisher_epoch>`), must equal the directory created.
    pub generation: String,
    /// UTC start timestamp, `%Y-%m-%dT%H:%M:%SZ`.
    pub started_utc: String,
    /// Total run duration in seconds. In explicit mode this is the single
    /// generation's duration; in rotation mode it is the optional overall run
    /// bound (`--duration-seconds`), not the rotation period — the period is
    /// passed separately to [`FileGenerationLayout::open_next_with_rotation`].
    pub duration_seconds: u64,
    /// Output byte cap for this generation.
    pub max_output_bytes: u64,
    /// Journal byte cap for this generation.
    pub max_journal_bytes: u64,
    /// Update-count cap for this generation.
    pub max_updates: u64,
    /// Headroom factor used to derive the caps.
    pub headroom_factor: u64,
    /// Whether the run resumed from an existing checkpoint.
    pub resumed: bool,
}

/// Per-generation resource ceilings.
///
/// Plain data mirroring the `--max-updates` / `--max-output-bytes` /
/// `--max-journal-bytes` caps live-run.sh derives for a run. **Reset
/// semantics:** the caps bound *per-generation* counters, so at every
/// [`FileGenerationLayout::open_next`] the runtime restarts its output-byte,
/// journal-byte and update counters at zero and measures them against these
/// ceilings again. The values themselves are recomputed by the caller only when
/// the rotation period changes (the caps scale with the period, exactly as
/// live-run.sh scales them with `DURATION_SECONDS`); an unchanged period carries
/// the same values into each new generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileGenerationCaps {
    /// Maximum emitted updates for one generation.
    pub max_updates: u64,
    /// Maximum output bytes for one generation.
    pub max_output_bytes: u64,
    /// Maximum journal bytes for one generation.
    pub max_journal_bytes: u64,
}

/// The lane directory under which every generation for one lane lives.
#[derive(Clone, Debug)]
pub struct FileGenerationLayout {
    lane_root: PathBuf,
    lane: String,
}

impl FileGenerationLayout {
    /// Bind a layout to `lane_root` for `lane`.
    ///
    /// Refuses a `lane_root` that does not exist, is not a directory, or (on
    /// Unix) is not owned by the calling effective uid. It never changes the
    /// directory's mode or owner — the check is existence + `is_dir` + owner
    /// only. Symlinks are followed (`metadata`, not `symlink_metadata`), so a
    /// lane root that is a symlink to an owned directory is accepted and the
    /// `is_dir`/owner checks apply to the link target.
    pub fn new(
        lane_root: impl AsRef<Path>,
        lane: impl Into<String>,
    ) -> Result<Self, FileGenerationError> {
        let lane_root = lane_root.as_ref().to_path_buf();
        let metadata = std::fs::metadata(&lane_root).map_err(|_| {
            FileGenerationError::LaneRootNotDirectory {
                path: lane_root.clone(),
            }
        })?;
        if !metadata.is_dir() {
            return Err(FileGenerationError::LaneRootNotDirectory { path: lane_root });
        }
        #[cfg(unix)]
        {
            let caller = rustix::process::geteuid().as_raw();
            let owner = metadata.uid();
            if owner != caller {
                return Err(FileGenerationError::LaneRootNotOwned {
                    path: lane_root,
                    owner,
                    caller,
                });
            }
        }
        Ok(Self {
            lane_root,
            lane: lane.into(),
        })
    }

    /// The lane root this layout is bound to.
    #[must_use]
    pub fn lane_root(&self) -> &Path {
        &self.lane_root
    }

    /// The lane this layout is bound to.
    #[must_use]
    pub fn lane(&self) -> &str {
        &self.lane
    }

    /// The `CURRENT` pointer path for this lane.
    #[must_use]
    pub fn current_path(&self) -> PathBuf {
        self.lane_root.join(CURRENT_FILE_NAME)
    }

    /// Create the next generation directory and publish it (explicit mode).
    ///
    /// A thin wrapper over [`FileGenerationLayout::open_next_with_rotation`] that
    /// records `rotation_seconds = meta.duration_seconds`. This is correct only
    /// in explicit mode, where a generation's duration *is* its rotation period;
    /// rotation-mode runtimes must call `open_next_with_rotation` with the real
    /// `--rotate-seconds` period so `RUN-META.json` does not misreport it.
    pub fn open_next(
        &self,
        now_unix: u64,
        rotated_from: Option<&str>,
        meta: &RunMeta,
    ) -> Result<OpenFileGeneration, FileGenerationError> {
        self.open_next_with_rotation(now_unix, rotated_from, meta, meta.duration_seconds)
    }

    /// Create the next generation directory and publish it with an explicit
    /// rotation period.
    ///
    /// Creates `gen-live-<lane>-<now_unix>` under the lane root, refusing if the
    /// directory already exists (the same-second-restart guard live-run.sh
    /// applies). Writes `RUN-META.json` — the ten `RunMeta` fields byte-for-byte
    /// as live-run.sh writes them, plus `rotated_from`, `rotation_seconds` (the
    /// caller-supplied rotation period) and `sealed_utc: null` — and then
    /// publishes `CURRENT` via a temp file and rename. `output.jsonl`/
    /// `journal.jsonl` are left for the runtime to open; their paths are
    /// returned.
    ///
    /// `rotation_seconds` is the real rotation period (`--rotate-seconds`),
    /// which in rotation mode differs from `meta.duration_seconds` (the optional
    /// total run bound). `meta.lane` must equal this layout's lane and
    /// `meta.generation` must equal the directory name; a mismatch is refused so
    /// the metadata can never disagree with the directory retention and
    /// healthcheck read.
    pub fn open_next_with_rotation(
        &self,
        now_unix: u64,
        rotated_from: Option<&str>,
        meta: &RunMeta,
        rotation_seconds: u64,
    ) -> Result<OpenFileGeneration, FileGenerationError> {
        let generation = format!("gen-live-{}-{now_unix}", self.lane);
        if meta.lane != self.lane {
            return Err(FileGenerationError::MetaMismatch {
                field: "lane",
                found: meta.lane.clone(),
                expected: self.lane.clone(),
            });
        }
        if meta.generation != generation {
            return Err(FileGenerationError::MetaMismatch {
                field: "generation",
                found: meta.generation.clone(),
                expected: generation,
            });
        }

        let dir = self.lane_root.join(&generation);
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(&dir).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                FileGenerationError::GenerationExists { path: dir.clone() }
            } else {
                FileGenerationError::Io(error)
            }
        })?;

        let rotated_from = rotated_from.map(str::to_owned);
        let run_meta_path = dir.join(RUN_META_FILE_NAME);
        let document = render_run_meta(meta, rotated_from.as_deref(), rotation_seconds, None);
        write_atomic(&run_meta_path, document.as_bytes())?;

        let current_path = self.current_path();
        write_atomic(&current_path, format!("{generation}\n").as_bytes())?;

        Ok(OpenFileGeneration {
            dir: dir.clone(),
            output_path: dir.join(OUTPUT_FILE_NAME),
            journal_path: dir.join(JOURNAL_FILE_NAME),
            opened_unix: now_unix,
            run_meta_path,
            meta: meta.clone(),
            rotated_from,
            rotation_seconds,
            sealed_utc: None,
        })
    }
}

/// A generation directory that has been created and published but not sealed.
#[derive(Clone, Debug)]
pub struct OpenFileGeneration {
    /// The generation directory.
    pub dir: PathBuf,
    /// Path the runtime opens for `output.jsonl`.
    pub output_path: PathBuf,
    /// Path the runtime opens for `journal.jsonl`.
    pub journal_path: PathBuf,
    /// The unix second the generation was opened (its name suffix).
    pub opened_unix: u64,
    run_meta_path: PathBuf,
    meta: RunMeta,
    rotated_from: Option<String>,
    rotation_seconds: u64,
    sealed_utc: Option<String>,
}

impl OpenFileGeneration {
    /// Seal the generation by rewriting `RUN-META.json` with `sealed_utc` set.
    ///
    /// Only `sealed_utc` changes; every other field keeps the exact bytes written
    /// at open. The rewrite is atomic (temp file + rename), so a reader never
    /// sees a torn `RUN-META.json`. Calling it again overwrites the seal
    /// timestamp with the new value.
    pub fn seal(&mut self, sealed_utc: impl Into<String>) -> Result<(), FileGenerationError> {
        let sealed_utc = sealed_utc.into();
        let document = render_run_meta(
            &self.meta,
            self.rotated_from.as_deref(),
            self.rotation_seconds,
            Some(&sealed_utc),
        );
        write_atomic(&self.run_meta_path, document.as_bytes())?;
        self.sealed_utc = Some(sealed_utc);
        Ok(())
    }

    /// The seal timestamp, once sealed.
    #[must_use]
    pub fn sealed_utc(&self) -> Option<&str> {
        self.sealed_utc.as_deref()
    }

    /// The `RUN-META.json` path for this generation.
    #[must_use]
    pub fn run_meta_path(&self) -> &Path {
        &self.run_meta_path
    }
}

/// Period-aligned rotation deadlines driven by the process clock.
///
/// Deadlines are computed from the first-open reading, never accumulated from
/// the previous deadline, so they cannot drift: the `n`-th deadline is always
/// `first_open_ns + n * period`. `first_open_ns` is the process clock's
/// `elapsed_ns` at the first [`FileGenerationLayout::open_next`]; the runtime
/// rotates when the clock reaches [`RotationSchedule::next_deadline`] and then
/// calls [`RotationSchedule::advance`]. The alignment is to the first open, not
/// to wall-clock quarters.
#[derive(Clone, Debug)]
pub struct RotationSchedule {
    first_open_ns: u64,
    period_ns: u128,
    periods_elapsed: u64,
}

/// The smallest rotation period a schedule will accept, in seconds.
///
/// A sub-second period would make deadlines land in the past faster than a
/// runtime can seal and open a generation, producing back-to-back rotations and
/// `GenerationExists` refusals. One second is the floor.
const MIN_ROTATION_PERIOD_SECS: u64 = 1;

impl RotationSchedule {
    /// Start a schedule from the first-open process-clock reading and a period.
    ///
    /// Panic-free: a period below [`MIN_ROTATION_PERIOD_SECS`] (including
    /// `Duration::ZERO`) is clamped up to that floor so existing callers keep
    /// compiling and never produce a stuck-in-the-past deadline. Callers that
    /// want an out-of-range period to be reported rather than clamped should use
    /// [`RotationSchedule::try_new`].
    #[must_use]
    pub fn new(first_open_ns: u64, period: Duration) -> Self {
        let floor = Duration::from_secs(MIN_ROTATION_PERIOD_SECS);
        let period = period.max(floor);
        Self {
            first_open_ns,
            period_ns: period.as_nanos(),
            periods_elapsed: 0,
        }
    }

    /// Start a schedule, rejecting a period shorter than
    /// [`MIN_ROTATION_PERIOD_SECS`].
    ///
    /// Unlike [`RotationSchedule::new`], which clamps, this reports a too-short
    /// (or zero) period as [`FileGenerationError::RotationPeriodTooShort`] so a
    /// runtime can refuse to start rather than silently rounding the operator's
    /// `--rotate-seconds` up.
    pub fn try_new(first_open_ns: u64, period: Duration) -> Result<Self, FileGenerationError> {
        if period < Duration::from_secs(MIN_ROTATION_PERIOD_SECS) {
            return Err(FileGenerationError::RotationPeriodTooShort {
                period,
                minimum: Duration::from_secs(MIN_ROTATION_PERIOD_SECS),
            });
        }
        Ok(Self {
            first_open_ns,
            period_ns: period.as_nanos(),
            periods_elapsed: 0,
        })
    }

    /// The next rotation deadline, in process-clock nanoseconds.
    ///
    /// Always `first_open_ns + (periods_elapsed + 1) * period`, saturating at
    /// `u64::MAX`.
    #[must_use]
    pub fn next_deadline(&self) -> u64 {
        let offset = self
            .period_ns
            .saturating_mul(u128::from(self.periods_elapsed) + 1);
        let deadline = u128::from(self.first_open_ns).saturating_add(offset);
        u64::try_from(deadline).unwrap_or(u64::MAX)
    }

    /// Advance past the deadline just reached.
    pub fn advance(&mut self) {
        self.periods_elapsed = self.periods_elapsed.saturating_add(1);
    }

    /// Skip forward to the first deadline strictly after `now_ns`.
    ///
    /// After a slow seal the process clock can already be several periods past
    /// [`RotationSchedule::next_deadline`]. Calling [`RotationSchedule::advance`]
    /// once per missed period would rotate back-to-back to "catch up", opening
    /// (and immediately having to seal) empty generations and risking
    /// `GenerationExists` on same-second names. Instead the runtime calls this
    /// after each seal: it sets `periods_elapsed` so that `next_deadline()` is
    /// the first period boundary *strictly* after `now_ns` — missed deadlines are
    /// skipped, never replayed. It only ever moves forward; a `now_ns` at or
    /// before the current deadline leaves the schedule unchanged.
    pub fn advance_to(&mut self, now_ns: u64) {
        // period_ns is always >= 1s (both constructors guarantee it), so the
        // division is well defined.
        let delta = u128::from(now_ns).saturating_sub(u128::from(self.first_open_ns));
        let periods = u64::try_from(delta / self.period_ns).unwrap_or(u64::MAX);
        self.periods_elapsed = self.periods_elapsed.max(periods);
    }

    /// How many periods have elapsed (rotations performed).
    #[must_use]
    pub fn periods_elapsed(&self) -> u64 {
        self.periods_elapsed
    }

    /// The SCHEDULED process-clock deadline the current generation opened at:
    /// `first_open_ns + periods_elapsed * period`, saturating at `u64::MAX`.
    ///
    /// This is the grid boundary the just-opened generation belongs to — 0
    /// rotations in, it is `first_open_ns` itself; after a rotation calls
    /// [`RotationSchedule::advance_to`], it is the deadline that rotation crossed.
    /// A runtime names each rotated generation from this grid second, never from
    /// the observed post-seal open time: because the schedule advances by at least
    /// one whole period per rotation (a rotation only fires once the clock reaches
    /// [`RotationSchedule::next_deadline`], and `advance_to` then lands strictly
    /// past it), consecutive generations' scheduled opens are always at least one
    /// period apart. Naming from the observed open instead lets a seal that
    /// overruns into the next wall second reuse the previous generation's second,
    /// which raised `GenerationExists` and killed the lane (C5).
    #[must_use]
    pub fn scheduled_open_ns(&self) -> u64 {
        let offset = self
            .period_ns
            .saturating_mul(u128::from(self.periods_elapsed));
        let scheduled = u128::from(self.first_open_ns).saturating_add(offset);
        u64::try_from(scheduled).unwrap_or(u64::MAX)
    }
}

/// Render `RUN-META.json`.
///
/// The first ten lines are byte-identical to live-run.sh's heredoc (same order,
/// two-space indent, quoted strings, bare numbers/booleans); the trailing three
/// rotation fields are appended, and the document ends with a newline, matching
/// the heredoc's final newline.
fn render_run_meta(
    meta: &RunMeta,
    rotated_from: Option<&str>,
    rotation_seconds: u64,
    sealed_utc: Option<&str>,
) -> String {
    let resumed = if meta.resumed { "true" } else { "false" };
    let rotated_from = json_nullable_string(rotated_from);
    let sealed_utc = json_nullable_string(sealed_utc);
    format!(
        "{{\n  \"lane\": \"{lane}\",\n  \"publisher_epoch\": \"{epoch}\",\n  \"generation\": \"{generation}\",\n  \"started_utc\": \"{started}\",\n  \"duration_seconds\": {duration},\n  \"max_output_bytes\": {max_output},\n  \"max_journal_bytes\": {max_journal},\n  \"max_updates\": {max_updates},\n  \"headroom_factor\": {headroom},\n  \"resumed\": {resumed},\n  \"rotated_from\": {rotated_from},\n  \"rotation_seconds\": {rotation_seconds},\n  \"sealed_utc\": {sealed_utc}\n}}\n",
        lane = json_escape(&meta.lane),
        epoch = json_escape(&meta.publisher_epoch),
        generation = json_escape(&meta.generation),
        started = json_escape(&meta.started_utc),
        duration = meta.duration_seconds,
        max_output = meta.max_output_bytes,
        max_journal = meta.max_journal_bytes,
        max_updates = meta.max_updates,
        headroom = meta.headroom_factor,
    )
}

/// Render a JSON value that is either a quoted, escaped string or `null`.
fn json_nullable_string(value: Option<&str>) -> String {
    match value {
        Some(text) => format!("\"{}\"", json_escape(text)),
        None => "null".to_string(),
    }
}

/// Escape a string for embedding inside JSON double quotes.
///
/// Handles the control characters and the two structural characters (`"` and
/// `\`) the JSON string grammar requires. Values with none of these — every
/// well-formed lane, epoch, generation and RFC-3339 timestamp — pass through
/// byte-for-byte, so the live-run.sh byte-identity contract is preserved; the
/// escaping only engages if a value is ever malformed, keeping `RUN-META.json`
/// parseable instead of emitting a torn document.
fn json_escape(value: &str) -> String {
    // Fast path: nothing needs escaping, return an owned copy of the same bytes.
    if !value.bytes().any(|b| b == b'"' || b == b'\\' || b < 0x20) {
        return value.to_string();
    }
    let mut escaped = String::with_capacity(value.len() + 8);
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{08}' => escaped.push_str("\\b"),
            '\u{0C}' => escaped.push_str("\\f"),
            c if (c as u32) < 0x20 => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Write `bytes` to `path` atomically: a sibling `<name>.tmp` file is written,
/// fsynced and renamed over `path`, then the parent directory is fsynced. On any
/// failure the temp file is removed, so a partial write never survives — the
/// same temp-then-rename discipline live-run.sh uses for `CURRENT`.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), FileGenerationError> {
    let file_name = path.file_name().ok_or_else(|| {
        FileGenerationError::Io(std::io::Error::from(std::io::ErrorKind::InvalidInput))
    })?;
    let mut temp_name = file_name.to_os_string();
    temp_name.push(".tmp");
    let temp = path.with_file_name(temp_name);

    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        sync_parent(path)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(FileGenerationError::Io)
}

/// Fsync the directory holding `path` so the rename is durable.
fn sync_parent(path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)?.sync_all()
}

/// A file-generation layout failure.
#[derive(Debug, Error)]
pub enum FileGenerationError {
    /// The lane root does not exist or is not a directory.
    #[error("lane root {path:?} is not an existing directory")]
    LaneRootNotDirectory {
        /// The offending lane root.
        path: PathBuf,
    },
    /// The lane root is not owned by the calling effective uid.
    #[error("lane root {path:?} is owned by uid {owner}, not the caller uid {caller}")]
    LaneRootNotOwned {
        /// The offending lane root.
        path: PathBuf,
        /// The directory's owner uid.
        owner: u32,
        /// The calling effective uid.
        caller: u32,
    },
    /// The generation directory already exists (same-epoch reuse refused).
    #[error("file generation {path:?} already exists; refusing to reuse an epoch")]
    GenerationExists {
        /// The directory that already existed.
        path: PathBuf,
    },
    /// `RunMeta` disagrees with the layout or the directory being created.
    #[error("run metadata {field} is {found:?}, expected {expected:?}")]
    MetaMismatch {
        /// The field that disagreed.
        field: &'static str,
        /// The value supplied.
        found: String,
        /// The value required.
        expected: String,
    },
    /// An I/O error while creating or publishing the generation.
    #[error("file generation I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// A rotation period shorter than the allowed minimum was requested.
    #[error("rotation period {period:?} is shorter than the minimum {minimum:?}")]
    RotationPeriodTooShort {
        /// The rejected period.
        period: Duration,
        /// The smallest accepted period.
        minimum: Duration,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta(lane: &str, now_unix: u64) -> RunMeta {
        RunMeta {
            lane: lane.to_string(),
            publisher_epoch: format!("live-{lane}-{now_unix}"),
            generation: format!("gen-live-{lane}-{now_unix}"),
            started_utc: "2026-09-04T12:00:00Z".to_string(),
            duration_seconds: 3600,
            max_output_bytes: 5_368_709_120,
            max_journal_bytes: 5_368_709_120,
            max_updates: 34_000_000,
            headroom_factor: 2,
            resumed: false,
        }
    }

    #[test]
    fn render_run_meta_has_the_expected_shape() {
        let meta = sample_meta("spot", 1_700_000_000);
        let document = render_run_meta(&meta, None, meta.duration_seconds, None);
        let expected = concat!(
            "{\n",
            "  \"lane\": \"spot\",\n",
            "  \"publisher_epoch\": \"live-spot-1700000000\",\n",
            "  \"generation\": \"gen-live-spot-1700000000\",\n",
            "  \"started_utc\": \"2026-09-04T12:00:00Z\",\n",
            "  \"duration_seconds\": 3600,\n",
            "  \"max_output_bytes\": 5368709120,\n",
            "  \"max_journal_bytes\": 5368709120,\n",
            "  \"max_updates\": 34000000,\n",
            "  \"headroom_factor\": 2,\n",
            "  \"resumed\": false,\n",
            "  \"rotated_from\": null,\n",
            "  \"rotation_seconds\": 3600,\n",
            "  \"sealed_utc\": null\n",
            "}\n",
        );
        assert_eq!(document, expected);
    }

    #[test]
    fn rotation_schedule_is_period_aligned_and_never_drifts() {
        let period = Duration::from_secs(1800);
        let first_open_ns = 1_234_567_890_u64;
        let mut schedule = RotationSchedule::new(first_open_ns, period);
        let period_ns = u64::try_from(period.as_nanos()).unwrap();
        for n in 1..=10_000_u64 {
            let expected = first_open_ns + n * period_ns;
            assert_eq!(schedule.next_deadline(), expected, "deadline {n}");
            // Each deadline is exactly one period past the previous — no drift.
            assert_eq!(
                schedule.next_deadline() - (first_open_ns + (n - 1) * period_ns),
                period_ns,
            );
            schedule.advance();
        }
        assert_eq!(schedule.periods_elapsed(), 10_000);
    }

    #[test]
    fn rotation_schedule_saturates_instead_of_overflowing() {
        let mut schedule = RotationSchedule::new(u64::MAX - 1, Duration::from_secs(1));
        assert_eq!(schedule.next_deadline(), u64::MAX);
        schedule.advance();
        assert_eq!(schedule.next_deadline(), u64::MAX);
    }

    #[test]
    fn try_new_rejects_a_sub_second_period_and_accepts_one_second() {
        assert!(matches!(
            RotationSchedule::try_new(0, Duration::ZERO),
            Err(FileGenerationError::RotationPeriodTooShort { .. })
        ));
        assert!(matches!(
            RotationSchedule::try_new(0, Duration::from_millis(999)),
            Err(FileGenerationError::RotationPeriodTooShort { .. })
        ));
        let schedule = RotationSchedule::try_new(0, Duration::from_secs(1)).unwrap();
        assert_eq!(schedule.next_deadline(), 1_000_000_000);
    }

    #[test]
    fn new_clamps_a_sub_second_period_up_to_one_second_without_panicking() {
        // Duration::ZERO would otherwise leave the deadline stuck in the past.
        let schedule = RotationSchedule::new(0, Duration::ZERO);
        assert_eq!(schedule.next_deadline(), 1_000_000_000);
        let clamped = RotationSchedule::new(0, Duration::from_millis(10));
        assert_eq!(clamped.next_deadline(), 1_000_000_000);
    }

    #[test]
    fn advance_to_skips_missed_deadlines_instead_of_catching_up() {
        let period = Duration::from_secs(1);
        let period_ns = u64::try_from(period.as_nanos()).unwrap();
        let first_open_ns = 1_000_u64;
        let mut schedule = RotationSchedule::new(first_open_ns, period);
        assert_eq!(schedule.next_deadline(), first_open_ns + period_ns);

        // The clock stalled three whole periods past the first deadline and is
        // now partway into the fourth interval.
        let now_ns = first_open_ns + 3 * period_ns + 500;
        schedule.advance_to(now_ns);

        // A single skip lands on the next boundary strictly after now_ns; the
        // three missed deadlines are not replayed back-to-back.
        assert_eq!(schedule.periods_elapsed(), 3);
        assert_eq!(schedule.next_deadline(), first_open_ns + 4 * period_ns);

        // A now_ns exactly on a boundary skips that boundary (strictly after).
        let mut on_boundary = RotationSchedule::new(first_open_ns, period);
        on_boundary.advance_to(first_open_ns + 2 * period_ns);
        assert_eq!(on_boundary.next_deadline(), first_open_ns + 3 * period_ns);

        // advance_to only ever moves forward.
        let mut ahead = RotationSchedule::new(first_open_ns, period);
        ahead.advance();
        ahead.advance();
        ahead.advance_to(first_open_ns); // behind the current position
        assert_eq!(ahead.periods_elapsed(), 2);
    }

    #[test]
    fn a_slow_seal_names_each_generation_from_its_scheduled_grid_second() {
        // C5 regression: the same-second `GenerationExists` collision. A file
        // generation's directory/epoch is named from a wall SECOND. The pre-fix
        // naming used the observed POST-SEAL open time, so a seal overrunning into
        // the next wall second let generation k+1 reuse generation k's second and
        // the lane died. Naming from `scheduled_open_ns` (the drift-free grid
        // deadline `first_open + n·period`) keeps every generation on its own
        // second because the schedule advances at least one whole period per
        // rotation.
        let period = Duration::from_secs(1);
        let period_ns = 1_000_000_000_u64;
        // The first open lands mid-second (`.65 s`) so an overrun can straddle a
        // wall boundary; the process-clock reading is independent of the wall origin.
        let first_open_ns = 42 * period_ns;
        let wall_first_open_ns = 178_650_000_000_u64;
        let wall_offset_ns = wall_first_open_ns - first_open_ns;
        let unix_second = |process_ns: u64| (wall_offset_ns + process_ns) / period_ns;

        // (a) Sub-period overruns whose raw post-seal opens share a wall second —
        // the exact collision — while their scheduled grid seconds stay distinct.
        let mut schedule = RotationSchedule::new(first_open_ns, period);
        let mut observed_secs = Vec::new();
        let mut scheduled_secs = Vec::new();
        for overrun in [400_000_000_u64, 300_000_000_u64] {
            let post_seal = schedule.next_deadline() + overrun;
            schedule.advance_to(post_seal);
            observed_secs.push(unix_second(post_seal));
            scheduled_secs.push(unix_second(schedule.scheduled_open_ns()));
        }
        assert_eq!(
            observed_secs[0], observed_secs[1],
            "the pre-fix observed-open naming collides on one wall second: {observed_secs:?}"
        );
        assert!(
            scheduled_secs[0] < scheduled_secs[1],
            "scheduled grid seconds are strictly increasing: {scheduled_secs:?}"
        );

        // (b) Overruns GREATER than one period still yield distinct scheduled names
        // at least one whole period apart (missed periods are skipped, never reused).
        let mut schedule = RotationSchedule::new(first_open_ns, period);
        let mut scheduled = Vec::new();
        for _ in 0..4 {
            let post_seal = schedule.next_deadline() + 1_500_000_000; // 1.5 s > period
            schedule.advance_to(post_seal);
            scheduled.push(schedule.scheduled_open_ns());
        }
        for pair in scheduled.windows(2) {
            assert!(
                pair[1] >= pair[0] + period_ns,
                "consecutive scheduled opens are at least one period apart: {pair:?}"
            );
        }
        let seconds: Vec<u64> = scheduled.iter().map(|ns| unix_second(*ns)).collect();
        let mut distinct = seconds.clone();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            seconds.len(),
            "no two generations share a wall second even under a >1 s seal overrun: {seconds:?}"
        );
    }

    #[test]
    fn json_escape_is_byte_identical_for_normal_values_and_escapes_specials() {
        // Every value RUN-META carries in a live run passes through unchanged.
        for value in [
            "spot",
            "live-binance-1700000000",
            "gen-live-oracle-1700000400",
            "2026-09-04T12:00:00Z",
        ] {
            assert_eq!(json_escape(value), value);
        }
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(json_escape("a\\b"), "a\\\\b");
        assert_eq!(json_escape("a\nb"), "a\\nb");
        assert_eq!(json_escape("a\tb\r"), "a\\tb\\r");
        assert_eq!(json_escape("\u{01}"), "\\u0001");
    }

    #[test]
    fn render_run_meta_with_a_quote_stays_valid_json() {
        let mut meta = sample_meta("spot", 1_700_000_000);
        meta.started_utc = "2026-09-04T12:00:00Z\"evil".to_string();
        let document = render_run_meta(&meta, None, meta.duration_seconds, None);
        let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
        assert_eq!(parsed["started_utc"], "2026-09-04T12:00:00Z\"evil");
    }

    #[test]
    fn caps_are_plain_copy_data() {
        let caps = FileGenerationCaps {
            max_updates: 1,
            max_output_bytes: 2,
            max_journal_bytes: 3,
        };
        let copied = caps;
        assert_eq!(caps, copied);
    }
}
