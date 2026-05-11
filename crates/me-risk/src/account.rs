use ahash::AHashMap;
use serde::{Deserialize, Serialize};

use me_types::{Amount, CurrencyId, UserId};

/// User balance ledger. `free` is spendable; `holds` is reserved against an
/// open order. The pair (free, held) sums to the user's actual claim against
/// the exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserAccount {
    pub user_id: UserId,
    pub balances: AHashMap<CurrencyId, Amount>,
    pub holds: AHashMap<CurrencyId, Amount>,
    pub is_suspended: bool,
}

impl UserAccount {
    pub fn new(user_id: UserId) -> Self {
        Self {
            user_id,
            balances: AHashMap::new(),
            holds: AHashMap::new(),
            is_suspended: false,
        }
    }

    pub fn free(&self, currency: CurrencyId) -> Amount {
        self.balances.get(&currency).copied().unwrap_or(Amount::ZERO)
    }

    pub fn held(&self, currency: CurrencyId) -> Amount {
        self.holds.get(&currency).copied().unwrap_or(Amount::ZERO)
    }

    pub fn total(&self, currency: CurrencyId) -> Amount {
        self.free(currency) + self.held(currency)
    }

    pub(crate) fn credit_free(&mut self, currency: CurrencyId, amount: Amount) {
        if amount.is_zero() {
            return;
        }
        let entry = self.balances.entry(currency).or_insert(Amount::ZERO);
        *entry += amount;
    }

    pub(crate) fn debit_free(&mut self, currency: CurrencyId, amount: Amount) -> bool {
        if amount.is_zero() {
            return true;
        }
        let entry = self.balances.entry(currency).or_insert(Amount::ZERO);
        if entry.raw() < amount.raw() {
            return false;
        }
        *entry -= amount;
        true
    }

    pub(crate) fn add_to_hold(&mut self, currency: CurrencyId, amount: Amount) {
        if amount.is_zero() {
            return;
        }
        let entry = self.holds.entry(currency).or_insert(Amount::ZERO);
        *entry += amount;
    }

    pub(crate) fn sub_from_hold(&mut self, currency: CurrencyId, amount: Amount) {
        if amount.is_zero() {
            return;
        }
        let entry = self.holds.entry(currency).or_insert(Amount::ZERO);
        *entry -= amount;
    }
}
