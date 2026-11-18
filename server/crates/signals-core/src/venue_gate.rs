//! The runtime venue allow-list (ADR 0026, ruling R7).
//!
//! Definitions for a rights-gated venue live in the public tree and are built
//! and tested to the same standard as every other definition. What keeps an
//! unadopted venue off the wire is not a missing file but this gate: a lane
//! whose venue token is absent from `SIGNALS_ENABLED_VENUES` refuses to start.
//!
//! The default — the variable unset — is the four adopted venues, so today's
//! deployment keeps working with no environment change and every venue whose
//! rights row is still open stays refused until an operator names it.
//!
//! This module owns only the vocabulary and the decision. Turning a refusal
//! into an exit status belongs to each lane binary, which maps it onto
//! `EXIT_FATAL` (78, `EX_CONFIG`).

use std::collections::BTreeSet;
use std::fmt;

/// The environment variable carrying the allow-list.
pub const VENUE_GATE_ENV: &str = "SIGNALS_ENABLED_VENUES";

/// Every token the allow-list accepts, byte-sorted.
///
/// `coinbase`, `kraken`, `polymarket` and `kalshi` are the adopted venues.
/// `binance` and `bybit` are rights-gated venues (R7). `oracle` is the lane
/// that publishes the on-chain and relayed oracle identities. `cfbrti` and
/// `pyth` are the index sockets rather than trading venues: they gate a
/// subscription, not a market.
pub const KNOWN_VENUE_TOKENS: [&str; 9] = [
    "binance",
    "bybit",
    "cfbrti",
    "coinbase",
    "kalshi",
    "kraken",
    "oracle",
    "polymarket",
    "pyth",
];

/// The allow-list applied when `SIGNALS_ENABLED_VENUES` is unset: the four
/// adopted venues, and nothing else.
pub const DEFAULT_ENABLED_VENUES: [&str; 4] = ["coinbase", "kalshi", "kraken", "polymarket"];

/// The index-socket tokens. Absent, they disable a subscription and **never
/// refuse a lane** (Round 2 ruling E2): taking an adopted, working lane down to
/// gate a subscription it does not need is the outcome E2 exists to forbid.
///
/// They gate the two channel families independently: `cfbrti` arms the CF
/// Benchmarks channels, `pyth` arms the Pyth relay. The two ride one socket, so
/// arming either opens it — but each family is subscribed only under its own
/// token (audit A1/F6).
pub const INDEX_SOCKET_TOKENS: [&str; 2] = ["cfbrti", "pyth"];

/// Which index-socket channel families the allow-list armed.
///
/// One bit per [`INDEX_SOCKET_TOKENS`] entry, so a lane can subscribe the CF
/// Benchmarks channels without the Pyth relay and the other way round. `Copy`
/// so a lane admission stays a value type.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArmedIndexSockets {
    armed: [bool; INDEX_SOCKET_TOKENS.len()],
}

impl ArmedIndexSockets {
    /// Nothing armed: the lane has no index socket, or the allow-list named
    /// neither token.
    pub const NONE: Self = Self {
        armed: [false; INDEX_SOCKET_TOKENS.len()],
    };

    /// True when `token` — one of [`INDEX_SOCKET_TOKENS`] — is armed. An
    /// unknown token is never armed rather than silently true.
    #[must_use]
    pub fn is_armed(self, token: &str) -> bool {
        INDEX_SOCKET_TOKENS
            .iter()
            .position(|known| *known == token)
            .is_some_and(|position| self.armed[position])
    }

    /// True when at least one family is armed, which is what decides whether
    /// the socket is opened at all.
    #[must_use]
    pub fn any(self) -> bool {
        self.armed.iter().any(|armed| *armed)
    }

    /// This set with `token` armed. An unknown token changes nothing.
    #[must_use]
    pub fn with(mut self, token: &str) -> Self {
        if let Some(position) = INDEX_SOCKET_TOKENS.iter().position(|known| *known == token) {
            self.armed[position] = true;
        }
        self
    }

    /// The armed tokens, in [`INDEX_SOCKET_TOKENS`] order.
    pub fn tokens(self) -> impl Iterator<Item = &'static str> {
        INDEX_SOCKET_TOKENS
            .into_iter()
            .enumerate()
            .filter_map(move |(position, token)| self.armed[position].then_some(token))
    }
}

/// A malformed allow-list. The gate fails closed: an unparseable value refuses
/// the lane rather than falling back to the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VenueGateError {
    /// The variable was set to a token this build does not know.
    UnknownToken(String),
}

