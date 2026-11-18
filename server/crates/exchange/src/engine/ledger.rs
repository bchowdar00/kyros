//! The account ledger: cash, identity, lineage and leaderboard standings.
//!
//! One ledger is shared by every market series. It owns everything that is
//! *per account* rather than per series: free and reserved cash, the
//! username/avatar, the user registry (which addresses are humans), the
//! parent → sub-account tree, and the per-user leaderboard standings.
//! Per-series exposure lives in each series' engine as
//! [`super::positions::PositionState`].
//!
//! Every money method is a single critical section over one or two
//! accounts and either succeeds completely or leaves the ledger untouched.
//! That atomicity is what makes it safe for several series to draw on the
//! same balance: a reservation either moves the full amount from
//! `usdc_balance` to `locked` or fails with `InsufficientBalance`.

use std::collections::{HashMap, HashSet};

use super::OrderError;
use super::accounts::AccountStore;
use super::model::{Account, LeaderboardStanding};
use super::sub_accounts::SubAccountError;
use super::{MAX_SUB_ACCOUNTS_PER_USER, MIN_SUB_ACCOUNT_ALLOCATION};
use crate::types::{Address, MarketId, SeriesId, Tenths};

#[derive(Debug)]
pub struct Ledger {
    pub(super) accounts: AccountStore,
    /// Addresses of human user accounts (top-level users and their
    /// strategy sub-accounts). Settled at resolution (receive payouts).
    /// All other accounts are bots and get balance-reset instead.
    pub(super) user_accounts: HashSet<Address>,
    /// Parent address → child sub-account addresses, in creation order.
    /// Mirrors `Account.parent`; kept as an index so listing a user's
    /// strategies is O(children) rather than a scan of every account.
    pub(super) sub_accounts: HashMap<Address, Vec<Address>>,
    /// Per-user cumulative PnL standings, updated once per round at
    /// resolution. Keyed by *root* user address; only registered users
    /// who placed at least one order in a round ever get an entry.
    pub(super) leaderboard: HashMap<Address, LeaderboardStanding>,
    starting_balance: Tenths,
}

impl Ledger {
    pub fn new(starting_balance: Tenths) -> Self {
        Self {
            accounts: AccountStore::new(),
            user_accounts: HashSet::new(),
            sub_accounts: HashMap::new(),
            leaderboard: HashMap::new(),
            starting_balance,
        }
    }

    // ────────────────────────────── money ──────────────────────────────

    /// Move `amt` from free cash into `locked`. Fails, without touching
    /// the account, if free cash is short.
    pub fn reserve(&mut self, address: Address, amt: Tenths) -> Result<(), OrderError> {
        let acct = self
            .accounts
            .get_mut(&address)
            .ok_or(OrderError::UnknownAccount)?;
        if acct.usdc_balance < amt {
            return Err(OrderError::InsufficientBalance {
                required: amt,
                available: acct.usdc_balance,
            });
        }
        acct.usdc_balance -= amt;
        acct.locked += amt;
        Ok(())
    }

    /// Return `amt` of reserved cash to free cash.
    pub fn release(&mut self, address: Address, amt: Tenths) {
        let acct = self
            .accounts
            .get_mut(&address)
            .expect("release on unknown account");
        debug_assert!(
            acct.locked >= amt,
            "release {} exceeds locked {} on {address}",
            amt.0,
            acct.locked.0,
        );
        acct.locked -= amt;
        acct.usdc_balance += amt;
    }

    /// Both sides of one match in one call: refund each side's reserved
    /// lock for the matched units, then apply the signed cash flow of the
    /// fill itself.
    pub fn apply_fill_cash(
        &mut self,
        buyer: Address,
        buyer_refund: Tenths,
        buyer_cash: Tenths,
        seller: Address,
        seller_refund: Tenths,
        seller_cash: Tenths,
    ) {
        for (who, refund, cash) in [(buyer, buyer_refund, buyer_cash), (seller, seller_refund, seller_cash)]
        {
            let acct = self.accounts.get_mut(&who).expect("fill party exists");
            acct.locked -= refund;
            acct.usdc_balance += refund;
            acct.usdc_balance += cash;
        }
    }

