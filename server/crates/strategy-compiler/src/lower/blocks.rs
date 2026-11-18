//! Block lowering: per-block plans — guard, do-items in authored order, transition —
//! plus each block's snapshot read set.
//!
//! Every AST in a plan is POST-SUBSTITUTION: named expressions are inlined, so a
//! `signal.*` read hidden inside one is in the block's read set and the program
//! emitter never sees an `expr.*` reference. The plan's item list preserves AUTHORED
//! do-list order (statements and intents interleaved as written): IR semantics say
//! items execute in listed order, and grouping statements first could flip an intent
//! expression's view of state written after it.

use std::collections::BTreeSet;

use crate::ir::DoItem;

use crate::parse::ast::expr::{Expr, Namespace};
use crate::parse::ast::stmt::Stmt;
use crate::parse::span::Spanned;
use crate::parse::walk::ParsedString;
use crate::validate::ValidStrategy;
use crate::validate::visit::walk_expr;

use super::intents::{self, IntentIndex, IntentLower};

/// One lowered block.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockPlan {
    /// The block's name (`# block:` comment, intent-id segment).
    pub name: String,
    /// The guard, substituted AST. Absent means the block always runs.
    pub guard: Option<Spanned<Expr>>,
    /// The distinct `signal.<nick>.<field>` reads of the guard, statements, and intent
    /// price/size expressions — transitively through named expressions — deduped, in
    /// deterministic (nick, field) order. Each becomes one hoisted `_<nick>_<field>`
    /// local guarded by an availability check.
    pub reads: Vec<(String, String)>,
    /// The do-items, authored order.
    pub items: Vec<PlanItem>,
    /// The `then` transition target, if any. Immediate and terminal.
    pub then: Option<String>,
}

/// One do-item in plan form.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanItem {
    /// A `set` statement, substituted ASTs.
    Set {
        /// The target slot name.
        slot: String,
        /// Index key expressions, present iff the slot is map-typed.
        index: Option<Vec<Spanned<Expr>>>,
        /// The right-hand side.
        rhs: Spanned<Expr>,
    },
    /// A `push` statement onto a window slot, substituted AST.
    Push {
        /// The target window slot name.
        slot: String,
        /// The appended value.
        value: Spanned<Expr>,
    },
    /// An order intent.
    Intent(IntentLower),
}

/// Lower every leg's blocks, filling the intent index as intents are visited.
/// Returns per-leg block plans in document order.
pub fn lower_blocks(valid: &ValidStrategy) -> (Vec<Vec<BlockPlan>>, IntentIndex) {
    let strategy = valid.strategy();
    let mut index = IntentIndex::new();
    let legs = strategy
        .legs
        .iter()
        .enumerate()
        .map(|(l, leg)| {
            leg.body
                .iter()
                .enumerate()
                .map(|(b, block)| lower_block(valid, l, leg, b, block, &mut index))
                .collect()
        })
        .collect();
    (legs, index)
}

