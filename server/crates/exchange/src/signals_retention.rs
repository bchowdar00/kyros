//! Disk retention for the in-process signal lanes.
//!
//! Every lane the supervisor in `signals.rs` runs writes hour-long file
//! generations (`gen-live-<lane>-<unix>/` with `output.jsonl`,
//! `journal.jsonl` and `RUN-META.json`) under `SIGNALS_LANE_ROOT/<lane>/`,
//! and the producers never delete anything: the production runbook leaves
//! that to an external retention script. Inside one container there is no
//! such script, and the spot and prediction lanes together write on the
//! order of gigabytes per hour, so an unattended deployment fills its disk
//! within a day and the lanes die.
//!
//! This module is that retention, in-process. Every [`INTERVAL`] it walks
//! each lane directory under the lane root, sorts the generation
//! directories by the unix stamp in their name and deletes all but the
//! newest [`Config::keep`] of them. The generation `CURRENT` points at is
//! never deleted, whatever the count, because it is the one being written.
//! Nothing outside `gen-live-*` directories is touched: the lane
//! checkpoints and `CURRENT` pointer live at the lane root and are what a
//! restart resumes from.
//!
//! The exchange never reads the generation files live (the in-process
//! stream hook delivers every message before it reaches the file), so the
//! only cost of pruning is the loss of the replay record for the deleted
//! window.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Number of generations kept per lane, newest first.
pub const KEEP_ENV: &str = "SIGNALS_KEEP_GENERATIONS";
const DEFAULT_KEEP: usize = 3;
/// A lane always keeps its open generation plus one sealed one, so a
/// consumer copying the newest sealed file never races the delete.
const MIN_KEEP: usize = 2;
/// How often the sweep runs. Generations rotate on a period of at least a
/// minute (`signals::MIN_ROTATE_SECONDS`), so a minute is as fine as it
/// needs to be.
const INTERVAL: Duration = Duration::from_secs(60);
const GENERATION_PREFIX: &str = "gen-live-";
const CURRENT_FILE: &str = "CURRENT";

/// Retention configuration, read once from the environment.
#[derive(Debug, Clone)]
pub struct Config {
    /// The supervisor's lane root (`signals::lane_root()`).
    pub lane_root: PathBuf,
    /// Generations kept per lane; at least [`MIN_KEEP`].
    pub keep: usize,
}

impl Config {
    /// Read `SIGNALS_KEEP_GENERATIONS` for the given lane root.
    pub fn from_env(lane_root: PathBuf) -> Self {
        let keep = std::env::var(KEEP_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_KEEP)
            .max(MIN_KEEP);
        Self { lane_root, keep }
    }
}

/// Spawn the retention sweep on the current Tokio runtime. The blocking
/// filesystem work runs on the blocking pool, never on a worker thread.
pub fn spawn(config: Config) {
    tracing::info!(
        root = %config.lane_root.display(),
        keep = config.keep,
        "signals: retention sweep every {}s",
        INTERVAL.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INTERVAL);
        loop {
            ticker.tick().await;
            let config = config.clone();
            let result = tokio::task::spawn_blocking(move || sweep(&config)).await;
            match result {
                Ok(report) if report.deleted > 0 => {
                    tracing::info!(
                        deleted = report.deleted,
                        freed_bytes = report.freed_bytes,
                        "signals: pruned sealed generations"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "signals: retention sweep panicked"),
            }
        }
    });
}

/// What one sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Generation directories removed.
    pub deleted: usize,
    /// Bytes those directories held, as measured before deletion.
    pub freed_bytes: u64,
}

/// Run one sweep over every lane under the root. A missing root (no lane
/// has started yet) is an empty sweep, not an error; per-lane failures are
/// logged and skipped so one bad directory never stops the others.
pub fn sweep(config: &Config) -> Report {
    let mut report = Report::default();
    let lanes = match std::fs::read_dir(&config.lane_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return report,
        Err(e) => {
            tracing::warn!(error = %e, root = %config.lane_root.display(), "signals: cannot read lane root");
            return report;
        }
    };
    for entry in lanes.flatten() {
        let lane_dir = entry.path();
        if !lane_dir.is_dir() {
            continue;
        }
        let lane = report_for_lane(&lane_dir, config.keep);
        report.deleted += lane.deleted;
        report.freed_bytes += lane.freed_bytes;
    }
    report
}

