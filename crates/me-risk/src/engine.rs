use ahash::AHashMap;
use serde::{Deserialize, Serialize};

use me_types::{
    AdjustBalance, Amount, Bps, CurrencyId, OrderId, PlaceOrder, Price, RejectReason, Side, Size,
    SymbolId, SymbolKindParams, SymbolSpec, Trade, UserId,
};

use crate::account::UserAccount;

/// Sentinel UserId reserved for the exchange's fee-revenue account.
/// All fees collected in any currency are credited here so the conservation
/// invariant (sum of internal balances = sum of external in/out) holds.
pub const EXCHANGE_ACCOUNT: UserId = UserId(0);

/// A reservation of funds against an open order. Indexed by OrderId.
///
/// For a Bid: the hold is in quote currency, sized at the *reserve* price
/// (typically the limit price) so a worst-case fill can be covered. Slack
/// between reserve and actual fill is refunded back to free at settlement.
///
/// For an Ask: the hold is in base currency, 1:1 with the order size.
/// Fees come out of the sale proceeds (in quote), so no separate fee hold.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Hold {
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    pub order_id: OrderId,
    pub side: Side,
    pub currency: CurrencyId,
    pub remaining_size: Size,
    pub remaining_amount: Amount,
    /// Bid only — the price used at pre-check (for slack-refund computation).
    pub bid_reserve_price: Option<Price>,
    /// Bps used at pre-check (always taker bps — worst case).
    pub pre_check_fee_bps: Bps,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RiskEngine {
    accounts: AHashMap<UserId, UserAccount>,
    /// One hold per resting/pending order. Removed when remaining_size hits 0
    /// (`apply_trade`) or on explicit `release_hold` (cancel / IOC drop / FOK reject).
    holds: AHashMap<OrderId, Hold>,
}

impl RiskEngine {
    pub fn new() -> Self {
        let mut me = Self::default();
        me.accounts.insert(EXCHANGE_ACCOUNT, UserAccount::new(EXCHANGE_ACCOUNT));
        me
    }

    pub fn add_user(&mut self, user_id: UserId) -> Result<(), RejectReason> {
        if user_id == EXCHANGE_ACCOUNT {
            return Err(RejectReason::UnsupportedCommand);
        }
        if self.accounts.contains_key(&user_id) {
            return Err(RejectReason::UnsupportedCommand);
        }
        self.accounts.insert(user_id, UserAccount::new(user_id));
        Ok(())
    }

    pub fn suspend(&mut self, user_id: UserId) -> Result<(), RejectReason> {
        let acct = self.accounts.get_mut(&user_id).ok_or(RejectReason::UnknownUser)?;
        acct.is_suspended = true;
        Ok(())
    }

    pub fn resume(&mut self, user_id: UserId) -> Result<(), RejectReason> {
        let acct = self.accounts.get_mut(&user_id).ok_or(RejectReason::UnknownUser)?;
        acct.is_suspended = false;
        Ok(())
    }

    /// Apply an external balance adjustment (deposit if positive, withdrawal if negative).
    /// Withdrawals fail with InsufficientFunds if the user doesn't have enough free balance.
    pub fn adjust_balance(&mut self, cmd: &AdjustBalance) -> Result<(), RejectReason> {
        let acct = self.accounts.get_mut(&cmd.user_id).ok_or(RejectReason::UnknownUser)?;
        if cmd.delta.raw() >= 0 {
            acct.credit_free(cmd.currency_id, cmd.delta);
            Ok(())
        } else {
            let amount = Amount(-cmd.delta.raw());
            if !acct.debit_free(cmd.currency_id, amount) {
                return Err(RejectReason::InsufficientFunds);
            }
            Ok(())
        }
    }

    pub fn account(&self, user_id: UserId) -> Option<&UserAccount> {
        self.accounts.get(&user_id)
    }

