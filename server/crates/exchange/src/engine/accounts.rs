//! In-memory account store keyed by Ethereum address.
//!
//! Owned by the single-writer engine — no internal locking. Holds free USDC
//! balance, locked collateral, optional username, and the user's signed
//! per-account identity. Per-series positions live in the engine.

use std::collections::HashMap;
use std::fmt;

use super::model::Account;
use crate::types::{Address, Tenths};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountError {
    UnknownAddress,
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountError::UnknownAddress => f.write_str("address has no account"),
        }
    }
}

impl std::error::Error for AccountError {}

#[derive(Debug, Default)]
pub struct AccountStore {
    by_address: HashMap<Address, Account>,
}

impl AccountStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, address: &Address) -> Option<&Account> {
        self.by_address.get(address)
    }

    pub fn get_mut(&mut self, address: &Address) -> Option<&mut Account> {
        self.by_address.get_mut(address)
    }

    /// Returns the existing account if present, otherwise creates a new one
    /// seeded with `starting_balance`. The starting balance is ignored on
    /// the second and subsequent calls — balances persist across rounds.
    pub fn get_or_create(&mut self, address: Address, starting_balance: Tenths) -> &mut Account {
        self.by_address
            .entry(address)
            .or_insert_with(|| Account::new(address, starting_balance))
    }

    /// Insert a fully-built account. Replaces any existing entry at the
    /// same address — callers must check first if that matters.
    pub fn insert(&mut self, account: Account) {
        self.by_address.insert(account.address, account);
    }

    /// Remove and return the account at `address`, if any.
    pub fn remove(&mut self, address: &Address) -> Option<Account> {
        self.by_address.remove(address)
    }

    pub fn set_username(&mut self, address: &Address, username: String) -> Result<(), AccountError> {
        let acct = self.by_address.get_mut(address).ok_or(AccountError::UnknownAddress)?;
        acct.username = Some(username);
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Account> {
        self.by_address.values()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Account> {
        self.by_address.values_mut()
    }

    pub fn len(&self) -> usize {
        self.by_address.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_address.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0x1111111111111111111111111111111111111111";
    const B: &str = "0x2222222222222222222222222222222222222222";

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    #[test]
    fn get_or_create_seeds_new_account() {
        let mut s = AccountStore::new();
        let a = s.get_or_create(addr(A), Tenths(1_000_000));
        assert_eq!(a.address, addr(A));
        assert_eq!(a.usdc_balance, Tenths(1_000_000));
        assert_eq!(a.locked, Tenths::ZERO);
        assert!(a.username.is_none());
    }

    #[test]
    fn get_or_create_is_idempotent() {
        let mut s = AccountStore::new();
        s.get_or_create(addr(A), Tenths(1_000_000)).usdc_balance = Tenths(500_000);
        // Second call with a different seed must return the existing account untouched.
        let a = s.get_or_create(addr(A), Tenths(999_999));
        assert_eq!(a.usdc_balance, Tenths(500_000));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn set_username_on_existing() {
        let mut s = AccountStore::new();
        s.get_or_create(addr(A), Tenths(1_000_000));
        s.set_username(&addr(A), "alice".to_string()).unwrap();
        assert_eq!(s.get(&addr(A)).unwrap().username.as_deref(), Some("alice"));
    }

    #[test]
    fn set_username_on_missing_errors() {
        let mut s = AccountStore::new();
        assert_eq!(
            s.set_username(&addr(B), "ghost".to_string()),
            Err(AccountError::UnknownAddress),
        );
    }

    #[test]
    fn iter_yields_all_accounts() {
        let mut s = AccountStore::new();
        s.get_or_create(addr(A), Tenths(1_000_000));
        s.get_or_create(addr(B), Tenths(1_000_000));
        let mut seen: Vec<Address> = s.iter().map(|a| a.address).collect();
        seen.sort();
        assert_eq!(seen, vec![addr(A), addr(B)]);
    }

    #[test]
    fn get_returns_none_for_missing() {
        let s = AccountStore::new();
        assert!(s.get(&addr(A)).is_none());
        assert!(s.is_empty());
    }
}