    /// Settlement payout (or any other unconditional credit).
    pub fn credit(&mut self, address: Address, amt: Tenths) {
        if let Some(acct) = self.accounts.get_mut(&address) {
            acct.usdc_balance += amt;
        }
    }

    /// Free-cash move between two accounts, bounded by the source balance.
    pub fn transfer(&mut self, from: Address, to: Address, amt: Tenths) -> Result<(), SubAccountError> {
        let available = self
            .accounts
            .get(&from)
            .ok_or(SubAccountError::UnknownSubAccount)?
            .usdc_balance;
        if available < amt {
            return Err(SubAccountError::InsufficientBalance {
                required: amt,
                available,
            });
        }
        if self.accounts.get(&to).is_none() {
            return Err(SubAccountError::UnknownSubAccount);
        }
        self.accounts.get_mut(&from).expect("checked").usdc_balance -= amt;
        self.accounts.get_mut(&to).expect("checked").usdc_balance += amt;
        Ok(())
    }

    /// Bot bankroll reset: `balance = reset_balance.unwrap_or(starting)`.
    /// `locked` is left alone — the caller has already cancelled the
    /// bot's resting orders in its home series, and a bot never rests
    /// anywhere else.
    pub fn reset_bot(&mut self, address: Address) {
        let starting = self.starting_balance;
        if let Some(acct) = self.accounts.get_mut(&address) {
            acct.usdc_balance = acct.reset_balance.unwrap_or(starting);
        }
    }

    // ───────────────────────────── registry ────────────────────────────

    pub fn account(&self, address: &Address) -> Option<&Account> {
        self.accounts.get(address)
    }

    pub fn account_mut(&mut self, address: &Address) -> Option<&mut Account> {
        self.accounts.get_mut(address)
    }

    /// Read-only iterator over every account. Order is unspecified.
    pub fn accounts_iter(&self) -> impl Iterator<Item = &Account> {
        self.accounts.iter()
    }

    /// Returns the account at `address`, creating it (seeded with the
    /// starting balance, no home series) if it doesn't yet exist.
    pub fn get_or_create(&mut self, address: Address) -> &mut Account {
        self.accounts.get_or_create(address, self.starting_balance)
    }

    /// Startup-only: create a fresh bot account at `address` with a custom
    /// bankroll and identity (username/color/emoji), bound to `home`. If
    /// the account already exists its balance is overwritten — callers
    /// should only invoke this during initialization, before any orders
    /// flow.
    pub fn seed_bot_account(
        &mut self,
        address: Address,
        balance: Tenths,
        username: String,
        color: Option<String>,
        emoji: Option<String>,
        home: SeriesId,
    ) {
        let acct = self.accounts.get_or_create(address, balance);
        acct.usdc_balance = balance;
        acct.reset_balance = Some(balance);
        acct.username = Some(username);
        acct.home_series = Some(home);
        if color.is_some() {
            acct.color = color;
        }
        if emoji.is_some() {
            acct.emoji = emoji;
        }
    }

    pub fn register_user(&mut self, address: Address) {
        self.user_accounts.insert(address);
    }

    pub fn is_user_account(&self, address: &Address) -> bool {
        self.user_accounts.contains(address)
    }

    /// The top-level identity behind `address`: its parent when it is a
    /// sub-account, otherwise itself.
    pub fn root_of(&self, address: &Address) -> Address {
        self.accounts
            .get(address)
            .and_then(|a| a.parent)
            .unwrap_or(*address)
    }

