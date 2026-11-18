use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::value::RawValue;

use crate::crc32c::crc32c_hex;

pub(crate) const JOURNAL_BUFFER_BYTES: usize = 1024 * 1024;
pub(crate) const JOURNAL_SYNC_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const JOURNAL_SYNC_INTERVAL: Duration = Duration::from_secs(5);
pub(crate) const MAX_JOURNAL_LINE_BYTES: usize = 64 * 1024 * 1024;

/// Hard minimum free filesystem space. Below this watermark the shared writer
/// stops through the ordinary writer-failure path rather than continuing to
/// append toward a full disk. A named constant with no environment override:
/// this crate keeps durability limits as constants and defines no other
/// environment-configured runtime knob to stay consistent with.
pub(crate) const MINIMUM_FREE_SPACE_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) enum WriterMessage<I, N> {
    Input(I),
    End(N),
}

pub(crate) trait JournalRecords {
    type Input;
    type End;
    type Record: Serialize;

    fn input_record(input: Self::Input) -> Self::Record;
    fn end_record(end: Self::End) -> Self::Record;
}

/// A pluggable filesystem free-space query. The production reporter calls
/// `statvfs`; tests inject a function that returns a chosen number of bytes so
/// the watermark policy is exercised without a full disk.
pub(crate) type SpaceReporter = fn(&Path) -> Result<u64, std::io::Error>;

/// Free-space guard for one open journal.
pub(crate) struct Watermark {
    path: PathBuf,
    minimum_bytes: u64,
    reporter: SpaceReporter,
}

impl Watermark {
    pub(crate) fn new(path: PathBuf, minimum_bytes: u64, reporter: SpaceReporter) -> Self {
        Self {
            path,
            minimum_bytes,
            reporter,
        }
    }

    fn check(&self) -> Result<(), JournalCoreError> {
        let available = (self.reporter)(&self.path).map_err(JournalCoreError::Io)?;
        if available < self.minimum_bytes {
            return Err(JournalCoreError::LowDiskSpace {
                available,
                minimum: self.minimum_bytes,
            });
        }
        Ok(())
    }
}

