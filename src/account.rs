//! The per-client account projection and its balance operations.
//!
//! Fields are private; every mutation goes through a method that upholds the
//! invariants — checked arithmetic, `held >= 0`, and a derived `total` that
//! can't desync. `available`/`total` may legitimately be negative (a realized
//! chargeback loss), so they are never clamped.

use crate::types::Amount;

/// Why a balance operation could not be applied. Distinguishing these lets the
/// engine report an accurate reason (insufficient funds vs. range overflow).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalanceError {
    /// A withdrawal exceeds available funds.
    InsufficientFunds,
    /// The operation would overflow the `Decimal` range.
    Overflow,
}

/// A client's balances plus its frozen flag. `total` is derived, never stored.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Account {
    available: Amount,
    held: Amount,
    locked: bool,
}

impl Account {
    pub fn available(&self) -> Amount {
        self.available
    }

    pub fn held(&self) -> Amount {
        self.held
    }

    /// Derived as `available + held` — never stored, so it can't fall out of sync.
    pub fn total(&self) -> Amount {
        self.available + self.held
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Credit available funds (deposit).
    pub fn credit(&mut self, amount: Amount) -> Result<(), BalanceError> {
        self.available = self
            .available
            .checked_add(amount)
            .ok_or(BalanceError::Overflow)?;
        Ok(())
    }

    /// Debit available funds (withdrawal). Fails (no change) if funds are
    /// insufficient — a withdrawal can never drive the balance negative.
    pub fn debit(&mut self, amount: Amount) -> Result<(), BalanceError> {
        if self.available < amount {
            return Err(BalanceError::InsufficientFunds);
        }
        self.available = self
            .available
            .checked_sub(amount)
            .ok_or(BalanceError::Overflow)?;
        Ok(())
    }

    /// Move funds from available to held (dispute). May drive available
    /// negative — that is intentional and not an error. Computes both new
    /// balances before committing, so a failure leaves the account untouched.
    pub fn hold(&mut self, amount: Amount) -> Result<(), BalanceError> {
        let available = self
            .available
            .checked_sub(amount)
            .ok_or(BalanceError::Overflow)?;
        let held = self
            .held
            .checked_add(amount)
            .ok_or(BalanceError::Overflow)?;
        self.available = available;
        self.held = held;
        Ok(())
    }

    /// Move funds from held back to available (resolve).
    pub fn release(&mut self, amount: Amount) -> Result<(), BalanceError> {
        let held = self
            .held
            .checked_sub(amount)
            .ok_or(BalanceError::Overflow)?;
        let available = self
            .available
            .checked_add(amount)
            .ok_or(BalanceError::Overflow)?;
        self.held = held;
        self.available = available;
        Ok(())
    }

    /// Remove held funds and freeze the account (chargeback). `total` drops by
    /// `amount` because `held` does.
    pub fn chargeback(&mut self, amount: Amount) -> Result<(), BalanceError> {
        self.held = self
            .held
            .checked_sub(amount)
            .ok_or(BalanceError::Overflow)?;
        self.locked = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amt(s: &str) -> Amount {
        s.parse().unwrap()
    }

    #[test]
    fn total_is_available_plus_held() {
        let mut a = Account::default();
        a.credit(amt("10")).unwrap();
        a.hold(amt("3")).unwrap();
        assert_eq!(a.available(), amt("7"));
        assert_eq!(a.held(), amt("3"));
        assert_eq!(a.total(), amt("10"));
    }

    #[test]
    fn debit_rejects_insufficient_funds() {
        let mut a = Account::default();
        a.credit(amt("5")).unwrap();
        assert_eq!(a.debit(amt("6")), Err(BalanceError::InsufficientFunds));
        assert_eq!(a.available(), amt("5"));
    }

    #[test]
    fn hold_may_go_negative() {
        let mut a = Account::default();
        a.credit(amt("10")).unwrap();
        a.debit(amt("10")).unwrap();
        a.hold(amt("10")).unwrap();
        assert_eq!(a.available(), amt("-10"));
        assert_eq!(a.held(), amt("10"));
        assert_eq!(a.total(), amt("0"));
    }

    #[test]
    fn chargeback_removes_held_and_locks() {
        let mut a = Account::default();
        a.credit(amt("10")).unwrap();
        a.hold(amt("10")).unwrap();
        a.chargeback(amt("10")).unwrap();
        assert_eq!(a.total(), amt("0"));
        assert!(a.is_locked());
    }
}
