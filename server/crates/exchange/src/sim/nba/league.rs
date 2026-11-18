//! League data: six real NBA teams with net ratings, and five notable
//! players each with a usage share, a three-point rate and a make-rate
//! adjustment. This table is the only roster: the registry mirrors it into
//! the play-by-play signal's metadata (player id → name, team) so a
//! consumer can resolve the ids the events carry.
//!
//! Player ids are NBA.com ids. Established players' ids are exact; the
//! 2024 and 2025 draftees' ids are best-effort and only need to be unique
//! here, since nothing joins them to an external source.

/// Three-letter team code, as the signals carry it.
pub type TeamCode = &'static str;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Team {
    pub code: TeamCode,
    pub city: &'static str,
    pub name: &'static str,
    /// Points per 100 possessions better than an average opponent on a
    /// neutral floor. The only way strength enters the game engine.
    pub net_rating: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Player {
    pub id: u32,
    pub name: &'static str,
    pub team: TeamCode,
    /// Share of the team's possessions this player finishes. Sums to 1
    /// per team.
    pub usage: f64,
    /// Share of this player's shot attempts that are threes.
    pub three_rate: f64,
    /// Added to the base make rate of every shot this player takes.
    pub make_adj: f64,
}

/// Points added to the home side's advantage.
pub const HOME_COURT: f64 = 1.5;

/// The six teams, in code order. The league is deliberately tight: with
/// [`HOME_COURT`] and the model's margin spread, every pregame line falls
/// between about 49 and 60 percent, so no game opens as a lock and the
/// score, not the matchup, is what moves the price.
pub const TEAMS: [Team; 6] = [
    Team { code: "BOS", city: "Boston", name: "Celtics", net_rating: 1.2 },
    Team { code: "LAL", city: "Los Angeles", name: "Lakers", net_rating: 0.9 },
    Team { code: "NYK", city: "New York", name: "Knicks", net_rating: 0.75 },
    Team { code: "PHI", city: "Philadelphia", name: "76ers", net_rating: 0.45 },
    Team { code: "SAS", city: "San Antonio", name: "Spurs", net_rating: 0.15 },
    Team { code: "TOR", city: "Toronto", name: "Raptors", net_rating: -0.45 },
];

/// Every rostered player, grouped by team in [`TEAMS`] order.
pub const ROSTERS: [Player; 30] = [
    // Boston Celtics
    Player { id: 1628369, name: "Jayson Tatum", team: "BOS", usage: 0.30, three_rate: 0.45, make_adj: 0.02 },
    Player { id: 1628401, name: "Derrick White", team: "BOS", usage: 0.20, three_rate: 0.55, make_adj: 0.01 },
    Player { id: 1630202, name: "Payton Pritchard", team: "BOS", usage: 0.18, three_rate: 0.60, make_adj: 0.01 },
    Player { id: 1629674, name: "Neemias Queta", team: "BOS", usage: 0.14, three_rate: 0.02, make_adj: 0.06 },
    Player { id: 1630573, name: "Sam Hauser", team: "BOS", usage: 0.18, three_rate: 0.80, make_adj: 0.02 },
    // Los Angeles Lakers
    Player { id: 1630559, name: "Austin Reaves", team: "LAL", usage: 0.22, three_rate: 0.45, make_adj: 0.01 },
    Player { id: 1629029, name: "Luka Dončić", team: "LAL", usage: 0.34, three_rate: 0.45, make_adj: 0.02 },
    Player { id: 1629028, name: "Deandre Ayton", team: "LAL", usage: 0.16, three_rate: 0.02, make_adj: 0.07 },
    Player { id: 203935, name: "Marcus Smart", team: "LAL", usage: 0.12, three_rate: 0.55, make_adj: -0.03 },
    Player { id: 1629060, name: "Rui Hachimura", team: "LAL", usage: 0.16, three_rate: 0.35, make_adj: 0.02 },
    // New York Knicks
    Player { id: 1628973, name: "Jalen Brunson", team: "NYK", usage: 0.30, three_rate: 0.35, make_adj: 0.02 },
    Player { id: 1628969, name: "Mikal Bridges", team: "NYK", usage: 0.18, three_rate: 0.45, make_adj: 0.01 },
    Player { id: 1628384, name: "OG Anunoby", team: "NYK", usage: 0.16, three_rate: 0.45, make_adj: 0.01 },
    Player { id: 1628404, name: "Josh Hart", team: "NYK", usage: 0.12, three_rate: 0.30, make_adj: 0.02 },
    Player { id: 1626157, name: "Karl-Anthony Towns", team: "NYK", usage: 0.24, three_rate: 0.35, make_adj: 0.03 },
    // Philadelphia 76ers
    Player { id: 203954, name: "Joel Embiid", team: "PHI", usage: 0.26, three_rate: 0.20, make_adj: 0.03 },
    Player { id: 1630178, name: "Tyrese Maxey", team: "PHI", usage: 0.24, three_rate: 0.45, make_adj: 0.01 },
    Player { id: 1642847, name: "VJ Edgecombe", team: "PHI", usage: 0.14, three_rate: 0.40, make_adj: -0.02 },
    Player { id: 1627759, name: "Jaylen Brown", team: "PHI", usage: 0.22, three_rate: 0.35, make_adj: 0.01 },
    Player { id: 2544, name: "LeBron James", team: "PHI", usage: 0.14, three_rate: 0.30, make_adj: 0.02 },
    // San Antonio Spurs
    Player { id: 1641705, name: "Victor Wembanyama", team: "SAS", usage: 0.30, three_rate: 0.35, make_adj: 0.02 },
    Player { id: 1628368, name: "De'Aaron Fox", team: "SAS", usage: 0.24, three_rate: 0.35, make_adj: 0.01 },
    Player { id: 1642264, name: "Stephon Castle", team: "SAS", usage: 0.18, three_rate: 0.35, make_adj: -0.01 },
    Player { id: 1642848, name: "Dylan Harper", team: "SAS", usage: 0.14, three_rate: 0.35, make_adj: -0.02 },
    Player { id: 1630170, name: "Devin Vassell", team: "SAS", usage: 0.14, three_rate: 0.55, make_adj: 0.01 },
    // Toronto Raptors
    Player { id: 1630567, name: "Scottie Barnes", team: "TOR", usage: 0.26, three_rate: 0.30, make_adj: 0.00 },
    Player { id: 1629628, name: "RJ Barrett", team: "TOR", usage: 0.22, three_rate: 0.35, make_adj: -0.01 },
    Player { id: 1630193, name: "Immanuel Quickley", team: "TOR", usage: 0.22, three_rate: 0.50, make_adj: 0.00 },
    Player { id: 1627751, name: "Jakob Poeltl", team: "TOR", usage: 0.14, three_rate: 0.00, make_adj: 0.07 },
    Player { id: 1642271, name: "Ja'Kobe Walter", team: "TOR", usage: 0.16, three_rate: 0.50, make_adj: -0.02 },
];

