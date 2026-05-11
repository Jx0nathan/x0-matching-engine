use ahash::AHashMap;
use serde::{Deserialize, Serialize};

use me_types::{Amount, CurrencyId, Price, Size, SymbolId, UserId};

/// User balance ledger. `free` is spendable; `holds` is reserved against an
/// open order. The pair (free, held) sums to the user's actual claim against
/// the exchange (excluding open derivative positions, which are tracked
/// separately in `positions`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserAccount {
    pub user_id: UserId,
    pub balances: AHashMap<CurrencyId, Amount>,
    pub holds: AHashMap<CurrencyId, Amount>,
    /// Derivative positions, isolated margin model: each `Position` owns its
    /// own `margin_locked` which is deducted from `holds` (in quote currency).
    pub positions: AHashMap<SymbolId, Position>,
    pub is_suspended: bool,
}

/// A derivative position. Size is signed: positive = long, negative = short,
/// zero = flat. `entry_price` is the size-weighted average entry; meaningless
/// when size = 0. `margin_locked` is the user's initial-margin reservation,
/// isolated to this position; its currency is recorded so conservation totals
/// can filter correctly across multi-currency portfolios.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub size: Size,
    pub entry_price: Price,
    pub margin_locked: Amount,
    pub margin_currency: CurrencyId,
}

impl Position {
    pub fn is_flat(&self) -> bool {
        self.size.is_zero()
    }
    pub fn is_long(&self) -> bool {
        self.size.raw() > 0
    }
    pub fn is_short(&self) -> bool {
        self.size.raw() < 0
    }
    pub fn abs_size(&self) -> Size {
        Size(self.size.raw().abs())
    }
}

impl UserAccount {
    pub fn new(user_id: UserId) -> Self {
        Self {
            user_id,
            balances: AHashMap::new(),
            holds: AHashMap::new(),
            positions: AHashMap::new(),
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

    pub fn position(&self, symbol: SymbolId) -> Option<&Position> {
        self.positions.get(&symbol)
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

    pub(crate) fn position_mut_or_insert(&mut self, symbol: SymbolId) -> &mut Position {
        self.positions.entry(symbol).or_default()
    }
}
