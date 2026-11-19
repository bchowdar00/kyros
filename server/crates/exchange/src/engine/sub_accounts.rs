//! Strategy sub-accounts.
//!
//! A sub-account is an ordinary [`Account`] with its own address, funded
//! by a cash transfer from a parent user account and linked back to that
//! parent via `Account.parent`. Every deployed strategy lives on exactly
//! one sub-account, so each strategy has an isolated position, collateral
//! and PnL while the engine's matching, clearing and settlement paths run
//! unchanged.
//!
//! Money only ever moves between parent and child as a plain transfer, so
//! `Σ usdc_balance + Σ locked + OI × $1` is invariant across create, fund
//! and close.

use std::fmt;

use crate::engine::{Exchange, OrderError, TradeSource};
use crate::types::{Address, Qty, SeriesId, Side, Tenths};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubAccountError {
    /// The parent address has no account.
    UnknownParent,
    /// The parent is a bot (unregistered) account.
    ParentNotUser,
    /// The parent is itself a sub-account; nesting is not allowed.
    ParentIsSubAccount,
    /// Allocation below [`MIN_SUB_ACCOUNT_ALLOCATION`].
    AllocationTooSmall { min: Tenths },
    /// The source side of a transfer lacks free cash.
    InsufficientBalance { required: Tenths, available: Tenths },
    /// The parent already has [`MAX_SUB_ACCOUNTS_PER_USER`] children.
    TooManySubAccounts { max: usize },
    /// No sub-account at that address.
    UnknownSubAccount,
    /// The sub-account exists but belongs to a different parent.
    NotOwner,
}

impl fmt::Display for SubAccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubAccountError::UnknownParent => write!(f, "parent account not found"),
            SubAccountError::ParentNotUser => write!(f, "parent is not a user account"),
            SubAccountError::ParentIsSubAccount => write!(f, "sub-accounts cannot be nested"),
            SubAccountError::AllocationTooSmall { min } => {
                write!(f, "allocation below minimum of {} tenths", min.0)
            }
            SubAccountError::InsufficientBalance { required, available } => write!(
                f,
                "insufficient balance: required {} available {}",
                required.0, available.0
            ),
            SubAccountError::TooManySubAccounts { max } => {
                write!(f, "at most {max} sub-accounts per user")
            }
            SubAccountError::UnknownSubAccount => write!(f, "sub-account not found"),
            SubAccountError::NotOwner => write!(f, "sub-account belongs to another parent"),
        }
    }
}

impl std::error::Error for SubAccountError {}

/// What happened when a sub-account was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseSubAccountReport {
    /// Cash returned to the parent. Zero when the child could not be
    /// flattened and was left open for the orphan sweep at resolution.
    pub swept: Tenths,
    /// Resting orders cancelled (locks refunded to the child first).
    pub cancelled_orders: u32,
    /// Signed position left on the child because there was no liquidity
    /// to close it. Zero on a clean close.
    pub residual_position: i64,
    /// `true` iff the child account was removed from the ledger.
    pub removed: bool,
}

impl Exchange {
    /// Create a funded sub-account under `parent`, bound to `home`.
    ///
    /// Validates the parent (exists, is a registered user, is not itself a
    /// child, is under the per-user cap), the allocation (≥ minimum,
    /// ≤ parent free cash), then generates a fresh random address, seeds
    /// the child with `allocation`, debits the parent and registers the
    /// child as a user so it is paid out at settlement.
    pub fn create_sub_account_in(
        &mut self,
        parent: Address,
        allocation: Tenths,
        home: SeriesId,
    ) -> Result<Address, SubAccountError> {
        let child = self.ledger.fresh_address();
        self.create_sub_account_at(parent, child, allocation, home)?;
        Ok(child)
    }