    pub fn hold(&self, order_id: OrderId) -> Option<&Hold> {
        self.holds.get(&order_id)
    }

    /// Sum of all users' (free + held) for the given currency, including the
    /// exchange revenue account. This is what conservation tests inspect.
    pub fn total_internal(&self, currency: CurrencyId) -> i128 {
        self.accounts
            .values()
            .map(|a| a.free(currency).raw() as i128 + a.held(currency).raw() as i128)
            .sum()
    }

    /// Pre-check a PlaceOrder: validate, compute and stash a Hold, and move
    /// the corresponding amount from free → held.
    ///
    /// On success, a Hold is registered under `order_id`. The caller (me-core)
    /// MUST eventually drain it via `apply_trade` (fully filled) or
    /// `release_hold` (cancel / drop / reject). Leaking a Hold is a bug.
    pub fn pre_check_place(
        &mut self,
        cmd: &PlaceOrder,
        spec: &SymbolSpec,
        order_id: OrderId,
    ) -> Result<(), RejectReason> {
        if spec.is_suspended {
            return Err(RejectReason::SymbolSuspended);
        }
        // Spot only for M2.
        if !matches!(spec.kind_params, SymbolKindParams::Spot) {
            return Err(RejectReason::UnsupportedCommand);
        }
        if !cmd.size.is_positive() {
            return Err(RejectReason::SizeBelowMinimum);
        }
        if cmd.size.raw() < spec.min_order_size.raw() {
            return Err(RejectReason::SizeBelowMinimum);
        }
        if cmd.size.raw() > spec.max_order_size.raw() {
            return Err(RejectReason::SizeAboveMaximum);
        }

        let price = cmd.price.ok_or(RejectReason::MissingPrice)?;

        // Tick / lot alignment.
        if spec.tick_size.raw() > 0 && price.raw() % spec.tick_size.raw() != 0 {
            return Err(RejectReason::PriceTickMisaligned);
        }
        if spec.lot_size.raw() > 0 && cmd.size.raw() % spec.lot_size.raw() != 0 {
            return Err(RejectReason::SizeLotMisaligned);
        }

        // Price band check.
        if let (Some(ref_price), Some(upper)) =
            (spec.price_band.reference_price, spec.price_band.upper_bps_from_ref)
        {
            let limit = (ref_price.raw() as i128) * (10_000 + upper.raw() as i128) / 10_000;
            if (price.raw() as i128) > limit {
                return Err(RejectReason::PriceBandViolation);
            }
        }
        if let (Some(ref_price), Some(lower)) =
            (spec.price_band.reference_price, spec.price_band.lower_bps_from_ref)
        {
            let limit = (ref_price.raw() as i128) * (10_000 - lower.raw() as i128) / 10_000;
            if (price.raw() as i128) < limit {
                return Err(RejectReason::PriceBandViolation);
            }
        }

        let account = self.accounts.get_mut(&cmd.user_id).ok_or(RejectReason::UnknownUser)?;
        if account.is_suspended {
            return Err(RejectReason::UserSuspended);
        }

        // Compute hold per side.
        let (currency, amount, bid_reserve_price) = match cmd.side {
            Side::Bid => {
                let reserve = cmd.reserve_price.unwrap_or(price);
                if reserve.raw() < price.raw() {
                    return Err(RejectReason::InvalidReservePrice);
                }
                let gross = quote_amount(reserve, cmd.size, spec)
                    .ok_or(RejectReason::ArithmeticOverflow)?;
                let fee = gross
                    .mul_bps_ceil(spec.fee_schedule.taker_bps)
                    .ok_or(RejectReason::ArithmeticOverflow)?;
                let hold = gross.checked_add(fee).ok_or(RejectReason::ArithmeticOverflow)?;
                (spec.quote_currency, hold, Some(reserve))
            }
            Side::Ask => {
                // Hold = size in base currency.
                (spec.base_currency, Amount(cmd.size.raw()), None)
            }
        };

        if !account.debit_free(currency, amount) {
            return Err(RejectReason::InsufficientFunds);
        }
        account.add_to_hold(currency, amount);

        self.holds.insert(
            order_id,
            Hold {
                user_id: cmd.user_id,
                symbol_id: cmd.symbol_id,
                order_id,
                side: cmd.side,
                currency,
                remaining_size: cmd.size,
                remaining_amount: amount,
                bid_reserve_price,
                pre_check_fee_bps: spec.fee_schedule.taker_bps,
            },
        );
        Ok(())
    }