impl fmt::Display for VenueGateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownToken(token) => write!(
                formatter,
                "{VENUE_GATE_ENV} names unknown venue token {token:?}; known tokens are {}",
                KNOWN_VENUE_TOKENS.join(", ")
            ),
        }
    }
}

impl std::error::Error for VenueGateError {}

/// The parsed allow-list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnabledVenues {
    tokens: BTreeSet<String>,
    /// True when the set came from the compiled default rather than the
    /// environment, so a refusal message can say which it was.
    defaulted: bool,
}

impl EnabledVenues {
    /// Parse the raw environment value. `None` — the variable unset — yields
    /// the adopted-venue default. An empty or whitespace-only value is a
    /// deliberate "enable nothing", not an unset variable, and yields an empty
    /// set that refuses every lane.
    pub fn parse(raw: Option<&str>) -> Result<Self, VenueGateError> {
        let Some(raw) = raw else {
            return Ok(Self {
                tokens: DEFAULT_ENABLED_VENUES
                    .iter()
                    .map(|token| (*token).to_owned())
                    .collect(),
                defaulted: true,
            });
        };
        let mut tokens = BTreeSet::new();
        for token in raw.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if !KNOWN_VENUE_TOKENS.contains(&token) {
                return Err(VenueGateError::UnknownToken(token.to_owned()));
            }
            tokens.insert(token.to_owned());
        }
        Ok(Self {
            tokens,
            defaulted: false,
        })
    }

    /// Read the allow-list from the process environment.
    pub fn from_env() -> Result<Self, VenueGateError> {
        match std::env::var(VENUE_GATE_ENV) {
            Ok(raw) => Self::parse(Some(&raw)),
            Err(std::env::VarError::NotPresent) => Self::parse(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(VenueGateError::UnknownToken("<non-utf8 value>".to_owned()))
            }
        }
    }

    pub fn is_enabled(&self, token: &str) -> bool {
        self.tokens.contains(token)
    }

    /// True when the set is the compiled default rather than an operator's.
    pub fn is_default(&self) -> bool {
        self.defaulted
    }

    /// The enabled tokens, byte-sorted.
    pub fn tokens(&self) -> impl Iterator<Item = &str> {
        self.tokens.iter().map(String::as_str)
    }
}

/// What one lane needs from the allow-list before it may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneRequirement {
    /// The lane's name, as the refusal message reports it.
    pub lane: &'static str,
    /// Tokens that must all be present, or the lane refuses to start.
    pub required: &'static [&'static str],
    /// Index-socket tokens that gate a subscription rather than the lane. Any
    /// that are absent leave the lane running with its index socket off.
    pub index_sockets: &'static [&'static str],
}

/// The spot lane publishes Coinbase and Kraken together and computes the
/// blended `btc.external.*` reference from both, so it needs both tokens.
pub const SPOT_LANE: LaneRequirement = LaneRequirement {
    lane: "spot",
    required: &["coinbase", "kraken"],
    index_sockets: &[],
};

pub const POLYMARKET_LANE: LaneRequirement = LaneRequirement {
    lane: "polymarket",
    required: &["polymarket"],
    index_sockets: &[],
};

/// The Kalshi lane's market channels do not depend on the index channels, so a
/// missing `cfbrti`/`pyth` token skips that channel family and leaves the market
/// channels running (E2). It is never a reason to refuse the lane, whether or
/// not this build knows how to open the channels.
pub const KALSHI_LANE: LaneRequirement = LaneRequirement {
    lane: "kalshi",
    required: &["kalshi"],
    index_sockets: &INDEX_SOCKET_TOKENS,
};

/// The Binance venue lane (spot + USD-M perp on one publisher, R5). Binance is
/// rights-gated (R7): its token must be allow-listed or the lane refuses with
/// exit 78 until the RIGHTS-LEDGER row is cleared.
pub const BINANCE_LANE: LaneRequirement = LaneRequirement {
    lane: "binance",
    required: &["binance"],
    index_sockets: &[],
};

/// The Bybit venue lane (spot + linear perp on one publisher, R5). Bybit is
/// rights-gated (R7) exactly as Binance is. This constant is the seam the
/// generic venue lane (group A6) consumes; group A4 applies it on the capture
/// path, and a lane binary maps a refusal onto `EXIT_FATAL` (78).
pub const BYBIT_LANE: LaneRequirement = LaneRequirement {
    lane: "bybit",
    required: &["bybit"],
    index_sockets: &[],
};

