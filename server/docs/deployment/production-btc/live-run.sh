#!/bin/sh
# TEMPLATE — one production producer process for one lane.
#
# This is NOT installed configuration. It is the ExecStart target of
# `signals-live@.service`; systemd owns supervision and this script owns
# exactly one process lifetime: pick a fresh publisher epoch, compute explicit
# byte caps, and exec the producer. It never loops, never restarts, and never
# deletes anything.
#
#   live-run.sh <spot|polymarket|kalshi|binance|bybit|oracle|cross|multi>
#
# ROTATION MODE (default; SIGNALS_LIVE_ROTATION unset or 1). The producer runs
# ONE process across many bounded file generations, sealing each and opening
# the next IN PROCESS: it owns the gen-<epoch> directories, RUN-META.json,
# CURRENT and the lane checkpoint, and it keeps every warm-up ring and
# processor in memory across a generation boundary, so a rollover leaves no
# sample gap and re-warms nothing (docs/decisions/0029). This wrapper passes
# `--lane-root` and `--rotate-seconds $DURATION_SECONDS` (the rotation period)
# with the per-generation caps computed for that period; it writes no
# generation directory, RUN-META.json or CURRENT of its own. The process runs
# until SIGTERM (`systemctl stop`), so a systemd restart now means the process
# actually died, not a planned rollover.
#
# EXPLICIT MODE (SIGNALS_LIVE_ROTATION=0, the rollback path). Today's behaviour
# byte-for-byte: one generation per process, this wrapper creates that epoch's
# generation directory, RUN-META.json and CURRENT, passes
# `--output`/`--journal`/`--duration-seconds`, and systemd rolls the completed
# window over into a fresh process via RestartForceExitStatus=0.
#
# Each file generation is a NEW output epoch whose stream opens with a `reset` at
# sequence 0, so both a restart and an in-process rotation look to a consumer like
# a normal epoch rollover it already handles (docs/CONTRACTS.md "publisher_epoch";
# docs/CONSUMER-QUICKSTART.md step 3; ADR 0029). The `--publisher-epoch` this
# wrapper passes is the process ADMISSION epoch (stamped into events, unchanged
# while the process lives); the binary derives each generation's OUTPUT epoch as
# <epoch>-<unix-at-open> (the first generation uses <epoch> verbatim). What a
# rotation keeps that a restart does not is warm in-memory state: every ring
# `Ready` before the boundary is `Ready` right after the `reset`, no re-warm. A
# genuine restart re-warms the rings honestly; internal market/settlement state
# comes from the lane's single checkpoint, resumed whenever it already exists.
#
# Exit codes are the producer's own (docs/deployment/README.md):
#   0 clean · 75 transient → restart · 78 operator · 130 signalled → stop.
# This wrapper adds only 78 for its own configuration errors, so a bad
# environment never enters a restart loop.

set -u

fail() {
    echo "live-run: $*" >&2
    exit 78
}

# --- lane -------------------------------------------------------------------

[ "$#" -eq 1 ] || fail "usage: live-run.sh <spot|polymarket|kalshi|binance|bybit|oracle|cross|multi>"
KIND="$1"
case "$KIND" in
    spot|polymarket|kalshi|binance|bybit|oracle|cross|multi) ;;
    *) fail "unknown lane '$KIND' (expected spot|polymarket|kalshi|binance|bybit|oracle|cross|multi)" ;;
esac

# --- configuration ----------------------------------------------------------
#
# SIGNALS_LIVE_ROOT        dedicated data root; default is the documented layout.
# SIGNALS_LIVE_SPOT_BIN    absolute path to the pinned `btc-signals` binary.
# SIGNALS_LIVE_PRED_BIN    absolute path to the pinned `prediction-signals` binary.
# SIGNALS_LIVE_VENUE_BIN   absolute path to the pinned `venue-signals` binary
#                          (the binance, bybit, oracle and cross lanes run through it).
# SIGNALS_ENABLED_VENUES   runtime venue allow-list (ADR 0026); unset means the
#                          four adopted venues. The lane binary is the authority
#                          and refuses to start with 78 on a venue it does not
#                          allow; this wrapper only rejects an unknown token
#                          early, so a typo fails before the lane is launched.
#                          Clearing a venue's RIGHTS-LEDGER row is the
#                          precondition to listing it.
# KALSHI_API_KEY_ID        kalshi lane only, from the credential environment file.
# KALSHI_PRIVATE_KEY_PATH  kalshi lane only, from the credential environment file.
#
# Credentials are read from the environment and are never placed in argv, in
# the run metadata, or in this script's log lines.

