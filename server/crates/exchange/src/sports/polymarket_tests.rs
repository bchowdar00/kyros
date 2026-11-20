//! Polymarket's listing and midpoint, as read on 2026-11-19 for Boston at
//! Denver, and the frame against the generated registry.

use super::*;
use crate::automation::signal_bus::{field_types, to_signal_event};
use crate::registry;

const LISTING: &str = include_str!("fixtures/polymarket-moneyline.json");
const TIPOFF: u64 = 1_795_055_400_000; // 2026-11-19T02:30:00Z, the 18th in New York

fn listing_with(edit: impl Fn(&mut Json)) -> String {
    let mut v: Json = serde_json::from_str(LISTING).unwrap();
    edit(&mut v[0]);
    v.to_string()
}

#[test]
fn a_game_is_filed_under_its_teams_and_its_new_york_date() {
    assert_eq!(market_slug("BOS", "DEN", TIPOFF), "nba-bos-den-2026-11-18");
    // 01:00Z on the 20th is still the 19th in New York.
    assert_eq!(market_slug("POR", "MIL", 1_795_136_400_000), "nba-por-mil-2026-11-19");
}

#[test]
fn every_team_has_the_name_polymarket_uses() {
    let codes = [
        "ATL", "BOS", "BKN", "CHA", "CHI", "CLE", "DAL", "DEN", "DET", "GSW", "HOU", "IND", "LAC", "LAL", "MEM", "MIA", "MIL", "MIN", "NOP", "NYK", "OKC",
        "ORL", "PHI", "PHX", "POR", "SAC", "SAS", "TOR", "UTA", "WAS",
    ];
    let names: std::collections::BTreeSet<_> = codes.iter().map(|c| nickname(c).unwrap_or_else(|| panic!("{c}"))).collect();
    assert_eq!(names.len(), 30, "thirty teams, thirty names");
    assert_eq!(nickname("POR"), Some("Trail Blazers"));
    assert_eq!(nickname("PHI"), Some("76ers"));
    assert_eq!(nickname("XYZ"), None);
}

#[test]
fn the_listing_says_which_token_is_the_home_side() {
    let listing = parse_listing(LISTING, "BOS", "DEN", TIPOFF).expect("the real listing reads");
    // Outcomes are ["Celtics", "Nuggets"]; the second token is Denver's.
    assert_eq!(listing.away_token, "99426483926333620812294297208645890584971222355906907335946591951004629045116");
    assert_eq!(listing.home_token, "41421390467027245083848930526071031519865310364219893010124515637518812247316");
}

#[test]
fn the_home_token_is_found_by_name_not_by_position() {
    let swapped = listing_with(|m| {
        m["outcomes"] = json!("[\"Nuggets\", \"Celtics\"]");
        m["clobTokenIds"] = json!("[\"111\", \"222\"]");
    });
    assert_eq!(parse_listing(&swapped, "BOS", "DEN", TIPOFF), Some(Listing { home_token: "111".into(), away_token: "222".into() }));
}

#[test]
fn a_listing_that_is_not_this_games_open_moneyline_is_not_used() {
    assert_eq!(parse_listing(LISTING, "BOS", "LAL", TIPOFF), None, "another home team");
    assert_eq!(parse_listing(LISTING, "DEN", "BOS", TIPOFF), None, "the same teams the other way round are another game");
    assert_eq!(parse_listing(LISTING, "BOS", "XYZ", TIPOFF), None, "a team with no name");
    assert_eq!(parse_listing(&listing_with(|m| m["closed"] = json!(true)), "BOS", "DEN", TIPOFF), None, "closed");
    assert_eq!(parse_listing(&listing_with(|m| m["active"] = json!(false)), "BOS", "DEN", TIPOFF), None, "not active");
    assert_eq!(parse_listing(&listing_with(|m| m["sportsMarketType"] = json!("spreads")), "BOS", "DEN", TIPOFF), None, "not a moneyline");
    assert_eq!(parse_listing(&listing_with(|m| m["clobTokenIds"] = json!("[\"1\"]")), "BOS", "DEN", TIPOFF), None, "one token");
    assert_eq!(parse_listing(&listing_with(|m| m["outcomes"] = json!("[\"Celtics\", \"Nuggets\", \"Draw\"]")), "BOS", "DEN", TIPOFF), None, "three outcomes");
    assert_eq!(parse_listing("[]", "BOS", "DEN", TIPOFF), None, "not listed");
    assert_eq!(parse_listing("not json", "BOS", "DEN", TIPOFF), None);
}

