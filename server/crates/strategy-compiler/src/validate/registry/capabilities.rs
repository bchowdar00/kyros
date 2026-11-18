//! Order flags against the instrument's declared capabilities: `post_only`,
//! `expire_after` (good-till-time) and `reduce_only` compile only for instruments whose
//! registry entry declares them, so a strategy never carries an order the venue would
//! reject at placement.

use crate::ir::{Capabilities, DoItem, DurationStr, Registry, Strategy};

use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::poison::Poisons;
use crate::validate::tables::{InstrumentEntry, Tables};

/// Check every flagged intent.
pub fn check(
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    poisons: &Poisons,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (l, leg) in strategy.legs.iter().enumerate() {
        for (b, block) in leg.body.iter().enumerate() {
            for (d, item) in block.do_items.iter().enumerate() {
                let base = DiagPath::root()
                    .key("legs")
                    .index(l)
                    .key("body")
                    .index(b)
                    .key("do")
                    .index(d);
                let (instrument, path, flags) = match item {
                    DoItem::Place(place_item) => {
                        let p = &place_item.place;
                        (
                            p.instrument.as_str(),
                            base.key("place"),
                            Flags::new(p.post_only, p.expire_after.as_ref(), p.reduce_only),
                        )
                    }
                    DoItem::CancelReplace(replace_item) => {
                        let r = &replace_item.cancel_replace;
                        (
                            r.instrument.as_str(),
                            base.key("cancel_replace"),
                            Flags::new(r.post_only, r.expire_after.as_ref(), r.reduce_only),
                        )
                    }
                    DoItem::TargetPosition(target_item) => {
                        let t = &target_item.target_position;
                        (
                            t.instrument.as_str(),
                            base.key("target_position"),
                            Flags::new(None, None, t.reduce_only),
                        )
                    }
                    DoItem::Statement(_) | DoItem::Cancel(_) => continue,
                };
                if flags.is_empty() || poisons.unresolved_instruments.contains(instrument) {
                    continue;
                }
                let declared = match tables.instruments.get(instrument) {
                    Some(InstrumentEntry::Market(i)) => registry.markets[*i].capabilities,
                    Some(InstrumentEntry::Series(i)) => registry.series[*i].capabilities,
                    Some(InstrumentEntry::Unresolved) | None => continue,
                }
                .unwrap_or_default();
                for (flag, supported) in [
                    ("post_only", flags.post_only, declared.post_only),
                    ("expire_after", flags.gtd, declared.gtd),
                    ("reduce_only", flags.reduce_only, declared.reduce_only),
                ]
                .into_iter()
                .filter_map(|(name, used, ok)| used.then_some((name, ok)))
                {
                    if !supported {
                        diagnostics.push(
                            Diagnostic::rule(
                                DiagCode::VenueCapabilityMissing,
                                path.clone().key(flag),
                                format!(
                                    "instrument `{instrument}` does not support `{flag}` (declared: {})",
                                    render(declared)
                                ),
                            )
                            .with_help("drop the flag, or trade an instrument whose registry entry declares it"),
                        );
                    }
                }
            }
        }
    }
}

struct Flags {
    post_only: bool,
    gtd: bool,
    reduce_only: bool,
}

impl Flags {
    fn new(post_only: Option<bool>, expire_after: Option<&DurationStr>, reduce_only: Option<bool>) -> Flags {
        Flags {
            post_only: post_only == Some(true),
            gtd: expire_after.is_some(),
            reduce_only: reduce_only == Some(true),
        }
    }

    fn is_empty(&self) -> bool {
        !(self.post_only || self.gtd || self.reduce_only)
    }
}

fn render(capabilities: Capabilities) -> String {
    let mut declared = Vec::new();
    if capabilities.post_only {
        declared.push("post_only");
    }
    if capabilities.gtd {
        declared.push("gtd");
    }
    if capabilities.reduce_only {
        declared.push("reduce_only");
    }
    if declared.is_empty() {
        "none".to_string()
    } else {
        declared.join(", ")
    }
}
