use crate::{ids::CurrencyId, numeric::Amount};
use std::collections::{HashMap, HashSet};

/// Conservation invariant for an exchange's books.
///
/// In every currency, at every point in time:
///   sum of all user balances == sum of (deposits − withdrawals)
///
/// "Internal" credits/debits are transfers inside the exchange ledger
/// (one user pays another in a trade, fee goes to an exchange revenue account).
/// They must net to zero on their own — they don't change the total amount of
/// money the exchange holds.
///
/// "External" in/out is money crossing the exchange boundary (real deposits
/// and withdrawals). The total external is what the exchange owes its users.
///
/// `check_balanced` returns Ok iff every currency satisfies the invariant.
/// Used by property tests to prove no input sequence can mint or destroy money.
pub trait ConservationLedger {
    fn record_credit(&mut self, currency: CurrencyId, amount: Amount);
    fn record_debit(&mut self, currency: CurrencyId, amount: Amount);
    fn record_external_in(&mut self, currency: CurrencyId, amount: Amount);
    fn record_external_out(&mut self, currency: CurrencyId, amount: Amount);
    fn check_balanced(&self) -> Result<(), Vec<ImbalanceReport>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImbalanceReport {
    pub currency: CurrencyId,
    pub internal_net: i128,
    pub external_net: i128,
    pub delta: i128,
}

#[derive(Default)]
pub struct InMemoryLedger {
    internal: HashMap<CurrencyId, i128>,
    external: HashMap<CurrencyId, i128>,
}

impl ConservationLedger for InMemoryLedger {
    fn record_credit(&mut self, currency: CurrencyId, amount: Amount) {
        *self.internal.entry(currency).or_insert(0) += amount.raw() as i128;
    }

    fn record_debit(&mut self, currency: CurrencyId, amount: Amount) {
        *self.internal.entry(currency).or_insert(0) -= amount.raw() as i128;
    }

    fn record_external_in(&mut self, currency: CurrencyId, amount: Amount) {
        *self.external.entry(currency).or_insert(0) += amount.raw() as i128;
    }

    fn record_external_out(&mut self, currency: CurrencyId, amount: Amount) {
        *self.external.entry(currency).or_insert(0) -= amount.raw() as i128;
    }

    fn check_balanced(&self) -> Result<(), Vec<ImbalanceReport>> {
        let mut reports = Vec::new();
        let all: HashSet<CurrencyId> = self
            .internal
            .keys()
            .chain(self.external.keys())
            .copied()
            .collect();
        for c in all {
            let internal = *self.internal.get(&c).unwrap_or(&0);
            let external = *self.external.get(&c).unwrap_or(&0);
            let delta = internal - external;
            if delta != 0 {
                reports.push(ImbalanceReport {
                    currency: c,
                    internal_net: internal,
                    external_net: external,
                    delta,
                });
            }
        }
        if reports.is_empty() {
            Ok(())
        } else {
            Err(reports)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deposit_credits_balance() {
        let mut l = InMemoryLedger::default();
        let usd = CurrencyId(1);
        l.record_external_in(usd, Amount(1000));
        l.record_credit(usd, Amount(1000));
        assert!(l.check_balanced().is_ok());
    }

    #[test]
    fn credit_without_external_breaks_balance() {
        let mut l = InMemoryLedger::default();
        let usd = CurrencyId(1);
        l.record_credit(usd, Amount(1000));
        let reports = l.check_balanced().unwrap_err();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].delta, 1000);
    }

    #[test]
    fn internal_trade_net_zero_preserves_balance() {
        let mut l = InMemoryLedger::default();
        let usd = CurrencyId(1);
        let btc = CurrencyId(2);

        l.record_external_in(usd, Amount(1000));
        l.record_credit(usd, Amount(1000));
        l.record_external_in(btc, Amount(1_000_000));
        l.record_credit(btc, Amount(1_000_000));

        // Alice (had USD) buys 500k satoshi from Bob (had BTC) at some price.
        l.record_debit(usd, Amount(1000));
        l.record_credit(btc, Amount(500_000));
        l.record_credit(usd, Amount(1000));
        l.record_debit(btc, Amount(500_000));

        assert!(l.check_balanced().is_ok());
    }

    #[test]
    fn fee_to_exchange_account_preserves_balance() {
        let mut l = InMemoryLedger::default();
        let usd = CurrencyId(1);

        l.record_external_in(usd, Amount(1000));
        l.record_credit(usd, Amount(1000));

        // 10 USD fee from user → exchange revenue (both are internal accounts).
        l.record_debit(usd, Amount(10));
        l.record_credit(usd, Amount(10));

        assert!(l.check_balanced().is_ok());
    }

    #[test]
    fn withdrawal_paired_with_debit_preserves_balance() {
        let mut l = InMemoryLedger::default();
        let usd = CurrencyId(1);
        l.record_external_in(usd, Amount(1000));
        l.record_credit(usd, Amount(1000));

        l.record_debit(usd, Amount(400));
        l.record_external_out(usd, Amount(400));

        assert!(l.check_balanced().is_ok());
    }
}
