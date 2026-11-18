//! Applying the `SIGNALS_ENABLED_VENUES` allow-list at lane startup (ADR 0026).
//!
//! [`signals_core::venue_gate`] owns the vocabulary and the decision; this
//! module is the thin binding to a process: it reads the environment, prints
//! the one-line reason on refusal, and hands back the `EX_CONFIG` exit class.
//!
//! Only a live lane is gated. Replaying a journal reads a file that was already
//! captured and publishes nothing to a venue, so it is not a venue subscription
//! and the allow-list does not apply to it.

use signals_core::{ArmedIndexSockets, EnabledVenues, LaneRequirement, LaneStart, lane_start};

use crate::exit_code::ExitClass;

/// Whether this build knows how to open a lane's index channels.
///
/// Ruling E2: a missing index token **never refuses a lane**. It disables a
/// subscription, not a venue, and the market channels the lane exists for keep
/// running — taking an adopted, working lane down to gate a subscription it does
/// not need is exactly the outcome E2 forbids. So this type does not decide
/// admission at all; it only says whether "index channels off" is worth a
/// startup line, which it is for a lane that could have opened them
/// ([`IndexSocket::Configured`]) and is noise for one that could not
/// ([`IndexSocket::NotConfigured`]).
///
/// The Kalshi lane passes [`IndexSocket::Configured`]: when a token is listed it
/// opens a dedicated index socket — a second connection alongside the market
/// socket (ruling E37d) — for the index channels, and opens none when no token
/// is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexSocket {
    Configured,
    NotConfigured,
}

/// The outcome of gating one live lane.
///
/// `#[must_use]`: the index-socket decision is half of ruling E2, and dropping
/// it silently is exactly how that half stops being enforced.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneAdmission {
    lane: &'static str,
    index_sockets: ArmedIndexSockets,
}

impl LaneAdmission {
    /// True when this lane should open its index socket at all. False when the
    /// lane has no index socket, or has one that the allow-list did not arm.
    #[must_use]
    pub fn index_socket_enabled(self) -> bool {
        self.index_sockets.any()
    }

    /// Which index-channel families the allow-list armed. Each family is
    /// subscribed only under its own token even though both ride one socket
    /// (audit A1/F6).
    #[must_use]
    pub fn index_sockets(self) -> ArmedIndexSockets {
        self.index_sockets
    }

    /// The startup record naming what the allow-list decided. Emitting it is
    /// what makes the decision observable in a lane's logs.
    #[must_use]
    pub fn startup_line(self) -> String {
        let armed: Vec<&str> = self.index_sockets.tokens().collect();
        format!(
            "lane {}: admitted, index socket {}",
            self.lane,
            if armed.is_empty() {
                "off".to_owned()
            } else {
                format!("armed ({})", armed.join(", "))
            }
        )
    }
}

/// Apply the allow-list to `requirement`.
///
/// A lane refuses — reason on stderr, [`ExitClass::Fatal`], exit 78,
/// `EX_CONFIG` — for exactly two reasons: its own venue token is not
/// allow-listed, or the variable itself is unusable (an unknown token, a
/// non-UTF-8 value). A configuration decision like that must not be papered over
/// by a blind restart.
///
/// A missing **index** token is never one of those reasons (ruling E2): the
/// index channels are simply not subscribed and the lane's market channels run
/// as they always did. `socket` only decides whether that is worth a startup
/// line.
pub fn admit_lane(
    requirement: &LaneRequirement,
    socket: IndexSocket,
) -> Result<LaneAdmission, ExitClass> {
    let enabled = match EnabledVenues::from_env() {
        Ok(enabled) => enabled,
        Err(error) => {
            eprintln!("Refusing to start: {error}");
            return Err(ExitClass::Fatal);
        }
    };
    admit_lane_with(requirement, socket, &enabled)
}