    /// Settle one trade. Updates both sides' Holds and balances. Removes a
    /// Hold whose remaining_size reaches zero.
    pub fn apply_trade(&mut self, trade: &Trade, spec: &SymbolSpec) -> Result<(), RejectReason> {
        // Pull both holds out so we can mutate while we work.
        let taker_hold = self.holds.remove(&trade.taker_order_id)
            .ok_or(RejectReason::InvalidOrderBookState)?;
        let maker_hold = self.holds.remove(&trade.maker_order_id)
            .ok_or(RejectReason::InvalidOrderBookState)?;

        let updated_taker = self.settle_side(trade, taker_hold, true, spec)?;
        let updated_maker = self.settle_side(trade, maker_hold, false, spec)?;

        if let Some(h) = updated_taker {
            self.holds.insert(h.order_id, h);
        }
        if let Some(h) = updated_maker {
            self.holds.insert(h.order_id, h);
        }
        Ok(())
    }

    /// Release any remaining hold for an order. Used on cancel / IOC drop /
    /// FOK reject. Returns Ok even if no hold exists (idempotent — useful when
    /// the order finished naturally and the hold was already cleaned up).
    pub fn release_hold(&mut self, order_id: OrderId) -> Result<(), RejectReason> {
        let Some(hold) = self.holds.remove(&order_id) else {
            return Ok(());
        };
        let acct = self
            .accounts
            .get_mut(&hold.user_id)
            .ok_or(RejectReason::UnknownUser)?;
        acct.sub_from_hold(hold.currency, hold.remaining_amount);
        acct.credit_free(hold.currency, hold.remaining_amount);
        Ok(())
    }