/// Report the bytes available to an unprivileged writer on the filesystem that
/// holds `path`. Available bytes are `f_bavail * f_frsize`. Uses the safe
/// `rustix` wrapper because the workspace forbids `unsafe`, so a direct
/// `libc::statvfs` FFI call is not permitted.
#[cfg(unix)]
pub(crate) fn statvfs_available_bytes(path: &Path) -> Result<u64, std::io::Error> {
    let stat = rustix::fs::statvfs(path).map_err(std::io::Error::from)?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// Non-Unix fallback: without `statvfs` the watermark cannot be evaluated, so it
/// reports an unbounded amount and never trips. The journal's supported targets
/// are Unix; this exists only to keep the crate portable to compile.
#[cfg(not(unix))]
pub(crate) fn statvfs_available_bytes(_path: &Path) -> Result<u64, std::io::Error> {
    Ok(u64::MAX)
}

pub(crate) struct WriterCore {
    maximum_bytes: u64,
    pub(crate) records_written: u64,
    pub(crate) admitted_inputs: u64,
    pub(crate) bytes_written: u64,
    pub(crate) data_syncs: u64,
    last_sync_bytes: u64,
    pub(crate) last_sync_at: Instant,
    scratch: Vec<u8>,
    frame: Vec<u8>,
    watermark: Option<Watermark>,
}

impl WriterCore {
    #[cfg(test)]
    pub(crate) fn new(maximum_bytes: u64) -> Self {
        Self::with_watermark(maximum_bytes, None)
    }

    pub(crate) fn with_watermark(maximum_bytes: u64, watermark: Option<Watermark>) -> Self {
        Self {
            maximum_bytes,
            records_written: 0,
            admitted_inputs: 0,
            bytes_written: 0,
            data_syncs: 0,
            last_sync_bytes: 0,
            last_sync_at: Instant::now(),
            scratch: Vec::with_capacity(4096),
            frame: Vec::with_capacity(4096),
            watermark,
        }
    }

    /// Reject the run when free filesystem space is below the watermark.
    pub(crate) fn check_watermark(&self) -> Result<(), JournalCoreError> {
        match &self.watermark {
            Some(watermark) => watermark.check(),
            None => Ok(()),
        }
    }

    pub(crate) fn write_record<R: Serialize>(
        &mut self,
        writer: &mut BufWriter<File>,
        record: &R,
    ) -> Result<(), JournalCoreError> {
        self.scratch.clear();
        serde_json::to_writer(&mut self.scratch, record)?;
        // Frame the record as `{"c":"<crc32c>","r":<record bytes>}`. The CRC
        // covers exactly the serialized record value, not the envelope.
        let checksum = crc32c_hex(&self.scratch);
        self.frame.clear();
        self.frame.extend_from_slice(br#"{"c":""#);
        self.frame.extend_from_slice(checksum.as_bytes());
        self.frame.extend_from_slice(br#"","r":"#);
        self.frame.extend_from_slice(&self.scratch);
        self.frame.push(b'}');
        if self.frame.len() >= MAX_JOURNAL_LINE_BYTES {
            return Err(JournalCoreError::LineTooLarge {
                maximum: MAX_JOURNAL_LINE_BYTES,
            });
        }
        self.frame.push(b'\n');
        let line_bytes = u64::try_from(self.frame.len()).map_err(|_| JournalCoreError::Counter)?;
        let attempted = self
            .bytes_written
            .checked_add(line_bytes)
            .ok_or(JournalCoreError::Counter)?;
        if attempted > self.maximum_bytes {
            return Err(JournalCoreError::SizeLimit {
                attempted,
                maximum: self.maximum_bytes,
            });
        }
        writer.write_all(&self.frame)?;
        self.bytes_written = attempted;
        self.records_written = self
            .records_written
            .checked_add(1)
            .ok_or(JournalCoreError::Counter)?;
        Ok(())
    }

    pub(crate) fn sync_if_due(
        &mut self,
        writer: &mut BufWriter<File>,
    ) -> Result<(), JournalCoreError> {
        let unsynced = self
            .bytes_written
            .checked_sub(self.last_sync_bytes)
            .ok_or(JournalCoreError::Counter)?;
        if unsynced >= JOURNAL_SYNC_BYTES || self.last_sync_at.elapsed() >= JOURNAL_SYNC_INTERVAL {
            self.sync(writer)?;
        }
        Ok(())
    }

    pub(crate) fn sync(&mut self, writer: &mut BufWriter<File>) -> Result<(), JournalCoreError> {
        // Re-check free space on the same cadence as durable syncs so a filling
        // disk stops the run before the next flush cannot complete.
        self.check_watermark()?;
        writer.flush()?;
        writer.get_ref().sync_data()?;
        self.data_syncs = self
            .data_syncs
            .checked_add(1)
            .ok_or(JournalCoreError::Counter)?;
        self.last_sync_bytes = self.bytes_written;
        self.last_sync_at = Instant::now();
        Ok(())
    }
}

pub(crate) fn try_enqueue<M>(sender: &SyncSender<M>, message: M) -> Result<(), JournalCoreError> {
    match sender.try_send(message) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(JournalCoreError::QueueFull),
        Err(TrySendError::Disconnected(_)) => Err(JournalCoreError::WriterStopped),
    }
}

pub(crate) fn writer_loop<T: JournalRecords>(
    mut writer: BufWriter<File>,
    mut core: WriterCore,
    receiver: Receiver<WriterMessage<T::Input, T::End>>,
) -> Result<WriterCore, JournalCoreError> {
    let mut terminal_written = false;
    loop {
        match receiver.recv_timeout(JOURNAL_SYNC_INTERVAL) {
            Ok(WriterMessage::Input(input)) => {
                if terminal_written {
                    return Err(JournalCoreError::InputAfterTerminal);
                }
                core.write_record(&mut writer, &T::input_record(input))?;
                core.admitted_inputs = core
                    .admitted_inputs
                    .checked_add(1)
                    .ok_or(JournalCoreError::Counter)?;
                core.sync_if_due(&mut writer)?;
            }
            Ok(WriterMessage::End(end)) => {
                if terminal_written {
                    return Err(JournalCoreError::MultipleTerminal);
                }
                core.write_record(&mut writer, &T::end_record(end))?;
                terminal_written = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => core.sync_if_due(&mut writer)?,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    core.sync(&mut writer)?;
    Ok(core)
}

/// How a retained journal frames each record line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameKind {
    /// Schema 4 and earlier: the record is the whole line, unframed.
    BareV4,
    /// Schema 5 and later: each line is a CRC envelope `{"c":..,"r":..}`.
    EnvelopedV5,
}

/// A framing failure, classified by the failing line's position in the file.
///
/// Both variants fail closed. They are distinct because they mean different
/// things: a torn tail is missing evidence (the process died mid-write, so the
/// terminal record never landed), while mid-stream corruption is a damaged
/// record inside an otherwise complete file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FramingError {
    /// The final line failed to parse or its CRC did not match. Because these
    /// journals require a terminal end record, a torn final line means the
    /// evidence is incomplete; replay is rejected.
    TornTail,
    /// A non-final line failed to parse or its CRC did not match. The `line`
    /// number (1-based, counting the session line as 1) names the damage.
    Corruption { line: u64 },
}

/// The CRC envelope wrapping every schema-5 record line.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CrcEnvelope<'a> {
    c: &'a str,
    #[serde(borrow)]
    r: &'a RawValue,
}

/// Parse and CRC-verify one enveloped line, returning the exact record bytes it
/// carries (borrowed from `line`). A parse failure or CRC mismatch is a framing
/// failure classified by `is_final`.
pub(crate) fn enveloped_record_bytes(
    line: &[u8],
    is_final: bool,
    line_number: u64,
) -> Result<&[u8], FramingError> {
    verify_envelope(line).map_err(|()| framing_error(is_final, line_number))
}

/// Whether the first line of a journal is a schema-5 CRC envelope, and if so its
/// verified record bytes. A non-envelope first line is treated as a bare
/// schema-4 record and returned unchanged for strict decoding to interpret. A
/// well-formed envelope whose CRC fails is a framing failure.
pub(crate) enum FirstFrame<'a> {
    BareV4(&'a [u8]),
    EnvelopedV5(&'a [u8]),
}

pub(crate) fn classify_first_frame(
    line: &[u8],
    is_final: bool,
) -> Result<FirstFrame<'_>, FramingError> {
    match serde_json::from_slice::<CrcEnvelope>(line) {
        Ok(envelope) => {
            let record = envelope.r.get().as_bytes();
            if envelope.c == crc32c_hex(record) {
                Ok(FirstFrame::EnvelopedV5(record))
            } else {
                Err(framing_error(is_final, 1))
            }
        }
        // A line that begins with the schema-5 envelope prefix but fails to parse
        // is a torn or corrupt envelope, not a bare schema-4 record: a process
        // that died mid-write on a fresh journal leaves exactly this. Classify it
        // as a framing failure so a torn first line is reported as a torn tail
        // rather than surfacing later as an opaque decode error. Bare schema-4
        // records begin with their `record_type` tag and never match the prefix.
        Err(_) if starts_with_envelope_prefix(line) => Err(framing_error(is_final, 1)),
        Err(_) => Ok(FirstFrame::BareV4(line)),
    }
}

/// Whether `line` opens with the schema-5 CRC-envelope prefix `{"c":`.
fn starts_with_envelope_prefix(line: &[u8]) -> bool {
    line.starts_with(br#"{"c":"#)
}

fn verify_envelope(line: &[u8]) -> Result<&[u8], ()> {
    let envelope: CrcEnvelope = serde_json::from_slice(line).map_err(|_| ())?;
    let record = envelope.r.get().as_bytes();
    if envelope.c == crc32c_hex(record) {
        Ok(record)
    } else {
        Err(())
    }
}

fn framing_error(is_final: bool, line_number: u64) -> FramingError {
    if is_final {
        FramingError::TornTail
    } else {
        FramingError::Corruption { line: line_number }
    }
}

/// Whether more bytes follow the current position, used to decide if a failing
/// line is the file's final line.
pub(crate) fn more_bytes_follow(reader: &mut BufReader<File>) -> Result<bool, std::io::Error> {
    use std::io::BufRead;
    Ok(!reader.fill_buf()?.is_empty())
}

#[derive(Debug)]
pub(crate) enum JournalCoreError {
    Io(std::io::Error),
    Json(serde_json::Error),
    LineTooLarge { maximum: usize },
    SizeLimit { attempted: u64, maximum: u64 },
    LowDiskSpace { available: u64, minimum: u64 },
    Counter,
    QueueFull,
    WriterStopped,
    InputAfterTerminal,
    MultipleTerminal,
}

impl From<std::io::Error> for JournalCoreError {
    fn from(error: std::io::Error) -> Self {
        JournalCoreError::Io(error)
    }
}

impl From<serde_json::Error> for JournalCoreError {
    fn from(error: serde_json::Error) -> Self {
        JournalCoreError::Json(error)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::io::BufWriter;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::{
        JournalCoreError, MINIMUM_FREE_SPACE_BYTES, Watermark, WriterCore, statvfs_available_bytes,
    };

    fn empty_disk(_: &Path) -> Result<u64, std::io::Error> {
        Ok(0)
    }

    fn spacious_disk(_: &Path) -> Result<u64, std::io::Error> {
        Ok(u64::MAX)
    }

    fn failing_disk(_: &Path) -> Result<u64, std::io::Error> {
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    #[test]
    fn watermark_stops_the_run_below_the_minimum() {
        let watermark = Watermark::new(
            Path::new("/watermark-test").to_path_buf(),
            MINIMUM_FREE_SPACE_BYTES,
            empty_disk,
        );
        assert!(matches!(
            watermark.check(),
            Err(JournalCoreError::LowDiskSpace {
                available: 0,
                minimum,
            }) if minimum == MINIMUM_FREE_SPACE_BYTES
        ));
    }

    #[test]
    fn watermark_admits_the_run_with_headroom() {
        let watermark = Watermark::new(
            Path::new("/watermark-test").to_path_buf(),
            MINIMUM_FREE_SPACE_BYTES,
            spacious_disk,
        );
        watermark.check().expect("ample free space admits the run");
    }

    #[test]
    fn watermark_boundary_is_inclusive_of_the_minimum() {
        fn exactly_minimum(_: &Path) -> Result<u64, std::io::Error> {
            Ok(MINIMUM_FREE_SPACE_BYTES)
        }
        let watermark = Watermark::new(
            Path::new("/watermark-test").to_path_buf(),
            MINIMUM_FREE_SPACE_BYTES,
            exactly_minimum,
        );
        watermark
            .check()
            .expect("free space equal to the minimum is admitted");
    }

    #[test]
    fn watermark_surfaces_a_reporter_error() {
        let watermark = Watermark::new(
            Path::new("/watermark-test").to_path_buf(),
            MINIMUM_FREE_SPACE_BYTES,
            failing_disk,
        );
        assert!(matches!(watermark.check(), Err(JournalCoreError::Io(_))));
    }

    static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A private, owner-only scratch journal file backing the writer-core disk
    /// tests. Dropped by the OS when the test process exits; the tests below do
    /// not depend on its bytes, only on the watermark decision.
    fn scratch_writer() -> (BufWriter<File>, PathBuf) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock follows Unix epoch")
            .as_nanos();
        let seq = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "btc-signal-journal-core-{}-{unique}-{seq}.jsonl",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).expect("scratch journal opens");
        (BufWriter::new(file), path)
    }

    /// SIG-T012d disk-exhaustion chaos (open path). Below the watermark at open,
    /// the writer core stops the run through `LowDiskSpace` before its first
    /// record is written — the same `check_watermark` call
    /// `SignalJournalWriter::create` makes before the session record.
    #[test]
    fn watermark_below_minimum_at_open_stops_the_run() {
        let (_, path) = scratch_writer();
        let core = WriterCore::with_watermark(
            u64::MAX,
            Some(Watermark::new(
                path.clone(),
                MINIMUM_FREE_SPACE_BYTES,
                empty_disk,
            )),
        );
        assert!(matches!(
            core.check_watermark(),
            Err(JournalCoreError::LowDiskSpace {
                available: 0,
                minimum,
            }) if minimum == MINIMUM_FREE_SPACE_BYTES
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// SIG-T012d disk-exhaustion chaos (mid-run path). A record appends to the
    /// buffer, but the next durable sync re-checks free space and fails closed
    /// with `LowDiskSpace` rather than flushing toward a full disk.
    #[test]
    fn watermark_trips_mid_run_at_the_next_sync() {
        let (mut writer, path) = scratch_writer();
        let mut core = WriterCore::with_watermark(
            u64::MAX,
            Some(Watermark::new(
                path.clone(),
                MINIMUM_FREE_SPACE_BYTES,
                empty_disk,
            )),
        );
        core.write_record(
            &mut writer,
            &json!({"record_type": "health_tick", "now_ns": 1}),
        )
        .expect("a bounded record appends to the buffer without a space check");
        assert!(matches!(
            core.sync(&mut writer),
            Err(JournalCoreError::LowDiskSpace {
                available: 0,
                minimum,
            }) if minimum == MINIMUM_FREE_SPACE_BYTES
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// SIG-T012d disk-exhaustion, real-filesystem reach (open path). The tests
    /// above inject a `SpaceReporter`; this drives the production
    /// `statvfs_available_bytes` reporter against the real filesystem backing a
    /// scratch journal, with the watermark configured above that filesystem's
    /// actual free space. The real disk query must surface the fail-closed
    /// `LowDiskSpace` refusal, proving the production reporter integrates with
    /// the writer core rather than only the injected policy.
    #[cfg(unix)]
    #[test]
    fn watermark_trips_on_a_real_filesystem_above_actual_free_space() {
        let (_, path) = scratch_writer();
        let core = WriterCore::with_watermark(
            u64::MAX,
            Some(Watermark::new(
                path.clone(),
                u64::MAX,
                statvfs_available_bytes,
            )),
        );
        match core.check_watermark() {
            Err(JournalCoreError::LowDiskSpace { available, minimum }) => {
                assert_eq!(minimum, u64::MAX);
                assert!(
                    available < u64::MAX,
                    "real statvfs reports finite free space, got {available}"
                );
            }
            other => panic!("real-filesystem watermark must fail closed, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// SIG-T012d disk-exhaustion, real-filesystem reach (mid-run path). A record
    /// appends to the buffer, then the next durable sync re-queries the real
    /// filesystem through `statvfs_available_bytes` and fails closed rather than
    /// flushing toward a threshold set above actual free space.
    #[cfg(unix)]
    #[test]
    fn watermark_trips_mid_run_on_a_real_filesystem_at_the_next_sync() {
        let (mut writer, path) = scratch_writer();
        let mut core = WriterCore::with_watermark(
            u64::MAX,
            Some(Watermark::new(
                path.clone(),
                u64::MAX,
                statvfs_available_bytes,
            )),
        );
        core.write_record(
            &mut writer,
            &json!({"record_type": "health_tick", "now_ns": 1}),
        )
        .expect("a bounded record appends to the buffer without a space check");
        match core.sync(&mut writer) {
            Err(JournalCoreError::LowDiskSpace { available, minimum }) => {
                assert_eq!(minimum, u64::MAX);
                assert!(
                    available < u64::MAX,
                    "real statvfs reports finite free space, got {available}"
                );
            }
            other => panic!("real-filesystem sync must fail closed, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }
}