    /// [`Exchange::create_sub_account_in`] with a caller-chosen child
    /// address. Panics if `child` already has an account — production
    /// callers go through `create_sub_account_in`, which picks a fresh
    /// address; this exists so tests and fixtures can use deterministic
    /// addresses.
    pub fn create_sub_account_at(
        &mut self,
        parent: Address,
        child: Address,
        allocation: Tenths,
        home: SeriesId,
    ) -> Result<(), SubAccountError> {
        self.ledger.create_sub_account_at(parent, child, allocation, home)
    }

    /// Move free cash between `parent` and `child`. Positive `amount`
    /// funds the child from the parent; negative withdraws from the
    /// child back to the parent. Bounded by free (unlocked) cash on the
    /// source side. Zero is a no-op. The child's `net_contributed` moves
    /// by `amount` so equity-versus-contributed PnL stays honest.
    pub fn fund_sub_account(
        &mut self,
        parent: Address,
        child: Address,
        amount: Tenths,
    ) -> Result<(), SubAccountError> {
        self.check_ownership(parent, child)?;
        if amount.0 == 0 {
            return Ok(());
        }
        let (from, to, need) = if amount.0 > 0 {
            (parent, child, amount)
        } else {
            (child, parent, Tenths(-amount.0))
        };
        self.ledger.transfer(from, to, need)?;
        let child_acct = self.ledger.account_mut(&child).expect("checked");
        child_acct.net_contributed = Some(child_acct.net_contributed.unwrap_or(Tenths::ZERO) + amount);
        Ok(())
    }

    /// Wind down a sub-account: cancel its resting orders, market-close
    /// its position, sweep its cash back to the parent and delete it. All
    /// trading happens in the child's home series — the only series it
    /// can hold anything in.
    ///
    /// If the position cannot be fully flattened (no opposing liquidity)
    /// the child is left in place with its strategy cleared and its
    /// remaining cash untouched; `settle_all` sweeps and removes it once
    /// the round resolves and the position is flat.
    pub fn close_sub_account(
        &mut self,
        parent: Address,
        child: Address,
    ) -> Result<CloseSubAccountReport, SubAccountError> {
        self.check_ownership(parent, child)?;
        let home = self
            .ledger
            .home_series(&child)
            .expect("sub-accounts always have a home series");
        let (engine, ledger) = self.engine_and_ledger(home);

        let cancelled_orders = engine.cancel_resting_for(ledger, child);

        let signed = engine.position(&child).position.0;
        // With real orders the position is held on the outside venue too:
        // flattening it against the pretend book would leave the two
        // apart. It rides to the round's result instead.
        if signed != 0 && engine.outside().is_none() {
            let side = if signed > 0 { Side::Ask } else { Side::Bid };
            let qty = Qty(signed.unsigned_abs() as u32);
            match engine.place_market(ledger, child, side, qty, TradeSource::Strategy) {
                Ok(_) | Err(OrderError::NoLiquidity) => {}
                Err(e) => {
                    // ZeroQuantity/UnknownAccount are impossible here;
                    // InsufficientBalance cannot happen on a pure close
                    // because closing releases collateral. Log and fall
                    // through to the orphan path rather than panic.
                    tracing::warn!(?child, error = %e, "close_sub_account: market close failed");
                }
            }
        }

        let residual_position = engine.position(&child).position.0;
        if residual_position != 0 {
            // Leave the account for the orphan sweep; make sure no
            // executor keeps driving it.
            if let Some(acct) = ledger.account_mut(&child) {
                acct.strategy = None;
            }
            return Ok(CloseSubAccountReport {
                swept: Tenths::ZERO,
                cancelled_orders,
                residual_position,
                removed: false,
            });
        }

        engine.forget_account(child);
        let swept = ledger.remove_sub_account(child);
        Ok(CloseSubAccountReport {
            swept,
            cancelled_orders,
            residual_position: 0,
            removed: true,
        })
    }

    fn check_ownership(&self, parent: Address, child: Address) -> Result<(), SubAccountError> {
        let acct = self
            .ledger
            .account(&child)
            .ok_or(SubAccountError::UnknownSubAccount)?;
        match acct.parent {
            None => Err(SubAccountError::UnknownSubAccount),
            Some(p) if p == parent => Ok(()),
            Some(_) => Err(SubAccountError::NotOwner),
        }
    }
}