    fn settle_side(
        &mut self,
        trade: &Trade,
        mut hold: Hold,
        is_taker: bool,
        spec: &SymbolSpec,
    ) -> Result<Option<Hold>, RejectReason> {
        let fee_bps = if is_taker {
            spec.fee_schedule.taker_bps
        } else {
            spec.fee_schedule.maker_bps
        };

        match hold.side {
            Side::Bid => {
                // Hold is in quote. Decrement proportionally to fill_size at
                // the *pre-check* (worst-case) rate. The difference between
                // worst-case hold and actual cost+fee is refunded to free.
                let reserve = hold.bid_reserve_price.expect("bid hold lacks reserve");
                let hold_release_gross = quote_amount(reserve, trade.size, spec)
                    .ok_or(RejectReason::ArithmeticOverflow)?;
                let hold_release_fee = hold_release_gross
                    .mul_bps_ceil(hold.pre_check_fee_bps)
                    .ok_or(RejectReason::ArithmeticOverflow)?;
                let hold_release = hold_release_gross
                    .checked_add(hold_release_fee)
                    .ok_or(RejectReason::ArithmeticOverflow)?;

                let actual_cost = quote_amount(trade.price, trade.size, spec)
                    .ok_or(RejectReason::ArithmeticOverflow)?;
                let actual_fee = actual_cost
                    .mul_bps_ceil(fee_bps)
                    .ok_or(RejectReason::ArithmeticOverflow)?;
                let actual_spend = actual_cost
                    .checked_add(actual_fee)
                    .ok_or(RejectReason::ArithmeticOverflow)?;

                // Slack = hold_release - actual_spend; guaranteed >= 0 because
                // reserve >= fill_price (validated at pre-check) and pre_check_bps
                // is taker, which is >= maker.
                let slack = hold_release
                    .checked_sub(actual_spend)
                    .ok_or(RejectReason::ArithmeticOverflow)?;

                let acct = self
                    .accounts
                    .get_mut(&hold.user_id)
                    .ok_or(RejectReason::UnknownUser)?;
                acct.sub_from_hold(hold.currency, hold_release);
                acct.credit_free(hold.currency, slack);
                acct.credit_free(spec.base_currency, Amount(trade.size.raw()));

                // Fee to exchange revenue.
                let revenue = self
                    .accounts
                    .get_mut(&EXCHANGE_ACCOUNT)
                    .expect("exchange account missing");
                revenue.credit_free(spec.quote_currency, actual_fee);

                hold.remaining_amount = Amount(hold.remaining_amount.raw() - hold_release.raw());
                hold.remaining_size = Size(hold.remaining_size.raw() - trade.size.raw());
            }
            Side::Ask => {
                // Hold is in base, 1:1 with size.
                let hold_release = Amount(trade.size.raw());
                let proceeds = quote_amount(trade.price, trade.size, spec)
                    .ok_or(RejectReason::ArithmeticOverflow)?;
                let fee = proceeds
                    .mul_bps_ceil(fee_bps)
                    .ok_or(RejectReason::ArithmeticOverflow)?;
                let net_to_seller = proceeds
                    .checked_sub(fee)
                    .ok_or(RejectReason::ArithmeticOverflow)?;

                let acct = self
                    .accounts
                    .get_mut(&hold.user_id)
                    .ok_or(RejectReason::UnknownUser)?;
                acct.sub_from_hold(hold.currency, hold_release);
                acct.credit_free(spec.quote_currency, net_to_seller);

                let revenue = self
                    .accounts
                    .get_mut(&EXCHANGE_ACCOUNT)
                    .expect("exchange account missing");
                revenue.credit_free(spec.quote_currency, fee);

                hold.remaining_amount = Amount(hold.remaining_amount.raw() - hold_release.raw());
                hold.remaining_size = Size(hold.remaining_size.raw() - trade.size.raw());
            }
        }

        if hold.remaining_size.is_zero() {
            // Hold consumed. Sanity: remaining_amount should also be zero modulo rounding.
            // If a stray amount remains due to rounding asymmetry, refund it.
            if !hold.remaining_amount.is_zero() {
                let acct = self
                    .accounts
                    .get_mut(&hold.user_id)
                    .ok_or(RejectReason::UnknownUser)?;
                acct.sub_from_hold(hold.currency, hold.remaining_amount);
                acct.credit_free(hold.currency, hold.remaining_amount);
            }
            Ok(None)
        } else {
            Ok(Some(hold))
        }
    }
}