/// The oracle lane (Chainlink on-chain now; Pyth Hermes when keyed). Unlike the
/// rights-gated venues it has **no required token**: it starts with the
/// allow-list unset (R7 does not apply — the on-chain data is public), so its
/// `required` set is empty and `lane_start` always admits it. The `oracle`
/// token still exists in [`KNOWN_VENUE_TOKENS`] so an operator can name it, but
/// naming it is never a precondition for the lane to run.
///
/// The composite oracle lane DOES open the Kalshi index socket (the `cfbrti`
/// CF Benchmarks family) and the Pyth relay (the `pyth` family), so it declares
/// both index-socket tokens: naming one arms that family exactly as it does on
/// the Kalshi lane, and — like there (E2) — a missing index token only skips a
/// subscription, never refuses the lane.
pub const ORACLE_LANE: LaneRequirement = LaneRequirement {
    lane: "oracle",
    required: &[],
    index_sockets: &INDEX_SOCKET_TOKENS,
};

/// The cross lane (C1, ruling E52): it embeds the two-venue USD spot reference
/// (Coinbase + Kraken, public) and the Bybit linear derivatives processor, and
/// publishes the three `btc.cross.*` identities.
///
/// A lane's `required` set names every venue whose socket the lane physically
/// dials: `run_lane` opens every declared stream unconditionally, so a token that
/// names an opened socket but is absent from `SIGNALS_ENABLED_VENUES` must refuse
/// the lane (exit 78) rather than let the gate stop being the authority over which
/// wires open. The cross lane dials Coinbase, Kraken and Bybit core streams
/// (`lane::CROSS`), so all three are required — exactly as `--lane spot`, which
/// opens the same Coinbase and Kraken sockets, refuses on a missing `coinbase` or
/// `kraken`. Bybit is additionally the rights-gated venue on this wire (R7); the
/// refusal path is identical either way. The Binance leg carries NO streams here
/// (E30/E34): its inputs are non-ready and the cross identities degrade rather
/// than refuse, so `binance` is deliberately NOT a required token.
pub const CROSS_LANE: LaneRequirement = LaneRequirement {
    lane: "cross",
    required: &["bybit", "coinbase", "kraken"],
    index_sockets: &[],
};

/// The outcome of applying the allow-list to one lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneStart {
    /// The lane starts. `index_sockets` names the index-channel families the
    /// allow-list armed, and is empty when the lane has no index socket or when
    /// neither token was listed.
    Start { index_sockets: ArmedIndexSockets },
    /// The lane refuses to start. The string is the one-line reason.
    Refuse { reason: String },
}

/// Decide whether `requirement`'s lane may start under `enabled`.
pub fn lane_start(requirement: &LaneRequirement, enabled: &EnabledVenues) -> LaneStart {
    let missing: Vec<&str> = requirement
        .required
        .iter()
        .copied()
        .filter(|token| !enabled.is_enabled(token))
        .collect();
    if !missing.is_empty() {
        let source = if enabled.is_default() {
            format!("{VENUE_GATE_ENV} is unset, so only the adopted venues are enabled")
        } else {
            format!("{VENUE_GATE_ENV} enables {}", joined(enabled.tokens()))
        };
        return LaneStart::Refuse {
            reason: format!(
                "the {} lane requires venue {} {} but {source}; clear the venue's RIGHTS-LEDGER row before listing it",
                requirement.lane,
                if missing.len() == 1 {
                    "token"
                } else {
                    "tokens"
                },
                joined(missing.into_iter()),
            ),
        };
    }
    let mut index_sockets = ArmedIndexSockets::NONE;
    for token in requirement.index_sockets {
        if enabled.is_enabled(token) {
            index_sockets = index_sockets.with(token);
        }
    }
    LaneStart::Start { index_sockets }
}