    /// Child sub-account addresses of `parent`, in creation order.
    pub fn sub_accounts_of(&self, parent: &Address) -> &[Address] {
        self.sub_accounts
            .get(parent)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Create `child` as a sub-account of `parent`, funded by moving
    /// `allocation` of the parent's free cash and bound to `home`. All
    /// validation happens before any money moves.
    pub fn create_sub_account_at(
        &mut self,
        parent: Address,
        child: Address,
        allocation: Tenths,
        home: SeriesId,
    ) -> Result<(), SubAccountError> {
        assert!(
            self.accounts.get(&child).is_none(),
            "create_sub_account_at: address {child} already has an account",
        );
        let parent_acct = self
            .accounts
            .get(&parent)
            .ok_or(SubAccountError::UnknownParent)?;
        if parent_acct.is_sub_account() {
            return Err(SubAccountError::ParentIsSubAccount);
        }
        if !self.user_accounts.contains(&parent) {
            return Err(SubAccountError::ParentNotUser);
        }
        if allocation < MIN_SUB_ACCOUNT_ALLOCATION {
            return Err(SubAccountError::AllocationTooSmall {
                min: MIN_SUB_ACCOUNT_ALLOCATION,
            });
        }
        if parent_acct.usdc_balance < allocation {
            return Err(SubAccountError::InsufficientBalance {
                required: allocation,
                available: parent_acct.usdc_balance,
            });
        }
        if self.sub_accounts_of(&parent).len() >= MAX_SUB_ACCOUNTS_PER_USER {
            return Err(SubAccountError::TooManySubAccounts {
                max: MAX_SUB_ACCOUNTS_PER_USER,
            });
        }

        let mut acct = Account::new(child, allocation);
        acct.parent = Some(parent);
        acct.net_contributed = Some(allocation);
        acct.home_series = Some(home);
        self.accounts.insert(acct);
        self.accounts
            .get_mut(&parent)
            .expect("validated above")
            .usdc_balance -= allocation;
        self.user_accounts.insert(child);
        self.sub_accounts.entry(parent).or_default().push(child);
        Ok(())
    }

    /// Delete `child` and sweep its free cash to the parent. The caller
    /// must have flattened its positions and cancelled its orders in
    /// every series first. Returns the swept amount.
    pub fn remove_sub_account(&mut self, child: Address) -> Tenths {
        let acct = self.accounts.remove(&child).expect("child exists");
        debug_assert_eq!(acct.locked.0, 0, "remove_sub_account with locked cash");
        let parent = acct.parent.expect("only sub-accounts are removed");
        let swept = acct.usdc_balance;
        if let Some(p) = self.accounts.get_mut(&parent) {
            p.usdc_balance += swept;
        }
        self.user_accounts.remove(&child);
        self.leaderboard.remove(&child);
        if let Some(children) = self.sub_accounts.get_mut(&parent) {
            children.retain(|c| *c != child);
            if children.is_empty() {
                self.sub_accounts.remove(&parent);
            }
        }
        swept
    }

    /// A random address with no account yet.
    pub fn fresh_address(&self) -> Address {
        loop {
            let candidate = Address(rand::random::<[u8; 20]>());
            if self.accounts.get(&candidate).is_none() {
                return candidate;
            }
        }
    }

    /// Cash every user account is seeded with — the only "deposit" a
    /// top-level user ever makes, so it anchors lifetime PnL.
    pub fn starting_balance(&self) -> Tenths {
        self.starting_balance
    }

    // ──────────────────────────── home series ──────────────────────────

    pub fn home_series(&self, address: &Address) -> Option<SeriesId> {
        self.accounts.get(address).and_then(|a| a.home_series)
    }

    /// Ok if the account has no home (a human's main account) or its home
    /// is `series`; otherwise the order is for the wrong series.
    pub fn assert_may_trade(&self, address: &Address, series: SeriesId) -> Result<(), OrderError> {
        match self.home_series(address) {
            Some(home) if home != series => Err(OrderError::SeriesMismatch {
                home,
                requested: series,
            }),
            _ => Ok(()),
        }
    }

    // ──────────────────────────── leaderboard ──────────────────────────

    /// Fold one resolved round into `root`'s standing. `family_pnl` is the
    /// realized PnL of the root plus every child in that series for the
    /// round, settlement included. Non-participants are skipped so
    /// `markets_played` stays a true count.
    pub fn record_round(
        &mut self,
        root: Address,
        series: SeriesId,
        market_id: MarketId,
        family_pnl_tenths: i64,
        participated: bool,
    ) {
        if !participated {
            return;
        }
        let st = self.leaderboard.entry(root).or_default();
        st.cumulative_pnl_tenths += family_pnl_tenths;
        st.last_market_pnl_tenths = family_pnl_tenths;
        st.markets_played += 1;
        if family_pnl_tenths > 0 {
            st.markets_won += 1;
        }
        st.last_market_id = market_id.0;
        st.last_series = series;
    }

    pub fn leaderboard_standings(&self) -> impl Iterator<Item = (&Address, &LeaderboardStanding)> {
        self.leaderboard.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: Tenths = Tenths(1_000_000);

    fn a(byte: u8) -> Address {
        Address([byte; 20])
    }

    #[test]
    fn reserve_fails_atomically_without_partial_move() {
        let mut l = Ledger::new(START);
        l.get_or_create(a(1));
        let err = l.reserve(a(1), Tenths(START.0 + 1)).unwrap_err();
        assert_eq!(
            err,
            OrderError::InsufficientBalance {
                required: Tenths(START.0 + 1),
                available: START,
            }
        );
        let acct = l.account(&a(1)).unwrap();
        assert_eq!(acct.usdc_balance, START);
        assert_eq!(acct.locked, Tenths::ZERO);

        l.reserve(a(1), START).unwrap();
        let acct = l.account(&a(1)).unwrap();
        assert_eq!(acct.usdc_balance, Tenths::ZERO);
        assert_eq!(acct.locked, START);
        assert!(l.reserve(a(1), Tenths(1)).is_err());
        assert_eq!(l.reserve(a(9), Tenths(1)), Err(OrderError::UnknownAccount));
    }

    #[test]
    fn release_after_partial_fill_returns_exact_remainder() {
        let mut l = Ledger::new(START);
        l.get_or_create(a(1));
        l.get_or_create(a(2));
        // Reserve 10 × 400 for a bid; 4 units fill at 400, refund 1600
        // against a cash draw of 1600; the remaining 6 units' lock is
        // released on cancel.
        l.reserve(a(1), Tenths(4000)).unwrap();
        l.apply_fill_cash(a(1), Tenths(1600), Tenths(-1600), a(2), Tenths::ZERO, Tenths(1600));
        assert_eq!(l.account(&a(1)).unwrap().locked, Tenths(2400));
        l.release(a(1), Tenths(2400));
        let acct = l.account(&a(1)).unwrap();
        assert_eq!(acct.locked, Tenths::ZERO);
        assert_eq!(acct.usdc_balance, Tenths(START.0 - 1600));
        assert_eq!(l.account(&a(2)).unwrap().usdc_balance, Tenths(START.0 + 1600));
    }

    #[test]
    fn record_round_accumulates_and_counts_wins() {
        let mut l = Ledger::new(START);
        let root = a(1);
        l.record_round(root, SeriesId(0), MarketId(1), 2500, true);
        l.record_round(root, SeriesId(1), MarketId(2), 0, false);
        l.record_round(root, SeriesId(1), MarketId(3), -5000, true);
        let st = l.leaderboard_standings().find(|(k, _)| **k == root).unwrap().1;
        assert_eq!(st.cumulative_pnl_tenths, -2500);
        assert_eq!(st.last_market_pnl_tenths, -5000);
        assert_eq!(st.markets_played, 2);
        assert_eq!(st.markets_won, 1);
        assert_eq!(st.last_market_id, 3);
        assert_eq!(st.last_series, SeriesId(1));
    }

    #[test]
    fn transfer_is_bounded_by_source_balance() {
        let mut l = Ledger::new(START);
        l.get_or_create(a(1));
        l.get_or_create(a(2));
        assert!(l.transfer(a(1), a(2), Tenths(START.0 + 1)).is_err());
        l.transfer(a(1), a(2), Tenths(1000)).unwrap();
        assert_eq!(l.account(&a(1)).unwrap().usdc_balance, Tenths(START.0 - 1000));
        assert_eq!(l.account(&a(2)).unwrap().usdc_balance, Tenths(START.0 + 1000));
    }
}
