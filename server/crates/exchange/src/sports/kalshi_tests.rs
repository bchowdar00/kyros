//! Kalshi's markets, as read today for Toronto at Detroit, and the frame
//! against the generated registry.

use super::*;
use crate::automation::signal_bus::{field_types, to_signal_event};
use crate::registry;

const FIXTURE: &str = include_str!("fixtures/kalshi-nba-game.json");
const TIPOFF: u64 = 1_795_221_000_000; // 2026-11-21T00:30:00Z, the 20th in New York

/// The fixture with one of its two markets edited. Index 0 is the home
/// market (Detroit), index 1 the away market (Toronto).
fn markets_with(edit: impl Fn(&mut Json)) -> String {
    let mut v: Json = serde_json::from_str(FIXTURE).unwrap();
    edit(&mut v["markets"]);
    v.to_string()
}

#[test]
fn a_game_is_filed_under_its_teams_and_its_new_york_date() {
    assert_eq!(event_ticker("TOR", "DET", TIPOFF), "KXNBAGAME-26NOV20TORDET");
    // 01:00Z on the 20th is still the 19th in New York.
    assert_eq!(event_ticker("POR", "MIL", 1_795_136_400_000), "KXNBAGAME-26NOV19PORMIL");
}

#[test]
fn the_markets_read_as_the_home_midpoint() {
    // Detroit (home) bids 0.7200 and asks 0.7300: the midpoint is 0.725.
    assert_eq!(parse_home_mid(FIXTURE, "TOR", "DET", TIPOFF), Some("0.725".parse().unwrap()));
}

#[test]
fn a_read_that_is_not_this_games_usable_home_market_is_refused() {
    let p = |body: &str| parse_home_mid(body, "TOR", "DET", TIPOFF);
    assert_eq!(p(&markets_with(|m| m[0]["status"] = json!("finalized"))), None, "home market not active");
    assert_eq!(p(&markets_with(|m| m[0]["market_type"] = json!("scalar"))), None, "home market not binary");
    assert_eq!(p(&markets_with(|m| { m.as_array_mut().unwrap().remove(1); })), None, "away market missing");
    assert_eq!(p(&markets_with(|m| m[0]["event_ticker"] = json!("KXNBAGAME-26NOV20TORCHI"))), None, "a market's event ticker differs");
    assert_eq!(p(&markets_with(|m| m[0]["expected_expiration_time"] = json!("2026-11-20T23:00:00Z"))), None, "expiration before tipoff");
    assert_eq!(p(&markets_with(|m| m[0]["expected_expiration_time"] = json!("2026-11-21T07:00:00Z"))), None, "expiration more than six hours after tipoff");
    assert_eq!(p(&markets_with(|m| m[0]["yes_bid_dollars"] = json!("0.0000"))), None, "a zero bid");
    assert_eq!(p(&markets_with(|m| m[0]["yes_ask_dollars"] = json!("1.0000"))), None, "a unit ask");
    assert_eq!(p(&markets_with(|m| m[0]["yes_bid_dollars"] = json!("0.7300"))), None, "bid equal to ask");
    assert_eq!(p(&markets_with(|m| m[0]["yes_bid_dollars"] = json!("0.7400"))), None, "bid above ask");
    assert_eq!(p(&markets_with(|m| m[0]["yes_ask_dollars"] = json!("not-a-number"))), None, "a non-numeric price");
    assert_eq!(p(&markets_with(|m| { m.as_array_mut().unwrap().remove(0); })), None, "home market missing");
    assert_eq!(p(&markets_with(|m| m[0]["ticker"] = json!("KXNBAGAME-26NOV20TORDET-DETX"))), None, "the home market's own ticker differs");
    assert_eq!(p(&markets_with(|m| { m[0].as_object_mut().unwrap().remove("expected_expiration_time"); })), None, "no stated expiration");
    assert_eq!(p(&markets_with(|m| m[0]["expected_expiration_time"] = json!("tonight"))), None, "an expiration that is not a time");
    assert_eq!(p(&markets_with(|m| { m[0].as_object_mut().unwrap().remove("yes_bid_dollars"); })), None, "no bid given");
    assert_eq!(p("not json"), None, "a body that is not JSON");
    // The same two teams the other way round is another game; its markets
    // are filed under another event ticker and this read finds nothing.
    assert_eq!(parse_home_mid(FIXTURE, "DET", "TOR", TIPOFF), None, "home and away swapped");
}

#[test]
fn the_frame_fits_the_registry_and_the_two_sides_sum_to_one() {
    let frame = line_frame("DET", "TOR", "0.725".parse().unwrap(), 7);
    assert_eq!(frame.signal_id, NBA_KALSHI_SIGNAL);
    assert_eq!(frame.values["home_team"], json!("DET"));
    assert_eq!(frame.values["away_team"], json!("TOR"));
    assert_eq!(frame.values["home_prob"], json!("0.725"));
    assert_eq!(frame.values["away_prob"], json!("0.275"));
    let registry = registry::for_tests();
    let def = registry.signals.iter().find(|s| s.signal_id == NBA_KALSHI_SIGNAL).expect("defined");
    to_signal_event(&frame, &field_types(def)).expect("the platform accepts it");
}

#[test]
fn only_a_game_kalshi_does_not_list_waits_the_long_retry() {
    let price = Read::Price("0.725".parse().unwrap());
    // Kalshi answered and has no usable market: nothing to follow, so look again in a minute.
    assert_eq!(wait_after(&Read::Unlisted, false), LISTING_RETRY);
    // One read that did not arrive is not an unlisted game: try again at the normal pace,
    // before any line stands and after one was withdrawn.
    assert_eq!(wait_after(&Read::Failed, false), POLL);
    // A standing line is read at the normal pace whatever the last read came to, so it
    // ages out through the staleness count and not through one long sleep.
    assert_eq!(wait_after(&Read::Unlisted, true), POLL);
    assert_eq!(wait_after(&Read::Failed, true), POLL);
    assert_eq!(wait_after(&price, false), POLL);
    assert_eq!(wait_after(&price, true), POLL);
}

#[test]
fn a_standing_line_survives_thirty_seconds_of_failed_reads() {
    // Reading faster must not make the line quicker to vanish: each withdrawal also
    // clears the snapshot a running strategy trades on.
    assert_eq!(POLL * super::super::polymarket::STALE_AFTER, Duration::from_secs(30));
}

#[test]
fn a_dead_price_never_stands_much_longer_than_a_minute() {
    // The worst case: every read hangs until its timeout before it counts as a miss.
    assert!((POLL + super::super::polymarket::READ_TIMEOUT) * super::super::polymarket::STALE_AFTER <= Duration::from_secs(70));
}
