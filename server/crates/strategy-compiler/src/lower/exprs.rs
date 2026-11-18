//! Expression lowering: a substituted body AST to the program's closed
//! [`ExprNode`] op-tree.
//!
//! Total on validated, substituted ASTs: every reference resolves (signal nicknames
//! to ids, envelope fields to their values), `expr.*` references are gone (inlined
//! by the substitution environment). Spans are dropped — the program carries no
//! source positions.

use std::collections::BTreeMap;

use crate::ir::Envelope;
use crate::parse::ast::expr::{ArithOp, CmpOp, Expr, Literal, LogicOp, Namespace, Ref, UnaryOp};
use crate::parse::builtins::Builtin as AstBuiltin;
use crate::parse::span::Spanned;
use crate::program::{Builtin, ExprNode, IfArm, LegFillField, PlatformRead, Reducer};
use crate::validate::context::EnvelopeField;

/// What lowering resolves names against.
pub struct ExprCtx<'a> {
    /// Signal nickname -> resolved signal id.
    pub signal_ids: &'a BTreeMap<String, String>,
    /// The submission envelope, for resolving `envelope.*` reads to values.
    pub envelope: &'a Envelope,
}

impl ExprCtx<'_> {
    fn signal_id(&self, nick: &str) -> String {
        self.signal_ids
            .get(nick)
            .expect("the validator resolves every signal nickname")
            .clone()
    }

    fn envelope_value(&self, field: &str) -> String {
        let field = EnvelopeField::from_name(field)
            .expect("the validator accepts only readable envelope fields");
        let value = match field {
            EnvelopeField::Allocation => Some(&self.envelope.allocation),
            EnvelopeField::LossBudget => Some(&self.envelope.loss_budget),
            EnvelopeField::RoundBudget => self.envelope.round_budget.as_ref(),
            EnvelopeField::MaxOrderSize => Some(&self.envelope.max_order_size),
            EnvelopeField::MaxPositionPerMarket => self.envelope.max_position_per_market.as_ref(),
            EnvelopeField::MaxOpenOrders => self.envelope.max_open_orders.as_ref(),
        };
        value
            .expect("the validator guarantees read envelope fields are present")
            .as_str()
            .to_string()
    }
}

/// Lower one substituted body expression.
pub fn lower_expr(expr: &Spanned<Expr>, ctx: &ExprCtx) -> ExprNode {
    let bin = |lhs: &Spanned<Expr>, rhs: &Spanned<Expr>| {
        (Box::new(lower_expr(lhs, ctx)), Box::new(lower_expr(rhs, ctx)))
    };
    match &expr.node {
        Expr::Literal(literal) => lower_literal(literal),
        Expr::Ref(r) => lower_ref(r, ctx),
        Expr::Unary { op, operand } => {
            let arg = Box::new(lower_expr(operand, ctx));
            match op {
                UnaryOp::Neg => ExprNode::Neg { arg },
                UnaryOp::Not => ExprNode::Not { arg },
            }
        }
        Expr::Arith { op, lhs, rhs } => {
            let (lhs, rhs) = bin(lhs, rhs);
            match op {
                ArithOp::Add => ExprNode::Add { lhs, rhs },
                ArithOp::Sub => ExprNode::Sub { lhs, rhs },
                ArithOp::Mul => ExprNode::Mul { lhs, rhs },
                ArithOp::Div => ExprNode::Div { lhs, rhs },
            }
        }
        Expr::Cmp { op, lhs, rhs } => {
            let (lhs, rhs) = bin(lhs, rhs);
            match op {
                CmpOp::Lt => ExprNode::Lt { lhs, rhs },
                CmpOp::Le => ExprNode::Le { lhs, rhs },
                CmpOp::Gt => ExprNode::Gt { lhs, rhs },
                CmpOp::Ge => ExprNode::Ge { lhs, rhs },
                CmpOp::Eq => ExprNode::Eq { lhs, rhs },
                CmpOp::Ne => ExprNode::Ne { lhs, rhs },
            }
        }
        Expr::Logic { op, lhs, rhs } => {
            let (lhs, rhs) = bin(lhs, rhs);
            match op {
                LogicOp::And => ExprNode::And { lhs, rhs },
                LogicOp::Or => ExprNode::Or { lhs, rhs },
            }
        }
        Expr::If { arms, else_value } => ExprNode::If {
            arms: arms
                .iter()
                .map(|arm| IfArm {
                    condition: lower_expr(&arm.condition, ctx),
                    value: lower_expr(&arm.value, ctx),
                })
                .collect(),
            else_value: Box::new(lower_expr(else_value, ctx)),
        },
        Expr::Call { builtin, args, .. } => lower_call(*builtin, args, ctx),
        Expr::Triggered { name } => ExprNode::Triggered {
            name: name.node.to_string(),
        },
    }
}

/// Lower a literal to its node. Decimal text is preserved verbatim.
pub fn lower_literal(literal: &Literal) -> ExprNode {
    match literal {
        Literal::Decimal(d) => ExprNode::Decimal {
            value: d.to_string(),
        },
        Literal::Bool(b) => ExprNode::Bool { value: *b },
        Literal::DurationMs(ms) => ExprNode::DurationMs { value: *ms },
        Literal::Variant(v) => ExprNode::Variant {
            value: v.to_string(),
        },
    }
}