/// Lower one block.
fn lower_block(
    valid: &ValidStrategy,
    leg_index: usize,
    leg: &crate::ir::Leg,
    block_index: usize,
    block: &crate::ir::Block,
    index: &mut IntentIndex,
) -> BlockPlan {
    let base = format!("legs[{leg_index}].body[{block_index}]");
    let mut reads = BTreeSet::new();

    let guard = block.guard.as_ref().map(|_| {
        let ast = expr_ast(valid, &format!("{base}.if"));
        collect_reads(&ast, &mut reads);
        ast
    });

    let mut items = Vec::with_capacity(block.do_items.len());
    for (d, item) in block.do_items.iter().enumerate() {
        let item_base = format!("{base}.do[{d}]");
        match item {
            DoItem::Statement(_) => {
                let stmt = stmt_ast(valid, &item_base);
                match &stmt.node {
                    Stmt::Set { slot, index, rhs } => {
                        let index = index.as_ref().map(|keys| {
                            keys.iter().map(|key| substituted(valid, key)).collect::<Vec<_>>()
                        });
                        let rhs = substituted(valid, rhs);
                        if let Some(keys) = &index {
                            for key in keys {
                                collect_reads(key, &mut reads);
                            }
                        }
                        collect_reads(&rhs, &mut reads);
                        items.push(PlanItem::Set {
                            slot: slot.node.to_string(),
                            index,
                            rhs,
                        });
                    }
                    Stmt::Push { slot, value } => {
                        let value = substituted(valid, value);
                        collect_reads(&value, &mut reads);
                        items.push(PlanItem::Push {
                            slot: slot.node.to_string(),
                            value,
                        });
                    }
                }
            }
            DoItem::Place(place_item) => {
                let intent_id = format!("{}.{}.{d}", leg.name.as_str(), block.name.as_str());
                let (lowered, entry) = intents::lower_place(
                    valid,
                    intent_id.clone(),
                    &place_item.place,
                    &format!("{item_base}.place"),
                );
                collect_order_reads(&lowered, &mut reads);
                index.insert(intent_id, entry);
                items.push(PlanItem::Intent(lowered));
            }
            DoItem::Cancel(cancel_item) => {
                items.push(PlanItem::Intent(IntentLower::Cancel {
                    tag: cancel_item.cancel.tag.as_str().to_string(),
                }));
            }
            DoItem::TargetPosition(target_item) => {
                let intent_id = format!("{}.{}.{d}", leg.name.as_str(), block.name.as_str());
                let (lowered, entry) = intents::lower_target_position(
                    valid,
                    intent_id.clone(),
                    &target_item.target_position,
                    &format!("{item_base}.target_position"),
                );
                collect_order_reads(&lowered, &mut reads);
                index.insert(intent_id, entry);
                items.push(PlanItem::Intent(lowered));
            }
            DoItem::CancelReplace(replace_item) => {
                let intent_id = format!("{}.{}.{d}", leg.name.as_str(), block.name.as_str());
                let (lowered, entry) = intents::lower_cancel_replace(
                    valid,
                    intent_id.clone(),
                    &replace_item.cancel_replace,
                    &format!("{item_base}.cancel_replace"),
                );
                collect_order_reads(&lowered, &mut reads);
                index.insert(intent_id, entry);
                items.push(PlanItem::Intent(lowered));
            }
        }
    }

    BlockPlan {
        name: block.name.as_str().to_string(),
        guard,
        reads: reads.into_iter().collect(),
        items,
        then: block.then.as_ref().map(|leg| leg.as_str().to_string()),
    }
}

/// Collect an order's price/size snapshot reads. `max_size` is excluded: it never
/// appears in generated code (the runner reads it from the intent index).
fn collect_order_reads(lowered: &IntentLower, reads: &mut BTreeSet<(String, String)>) {
    let order = match lowered {
        IntentLower::Place { order, .. }
        | IntentLower::CancelReplace { order, .. }
        | IntentLower::TargetPosition { order, .. } => order,
        IntentLower::Cancel { .. } => return,
    };
    collect_reads(&order.size, reads);
    if let Some(price) = &order.price {
        collect_reads(price, reads);
    }
}

/// Add every `signal.<nick>.<field>` read of a substituted expression — named-
/// expression bodies are already inlined, so they contribute transitively.
fn collect_reads(expr: &Spanned<Expr>, reads: &mut BTreeSet<(String, String)>) {
    walk_expr(expr, &mut |node| {
        if let Expr::Ref(r) = &node.node
            && r.namespace == Namespace::Signal
            && let [nick, field] = r.path.as_slice()
        {
            reads.insert((nick.node.to_string(), field.node.to_string()));
        }
    });
}

/// The SUBSTITUTED expression AST at a walker path. Total on validated documents.
fn expr_ast(valid: &ValidStrategy, path: &str) -> Spanned<Expr> {
    let walked = valid
        .tables()
        .ast_at(path)
        .expect("validated documents have every body expression parsed");
    match &walked.parsed {
        ParsedString::Expr(ast) => substituted(valid, ast),
        ParsedString::Stmt(_) => unreachable!("an expression path never parses as a statement"),
    }
}

/// Inline every named-expression reference. Total on validated documents.
pub(super) fn substituted(valid: &ValidStrategy, expr: &Spanned<Expr>) -> Spanned<Expr> {
    valid
        .substituted(expr)
        .expect("validated documents substitute every body expression")
}

/// The raw statement AST at a walker path. Total on validated documents.
fn stmt_ast(valid: &ValidStrategy, path: &str) -> Spanned<Stmt> {
    let walked = valid
        .tables()
        .ast_at(path)
        .expect("validated documents have every statement parsed");
    match &walked.parsed {
        ParsedString::Stmt(ast) => ast.clone(),
        ParsedString::Expr(_) => unreachable!("a statement path never parses as an expression"),
    }
}