/// The team with `code`. Panics on an unknown code: codes only ever come
/// from this table.
pub fn team(code: &str) -> &'static Team {
    TEAMS.iter().find(|t| t.code == code).unwrap_or_else(|| panic!("unknown team `{code}`"))
}

/// The players rostered on `code`, table order.
pub fn players_of(code: &str) -> impl Iterator<Item = &'static Player> {
    ROSTERS.iter().filter(move |p| p.team == code)
}

/// The display name of the player with `id`, or empty for `0`: tipoff and
/// the final carry no player. Unknown ids cannot occur (events only ever
/// name rostered players) and read as empty rather than panicking on the
/// publishing path.
pub fn player_name(id: u32) -> &'static str {
    ROSTERS.iter().find(|p| p.id == id).map_or("", |p| p.name)
}

/// The team codes, table order.
pub fn team_codes() -> [TeamCode; 6] {
    let mut out = [""; 6];
    for (i, t) in TEAMS.iter().enumerate() {
        out[i] = t.code;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn six_teams_five_players_each_usage_sums_to_one() {
        assert_eq!(TEAMS.len(), 6);
        for t in TEAMS {
            let players: Vec<_> = players_of(t.code).collect();
            assert_eq!(players.len(), 5, "{}", t.code);
            let usage: f64 = players.iter().map(|p| p.usage).sum();
            assert!((usage - 1.0).abs() < 1e-9, "{}: usage sums to {usage}", t.code);
            for p in players {
                assert!((0.0..=1.0).contains(&p.three_rate));
                assert!(p.make_adj.abs() < 0.15);
            }
        }
    }

    #[test]
    fn ids_and_codes_are_unique_and_every_player_has_a_team() {
        let ids: HashSet<u32> = ROSTERS.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), ROSTERS.len());
        let codes: HashSet<&str> = TEAMS.iter().map(|t| t.code).collect();
        assert_eq!(codes.len(), TEAMS.len());
        for p in ROSTERS {
            assert!(codes.contains(p.team), "{} on unknown team {}", p.name, p.team);
        }
        assert_eq!(team("BOS").name, "Celtics");
        assert_eq!(team_codes()[0], "BOS");
        assert_eq!(player_name(1628369), "Jayson Tatum");
        assert_eq!(player_name(0), "");
    }

    /// Every pregame line sits between 49 and 56 percent: the strongest
    /// side at home against the weakest is a mild favourite, the reverse
    /// is not quite even.
    #[test]
    fn every_pregame_line_is_between_49_and_56_percent() {
        use crate::sim::nba::winprob::{REGULATION_SECONDS, expected_margin, win_probability};
        let mut lo = 1.0f64;
        let mut hi = 0.0f64;
        for h in TEAMS {
            for a in TEAMS {
                if h.code == a.code {
                    continue;
                }
                let p = win_probability(0.0, REGULATION_SECONDS, expected_margin(h.net_rating, a.net_rating, HOME_COURT));
                lo = lo.min(p);
                hi = hi.max(p);
            }
        }
        assert!(lo > 0.49 && lo < 0.51, "weakest home line {lo}");
        assert!(hi > 0.53 && hi < 0.56, "strongest home line {hi}");
    }
}