fn lower_ref(r: &Ref, ctx: &ExprCtx) -> ExprNode {
    match r.namespace {
        Namespace::Signal => {
            let [nick, field] = r.path.as_slice() else {
                unreachable!("the validator pins signal reads to nick.field")
            };
            ExprNode::Signal {
                signal_id: ctx.signal_id(&nick.node),
                field: field.node.to_string(),
            }
        }
        Namespace::Event => {
            let [field] = r.path.as_slice() else {
                unreachable!("the validator pins event reads to one field")
            };
            ExprNode::Event {
                field: field.node.to_string(),
            }
        }
        Namespace::State => {
            let [slot] = r.path.as_slice() else {
                unreachable!("the validator pins state reads to one slot segment")
            };
            match &r.index {
                None => ExprNode::State {
                    slot: slot.node.to_string(),
                },
                Some(keys) => ExprNode::StateMap {
                    slot: slot.node.to_string(),
                    keys: keys.iter().map(|k| lower_expr(k, ctx)).collect(),
                },
            }
        }
        Namespace::Platform => {
            let spelled: Vec<&str> = r.path.iter().map(|s| s.node.as_ref()).collect();
            let read = PlatformRead::from_path(&spelled)
                .expect("the validator accepts only PlatformRead::from_path spellings");
            ExprNode::Platform { read }
        }
        Namespace::Legs => {
            let (leg, tag, field) = match r.path.as_slice() {
                [leg, field] => (leg, None, field),
                [leg, _tag_kw, tag, field] => (leg, Some(tag.node.to_string()), field),
                _ => unreachable!("the validator pins leg reads to two or four segments"),
            };
            ExprNode::LegFill {
                leg: leg.node.to_string(),
                tag,
                field: LegFillField::from_name(&field.node)
                    .expect("the validator closes the leg fill field set"),
            }
        }
        Namespace::Envelope => {
            let [field] = r.path.as_slice() else {
                unreachable!("the validator pins envelope reads to one field")
            };
            ExprNode::Envelope {
                field: field.node.to_string(),
                value: ctx.envelope_value(&field.node),
            }
        }
        Namespace::Param => {
            let [name] = r.path.as_slice() else {
                unreachable!("the validator pins param reads to one segment")
            };
            ExprNode::Param {
                name: name.node.to_string(),
            }
        }
        Namespace::Expr => unreachable!("named expressions are inlined before lowering"),
        Namespace::Table => unreachable!("table references appear only under lookup"),
        Namespace::Entry => ExprNode::Entry,
    }
}

fn lower_call(builtin: AstBuiltin, args: &[Spanned<Expr>], ctx: &ExprCtx) -> ExprNode {
    let math = |b: Builtin| ExprNode::Call {
        builtin: b,
        args: args.iter().map(|a| lower_expr(a, ctx)).collect(),
    };
    match builtin {
        AstBuiltin::Abs => math(Builtin::Abs),
        AstBuiltin::Min => math(Builtin::Min),
        AstBuiltin::Max => math(Builtin::Max),
        AstBuiltin::Clamp => math(Builtin::Clamp),
        AstBuiltin::Floor => math(Builtin::Floor),
        AstBuiltin::Ceil => math(Builtin::Ceil),
        AstBuiltin::Round => math(Builtin::Round),
        AstBuiltin::Exp => math(Builtin::Exp),
        AstBuiltin::Ln => math(Builtin::Ln),
        AstBuiltin::Sqrt => math(Builtin::Sqrt),
        AstBuiltin::Pow => math(Builtin::Pow),
        AstBuiltin::NormCdf => math(Builtin::NormCdf),
        AstBuiltin::Lookup => {
            let Expr::Ref(table_ref) = &args[0].node else {
                unreachable!("the validator pins lookup's first argument to a table.*")
            };
            let [name] = table_ref.path.as_slice() else {
                unreachable!("table references carry no fields")
            };
            ExprNode::Lookup {
                table: name.node.to_string(),
                key: Box::new(lower_expr(&args[1], ctx)),
            }
        }
        AstBuiltin::Count => reduce(Reducer::Count, args, ctx),
        AstBuiltin::Sum => reduce(Reducer::Sum, args, ctx),
        AstBuiltin::MaxOf => reduce(Reducer::MaxOf, args, ctx),
        AstBuiltin::MinOf => reduce(Reducer::MinOf, args, ctx),
        AstBuiltin::Oldest => reduce(Reducer::Oldest, args, ctx),
        AstBuiltin::Newest => reduce(Reducer::Newest, args, ctx),
        AstBuiltin::CountWhere => reduce(Reducer::CountWhere, args, ctx),
        AstBuiltin::Triggered => {
            unreachable!("triggered() parses to its own node, never a call")
        }
    }
}

/// A reducer call: the window slot by name, plus `count_where`'s predicate.
fn reduce(op: Reducer, args: &[Spanned<Expr>], ctx: &ExprCtx) -> ExprNode {
    let Expr::Ref(window) = &args[0].node else {
        unreachable!("the validator pins a reducer's first argument to a state.<window>")
    };
    let [slot] = window.path.as_slice() else {
        unreachable!("window references carry one segment")
    };
    ExprNode::Reduce {
        reducer: op,
        slot: slot.node.to_string(),
        predicate: args.get(1).map(|p| Box::new(lower_expr(p, ctx))),
    }
}