/// Fixtures shared by test modules across the crate.
#[cfg(test)]
pub mod test_support {
    use crate::engine::DeployedStrategy;
    use crate::engine::strategies::test_support::idle_record;
    use crate::types::SeriesId;

    /// A compiled, do-nothing strategy record for `series`, so a
    /// sub-account counts as occupied.
    pub fn record(series: SeriesId) -> DeployedStrategy {
        named("Test", series)
    }

    /// [`record`] with a display name.
    pub fn named(name: &str, series: SeriesId) -> DeployedStrategy {
        idle_record(name, series, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::record;
    use crate::engine::Engine;
    use crate::engine::{MAX_SUB_ACCOUNTS_PER_USER, MIN_SUB_ACCOUNT_ALLOCATION, PAYOUT_PER_CONTRACT};
    use crate::engine::strategies::StrategyError;
    use crate::ticks::PriceTick;

    const START: Tenths = Tenths(1_000_000);

    fn addr(byte: u8) -> Address {
        Address([byte; 20])
    }

    fn tick(t: u16) -> PriceTick {
        PriceTick::from_tenths(t).unwrap()
    }

    fn user_engine(users: &[Address]) -> Engine {
        let mut e = Engine::new(START);
        for u in users {
            e.register_user(*u);
            e.get_or_create_account(*u);
        }
        e
    }

    /// Cash + locked across every account plus escrow must equal the
    /// seeded total. Sub-accounts are created from parent cash so they
    /// are included automatically by iterating the ledger.
    fn assert_conservation(e: &Engine, seeded_accounts: usize) {
        let total: i64 = e
            .accounts_iter()
            .map(|a| a.usdc_balance.0 + a.locked.0)
            .sum();
        let escrow = (e.open_interest() as i64) * PAYOUT_PER_CONTRACT;
        assert_eq!(total + escrow, seeded_accounts as i64 * START.0);
        let pos: i64 = e.positions_iter().map(|(_, p)| p.position.0).sum();
        assert_eq!(pos, 0);
    }

    // ───────────────────────────── create ─────────────────────────────

    #[test]
    fn create_debits_parent_and_links_child() {
        let p = addr(1);
        let mut e = user_engine(&[p]);
        let c = e.create_sub_account(p, Tenths(50_000)).unwrap();

        assert_eq!(e.account(&p).unwrap().usdc_balance, Tenths(950_000));
        let child = e.account(&c).unwrap();
        assert_eq!(child.usdc_balance, Tenths(50_000));
        assert_eq!(child.parent, Some(p));
        assert_eq!(child.net_contributed, Some(Tenths(50_000)));
        assert!(child.is_sub_account());
        assert!(e.is_user_account(&c));
        assert_eq!(e.sub_accounts_of(&p), &[c]);
        assert_eq!(e.root_of(&c), p);
        assert_eq!(e.root_of(&p), p);
        assert_conservation(&e, 1);
    }

    #[test]
    fn create_preserves_creation_order() {
        let p = addr(1);
        let mut e = user_engine(&[p]);
        let a = e.create_sub_account(p, Tenths(10_000)).unwrap();
        let b = e.create_sub_account(p, Tenths(10_000)).unwrap();
        let c = e.create_sub_account(p, Tenths(10_000)).unwrap();
        assert_eq!(e.sub_accounts_of(&p), &[a, b, c]);
    }

    #[test]
    fn create_rejects_below_minimum() {
        let p = addr(1);
        let mut e = user_engine(&[p]);
        assert_eq!(
            e.create_sub_account(p, Tenths(9_999)),
            Err(SubAccountError::AllocationTooSmall {
                min: MIN_SUB_ACCOUNT_ALLOCATION
            })
        );
        assert!(e.create_sub_account(p, MIN_SUB_ACCOUNT_ALLOCATION).is_ok());
    }

    #[test]
    fn create_rejects_insufficient_balance() {
        let p = addr(1);
        let mut e = user_engine(&[p]);
        assert_eq!(
            e.create_sub_account(p, Tenths(1_000_001)),
            Err(SubAccountError::InsufficientBalance {
                required: Tenths(1_000_001),
                available: START,
            })
        );
    }

    #[test]
    fn create_rejects_unknown_and_bot_parents() {
        let mut e = Engine::new(START);
        assert_eq!(
            e.create_sub_account(addr(9), Tenths(10_000)),
            Err(SubAccountError::UnknownParent)
        );
        let bot = addr(2);
        e.get_or_create_account(bot); // not registered → bot
        assert_eq!(
            e.create_sub_account(bot, Tenths(10_000)),
            Err(SubAccountError::ParentNotUser)
        );
    }

    #[test]
    fn create_rejects_nesting() {
        let p = addr(1);
        let mut e = user_engine(&[p]);
        let c = e.create_sub_account(p, Tenths(50_000)).unwrap();
        assert_eq!(
            e.create_sub_account(c, Tenths(10_000)),
            Err(SubAccountError::ParentIsSubAccount)
        );
    }

    #[test]
    fn create_enforces_per_user_cap() {
        let p = addr(1);
        let mut e = user_engine(&[p]);
        // Give the parent enough cash for cap+1 minimum allocations.
        e.get_or_create_account(p).usdc_balance =
            Tenths(MIN_SUB_ACCOUNT_ALLOCATION.0 * (MAX_SUB_ACCOUNTS_PER_USER as i64 + 1));
        for _ in 0..MAX_SUB_ACCOUNTS_PER_USER {
            e.create_sub_account(p, MIN_SUB_ACCOUNT_ALLOCATION).unwrap();
        }
        assert_eq!(
            e.create_sub_account(p, MIN_SUB_ACCOUNT_ALLOCATION),
            Err(SubAccountError::TooManySubAccounts {
                max: MAX_SUB_ACCOUNTS_PER_USER
            })
        );
    }

    // ────────────────────────── deploy guard ──────────────────────────

    #[test]
    fn deploy_strategy_requires_sub_account() {
        let p = addr(1);
        let mut e = user_engine(&[p]);
        let err = e.deploy_strategy(p, record(e.series())).unwrap_err();
        assert!(matches!(err, StrategyError::InvalidStrategy(_)));

        let c = e.create_sub_account(p, Tenths(50_000)).unwrap();
        let s = e.deploy_strategy(c, record(e.series())).unwrap();
        assert_eq!(e.get_strategy(&c).unwrap(), Some(s));
    }

    // ───────────────────────────── fund ───────────────────────────────

    #[test]
    fn fund_moves_cash_both_directions_within_bounds() {
        let p = addr(1);
        let mut e = user_engine(&[p]);
        let c = e.create_sub_account(p, Tenths(50_000)).unwrap();

        e.fund_sub_account(p, c, Tenths(25_000)).unwrap();
        assert_eq!(e.account(&c).unwrap().usdc_balance, Tenths(75_000));
        assert_eq!(e.account(&p).unwrap().usdc_balance, Tenths(925_000));
        assert_eq!(e.account(&c).unwrap().net_contributed, Some(Tenths(75_000)));

        e.fund_sub_account(p, c, Tenths(-70_000)).unwrap();
        assert_eq!(e.account(&c).unwrap().usdc_balance, Tenths(5_000));
        assert_eq!(e.account(&p).unwrap().usdc_balance, Tenths(995_000));
        assert_eq!(e.account(&c).unwrap().net_contributed, Some(Tenths(5_000)));

        assert_eq!(
            e.fund_sub_account(p, c, Tenths(-5_001)),
            Err(SubAccountError::InsufficientBalance {
                required: Tenths(5_001),
                available: Tenths(5_000),
            })
        );
        assert!(e.fund_sub_account(p, c, Tenths::ZERO).is_ok());
        assert_conservation(&e, 1);
    }

    #[test]
    fn fund_and_close_check_ownership() {
        let (p, q) = (addr(1), addr(2));
        let mut e = user_engine(&[p, q]);
        let c = e.create_sub_account(p, Tenths(50_000)).unwrap();
        assert_eq!(
            e.fund_sub_account(q, c, Tenths(1_000)),
            Err(SubAccountError::NotOwner)
        );
        assert_eq!(e.close_sub_account(q, c), Err(SubAccountError::NotOwner));
        assert_eq!(
            e.close_sub_account(p, addr(77)),
            Err(SubAccountError::UnknownSubAccount)
        );
        // A top-level account is not a sub-account even though it exists.
        assert_eq!(
            e.close_sub_account(p, q),
            Err(SubAccountError::UnknownSubAccount)
        );
    }

    // ───────────────────────────── close ──────────────────────────────

    #[test]
    fn close_flat_child_sweeps_and_removes() {
        let p = addr(1);
        let mut e = user_engine(&[p]);
        let c = e.create_sub_account(p, Tenths(50_000)).unwrap();
        e.deploy_strategy(c, record(e.series())).unwrap();

        let r = e.close_sub_account(p, c).unwrap();
        assert_eq!(
            r,
            CloseSubAccountReport {
                swept: Tenths(50_000),
                cancelled_orders: 0,
                residual_position: 0,
                removed: true,
            }
        );
        assert!(e.account(&c).is_none());
        assert!(!e.is_user_account(&c));
        assert!(e.sub_accounts_of(&p).is_empty());
        assert_eq!(e.account(&p).unwrap().usdc_balance, START);
        assert_conservation(&e, 1);
    }

    #[test]
    fn close_drops_the_child_settled_order_archive() {
        let (p, mm) = (addr(1), addr(2));
        let mut e = user_engine(&[p, mm]);
        e.open_first_market(50_000.0, 0, 60);
        let c = e.create_sub_account(p, Tenths(50_000)).unwrap();
        // A strategy keeps the child alive across resolution (a
        // strategy-less child is an orphan and gets swept).
        e.deploy_strategy(c, record(e.series())).unwrap();
        e.place_limit(mm, Side::Ask, tick(500), Qty(1), TradeSource::Bot).unwrap();
        e.place_market(c, Side::Bid, Qty(1), TradeSource::Strategy).unwrap();
        e.resolve_and_open_next(50_001.0, 50_001.0, 60, 60);
        assert_eq!(e.settled_user_orders(&c).len(), 1);

        let r = e.close_sub_account(p, c).unwrap();
        assert!(r.removed);
        assert!(e.settled_user_orders(&c).is_empty());
        assert_conservation(&e, 2);
    }

    #[test]
    fn close_cancels_resting_orders_and_refunds_locks() {
        let p = addr(1);
        let mut e = user_engine(&[p]);
        let c = e.create_sub_account(p, Tenths(50_000)).unwrap();
        e.place_limit(c, Side::Bid, tick(400), Qty(10), TradeSource::Strategy).unwrap();
        e.place_limit(c, Side::Ask, tick(600), Qty(5), TradeSource::Strategy).unwrap();
        assert!(e.account(&c).unwrap().locked.0 > 0);

        let r = e.close_sub_account(p, c).unwrap();
        assert_eq!(r.cancelled_orders, 2);
        assert_eq!(r.swept, Tenths(50_000));
        assert!(r.removed);
        assert!(e.book().is_empty());
        assert_conservation(&e, 1);
    }

    #[test]
    fn close_flattens_open_position_when_liquidity_exists() {
        let (p, mm) = (addr(1), addr(2));
        let mut e = user_engine(&[p, mm]);
        let c = e.create_sub_account(p, Tenths(100_000)).unwrap();

        // Child buys 10 YES at 50¢ from mm; mm then quotes a bid at 40¢
        // so the child can sell back.
        e.place_limit(mm, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();
        e.place_market(c, Side::Bid, Qty(10), TradeSource::Strategy).unwrap();
        assert_eq!(e.position(&c).position.0, 10);
        e.place_limit(mm, Side::Bid, tick(400), Qty(10), TradeSource::Bot).unwrap();

        let r = e.close_sub_account(p, c).unwrap();
        assert!(r.removed);
        assert_eq!(r.residual_position, 0);
        // Bought at 50¢, sold at 40¢: lost 10 × 100 tenths = 1000.
        assert_eq!(r.swept, Tenths(99_000));
        assert_eq!(e.account(&p).unwrap().usdc_balance, Tenths(999_000));
        assert!(e.account(&c).is_none());
        assert_eq!(e.open_interest(), 0);
        assert_conservation(&e, 2);
    }

    #[test]
    fn close_with_no_liquidity_leaves_orphan_for_settlement() {
        let (p, mm) = (addr(1), addr(2));
        let mut e = user_engine(&[p, mm]);
        let c = e.create_sub_account(p, Tenths(100_000)).unwrap();
        e.deploy_strategy(c, record(e.series())).unwrap();

        e.place_limit(mm, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();
        e.place_market(c, Side::Bid, Qty(10), TradeSource::Strategy).unwrap();
        assert!(e.book().is_empty());

        let r = e.close_sub_account(p, c).unwrap();
        assert!(!r.removed);
        assert_eq!(r.residual_position, 10);
        assert_eq!(r.swept, Tenths::ZERO);
        let child = e.account(&c).expect("orphan kept");
        assert!(child.strategy.is_none(), "executor must be told to stop");
        assert_eq!(child.usdc_balance, Tenths(95_000));
        assert_eq!(e.sub_accounts_of(&p), &[c]);
        assert_conservation(&e, 2);
    }

    // ───────────────────────── two series ─────────────────────────────

    #[test]
    fn close_sub_account_flattens_in_home_series_only() {
        use crate::series::SERIES;
        use crate::types::SeriesId;
        let (p, mm) = (addr(1), addr(2));
        let mut x = Exchange::new(START, SERIES);
        x.register_user(p);
        x.get_or_create_account(p);
        x.get_or_create_account(mm);
        x.open_first_market(SeriesId(0), 50_000.0, 0, 60);
        x.open_first_market(SeriesId(1), 50_000.0, 0, 900);
        let c = x.create_sub_account_in(p, Tenths(100_000), SeriesId(1)).unwrap();
        x.deploy_strategy_in(c, record(SeriesId(1))).unwrap();
        // The parent holds a 2m position of its own; the child a 15m one.
        x.place_limit(SeriesId(0), mm, Side::Ask, tick(500), Qty(5), TradeSource::Bot).unwrap();
        x.place_market(SeriesId(0), p, Side::Bid, Qty(5), TradeSource::Ui).unwrap();
        x.place_limit(SeriesId(1), mm, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();
        x.place_market(SeriesId(1), c, Side::Bid, Qty(10), TradeSource::Strategy).unwrap();
        x.place_limit(SeriesId(1), mm, Side::Bid, tick(500), Qty(10), TradeSource::Bot).unwrap();
        x.debug_assert_invariants();

        let r = x.close_sub_account(p, c).unwrap();
        assert!(r.removed);
        assert_eq!(r.residual_position, 0);
        assert_eq!(r.swept, Tenths(100_000));
        // The child is gone from both engines; the parent's 2m position
        // and the 2m book were never touched.
        assert!(x.account(&c).is_none());
        assert!(x.position(SeriesId(1), &c).is_flat());
        assert_eq!(x.position(SeriesId(0), &p).position.0, 5);
        assert_eq!(x.engine(SeriesId(0)).open_interest(), 5);
        assert_eq!(x.engine(SeriesId(1)).open_interest(), 0);
        assert_eq!(x.account(&p).unwrap().usdc_balance, Tenths(START.0 - 2500));
        x.debug_assert_invariants();
    }

    #[test]
    fn closing_a_sub_account_whose_fills_are_real_does_not_trade_the_paper_book() {
        use crate::series::SERIES;
        use crate::types::SeriesId;
        let (p, mm, outside) = (addr(1), addr(2), addr(9));
        let mut x = Exchange::new(START, SERIES);
        x.register_user(p);
        x.get_or_create_account(p);
        x.get_or_create_account(mm);
        x.seed_bot_account_in(outside, Tenths(1_000_000_000), "gemini-fills".into(), None, None, SeriesId(0));
        x.set_outside(SeriesId(0), outside);
        x.open_first_market(SeriesId(0), 50_000.0, 0, 60);
        let c = x.create_sub_account_in(p, Tenths(100_000), SeriesId(0)).unwrap();
        x.deploy_strategy_in(c, record(SeriesId(0))).unwrap();
        x.apply_external_fill(SeriesId(0), c, Side::Bid, tick(500), Qty(4)).unwrap();
        // A bid on the paper book that a paper close would hit.
        x.place_limit(SeriesId(0), mm, Side::Bid, tick(500), Qty(10), TradeSource::Bot).unwrap();

        let r = x.close_sub_account(p, c).unwrap();
        assert!(!r.removed);
        assert_eq!(r.residual_position, 4, "the position is Gemini's to settle");
        assert_eq!(x.position(SeriesId(0), &c).position.0, 4);
        assert_eq!(x.position(SeriesId(0), &mm).position.0, 0, "the paper book was not traded");
        x.debug_assert_invariants();
    }

    #[test]
    fn orphan_sweep_runs_at_home_series_settlement() {
        use crate::series::SERIES;
        use crate::types::SeriesId;
        let (p, mm) = (addr(1), addr(2));
        let mut x = Exchange::new(START, SERIES);
        x.register_user(p);
        x.get_or_create_account(p);
        x.get_or_create_account(mm);
        x.open_first_market(SeriesId(0), 50_000.0, 0, 60);
        x.open_first_market(SeriesId(1), 50_000.0, 0, 900);
        let c = x.create_sub_account_in(p, Tenths(100_000), SeriesId(1)).unwrap();
        x.deploy_strategy_in(c, record(SeriesId(1))).unwrap();
        x.place_limit(SeriesId(1), mm, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();
        x.place_market(SeriesId(1), c, Side::Bid, Qty(10), TradeSource::Strategy).unwrap();
        // No bid to close into → orphan awaiting the 15m settlement.
        let r = x.close_sub_account(p, c).unwrap();
        assert!(!r.removed);
        assert_eq!(r.residual_position, 10);

        // The 2m series settling must not sweep a 15m orphan.
        x.resolve_and_open_next(SeriesId(0), 50_500.0, 50_500.0, 60, 60);
        assert!(x.account(&c).is_some(), "orphan survives the other series' settlement");
        assert_eq!(x.position(SeriesId(1), &c).position.0, 10);
        x.debug_assert_invariants();

        // Its own series settles YES: paid out, swept to the parent, gone.
        x.resolve_and_open_next(SeriesId(1), 50_500.0, 50_500.0, 900, 900);
        assert!(x.account(&c).is_none());
        assert!(x.sub_accounts_of(&p).is_empty());
        // Parent: START − 100_000 allocation + (95_000 cash + 10_000 payout).
        assert_eq!(x.account(&p).unwrap().usdc_balance, Tenths(START.0 + 5_000));
        x.debug_assert_invariants();
    }

    // ─────────────────────────── settlement ───────────────────────────

    fn standing(e: &Engine, who: Address) -> Option<crate::engine::LeaderboardStanding> {
        e.leaderboard_standings()
            .find(|(a, _)| **a == who)
            .map(|(_, s)| *s)
    }

    #[test]
    fn settlement_pays_child_and_rolls_pnl_up_to_parent() {
        let (p, mm) = (addr(1), addr(2));
        let mut e = user_engine(&[p]);
        e.get_or_create_account(mm); // bot
        e.open_first_market(50_000.0, 0, 60);
        let c = e.create_sub_account(p, Tenths(100_000)).unwrap();
        e.deploy_strategy(c, record(e.series())).unwrap();

        // Child buys 10 YES @ 50¢. Parent places nothing.
        e.place_limit(mm, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();
        e.place_market(c, Side::Bid, Qty(10), TradeSource::Strategy).unwrap();
        e.resolve_and_open_next(50_500.0, 50_500.0, 60, 60); // YES

        // Child paid $10 (10 × $1), so it holds 95_000 + 10_000 = 105_000.
        let child = e.account(&c).expect("child persists across rounds");
        let child_ps = e.position(&c);
        assert_eq!(child.usdc_balance, Tenths(105_000));
        assert_eq!(child_ps.position.0, 0);
        assert!(child.strategy.is_some());

        // Leaderboard: keyed by the parent, cumulative = +$5.00, and the
        // child-only activity counts as participation.
        let st = standing(&e, p).expect("parent standing");
        assert_eq!(st.cumulative_pnl_tenths, 5_000);
        assert_eq!(st.markets_played, 1);
        assert_eq!(st.markets_won, 1);
        assert!(standing(&e, c).is_none(), "children never get their own row");
    }

    #[test]
    fn allocation_alone_is_not_a_loss_on_the_leaderboard() {
        let (p, mm) = (addr(1), addr(2));
        let mut e = user_engine(&[p]);
        e.get_or_create_account(mm);
        e.open_first_market(50_000.0, 0, 60);
        let _c = e.create_sub_account(p, Tenths(300_000)).unwrap();

        // Parent trades manually and breaks even by resolution (buys 1 YES
        // @ 50¢, market resolves YES → +50¢).
        e.place_limit(mm, Side::Ask, tick(500), Qty(1), TradeSource::Bot).unwrap();
        e.place_market(p, Side::Bid, Qty(1), TradeSource::Ui).unwrap();
        e.resolve_and_open_next(50_500.0, 50_500.0, 60, 60);

        let st = standing(&e, p).unwrap();
        assert_eq!(st.cumulative_pnl_tenths, 500, "the $300 allocation must not count");
    }

    #[test]
    fn parent_with_idle_children_does_not_count_as_participating() {
        let p = addr(1);
        let mut e = user_engine(&[p]);
        e.open_first_market(50_000.0, 0, 60);
        let _c = e.create_sub_account(p, Tenths(50_000)).unwrap();
        e.resolve_and_open_next(50_500.0, 50_500.0, 60, 60);
        assert!(standing(&e, p).is_none());
    }

    #[test]
    fn settlement_sweeps_orphaned_children() {
        let (p, mm) = (addr(1), addr(2));
        let mut e = user_engine(&[p]);
        e.get_or_create_account(mm);
        e.open_first_market(50_000.0, 0, 60);
        let c = e.create_sub_account(p, Tenths(100_000)).unwrap();
        e.deploy_strategy(c, record(e.series())).unwrap();

        e.place_limit(mm, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();
        e.place_market(c, Side::Bid, Qty(10), TradeSource::Strategy).unwrap();
        let r = e.close_sub_account(p, c).unwrap();
        assert!(!r.removed);

        e.resolve_and_open_next(49_000.0, 49_000.0, 60, 60); // NO: child's YES worthless

        assert!(e.account(&c).is_none(), "orphan removed at resolution");
        assert!(e.sub_accounts_of(&p).is_empty());
        assert!(!e.is_user_account(&c));
        // Parent got the child's remaining 95_000 back: 900_000 + 95_000.
        assert_eq!(e.account(&p).unwrap().usdc_balance, Tenths(995_000));
        let st = standing(&e, p).unwrap();
        assert_eq!(st.cumulative_pnl_tenths, -5_000);
        // (No conservation check here: the bot's winnings are wiped by the
        // bot balance reset at resolution, by design.)
    }
}