#[test]
fn the_listing_must_be_for_this_tipoff() {
    // The same two teams meet again later in the season under another
    // address; a listing that says another start is another game.
    let other_day = listing_with(|m| m["gameStartTime"] = json!("2026-12-01 02:30:00+00"));
    assert_eq!(parse_listing(&other_day, "BOS", "DEN", TIPOFF), None);
    assert!(parse_listing(LISTING, "BOS", "DEN", TIPOFF).is_some());
    // Venues disagree on the minute, not on the evening.
    assert!(parse_listing(LISTING, "BOS", "DEN", TIPOFF + 30 * 60_000).is_some());
    // Filed under another day's address: not this game, whatever it says inside.
    let other_address = listing_with(|m| m["slug"] = json!("nba-bos-den-2026-11-17"));
    assert_eq!(parse_listing(&other_address, "BOS", "DEN", TIPOFF), None);
}

#[test]
fn a_midpoint_is_a_price_strictly_between_zero_and_one() {
    assert_eq!(parse_midpoint(r#"{"mid":"0.225"}"#), Some("0.225".parse().unwrap()));
    assert_eq!(parse_midpoint(r#"{"mid":"0"}"#), None);
    assert_eq!(parse_midpoint(r#"{"mid":"1"}"#), None);
    assert_eq!(parse_midpoint(r#"{"mid":"-0.2"}"#), None);
    assert_eq!(parse_midpoint(r#"{"mid":"abc"}"#), None);
    assert_eq!(parse_midpoint(r#"{"error":"No orderbook exists for the requested token id"}"#), None);
    assert_eq!(parse_midpoint(""), None);
}

#[test]
fn the_frame_fits_the_registry_and_the_two_sides_sum_to_one() {
    let frame = line_frame("DEN", "BOS", "0.225".parse().unwrap(), 7);
    assert_eq!(frame.signal_id, NBA_POLYMARKET_SIGNAL);
    assert_eq!(frame.values["home_team"], json!("DEN"));
    assert_eq!(frame.values["away_team"], json!("BOS"));
    assert_eq!(frame.values["home_prob"], json!("0.225"));
    assert_eq!(frame.values["away_prob"], json!("0.775"));
    let registry = registry::for_tests();
    let def = registry.signals.iter().find(|s| s.signal_id == NBA_POLYMARKET_SIGNAL).expect("defined");
    to_signal_event(&frame, &field_types(def)).expect("the platform accepts it");
}

#[test]
fn a_line_that_stops_arriving_is_withdrawn_once_and_comes_back_with_the_next_reading() {
    let mid: Decimal = "0.225".parse().unwrap();
    let mut watch = LineWatch::default();
    assert_eq!(watch.read(None), Step::Nothing, "nothing was published, so there is nothing to withdraw");
    assert_eq!(watch.read(Some(mid)), Step::Publish(mid));
    // One slow answer is not a dead line.
    for _ in 1..STALE_AFTER {
        assert_eq!(watch.read(None), Step::Nothing);
    }
    assert_eq!(watch.read(None), Step::Withdraw, "a strategy must not trade on a price this old");
    assert_eq!(watch.read(None), Step::Nothing, "said once");
    assert_eq!(watch.read(Some(mid)), Step::Publish(mid));
    assert_eq!(watch.read(None), Step::Nothing, "the count starts again");
}

#[test]
fn the_line_is_withdrawn_when_its_game_is_no_longer_the_round() {
    let mid: Decimal = "0.4".parse().unwrap();
    let mut watch = LineWatch::default();
    assert_eq!(watch.end(), Step::Nothing);
    watch.read(Some(mid));
    assert_eq!(watch.end(), Step::Withdraw, "the last price was for another game");
    assert_eq!(watch.end(), Step::Nothing);
}

#[test]
fn a_standing_line_survives_thirty_seconds_of_failed_reads() {
    // Reading faster must not make the line quicker to vanish: each withdrawal also
    // clears the snapshot a running strategy trades on.
    assert_eq!(POLL * STALE_AFTER, Duration::from_secs(30));
}

#[test]
fn a_dead_price_never_stands_much_longer_than_a_minute() {
    // The worst case: every read hangs until its timeout before it counts as a miss.
    assert!((POLL + READ_TIMEOUT) * STALE_AFTER <= Duration::from_secs(70));
}