# Preflight the allow-list when it is set: every token must be one the lane
# binaries know. An unset variable is the documented default and is left alone.
if [ -n "${SIGNALS_ENABLED_VENUES+set}" ]; then
    _venues="$SIGNALS_ENABLED_VENUES"
    while [ -n "$_venues" ]; do
        case "$_venues" in
            *,*) _token="${_venues%%,*}"; _venues="${_venues#*,}" ;;
            *) _token="$_venues"; _venues="" ;;
        esac
        # Trim leading and trailing whitespace only, matching `str::trim` in
        # the binaries. `tr -d` would delete interior whitespace too, and then
        # a value like 'coin base' would pass here and exit 78 in the lane.
        _token="$(printf '%s' "$_token" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"
        [ -n "$_token" ] || continue
        case "$_token" in
            coinbase|kraken|polymarket|kalshi|binance|bybit|oracle|cfbrti|pyth) ;;
            *) fail "SIGNALS_ENABLED_VENUES names unknown venue token '$_token' (expected coinbase, kraken, polymarket, kalshi, binance, bybit, oracle, cfbrti, or pyth)" ;;
        esac
    done
    unset _venues _token
fi

LIVE_ROOT="${SIGNALS_LIVE_ROOT:-/var/lib/signals-live}"
case "$LIVE_ROOT" in
    /*) ;;
    *) fail "SIGNALS_LIVE_ROOT must be absolute (got '$LIVE_ROOT')" ;;
esac

LANE_ROOT="$LIVE_ROOT/$KIND"
[ -d "$LANE_ROOT" ] || fail "lane root '$LANE_ROOT' does not exist (create it 0700 admin:admin before enabling the unit)"
[ -w "$LANE_ROOT" ] || fail "lane root '$LANE_ROOT' is not writable by this service identity"

# --- duration ---------------------------------------------------------------
#
# Duration bounds one generation's file sizes, NOT the demo. One hour for spot
# and thirty minutes for the prediction venues keeps every generation small
# enough to copy, replay, and verify inside the demo window while the lane
# itself runs continuously across rollovers. The producers accept up to
# 7 days (MAX_RUN_SECONDS), so these are policy, not a binary limit.
#
# In the default rotation mode DURATION_SECONDS is the in-process rotation
# period (`--rotate-seconds`); in explicit mode (SIGNALS_LIVE_ROTATION=0) it is
# the per-process window (`--duration-seconds`) that systemd rolls over. The
# caps below are computed from it identically in both modes.
#
# Spot was 6 h (21600 s) and is now 1 h. The generation is the unit retention
# works in: with retention.sh's keep floor of 2 generations per lane, a 6-hour
# spot generation meant up to 18 hours of spot on disk before anything could be
# reclaimed, and its 2-hour min-age floor could not even reach the newest ones.
# One hour makes the floor mean roughly three hours of spot — a feed-drop
# safety net (SIG-050/SIG-051), which is all this storage is for — and cuts the
# spot caps below from 30 GiB to 5 GiB each.

case "$KIND" in
    spot)                     DURATION_SECONDS="${SIGNALS_LIVE_SPOT_DURATION_SECONDS:-3600}" ;;
    polymarket|kalshi)        DURATION_SECONDS="${SIGNALS_LIVE_PRED_DURATION_SECONDS:-1800}" ;;
    binance|bybit|oracle|cross)     DURATION_SECONDS="${SIGNALS_LIVE_VENUE_DURATION_SECONDS:-1800}" ;;
    multi)                    DURATION_SECONDS="${SIGNALS_LIVE_MULTI_DURATION_SECONDS:-1800}" ;;
esac
case "$DURATION_SECONDS" in
    ''|*[!0-9]*) fail "duration must be a positive integer (got '$DURATION_SECONDS')" ;;
esac
[ "$DURATION_SECONDS" -gt 0 ] || fail "duration must be greater than zero"
[ "$DURATION_SECONDS" -le 604800 ] || fail "duration exceeds the producer's 7-day maximum"

# --- caps -------------------------------------------------------------------
#
# Caps are ALWAYS explicit: the binary defaults (2 GiB output / 4 GiB journal)
# are unrelated to these run windows and would either truncate a spot
# generation or fail to bound a prediction one.
#
#   cap = measured rate/hour * duration_seconds / 3600 * HEADROOM_FACTOR
#
# At the durations above that is 5 GiB output and 5 GiB journal for a 1-hour
# spot generation, 8 GiB output / 5 GiB journal for a 30-minute prediction
# generation, and 3 GiB output and 3 GiB journal — each 3 GiB + 1 MiB per stream
# — for a 30-minute binance/bybit/oracle venue generation (README.md, "Durations
# and caps", carries the same table).
#
# Measured rates, last calibrated 2026-08-26 (recalibrate from the run digests,
# not from memory):
#   spot        ~2.4 GiB/h journal, rounded up to 2.5 GiB/h; output proxied
#               off the journal rate, which is the larger of the two.
#   prediction  ~8 GiB/h output, ~5 GiB/h journal, ~17M updates/h.
#   venue       3 GiB/h (binance+bybit+oracle). The 2026-09-04 smoke measured
#               binance 1.9 GB/h journal+output (763 MB in 25 min, ~0.9 GiB per
#               30-min generation), bybit 0.7 GB/h, oracle < 0.01 GB/h — all well
#               under the 3 GiB/h ceiling, which is held generous so a generation
#               never nears the cap; output proxied off the same rate. Yields a
#               3 GiB + 1 MiB per-stream cap per 30-min generation.
# The prediction lanes changed emission on 2026-09-02 in slice 1 (19568f7:
# publish on measurement change plus a per-interval refresh instead of restating
# every instance per admitted event; clock-derived fields ride along but do not
# trigger), so the two prediction output numbers above are stale high and are
# due for recalibration from the next run digests. The runbook projects
# ~1.4 GiB/h per venue from a synthetic replay; that is a projection, not a
# digest, so the caps here are deliberately left at the 2026-08-26 measurement
# until a real soak digest replaces it.
#
# HEADROOM_FACTOR is 2 here, not the 3 used by the audited evidence lanes:
# these caps guard a lane whose disk is monitored by healthcheck.sh
# every minute, and 2x already covers the observed burst spread. Raise it,
# never lower it, if a lane ever refuses on a cap.

GIB=1073741824
HEADROOM_FACTOR="${SIGNALS_LIVE_HEADROOM_FACTOR:-2}"
case "$HEADROOM_FACTOR" in
    ''|*[!0-9]*) fail "headroom factor must be a positive integer" ;;
esac
[ "$HEADROOM_FACTOR" -ge 2 ] || fail "headroom factor must be at least 2"

RATE_SPOT_JOURNAL_BYTES_PER_HOUR=$(( 5 * GIB / 2 ))   # 2.5 GiB/h (2.4 observed)
RATE_PRED_OUTPUT_BYTES_PER_HOUR=$(( 8 * GIB ))        # 8 GiB/h   (measured)
RATE_PRED_JOURNAL_BYTES_PER_HOUR=$(( 5 * GIB ))       # 5 GiB/h   (measured)
RATE_UPDATES_PER_HOUR=17000000                        # 17M/h     (measured, prediction)
# The binance and bybit venue lanes publish 35 and 37 tier-1 signal identities respectively
# (14 spot siblings + B2 derivatives (3 binance / 5 bybit, after E62 deleted perp_cvd/liquidation_pressure) + 14 C3a TA + 4 C2, ADR 0026/0028/E62, E49/E51) and the oracle lane
# publishes 1 (btc.chainlink.oracle_reference); the B-wave signal processors have
# landed, so these lanes are no longer feed-only. Recalibrate from a soak digest
# as live hours accrue.
RATE_VENUE_BYTES_PER_HOUR=$(( 3 * GIB ))              # 3 GiB/h — generous ceiling over the 2026-09-04 smoke (binance 1.9 GB/h journal+output, bybit 0.7 GB/h, oracle < 0.01 GB/h); see RUN-THE-PRODUCER.md
CAP_FIXED_HEADROOM_BYTES=$(( 1024 * 1024 ))           # covers the terminal record

cap_bytes() {
    # cap_bytes <rate_per_hour>
    echo $(( $1 * HEADROOM_FACTOR * DURATION_SECONDS / 3600 + CAP_FIXED_HEADROOM_BYTES ))
}

cap_count() {
    # cap_count <rate_per_hour> — no byte headroom addend
    echo $(( $1 * HEADROOM_FACTOR * DURATION_SECONDS / 3600 ))
}

# --- binary and lane arguments ----------------------------------------------

case "$KIND" in
    spot)
        BIN="${SIGNALS_LIVE_SPOT_BIN:-}"
        [ -n "$BIN" ] || fail "SIGNALS_LIVE_SPOT_BIN is required"
        MAX_OUTPUT_FLAG=--max-bytes
        MAX_OUTPUT_BYTES=$(cap_bytes "$RATE_SPOT_JOURNAL_BYTES_PER_HOUR")
        MAX_JOURNAL_BYTES=$(cap_bytes "$RATE_SPOT_JOURNAL_BYTES_PER_HOUR")
        ;;
    polymarket|kalshi|multi)
        # The multi lane (C1/E57) runs both prediction legs in one process through
        # the prediction producer, so it uses the same binary and cap rates.
        BIN="${SIGNALS_LIVE_PRED_BIN:-}"
        [ -n "$BIN" ] || fail "SIGNALS_LIVE_PRED_BIN is required"
        MAX_OUTPUT_FLAG=--max-output-bytes
        MAX_OUTPUT_BYTES=$(cap_bytes "$RATE_PRED_OUTPUT_BYTES_PER_HOUR")
        MAX_JOURNAL_BYTES=$(cap_bytes "$RATE_PRED_JOURNAL_BYTES_PER_HOUR")
        ;;
    binance|bybit|oracle|cross)
        BIN="${SIGNALS_LIVE_VENUE_BIN:-}"
        [ -n "$BIN" ] || fail "SIGNALS_LIVE_VENUE_BIN is required"
        MAX_OUTPUT_FLAG=--max-output-bytes
        MAX_OUTPUT_BYTES=$(cap_bytes "$RATE_VENUE_BYTES_PER_HOUR")
        MAX_JOURNAL_BYTES=$(cap_bytes "$RATE_VENUE_BYTES_PER_HOUR")
        ;;
esac
MAX_UPDATES=$(cap_count "$RATE_UPDATES_PER_HOUR")

case "$BIN" in
    /*) ;;
    *) fail "producer binary path must be absolute (got '$BIN')" ;;
esac
[ -x "$BIN" ] || fail "producer binary '$BIN' is not executable"

if [ "$KIND" = kalshi ] || [ "$KIND" = multi ]; then
    # The multi lane runs the Kalshi leg (index-disarmed), so it needs the Kalshi
    # market credentials exactly as the single-venue Kalshi lane does.
    [ -n "${KALSHI_API_KEY_ID:-}" ] || fail "KALSHI_API_KEY_ID is not in the environment (EnvironmentFile missing?)"
    [ -n "${KALSHI_PRIVATE_KEY_PATH:-}" ] || fail "KALSHI_PRIVATE_KEY_PATH is not in the environment"
    [ -r "$KALSHI_PRIVATE_KEY_PATH" ] || fail "KALSHI_PRIVATE_KEY_PATH is not readable by this service identity"
fi

# --- rotation mode ----------------------------------------------------------
#
# SIGNALS_LIVE_ROTATION selects how a lane produces generations:
#   1 (default, or unset)  in-process rotation — the producer owns the
#                          generation dirs, RUN-META.json, CURRENT and the seal
#                          → open cycle; this wrapper writes none of them.
#   0                      explicit mode (rollback path) — today's behaviour
#                          byte-for-byte: one generation per process written by
#                          this wrapper, rolled over by systemd.

ROTATION="${SIGNALS_LIVE_ROTATION:-1}"
case "$ROTATION" in
    0|1) ;;
    *) fail "SIGNALS_LIVE_ROTATION must be 0 or 1 (got '$ROTATION')" ;;
esac

# --- epoch and generation ---------------------------------------------------
#
# One epoch per start, named live-<kind>-<unixtime>. The producer refuses to
# reuse any epoch in a resumed checkpoint's lineage, so a same-second restart
# is caught by the binary rather than silently accepted; explicit mode guards
# it here too so the failure is a clear configuration exit instead of a lineage
# error.

EPOCH="live-$KIND-$(date -u +%s)"
CHECKPOINT="$LANE_ROOT/checkpoint.json"

if [ "$ROTATION" = 1 ]; then
    # Rotation mode: the producer creates gen-<epoch> dirs, RUN-META.json,
    # CURRENT and the checkpoint itself (checkpoint defaults to
    # <lane-root>/checkpoint.json), and rotates in process every
    # DURATION_SECONDS with the per-generation caps below, keeping rings and
    # processors in memory across the boundary (docs/decisions/0029). No
    # --duration-seconds is passed, so the process runs until SIGTERM; a
    # planned rollover is no longer a process exit.
    set -- \
        --lane-root "$LANE_ROOT" \
        --rotate-seconds "$DURATION_SECONDS" \
        --publisher-epoch "$EPOCH" \
        --max-updates "$MAX_UPDATES" \
        "$MAX_OUTPUT_FLAG" "$MAX_OUTPUT_BYTES" \
        --max-journal-bytes "$MAX_JOURNAL_BYTES" \
        --headroom-factor "$HEADROOM_FACTOR"
    # Resume whenever the lane already has a checkpoint; the binary re-warms its
    # rings honestly on a crash-resume, but a planned rotation never resumes.
    [ -f "$CHECKPOINT" ] && set -- "$@" --resume-from-checkpoint
    LAUNCH_LOG="epoch=$EPOCH rotate=${DURATION_SECONDS}s lane-root=$LANE_ROOT"
else
    # Explicit mode (SIGNALS_LIVE_ROTATION=0): one generation per process, this
    # wrapper writes the generation dir / RUN-META.json / CURRENT and systemd
    # rolls the completed window over (RestartForceExitStatus=0).
    GENERATION="$LANE_ROOT/gen-$EPOCH"
    [ -e "$GENERATION" ] && fail "generation '$GENERATION' already exists; refusing to reuse an epoch"

    OUTPUT="$GENERATION/output.jsonl"
    JOURNAL="$GENERATION/journal.jsonl"

    (umask 0077 && mkdir "$GENERATION") || fail "cannot create generation directory '$GENERATION'"

    # RUN-META.json is operator metadata for healthcheck.sh, the retention job
    # and the runbook. It is not an evidence receipt and carries no credential.
    cat > "$GENERATION/RUN-META.json" <<EOF
{
  "lane": "$KIND",
  "publisher_epoch": "$EPOCH",
  "generation": "gen-$EPOCH",
  "started_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "duration_seconds": $DURATION_SECONDS,
  "max_output_bytes": $MAX_OUTPUT_BYTES,
  "max_journal_bytes": $MAX_JOURNAL_BYTES,
  "max_updates": $MAX_UPDATES,
  "headroom_factor": $HEADROOM_FACTOR,
  "resumed": $( [ -f "$CHECKPOINT" ] && echo true || echo false )
}
EOF

    # CURRENT names the newest generation for this lane. The stream server and
    # the retention job both read it; retention NEVER deletes the one it names.
    printf '%s\n' "gen-$EPOCH" > "$LANE_ROOT/CURRENT.tmp" \
        && mv "$LANE_ROOT/CURRENT.tmp" "$LANE_ROOT/CURRENT" \
        || fail "cannot publish CURRENT for lane '$KIND'"

    set -- \
        --output "$OUTPUT" \
        --journal "$JOURNAL" \
        --checkpoint "$CHECKPOINT" \
        --publisher-epoch "$EPOCH" \
        --duration-seconds "$DURATION_SECONDS" \
        --max-updates "$MAX_UPDATES" \
        "$MAX_OUTPUT_FLAG" "$MAX_OUTPUT_BYTES" \
        --max-journal-bytes "$MAX_JOURNAL_BYTES"

    # Resume whenever the lane already has a checkpoint. A first-ever start has
    # none and begins clean; every later start — rollover or exit-75 restart —
    # continues the same lineage under the new epoch.
    [ -f "$CHECKPOINT" ] && set -- "$@" --resume-from-checkpoint
    LAUNCH_LOG="epoch=$EPOCH duration=${DURATION_SECONDS}s gen=gen-$EPOCH"
fi

# --- launch -----------------------------------------------------------------

case "$KIND" in
    spot)
        echo "live-run: spot $LAUNCH_LOG"
        exec "$BIN" run-spot-signals "$@"
        ;;
    polymarket|kalshi)
        echo "live-run: $KIND $LAUNCH_LOG"
        exec "$BIN" run --venue "$KIND" "$@"
        ;;
    multi)
        # The multi lane (C1/E57) composes the Kalshi (index-disarmed) and
        # Polymarket prediction pipelines in one process over a merged venue-tagged
        # journal, publishing btc.multi.prediction_dislocation and
        # btc.multi.pair_cost_spread. Gated on BOTH venues' allow-list entries; the
        # same C5 rotation-mode argv drives it (`run --venue multi`).
        echo "live-run: multi $LAUNCH_LOG"
        exec "$BIN" run --venue multi "$@"
        ;;
    binance|bybit|oracle|cross)
        # The binance, bybit, oracle and cross lanes run through the one venue-lane
        # shell (venue-signals, ruling E26). binance and bybit refuse with 78
        # until SIGNALS_ENABLED_VENUES lists them (R7); oracle starts unset; the
        # cross lane (C1/E52) dials the Coinbase, Kraken and Bybit sockets, so it
        # refuses with 78 unless SIGNALS_ENABLED_VENUES lists `bybit`, `coinbase`
        # and `kraken` (coinbase and kraken are on by default, so arming cross
        # means adding `bybit`).
        echo "live-run: $KIND $LAUNCH_LOG"
        exec "$BIN" run --lane "$KIND" "$@"
        ;;
esac
