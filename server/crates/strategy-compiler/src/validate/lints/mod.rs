//! Family F: the three idiom lints and the arm-set warnings. ALL WARNINGS — the
//! shapes are legal but usually wrong; nothing here blocks the typestate gate.
//!
//! The lints share one per-block digest (`BlockInfo`), computed here once: what the
//! guard pins, what state it reads, what the do-list writes, whether the block bears
//! intents or a transition.

mod arming;
mod coverage;
mod machine_gun;
mod missed_wake;
mod rate;

use std::collections::BTreeSet;

use crate::ir::{DoItem, Registry, Strategy};

use crate::parse::ast::expr::{Expr, Namespace};
use crate::parse::ast::stmt::Stmt;
use crate::parse::walk::ParsedString;
use crate::validate::diag::Diagnostic;
use crate::validate::poison::Poisons;
use crate::validate::tables::Tables;
use crate::validate::visit::walk_expr;

/// One block's lint-relevant digest.
pub(super) struct BlockInfo {
    /// Indexes into the document (`legs[leg].body[block]`).
    pub leg: usize,
    /// Block index within the leg.
    pub block: usize,
    /// The block's name (messages).
    pub name: String,
    /// The guard string failed to parse — per-block lints stay quiet.
    pub guard_failed: bool,
    /// Trigger names the guard tests via `triggered()`.
    pub triggered: Vec<String>,
    /// State slots the guard reads.
    pub guard_state_reads: BTreeSet<String>,
    /// State slots the do-list writes (`set` lvalues).
    pub writes: BTreeSet<String>,
    /// Whether the do-list carries any order intent (place or cancel-replace) — the
    /// coverage lint's notion of "intent-bearing" (ir-spec §14).
    pub has_intent: bool,
    /// Whether the do-list carries a bare `place` — the machine-gun lint's
    /// intent-bearing set (ruled 2026-07-20: place ONLY; see `machine_gun.rs`).
    pub has_place: bool,
    /// Whether the do-list carries anything that submits to the venue on every
    /// wake (`place`, `cancel_replace`, `target_position`) — the requote-rate
    /// lint's set (see `rate.rs`).
    pub has_requote: bool,
    /// Whether the block transitions (`then`).
    pub has_then: bool,
}

/// Run every lint.
pub fn check(
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    poisons: &Poisons,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let blocks = digest(strategy, tables, poisons);
    missed_wake::check(&blocks, tables, diagnostics);
    machine_gun::check(&blocks, tables, diagnostics);
    rate::check(&blocks, registry, tables, diagnostics);
    coverage::check(strategy, tables, &blocks, diagnostics);
    arming::check(strategy, tables, &blocks, diagnostics);
}

/// Build every block's digest.
fn digest(strategy: &Strategy, tables: &Tables, poisons: &Poisons) -> Vec<BlockInfo> {
    let mut blocks = Vec::new();
    for (l, leg) in strategy.legs.iter().enumerate() {
        for (b, block) in leg.body.iter().enumerate() {
            let guard_failed = poisons.failed_guards.contains(&(l, b));
            let mut triggered = Vec::new();
            let mut guard_state_reads = BTreeSet::new();
            if !guard_failed
                && block.guard.is_some()
                && let Some(walked) = tables.ast_at(&format!("legs[{l}].body[{b}].if"))
                && let ParsedString::Expr(guard) = &walked.parsed
            {
                walk_expr(guard, &mut |node| match &node.node {
                    Expr::Triggered { name } => triggered.push(name.node.to_string()),
                    Expr::Ref(r) if r.namespace == Namespace::State => {
                        if let Some(head) = r.path.first() {
                            guard_state_reads.insert(head.node.to_string());
                        }
                    }
                    Expr::Ref(_)
                    | Expr::Literal(_)
                    | Expr::Unary { .. }
                    | Expr::Arith { .. }
                    | Expr::Cmp { .. }
                    | Expr::Logic { .. }
                    | Expr::If { .. }
                    | Expr::Call { .. }
                    => {}
                });
            }

            let mut writes = BTreeSet::new();
            let mut has_intent = false;
            let mut has_place = false;
            let mut has_requote = false;
            for (d, item) in block.do_items.iter().enumerate() {
                match item {
                    DoItem::Statement(_) => {
                        if let Some(walked) = tables.ast_at(&format!("legs[{l}].body[{b}].do[{d}]"))
                            && let ParsedString::Stmt(stmt) = &walked.parsed
                        {
                            let (Stmt::Set { slot, .. } | Stmt::Push { slot, .. }) = &stmt.node;
                            writes.insert(slot.node.to_string());
                        }
                    }
                    DoItem::Place(_) => {
                        has_intent = true;
                        has_place = true;
                        has_requote = true;
                    }
                    // Self-limiting like cancel_replace: the runtime places nothing when
                    // already at target, so it does not stack exposure.
                    DoItem::CancelReplace(_) | DoItem::TargetPosition(_) => {
                        has_intent = true;
                        has_requote = true;
                    }
                    DoItem::Cancel(_) => {}
                }
            }

            blocks.push(BlockInfo {
                leg: l,
                block: b,
                name: block.name.as_str().to_string(),
                guard_failed,
                triggered,
                guard_state_reads,
                writes,
                has_intent,
                has_place,
                has_requote,
                has_then: block.then.is_some(),
            });
        }
    }
    blocks
}