fn joined<'a>(tokens: impl Iterator<Item = &'a str>) -> String {
    let tokens: Vec<&str> = tokens.collect();
    if tokens.is_empty() {
        "nothing".to_owned()
    } else {
        tokens.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_lane_starts_without_any_allow_list_token() {
        // R7 does not apply to the oracle lane: it requires no token, so it
        // starts with the allow-list unset, with an unrelated list, and even
        // when `oracle` is present it is never required (E2). Its index sockets
        // stay off until an index token names them.
        for enabled in [
            EnabledVenues::parse(None).expect("unset parses"),
            EnabledVenues::parse(Some("coinbase,kraken")).expect("parses"),
            EnabledVenues::parse(Some("oracle")).expect("parses"),
        ] {
            assert_eq!(
                lane_start(&ORACLE_LANE, &enabled),
                LaneStart::Start {
                    index_sockets: ArmedIndexSockets::NONE
                },
                "the oracle lane starts unconditionally with its index sockets off"
            );
        }
        // Naming the index tokens arms the composite lane's index sockets — it
        // still starts (no required token), now with the families armed (E2).
        let full = EnabledVenues::parse(Some(&KNOWN_VENUE_TOKENS.join(","))).expect("parses");
        let LaneStart::Start { index_sockets } = lane_start(&ORACLE_LANE, &full) else {
            panic!("the oracle lane starts unconditionally");
        };
        assert_eq!(
            index_sockets.tokens().collect::<Vec<_>>(),
            INDEX_SOCKET_TOKENS.to_vec(),
            "a full allow-list arms both index families on the oracle lane"
        );
    }

    #[test]
    fn known_tokens_are_sorted_unique_and_cover_the_default() {
        let mut sorted = KNOWN_VENUE_TOKENS;
        sorted.sort_unstable();
        assert_eq!(sorted, KNOWN_VENUE_TOKENS, "known tokens are byte-sorted");
        assert_eq!(
            KNOWN_VENUE_TOKENS.iter().collect::<BTreeSet<_>>().len(),
            KNOWN_VENUE_TOKENS.len(),
            "known tokens are unique"
        );
        for token in DEFAULT_ENABLED_VENUES {
            assert!(KNOWN_VENUE_TOKENS.contains(&token), "{token} is known");
        }
        for token in INDEX_SOCKET_TOKENS {
            assert!(KNOWN_VENUE_TOKENS.contains(&token), "{token} is known");
        }
    }

    #[test]
    fn unset_enables_exactly_the_four_adopted_venues() {
        let enabled = EnabledVenues::parse(None).expect("unset parses");
        assert!(enabled.is_default());
        assert_eq!(
            enabled.tokens().collect::<Vec<_>>(),
            vec!["coinbase", "kalshi", "kraken", "polymarket"]
        );
        for token in ["binance", "bybit", "oracle", "cfbrti", "pyth"] {
            assert!(
                !enabled.is_enabled(token),
                "{token} stays refused by default"
            );
        }
    }

    #[test]
    fn todays_deployment_starts_unchanged_with_no_env_var() {
        let enabled = EnabledVenues::parse(None).expect("unset parses");
        assert_eq!(
            lane_start(&SPOT_LANE, &enabled),
            LaneStart::Start {
                index_sockets: ArmedIndexSockets::NONE
            }
        );
        assert_eq!(
            lane_start(&POLYMARKET_LANE, &enabled),
            LaneStart::Start {
                index_sockets: ArmedIndexSockets::NONE
            }
        );
        // Kalshi market channels run; the index socket is off because neither
        // index token is in the default set.
        assert_eq!(
            lane_start(&KALSHI_LANE, &enabled),
            LaneStart::Start {
                index_sockets: ArmedIndexSockets::NONE
            }
        );
    }

    #[test]
    fn parsing_trims_tolerates_blanks_and_rejects_unknown_tokens() {
        let enabled = EnabledVenues::parse(Some(" coinbase , kraken ,, binance ")).expect("parses");
        assert!(!enabled.is_default());
        assert_eq!(
            enabled.tokens().collect::<Vec<_>>(),
            vec!["binance", "coinbase", "kraken"]
        );
        assert_eq!(
            EnabledVenues::parse(Some("coinbase,ftx")),
            Err(VenueGateError::UnknownToken("ftx".to_owned()))
        );
        // Case is significant: the tokens are lowercase, tree convention.
        assert_eq!(
            EnabledVenues::parse(Some("Coinbase")),
            Err(VenueGateError::UnknownToken("Coinbase".to_owned()))
        );
    }

    #[test]
    fn an_empty_value_is_enable_nothing_not_unset() {
        let enabled = EnabledVenues::parse(Some("")).expect("empty parses");
        assert!(!enabled.is_default());
        assert_eq!(enabled.tokens().count(), 0);
        assert!(matches!(
            lane_start(&SPOT_LANE, &enabled),
            LaneStart::Refuse { .. }
        ));
    }

    #[test]
    fn a_lane_refuses_when_any_required_token_is_absent() {
        let coinbase_only = EnabledVenues::parse(Some("coinbase")).expect("parses");
        let LaneStart::Refuse { reason } = lane_start(&SPOT_LANE, &coinbase_only) else {
            panic!("the spot lane needs both spot venues");
        };
        assert!(reason.contains("kraken"), "{reason}");
        assert!(reason.contains("RIGHTS-LEDGER"), "{reason}");
        assert!(!reason.contains('\n'), "the reason is one line: {reason}");

        let enabled = EnabledVenues::parse(Some("coinbase,kraken")).expect("parses");
        assert!(matches!(
            lane_start(&POLYMARKET_LANE, &enabled),
            LaneStart::Refuse { .. }
        ));
        assert!(matches!(
            lane_start(&KALSHI_LANE, &enabled),
            LaneStart::Refuse { .. }
        ));
    }

    #[test]
    fn a_rights_gated_venue_permits_its_lane_once_listed() {
        let enabled = EnabledVenues::parse(Some(
            "binance,bybit,coinbase,kalshi,kraken,polymarket,oracle",
        ))
        .expect("parses");
        for token in ["binance", "bybit", "oracle"] {
            assert!(enabled.is_enabled(token));
        }
        assert_eq!(
            lane_start(&SPOT_LANE, &enabled),
            LaneStart::Start {
                index_sockets: ArmedIndexSockets::NONE
            }
        );
    }

    #[test]
    fn e2_a_missing_index_token_only_disables_the_kalshi_index_socket() {
        let without = EnabledVenues::parse(Some("kalshi")).expect("parses");
        assert_eq!(
            lane_start(&KALSHI_LANE, &without),
            LaneStart::Start {
                index_sockets: ArmedIndexSockets::NONE
            },
            "market channels keep running without an index token"
        );
        // F6: each family is armed by its own token, never by its sibling's.
        for token in INDEX_SOCKET_TOKENS {
            let with = EnabledVenues::parse(Some(&format!("kalshi,{token}"))).expect("parses");
            let LaneStart::Start { index_sockets } = lane_start(&KALSHI_LANE, &with) else {
                panic!("the kalshi lane starts with {token}");
            };
            assert!(index_sockets.any(), "{token} opens the index socket");
            assert!(index_sockets.is_armed(token), "{token} arms its own family");
            for other in INDEX_SOCKET_TOKENS.iter().filter(|other| **other != token) {
                assert!(
                    !index_sockets.is_armed(other),
                    "{token} must not arm {other}"
                );
            }
            assert_eq!(index_sockets.tokens().collect::<Vec<_>>(), vec![token]);
        }
        let both = EnabledVenues::parse(Some("kalshi,cfbrti,pyth")).expect("parses");
        let LaneStart::Start { index_sockets } = lane_start(&KALSHI_LANE, &both) else {
            panic!("the kalshi lane starts");
        };
        assert_eq!(
            index_sockets.tokens().collect::<Vec<_>>(),
            INDEX_SOCKET_TOKENS.to_vec()
        );
        assert!(
            !index_sockets.is_armed("kalshi"),
            "an unknown token is never armed"
        );
        // The index tokens alone never substitute for the lane's own token.
        let index_only = EnabledVenues::parse(Some("cfbrti,pyth")).expect("parses");
        assert!(matches!(
            lane_start(&KALSHI_LANE, &index_only),
            LaneStart::Refuse { .. }
        ));
    }

    #[test]
    fn lane_refuses_with_exit_78_when_signals_enabled_venues_omits_bybit() {
        // A lane binary maps a `Refuse` onto EXIT_FATAL (78, EX_CONFIG); this
        // pins the decision the mapping rests on. Unset defaults to the adopted
        // venues, which do not include bybit.
        let unset = EnabledVenues::parse(None).expect("unset parses");
        let LaneStart::Refuse { reason } = lane_start(&BYBIT_LANE, &unset) else {
            panic!("the bybit lane must refuse when the allow-list omits bybit");
        };
        assert!(reason.contains("bybit"), "{reason}");
        assert!(reason.contains("RIGHTS-LEDGER"), "{reason}");
        assert!(!reason.contains('\n'), "the reason is one line: {reason}");
        // Listing other venues but not bybit still refuses.
        let others = EnabledVenues::parse(Some("coinbase,kraken,binance")).expect("parses");
        assert!(matches!(
            lane_start(&BYBIT_LANE, &others),
            LaneStart::Refuse { .. }
        ));
    }

    #[test]
    fn lane_starts_when_signals_enabled_venues_lists_bybit() {
        let enabled = EnabledVenues::parse(Some("bybit")).expect("parses");
        assert_eq!(
            lane_start(&BYBIT_LANE, &enabled),
            LaneStart::Start {
                index_sockets: ArmedIndexSockets::NONE
            }
        );
    }

    #[test]
    fn cross_lane_requires_every_venue_it_dials() {
        // R7 / audit C1b: the cross lane opens Coinbase, Kraken AND Bybit sockets,
        // and `run_lane` opens every declared stream unconditionally, so all three
        // tokens are required — a partial allow-list refuses exactly like `--lane
        // spot` does on a missing coinbase/kraken. `bybit` alone is NOT enough.
        let bybit_only = EnabledVenues::parse(Some("bybit")).expect("parses");
        let LaneStart::Refuse { reason } = lane_start(&CROSS_LANE, &bybit_only) else {
            panic!("the cross lane needs coinbase and kraken as well as bybit");
        };
        assert!(reason.contains("coinbase"), "{reason}");
        assert!(reason.contains("kraken"), "{reason}");
        assert!(reason.contains("RIGHTS-LEDGER"), "{reason}");
        assert!(!reason.contains('\n'), "the reason is one line: {reason}");

        // Missing only bybit still refuses (the rights-gated venue on the wire).
        let spot_only = EnabledVenues::parse(Some("coinbase,kraken")).expect("parses");
        let LaneStart::Refuse { reason } = lane_start(&CROSS_LANE, &spot_only) else {
            panic!("the cross lane needs bybit");
        };
        assert!(reason.contains("bybit"), "{reason}");

        // The default (unset) omits all three: the cross lane refuses.
        let default = EnabledVenues::parse(None).expect("unset parses");
        assert!(matches!(
            lane_start(&CROSS_LANE, &default),
            LaneStart::Refuse { .. }
        ));

        // The full list admits, with no index socket.
        let full = EnabledVenues::parse(Some("bybit,coinbase,kraken")).expect("parses");
        assert_eq!(
            lane_start(&CROSS_LANE, &full),
            LaneStart::Start {
                index_sockets: ArmedIndexSockets::NONE
            },
            "the cross lane admits once every venue it dials is listed"
        );
    }

    #[test]
    fn a_lane_without_an_index_socket_never_reports_one() {
        let everything = EnabledVenues::parse(Some(&KNOWN_VENUE_TOKENS.join(","))).expect("parses");
        assert_eq!(
            lane_start(&SPOT_LANE, &everything),
            LaneStart::Start {
                index_sockets: ArmedIndexSockets::NONE
            }
        );
        assert_eq!(
            lane_start(&POLYMARKET_LANE, &everything),
            LaneStart::Start {
                index_sockets: ArmedIndexSockets::NONE
            }
        );
        let LaneStart::Start { index_sockets } = lane_start(&KALSHI_LANE, &everything) else {
            panic!("the kalshi lane starts");
        };
        assert_eq!(
            index_sockets.tokens().collect::<Vec<_>>(),
            INDEX_SOCKET_TOKENS.to_vec()
        );
    }

    #[test]
    fn rights_gated_binance_and_bybit_refuse_by_default_and_admit_once_listed() {
        let default = EnabledVenues::parse(None).expect("unset parses");
        for requirement in [&BINANCE_LANE, &BYBIT_LANE] {
            assert!(
                matches!(lane_start(requirement, &default), LaneStart::Refuse { .. }),
                "{} refuses until its rights row is cleared",
                requirement.lane
            );
        }
        let listed = EnabledVenues::parse(Some("coinbase,kraken,binance,bybit")).expect("parses");
        for requirement in [&BINANCE_LANE, &BYBIT_LANE] {
            assert_eq!(
                lane_start(requirement, &listed),
                LaneStart::Start {
                    index_sockets: ArmedIndexSockets::NONE
                },
                "{} admits once listed",
                requirement.lane
            );
        }
    }

    #[test]
    fn the_oracle_lane_starts_with_the_allow_list_unset() {
        // R7 does not apply to the oracle lane: the on-chain data is public, so
        // it has no required token and starts even with SIGNALS_ENABLED_VENUES
        // unset — and every other allow-list, including one that names nothing.
        for raw in [None, Some(""), Some("coinbase")] {
            let enabled = EnabledVenues::parse(raw).expect("allow-list parses");
            assert_eq!(
                lane_start(&ORACLE_LANE, &enabled),
                LaneStart::Start {
                    index_sockets: ArmedIndexSockets::NONE
                },
                "the oracle lane starts regardless of the allow-list"
            );
        }
    }
}