/// [`admit_lane`] against an already-parsed allow-list.
///
/// Split out so the decision is testable without mutating the process
/// environment, which this crate cannot do: `unsafe_code` is forbidden and
/// `set_var` is unsafe.
pub fn admit_lane_with(
    requirement: &LaneRequirement,
    socket: IndexSocket,
    enabled: &EnabledVenues,
) -> Result<LaneAdmission, ExitClass> {
    match lane_start(requirement, enabled) {
        LaneStart::Start { index_sockets } => {
            if socket == IndexSocket::Configured && !index_sockets.any() {
                eprintln!(
                    "{}: {} lists no index venue ({}), so the index channels are not subscribed and index-dependent instances publish unavailable; the market channels run",
                    requirement.lane,
                    signals_core::VENUE_GATE_ENV,
                    requirement.index_sockets.join(", "),
                );
            }
            Ok(LaneAdmission {
                lane: requirement.lane,
                index_sockets,
            })
        }
        LaneStart::Refuse { reason } => {
            eprintln!("Refusing to start: {reason}");
            Err(ExitClass::Fatal)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signals_core::{KALSHI_LANE, POLYMARKET_LANE, SPOT_LANE};

    fn enabled(raw: &str) -> EnabledVenues {
        EnabledVenues::parse(Some(raw)).expect("the allow-list parses")
    }

    #[test]
    fn an_admitted_lane_reports_whether_its_index_socket_is_armed() {
        let list = enabled("coinbase,kraken,kalshi,polymarket");

        let spot =
            admit_lane_with(&SPOT_LANE, IndexSocket::NotConfigured, &list).expect("spot admitted");
        assert!(!spot.index_socket_enabled(), "the spot lane has no socket");
        assert!(spot.startup_line().contains("index socket off"));

        let polymarket = admit_lane_with(&POLYMARKET_LANE, IndexSocket::NotConfigured, &list)
            .expect("polymarket admitted");
        assert!(!polymarket.index_socket_enabled());

        // Kalshi with no index token: market channels run, socket off (E2).
        let kalshi = admit_lane_with(&KALSHI_LANE, IndexSocket::NotConfigured, &list)
            .expect("kalshi admitted");
        assert!(!kalshi.index_socket_enabled());
        assert!(kalshi.startup_line().contains("index socket off"));

        // Arm it and the flag flips.
        let armed = admit_lane_with(
            &KALSHI_LANE,
            IndexSocket::NotConfigured,
            &enabled("kalshi,cfbrti"),
        )
        .expect("kalshi admitted");
        assert!(armed.index_socket_enabled());
        assert!(armed.startup_line().contains("index socket armed"));
    }

    /// Ruling N1/E2: a missing index token disables a subscription, never a
    /// lane. The Kalshi lane passes `IndexSocket::Configured` in production, so
    /// this is the deployed configuration — and it must start.
    #[test]
    fn the_kalshi_lane_starts_with_the_unset_default_and_skips_the_index_channels() {
        let default = EnabledVenues::parse(None).expect("unset parses");
        assert!(
            !default.is_enabled("cfbrti") && !default.is_enabled("pyth"),
            "the unset default lists neither index token"
        );
        for list in [
            default,
            enabled("coinbase,kraken,polymarket,kalshi"),
            enabled("kalshi"),
        ] {
            let admission = admit_lane_with(&KALSHI_LANE, IndexSocket::Configured, &list)
                .expect("the Kalshi lane starts without an index token");
            assert!(
                !admission.index_socket_enabled(),
                "the index channels are skipped, not refused"
            );
            assert!(admission.startup_line().contains("index socket off"));
        }
    }

    #[test]
    fn e2_index_tokens_arm_a_configured_socket_one_family_at_a_time() {
        // With a token listed, a configured socket starts and is armed.
        for token in ["cfbrti", "pyth"] {
            let armed = admit_lane_with(
                &KALSHI_LANE,
                IndexSocket::Configured,
                &enabled(&format!("kalshi,{token}")),
            )
            .expect("kalshi admitted");
            assert!(armed.index_socket_enabled(), "{token}");
            assert!(armed.index_sockets().is_armed(token), "{token} arms itself");
            for other in ["cfbrti", "pyth"]
                .into_iter()
                .filter(|other| *other != token)
            {
                assert!(
                    !armed.index_sockets().is_armed(other),
                    "{token} must not arm {other}"
                );
            }
            assert!(armed.startup_line().contains(token), "{token}");
        }
    }

    #[test]
    fn an_unlisted_venue_refuses_whatever_the_socket_state() {
        for socket in [IndexSocket::Configured, IndexSocket::NotConfigured] {
            assert_eq!(
                admit_lane_with(&SPOT_LANE, socket, &enabled("coinbase")),
                Err(ExitClass::Fatal),
                "the spot lane needs both spot venues"
            );
            assert_eq!(
                admit_lane_with(&KALSHI_LANE, socket, &enabled("cfbrti,pyth")),
                Err(ExitClass::Fatal),
                "index tokens never substitute for the lane's own token"
            );
        }
    }

    #[test]
    fn the_unset_default_admits_todays_three_lanes() {
        let default = EnabledVenues::parse(None).expect("unset parses");
        for requirement in [&SPOT_LANE, &POLYMARKET_LANE, &KALSHI_LANE] {
            let admission = admit_lane_with(requirement, IndexSocket::NotConfigured, &default)
                .expect("admitted with no env change");
            assert!(
                !admission.index_socket_enabled(),
                "no index venue is enabled by default"
            );
        }
    }
}