/// Prune one lane directory: keep the newest `keep` generations by the
/// unix stamp in their name plus whatever `CURRENT` names, delete the rest.
fn report_for_lane(lane_dir: &Path, keep: usize) -> Report {
    let mut report = Report::default();
    let current = std::fs::read_to_string(lane_dir.join(CURRENT_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut generations: Vec<(u64, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(lane_dir) else {
        return report;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stamp) = generation_stamp(name) else {
            continue;
        };
        generations.push((stamp, path));
    }
    // Newest first; a stamp tie is broken by name so the order is total.
    generations.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    for (_, path) in generations.into_iter().skip(keep) {
        let is_current = current
            .as_deref()
            .is_some_and(|c| path.file_name().and_then(|n| n.to_str()) == Some(c));
        if is_current {
            continue;
        }
        let size = dir_size(&path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                report.deleted += 1;
                report.freed_bytes += size;
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "signals: could not delete generation");
            }
        }
    }
    report
}

/// The unix stamp of a `gen-live-<lane>-<unix>` directory name, or `None`
/// for any other name.
fn generation_stamp(name: &str) -> Option<u64> {
    let rest = name.strip_prefix(GENERATION_PREFIX)?;
    let (_, stamp) = rest.rsplit_once('-')?;
    stamp.parse().ok()
}

/// Total size of the regular files directly under `dir` (generations are
/// flat). Errors count as zero; this is a log figure, not accounting.
fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_gen(lane: &Path, name: &str, bytes: usize) -> PathBuf {
        let dir = lane.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("output.jsonl"), vec![b'x'; bytes]).unwrap();
        dir
    }

    #[test]
    fn keeps_newest_and_current_deletes_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let lane = root.path().join("spot");
        std::fs::create_dir_all(&lane).unwrap();
        std::fs::write(lane.join("checkpoint.json"), "{}").unwrap();
        let g1 = make_gen(&lane, "gen-live-spot-100", 10);
        let g2 = make_gen(&lane, "gen-live-spot-200", 20);
        let g3 = make_gen(&lane, "gen-live-spot-300", 30);
        let g4 = make_gen(&lane, "gen-live-spot-400", 40);
        // CURRENT names an old generation on purpose: it must survive even
        // though it is outside the keep window.
        std::fs::write(lane.join("CURRENT"), "gen-live-spot-100\n").unwrap();
        let stray = lane.join("not-a-generation");
        std::fs::create_dir_all(&stray).unwrap();

        let report = sweep(&Config {
            lane_root: root.path().to_path_buf(),
            keep: 2,
        });

        assert_eq!(
            report,
            Report {
                deleted: 1,
                freed_bytes: 20
            }
        );
        assert!(g1.exists(), "CURRENT generation is never deleted");
        assert!(!g2.exists());
        assert!(g3.exists());
        assert!(g4.exists());
        assert!(stray.exists());
        assert!(lane.join("checkpoint.json").exists());
    }

    #[test]
    fn missing_root_is_an_empty_sweep() {
        let root = tempfile::tempdir().unwrap();
        let config = Config {
            lane_root: root.path().join("absent"),
            keep: 3,
        };
        assert_eq!(sweep(&config), Report::default());
    }

    #[test]
    fn keep_floor_and_env_parse() {
        assert_eq!(
            generation_stamp("gen-live-polymarket-1788882742"),
            Some(1788882742)
        );
        assert_eq!(generation_stamp("gen-live-spot-x"), None);
        assert_eq!(generation_stamp("checkpoint.json"), None);
        assert!(Config::from_env(PathBuf::from(".")).keep >= MIN_KEEP);
    }
}
