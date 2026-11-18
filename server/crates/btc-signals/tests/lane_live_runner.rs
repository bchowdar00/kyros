//! B1/B2 guards. Every stream a shipped `LaneSpec` drives must have a concrete
//! live runner, and the composite oracle lane's index sockets and Pyth Hermes
//! stream are gated by the `SIGNALS_ENABLED_VENUES` allow-list and the Hermes
//! credential. The arming/credential decision is resolved into an injectable
//! `LaneIndexGating`, so every test here passes explicit values — none reads the
//! host environment, and none dials.

use btc_feeds::CaptureStream;
use btc_feeds::live::stream_has_live_runner;
use btc_signals::lane::{self, LaneDriverSpec, LaneSpec, LaneStream};
use btc_signals::lane_runtime::{LaneIndexGating, active_composite_streams};
use signals_core::EnabledVenues;

/// Every `CaptureStream` a lane drives: its driver streams (Streams or the stream
/// half of Composite) plus its reference streams. The poll half of a Composite or
/// a Poll lane is driven by the `PollDriver` seam, not `run_live`.
fn lane_capture_streams(spec: &LaneSpec) -> Vec<CaptureStream> {
    let mut streams = Vec::new();
    match &spec.driver {
        LaneDriverSpec::Streams(list) => streams.extend(list.iter().map(|s| s.stream)),
        LaneDriverSpec::Composite { streams: list, .. } => {
            streams.extend(list.iter().map(|s| s.stream));
        }
        LaneDriverSpec::Poll { .. } => {}
    }
    streams.extend(spec.reference_streams.iter().map(|s| s.stream));
    streams
}

/// The composite oracle lane's declared driver streams.
fn oracle_streams() -> &'static [LaneStream] {
    match &lane::ORACLE.driver {
        LaneDriverSpec::Composite { streams, .. } => streams,
        _ => panic!("the oracle lane is composite (C1)"),
    }
}

fn gating(
    tokens: Option<&str>,
    hermes_keyed: bool,
    kalshi_index_credentialed: bool,
) -> LaneIndexGating {
    LaneIndexGating {
        enabled: EnabledVenues::parse(tokens).expect("the allow-list parses"),
        hermes_keyed,
        kalshi_index_credentialed,
    }
}

#[test]
fn every_lane_spec_stream_has_a_live_runner() {
    for spec in [&lane::BINANCE, &lane::BYBIT, &lane::ORACLE] {
        for stream in lane_capture_streams(spec) {
            assert!(
                stream_has_live_runner(stream),
                "lane {} drives {stream:?} but the live path has no concrete runner for it",
                spec.requirement.lane
            );
        }
    }
    // Pin the two streams the composite oracle lane added (B1/B2): a regression
    // that drops either runner arm fails here, not only at live start-up.
    assert!(stream_has_live_runner(CaptureStream::KalshiIndex), "B2");
    assert!(
        stream_has_live_runner(CaptureStream::PythHermesStream),
        "B1"
    );
}

#[test]
fn oracle_index_sockets_arm_from_the_allow_list() {
    use btc_feeds::live::oracle_index_sockets_for;

    let both = oracle_index_sockets_for(&EnabledVenues::parse(Some("cfbrti,pyth")).unwrap());
    assert!(both.is_armed("cfbrti") && both.is_armed("pyth") && both.any());
    let cfbrti_only =
        oracle_index_sockets_for(&EnabledVenues::parse(Some("cfbrti,coinbase")).unwrap());
    assert!(cfbrti_only.is_armed("cfbrti") && !cfbrti_only.is_armed("pyth") && cfbrti_only.any());
    for disarmed in [
        EnabledVenues::parse(None).unwrap(),
        EnabledVenues::parse(Some("coinbase,kraken,kalshi,polymarket")).unwrap(),
    ] {
        assert!(!oracle_index_sockets_for(&disarmed).any());
    }
}

#[test]
fn composite_streams_are_gated_by_the_allow_list_and_the_hermes_key() {
    let streams = oracle_streams();
    let kinds =
        |active: &[LaneStream]| -> Vec<CaptureStream> { active.iter().map(|s| s.stream).collect() };
    let has = |active: &[LaneStream], stream: CaptureStream| kinds(active).contains(&stream);

    // Disarmed and unkeyed: both index streams omitted, so the composite lane
    // runs the Chainlink poll alone (empty active set) — no rights-gated dial.
    assert!(
        active_composite_streams(streams, &gating(None, false, false)).is_empty(),
        "a disarmed, unkeyed host omits both index streams"
    );
    assert!(
        active_composite_streams(streams, &gating(Some("coinbase,kalshi"), true, true)).is_empty(),
        "without cfbrti/pyth the index streams stay omitted even when keyed and credentialed"
    );

    // Armed for both, keyed, and the Kalshi index credentials present: both index
    // streams drive.
    let both = active_composite_streams(streams, &gating(Some("cfbrti,pyth"), true, true));
    assert_eq!(both.len(), 2);
    assert!(has(&both, CaptureStream::KalshiIndex) && has(&both, CaptureStream::PythHermesStream));

    // Armed and keyed but the Kalshi index credentials are ABSENT: the Kalshi
    // index socket is omitted (arming it without credentials would fail
    // KalshiAuthClient::from_env and take the Chainlink poll down), the Hermes
    // stream still drives, and the lane keeps running.
    let no_kalshi_creds =
        active_composite_streams(streams, &gating(Some("cfbrti,pyth"), true, false));
    assert_eq!(no_kalshi_creds.len(), 1);
    assert!(has(&no_kalshi_creds, CaptureStream::PythHermesStream));
    assert!(!has(&no_kalshi_creds, CaptureStream::KalshiIndex));

    // Armed and credentialed but the Hermes credential is absent: the index socket
    // still drives (an index token is armed and the Kalshi creds are present), the
    // Hermes stream is omitted (unkeyed).
    let no_key = active_composite_streams(streams, &gating(Some("cfbrti,pyth"), false, true));
    assert_eq!(no_key.len(), 1);
    assert!(has(&no_key, CaptureStream::KalshiIndex));

    // Only cfbrti armed (the index socket), keyed, credentialed: the Hermes stream
    // needs its own `pyth` token, so it stays omitted; the index socket drives.
    let cfbrti_only = active_composite_streams(streams, &gating(Some("cfbrti"), true, true));
    assert_eq!(cfbrti_only.len(), 1);
    assert!(has(&cfbrti_only, CaptureStream::KalshiIndex));
}