/// `Price × Size / base_minor_per_major`, fitted into Amount (i64).
/// Returns None on overflow or zero divisor.
fn quote_amount(price: Price, size: Size, spec: &SymbolSpec) -> Option<Amount> {
    Amount::from_scaled_i128(price.mul_size(size), spec.base_minor_per_major as i128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_types::{
        ClientOrderId, FeeSchedule, OrderType, PriceBand, SelfTradePrevention, SymbolKindParams,
        TimeInForce,
    };

    fn spec_btc_usdt() -> SymbolSpec {
        SymbolSpec {
            symbol_id: SymbolId(1),
            base_currency: CurrencyId(1),  // BTC
            quote_currency: CurrencyId(2), // USDT
            base_minor_per_major: 100_000_000, // satoshi
            quote_minor_per_major: 1_000_000,  // micro-USDT
            tick_size: Price(1),
            lot_size: Size(1),
            min_order_size: Size(1),
            max_order_size: Size(i64::MAX),
            fee_schedule: FeeSchedule { maker_bps: Bps(10), taker_bps: Bps(20) },
            price_band: PriceBand::none(),
            kind_params: SymbolKindParams::Spot,
            is_suspended: false,
        }
    }

    fn po(uid: u64, side: Side, price: i64, size: i64) -> PlaceOrder {
        PlaceOrder {
            user_id: UserId(uid),
            symbol_id: SymbolId(1),
            client_order_id: ClientOrderId(uid),
            side,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            price: Some(Price(price)),
            size: Size(size),
            reserve_price: None,
            stop_price: None,
            visible_size: None,
            self_trade_prevention: SelfTradePrevention::None,
        }
    }

    #[test]
    fn deposit_credits_free() {
        let mut r = RiskEngine::new();
        r.add_user(UserId(1)).unwrap();
        r.adjust_balance(&AdjustBalance {
            user_id: UserId(1),
            currency_id: CurrencyId(2),
            delta: Amount(1_000_000),
            transaction_id: 1,
        })
        .unwrap();
        assert_eq!(r.account(UserId(1)).unwrap().free(CurrencyId(2)), Amount(1_000_000));
    }

    #[test]
    fn pre_check_bid_holds_quote() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_usdt();
        r.add_user(UserId(1)).unwrap();
        r.adjust_balance(&AdjustBalance {
            user_id: UserId(1),
            currency_id: CurrencyId(2),
            delta: Amount(1_000_000_000),
            transaction_id: 1,
        })
        .unwrap();
        // Buy 50M satoshi (0.5 BTC) at price 50e6 micro-USDT/BTC.
        // gross = 50_000_000 * 50_000_000 / 100_000_000 = 25_000_000
        // fee = 25_000_000 * 20 / 10000 = 50_000 (ceil)
        // hold = 25_050_000
        r.pre_check_place(&po(1, Side::Bid, 50_000_000, 50_000_000), &spec, OrderId(1))
            .unwrap();
        let a = r.account(UserId(1)).unwrap();
        assert_eq!(a.held(CurrencyId(2)), Amount(25_050_000));
        assert_eq!(a.free(CurrencyId(2)), Amount(1_000_000_000 - 25_050_000));
    }

    #[test]
    fn pre_check_ask_holds_base_one_to_one() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_usdt();
        r.add_user(UserId(1)).unwrap();
        r.adjust_balance(&AdjustBalance {
            user_id: UserId(1),
            currency_id: CurrencyId(1),
            delta: Amount(100_000_000),
            transaction_id: 1,
        })
        .unwrap();
        r.pre_check_place(&po(1, Side::Ask, 50_000_000, 50_000_000), &spec, OrderId(1))
            .unwrap();
        let a = r.account(UserId(1)).unwrap();
        assert_eq!(a.held(CurrencyId(1)), Amount(50_000_000));
        assert_eq!(a.free(CurrencyId(1)), Amount(50_000_000));
    }

    #[test]
    fn release_hold_refunds_all() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_usdt();
        r.add_user(UserId(1)).unwrap();
        r.adjust_balance(&AdjustBalance {
            user_id: UserId(1),
            currency_id: CurrencyId(2),
            delta: Amount(1_000_000_000),
            transaction_id: 1,
        })
        .unwrap();
        r.pre_check_place(&po(1, Side::Bid, 50_000_000, 50_000_000), &spec, OrderId(1))
            .unwrap();
        r.release_hold(OrderId(1)).unwrap();
        let a = r.account(UserId(1)).unwrap();
        assert_eq!(a.held(CurrencyId(2)), Amount::ZERO);
        assert_eq!(a.free(CurrencyId(2)), Amount(1_000_000_000));
    }

    #[test]
    fn release_unknown_order_is_noop() {
        let mut r = RiskEngine::new();
        assert!(r.release_hold(OrderId(999)).is_ok());
    }

    #[test]
    fn full_trade_settles_both_sides_and_conserves_money() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_usdt();
        r.add_user(UserId(1)).unwrap(); // seller
        r.add_user(UserId(2)).unwrap(); // buyer

        // Seller has BTC, buyer has USDT.
        r.adjust_balance(&AdjustBalance {
            user_id: UserId(1),
            currency_id: CurrencyId(1),
            delta: Amount(100_000_000),
            transaction_id: 1,
        })
        .unwrap();
        r.adjust_balance(&AdjustBalance {
            user_id: UserId(2),
            currency_id: CurrencyId(2),
            delta: Amount(1_000_000_000),
            transaction_id: 2,
        })
        .unwrap();

        let total_btc_before = r.total_internal(CurrencyId(1));
        let total_usdt_before = r.total_internal(CurrencyId(2));

        // Seller posts an ask.
        r.pre_check_place(&po(1, Side::Ask, 50_000_000, 100_000_000), &spec, OrderId(1))
            .unwrap();
        // Buyer takes it.
        r.pre_check_place(&po(2, Side::Bid, 50_000_000, 100_000_000), &spec, OrderId(2))
            .unwrap();

        let trade = Trade {
            symbol_id: SymbolId(1),
            price: Price(50_000_000),
            size: Size(100_000_000),
            taker_order_id: OrderId(2),
            taker_user_id: UserId(2),
            maker_order_id: OrderId(1),
            maker_user_id: UserId(1),
            taker_side: Side::Bid,
            timestamp: me_types::Timestamp(0),
        };
        r.apply_trade(&trade, &spec).unwrap();

        assert_eq!(r.total_internal(CurrencyId(1)), total_btc_before);
        assert_eq!(r.total_internal(CurrencyId(2)), total_usdt_before);

        // Both holds gone.
        assert!(r.hold(OrderId(1)).is_none());
        assert!(r.hold(OrderId(2)).is_none());

        // Seller now has USDT (proceeds minus fee).
        assert!(r.account(UserId(1)).unwrap().free(CurrencyId(2)).raw() > 0);
        // Buyer now has BTC.
        assert_eq!(r.account(UserId(2)).unwrap().free(CurrencyId(1)), Amount(100_000_000));
        // Exchange got some fees.
        assert!(r.account(EXCHANGE_ACCOUNT).unwrap().free(CurrencyId(2)).raw() > 0);
    }

    #[test]
    fn partial_fill_keeps_proportional_hold() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_usdt();
        r.add_user(UserId(1)).unwrap();
        r.add_user(UserId(2)).unwrap();
        r.adjust_balance(&AdjustBalance {
            user_id: UserId(1),
            currency_id: CurrencyId(1),
            delta: Amount(100_000_000),
            transaction_id: 1,
        })
        .unwrap();
        r.adjust_balance(&AdjustBalance {
            user_id: UserId(2),
            currency_id: CurrencyId(2),
            delta: Amount(1_000_000_000),
            transaction_id: 2,
        })
        .unwrap();
        r.pre_check_place(&po(1, Side::Ask, 50_000_000, 100_000_000), &spec, OrderId(1))
            .unwrap();
        r.pre_check_place(&po(2, Side::Bid, 50_000_000, 100_000_000), &spec, OrderId(2))
            .unwrap();
        let trade = Trade {
            symbol_id: SymbolId(1),
            price: Price(50_000_000),
            size: Size(40_000_000),
            taker_order_id: OrderId(2),
            taker_user_id: UserId(2),
            maker_order_id: OrderId(1),
            maker_user_id: UserId(1),
            taker_side: Side::Bid,
            timestamp: me_types::Timestamp(0),
        };
        r.apply_trade(&trade, &spec).unwrap();
        // Holds remain at 60% of original.
        assert_eq!(r.hold(OrderId(1)).unwrap().remaining_size, Size(60_000_000));
        assert_eq!(r.hold(OrderId(2)).unwrap().remaining_size, Size(60_000_000));
    }
}
