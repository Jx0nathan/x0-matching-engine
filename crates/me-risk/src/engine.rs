use ahash::AHashMap;
use serde::{Deserialize, Serialize};

use me_types::{
    AdjustBalance, Amount, Bps, CurrencyId, OrderId, PerpParams, PlaceOrder, Price, RejectReason,
    Side, Size, SymbolId, SymbolKindParams, SymbolSpec, Trade, UserId,
};

use crate::account::UserAccount;

/// Sentinel UserId reserved for the exchange's fee-revenue account.
/// All fees collected in any currency are credited here so the conservation
/// invariant (sum of internal balances = sum of external in/out) holds.
pub const EXCHANGE_ACCOUNT: UserId = UserId(0);

/// A reservation of funds against an open order. Indexed by OrderId. The
/// `kind` field distinguishes spot semantics from derivative semantics —
/// settlement and refund logic differ between them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Hold {
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    pub order_id: OrderId,
    pub side: Side,
    pub currency: CurrencyId,
    pub remaining_size: Size,
    pub remaining_amount: Amount,
    /// Bps used at pre-check (always taker bps — worst case).
    pub pre_check_fee_bps: Bps,
    pub kind: HoldKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum HoldKind {
    /// Spot order: hold is in quote for Bid (sized at reserve_price), in base
    /// for Ask (1:1 with size). Settles by transferring base/quote between
    /// the two users.
    Spot {
        bid_reserve_price: Option<Price>,
    },
    /// Derivative order: hold is always in quote currency, sized at order
    /// price × IMR (plus fee). On fill, margin moves from the user's
    /// account.holds bucket into the per-symbol Position.margin_locked.
    /// M4.1: opening / increasing positions only; reverse-direction orders
    /// (which would reduce or flip) are rejected at pre-check.
    Derivative {
        /// Total margin reserved at pre-check, sized for the full order
        /// at the order price × IMR_bps. Drawn down proportionally as fills
        /// happen (`reserve_margin × fill_size / original_size`). Storing the
        /// totals avoids per-unit truncation at small minor-unit scales.
        reserve_margin: Amount,
        /// Total fee reserved (full order at taker bps). Same drawdown.
        reserve_fee: Amount,
        /// Original order size — used as the denominator for proportional drawdown.
        original_size: Size,
        ref_price: Price,
    },
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

    /// Sum across all users of (free + held + position margin in this currency).
    /// Includes the exchange revenue account. Position margin is only counted
    /// when its `margin_currency` matches — without this filter we'd
    /// double-count a USDT position's margin into a BTC conservation total.
    /// This is the conservation-test gate for M4 derivatives.
    pub fn total_internal(&self, currency: CurrencyId) -> i128 {
        self.accounts
            .values()
            .map(|a| {
                let balance_part =
                    a.free(currency).raw() as i128 + a.held(currency).raw() as i128;
                let position_part: i128 = a
                    .positions
                    .values()
                    .filter(|p| p.margin_currency == currency)
                    .map(|p| p.margin_locked.raw() as i128)
                    .sum();
                balance_part + position_part
            })
            .sum()
    }

    pub fn pre_check_place(
        &mut self,
        cmd: &PlaceOrder,
        spec: &SymbolSpec,
        order_id: OrderId,
    ) -> Result<(), RejectReason> {
        if spec.is_suspended {
            return Err(RejectReason::SymbolSuspended);
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

        if spec.tick_size.raw() > 0 && price.raw() % spec.tick_size.raw() != 0 {
            return Err(RejectReason::PriceTickMisaligned);
        }
        if spec.lot_size.raw() > 0 && cmd.size.raw() % spec.lot_size.raw() != 0 {
            return Err(RejectReason::SizeLotMisaligned);
        }
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

        match &spec.kind_params {
            SymbolKindParams::Spot => self.pre_check_spot(cmd, spec, price, order_id),
            SymbolKindParams::PerpetualSwap(p) => {
                self.pre_check_derivative(cmd, spec, p, price, order_id)
            }
            SymbolKindParams::Future(f) => {
                // Same logic as perp for M4.1; expiry/settlement is M4.3.
                let p = PerpParams {
                    initial_margin_bps: f.initial_margin_bps,
                    maintenance_margin_bps: f.maintenance_margin_bps,
                    funding_interval_secs: 0,
                    max_leverage: 0,
                };
                self.pre_check_derivative(cmd, spec, &p, price, order_id)
            }
        }
    }

    fn pre_check_spot(
        &mut self,
        cmd: &PlaceOrder,
        spec: &SymbolSpec,
        price: Price,
        order_id: OrderId,
    ) -> Result<(), RejectReason> {
        let account = self
            .accounts
            .get_mut(&cmd.user_id)
            .expect("user existence checked above");
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
            Side::Ask => (spec.base_currency, Amount(cmd.size.raw()), None),
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
                pre_check_fee_bps: spec.fee_schedule.taker_bps,
                kind: HoldKind::Spot { bid_reserve_price },
            },
        );
        Ok(())
    }

    fn pre_check_derivative(
        &mut self,
        cmd: &PlaceOrder,
        spec: &SymbolSpec,
        params: &PerpParams,
        price: Price,
        order_id: OrderId,
    ) -> Result<(), RejectReason> {
        // M4.1: opening / increasing positions only. Reduce/close/flip is
        // routed through future Command::ClosePosition (M4.2).
        let account = self
            .accounts
            .get_mut(&cmd.user_id)
            .expect("user existence checked above");
        let current_signed = account
            .positions
            .get(&cmd.symbol_id)
            .map(|p| p.size.raw())
            .unwrap_or(0);

        let order_signed = match cmd.side {
            Side::Bid => cmd.size.raw(),
            Side::Ask => -cmd.size.raw(),
        };
        // Reject reverse-direction orders for M4.1.
        if (current_signed > 0 && order_signed < 0) || (current_signed < 0 && order_signed > 0) {
            return Err(RejectReason::UnsupportedCommand);
        }

        // Compute totals first (per-unit math truncates at small minor-unit scales).
        let total_notional = quote_amount(price, cmd.size, spec)
            .ok_or(RejectReason::ArithmeticOverflow)?;
        let reserve_margin = total_notional
            .mul_bps_ceil(params.initial_margin_bps)
            .ok_or(RejectReason::ArithmeticOverflow)?;
        let reserve_fee = total_notional
            .mul_bps_ceil(spec.fee_schedule.taker_bps)
            .ok_or(RejectReason::ArithmeticOverflow)?;
        let to_lock = reserve_margin
            .checked_add(reserve_fee)
            .ok_or(RejectReason::ArithmeticOverflow)?;

        if !account.debit_free(spec.quote_currency, to_lock) {
            return Err(RejectReason::InsufficientMargin);
        }
        account.add_to_hold(spec.quote_currency, to_lock);

        self.holds.insert(
            order_id,
            Hold {
                user_id: cmd.user_id,
                symbol_id: cmd.symbol_id,
                order_id,
                side: cmd.side,
                currency: spec.quote_currency,
                remaining_size: cmd.size,
                remaining_amount: to_lock,
                pre_check_fee_bps: spec.fee_schedule.taker_bps,
                kind: HoldKind::Derivative {
                    reserve_margin,
                    reserve_fee,
                    original_size: cmd.size,
                    ref_price: price,
                },
            },
        );
        Ok(())
    }

    pub fn apply_trade(&mut self, trade: &Trade, spec: &SymbolSpec) -> Result<(), RejectReason> {
        let taker_hold = self
            .holds
            .remove(&trade.taker_order_id)
            .ok_or(RejectReason::InvalidOrderBookState)?;
        let maker_hold = self
            .holds
            .remove(&trade.maker_order_id)
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
        hold: Hold,
        is_taker: bool,
        spec: &SymbolSpec,
    ) -> Result<Option<Hold>, RejectReason> {
        match hold.kind {
            HoldKind::Spot { .. } => self.settle_spot(trade, hold, is_taker, spec),
            HoldKind::Derivative { .. } => self.settle_derivative(trade, hold, is_taker, spec),
        }
    }

    fn settle_spot(
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
                let HoldKind::Spot { bid_reserve_price } = hold.kind else {
                    unreachable!("settle_spot called on non-spot hold");
                };
                let reserve = bid_reserve_price.expect("bid hold lacks reserve");
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

                let revenue = self
                    .accounts
                    .get_mut(&EXCHANGE_ACCOUNT)
                    .expect("exchange account missing");
                revenue.credit_free(spec.quote_currency, actual_fee);

                hold.remaining_amount = Amount(hold.remaining_amount.raw() - hold_release.raw());
                hold.remaining_size = Size(hold.remaining_size.raw() - trade.size.raw());
            }
            Side::Ask => {
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

    fn settle_derivative(
        &mut self,
        trade: &Trade,
        mut hold: Hold,
        is_taker: bool,
        spec: &SymbolSpec,
    ) -> Result<Option<Hold>, RejectReason> {
        let HoldKind::Derivative { reserve_margin, reserve_fee, original_size, ref_price: _ } =
            hold.kind
        else {
            unreachable!("settle_derivative called on non-derivative hold");
        };

        // Bid → positive contribution, Ask → negative.
        let contribution_signed: i64 = match hold.side {
            Side::Bid => trade.size.raw(),
            Side::Ask => -trade.size.raw(),
        };

        // Proportional drawdown of the pre-check reserves for this fill size.
        // Use i128 to avoid intermediate overflow on large reserves × size.
        let reserved_margin_for_fill = Amount(
            ((reserve_margin.raw() as i128) * (trade.size.raw() as i128)
                / (original_size.raw() as i128)) as i64,
        );
        let reserved_fee_for_fill = Amount(
            ((reserve_fee.raw() as i128) * (trade.size.raw() as i128)
                / (original_size.raw() as i128)) as i64,
        );
        let reserved_total_for_fill = reserved_margin_for_fill
            .checked_add(reserved_fee_for_fill)
            .ok_or(RejectReason::ArithmeticOverflow)?;

        // Actual fee at trade price using this user's fee rate.
        let fee_bps = if is_taker {
            spec.fee_schedule.taker_bps
        } else {
            spec.fee_schedule.maker_bps
        };
        let actual_proceeds = quote_amount(trade.price, trade.size, spec)
            .ok_or(RejectReason::ArithmeticOverflow)?;
        let actual_fee = actual_proceeds
            .mul_bps_ceil(fee_bps)
            .ok_or(RejectReason::ArithmeticOverflow)?;

        let imr_bps = match &spec.kind_params {
            SymbolKindParams::PerpetualSwap(p) => p.initial_margin_bps,
            SymbolKindParams::Future(f) => f.initial_margin_bps,
            SymbolKindParams::Spot => unreachable!("settle_derivative on spot symbol"),
        };
        let actual_margin = actual_proceeds
            .mul_bps_ceil(imr_bps)
            .ok_or(RejectReason::ArithmeticOverflow)?;

        let actual_consumed = actual_margin
            .checked_add(actual_fee)
            .ok_or(RejectReason::ArithmeticOverflow)?;
        // Slack: reserve was at order price, actual at trade price (Bid: trade ≤ order, Ask: trade ≥ order).
        // For Bid the reserve overshoots → positive slack. For Ask the reserve can undershoot;
        // saturating_sub means no negative-slack credit, but `actual_margin > reserved_margin_for_fill`
        // on Ask is OK because additional margin can come from free balance (handled below if needed).
        let slack = reserved_total_for_fill
            .raw()
            .saturating_sub(actual_consumed.raw());

        {
            let acct = self
                .accounts
                .get_mut(&hold.user_id)
                .ok_or(RejectReason::UnknownUser)?;
            acct.sub_from_hold(hold.currency, reserved_total_for_fill);
            if slack > 0 {
                acct.credit_free(hold.currency, Amount(slack));
            }
            // If actual_consumed > reserved (Ask side fills above limit), pull
            // the difference from free balance.
            if actual_consumed.raw() > reserved_total_for_fill.raw() {
                let shortfall = Amount(actual_consumed.raw() - reserved_total_for_fill.raw());
                if !acct.debit_free(hold.currency, shortfall) {
                    return Err(RejectReason::InsufficientMargin);
                }
            }

            let position = acct.position_mut_or_insert(hold.symbol_id);
            let prev_signed = position.size.raw();
            let prev_entry = position.entry_price.raw();
            let prev_abs = prev_signed.unsigned_abs() as i64;
            let new_signed = prev_signed + contribution_signed;

            // M4.1 invariant: pre_check_derivative rejects reverse-direction orders,
            // so prev_signed and contribution_signed have the same sign (or prev==0).
            let new_entry = if new_signed == 0 {
                0
            } else if prev_signed == 0 {
                trade.price.raw()
            } else {
                let prev_notional = (prev_abs as i128) * (prev_entry as i128);
                let fill_notional = (trade.size.raw() as i128) * (trade.price.raw() as i128);
                let new_abs = new_signed.unsigned_abs() as i128;
                ((prev_notional + fill_notional) / new_abs) as i64
            };
            position.size = Size(new_signed);
            position.entry_price = Price(new_entry);
            position.margin_locked = Amount(position.margin_locked.raw() + actual_margin.raw());
            position.margin_currency = hold.currency;
        }

        let revenue = self
            .accounts
            .get_mut(&EXCHANGE_ACCOUNT)
            .expect("exchange account missing");
        revenue.credit_free(spec.quote_currency, actual_fee);

        hold.remaining_amount = Amount(hold.remaining_amount.raw() - reserved_total_for_fill.raw());
        hold.remaining_size = Size(hold.remaining_size.raw() - trade.size.raw());

        if hold.remaining_size.is_zero() {
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
fn quote_amount(price: Price, size: Size, spec: &SymbolSpec) -> Option<Amount> {
    Amount::from_scaled_i128(price.mul_size(size), spec.base_minor_per_major as i128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_types::{
        ClientOrderId, FeeSchedule, OrderType, PerpParams, PriceBand, SelfTradePrevention,
        SymbolKindParams, TimeInForce, Timestamp,
    };

    fn spec_btc_usdt() -> SymbolSpec {
        SymbolSpec {
            symbol_id: SymbolId(1),
            base_currency: CurrencyId(1),
            quote_currency: CurrencyId(2),
            base_minor_per_major: 100_000_000,
            quote_minor_per_major: 1_000_000,
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

    fn spec_btc_perp() -> SymbolSpec {
        SymbolSpec {
            symbol_id: SymbolId(2),
            base_currency: CurrencyId(1),
            quote_currency: CurrencyId(2),
            base_minor_per_major: 100_000_000,
            quote_minor_per_major: 1_000_000,
            tick_size: Price(1),
            lot_size: Size(1),
            min_order_size: Size(1),
            max_order_size: Size(i64::MAX),
            fee_schedule: FeeSchedule { maker_bps: Bps(10), taker_bps: Bps(20) },
            price_band: PriceBand::none(),
            kind_params: SymbolKindParams::PerpetualSwap(PerpParams {
                initial_margin_bps: Bps(500),       // 5% IM
                maintenance_margin_bps: Bps(250),   // 2.5% MM
                funding_interval_secs: 28_800,
                max_leverage: 20,
            }),
            is_suspended: false,
        }
    }

    fn po(uid: u64, symbol: SymbolId, side: Side, price: i64, size: i64) -> PlaceOrder {
        PlaceOrder {
            user_id: UserId(uid),
            symbol_id: symbol,
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

    fn deposit(r: &mut RiskEngine, uid: u64, cur: CurrencyId, amt: i64) {
        r.adjust_balance(&AdjustBalance {
            user_id: UserId(uid),
            currency_id: cur,
            delta: Amount(amt),
            transaction_id: 0,
        })
        .unwrap();
    }

    #[test]
    fn deposit_credits_free() {
        let mut r = RiskEngine::new();
        r.add_user(UserId(1)).unwrap();
        deposit(&mut r, 1, CurrencyId(2), 1_000_000);
        assert_eq!(r.account(UserId(1)).unwrap().free(CurrencyId(2)), Amount(1_000_000));
    }

    #[test]
    fn pre_check_bid_holds_quote() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_usdt();
        r.add_user(UserId(1)).unwrap();
        deposit(&mut r, 1, CurrencyId(2), 1_000_000_000);
        r.pre_check_place(&po(1, SymbolId(1), Side::Bid, 50_000_000, 50_000_000), &spec, OrderId(1))
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
        deposit(&mut r, 1, CurrencyId(1), 100_000_000);
        r.pre_check_place(&po(1, SymbolId(1), Side::Ask, 50_000_000, 50_000_000), &spec, OrderId(1))
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
        deposit(&mut r, 1, CurrencyId(2), 1_000_000_000);
        r.pre_check_place(&po(1, SymbolId(1), Side::Bid, 50_000_000, 50_000_000), &spec, OrderId(1))
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
        r.add_user(UserId(1)).unwrap();
        r.add_user(UserId(2)).unwrap();
        deposit(&mut r, 1, CurrencyId(1), 100_000_000);
        deposit(&mut r, 2, CurrencyId(2), 1_000_000_000);

        let total_btc_before = r.total_internal(CurrencyId(1));
        let total_usdt_before = r.total_internal(CurrencyId(2));

        r.pre_check_place(&po(1, SymbolId(1), Side::Ask, 50_000_000, 100_000_000), &spec, OrderId(1))
            .unwrap();
        r.pre_check_place(&po(2, SymbolId(1), Side::Bid, 50_000_000, 100_000_000), &spec, OrderId(2))
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
            timestamp: Timestamp(0),
        };
        r.apply_trade(&trade, &spec).unwrap();

        assert_eq!(r.total_internal(CurrencyId(1)), total_btc_before);
        assert_eq!(r.total_internal(CurrencyId(2)), total_usdt_before);
        assert!(r.hold(OrderId(1)).is_none());
        assert!(r.hold(OrderId(2)).is_none());
        assert!(r.account(UserId(1)).unwrap().free(CurrencyId(2)).raw() > 0);
        assert_eq!(r.account(UserId(2)).unwrap().free(CurrencyId(1)), Amount(100_000_000));
        assert!(r.account(EXCHANGE_ACCOUNT).unwrap().free(CurrencyId(2)).raw() > 0);
    }

    #[test]
    fn partial_fill_keeps_proportional_hold() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_usdt();
        r.add_user(UserId(1)).unwrap();
        r.add_user(UserId(2)).unwrap();
        deposit(&mut r, 1, CurrencyId(1), 100_000_000);
        deposit(&mut r, 2, CurrencyId(2), 1_000_000_000);
        r.pre_check_place(&po(1, SymbolId(1), Side::Ask, 50_000_000, 100_000_000), &spec, OrderId(1))
            .unwrap();
        r.pre_check_place(&po(2, SymbolId(1), Side::Bid, 50_000_000, 100_000_000), &spec, OrderId(2))
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
            timestamp: Timestamp(0),
        };
        r.apply_trade(&trade, &spec).unwrap();
        assert_eq!(r.hold(OrderId(1)).unwrap().remaining_size, Size(60_000_000));
        assert_eq!(r.hold(OrderId(2)).unwrap().remaining_size, Size(60_000_000));
    }

    // ---- Derivatives (M4.1) ----

    #[test]
    fn derivative_pre_check_locks_isolated_margin() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_perp();
        r.add_user(UserId(1)).unwrap();
        deposit(&mut r, 1, CurrencyId(2), 10_000_000_000);

        // Bid 50M satoshi at 50e6 micro-USDT/BTC, IMR = 5%, taker fee = 0.2%.
        // notional = 50_000_000 * 50_000_000 / 100_000_000 = 25_000_000
        // margin = 25_000_000 * 500 / 10_000 = 1_250_000
        // fee = 25_000_000 * 20 / 10_000 = 50_000
        // total locked = 1_300_000
        r.pre_check_place(&po(1, SymbolId(2), Side::Bid, 50_000_000, 50_000_000), &spec, OrderId(1))
            .unwrap();
        let a = r.account(UserId(1)).unwrap();
        assert_eq!(a.held(CurrencyId(2)), Amount(1_300_000));
        assert_eq!(a.free(CurrencyId(2)), Amount(10_000_000_000 - 1_300_000));
    }

    #[test]
    fn derivative_reverse_direction_rejected() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_perp();
        r.add_user(UserId(1)).unwrap();
        deposit(&mut r, 1, CurrencyId(2), 10_000_000_000);

        // Open long.
        r.pre_check_place(&po(1, SymbolId(2), Side::Bid, 50_000_000, 50_000_000), &spec, OrderId(1))
            .unwrap();
        let trade = Trade {
            symbol_id: SymbolId(2),
            price: Price(50_000_000),
            size: Size(50_000_000),
            taker_order_id: OrderId(1),
            taker_user_id: UserId(1),
            maker_order_id: OrderId(99),
            maker_user_id: UserId(99),
            taker_side: Side::Bid,
            timestamp: Timestamp(0),
        };
        // No matching maker for this test; manually set up maker.
        r.add_user(UserId(99)).unwrap();
        deposit(&mut r, 99, CurrencyId(2), 10_000_000_000);
        r.pre_check_place(&po(99, SymbolId(2), Side::Ask, 50_000_000, 50_000_000), &spec, OrderId(99))
            .unwrap();
        r.apply_trade(&trade, &spec).unwrap();

        // User 1 now has a long position. Submitting an Ask must be rejected.
        let err = r
            .pre_check_place(
                &po(1, SymbolId(2), Side::Ask, 51_000_000, 10_000_000),
                &spec,
                OrderId(2),
            )
            .unwrap_err();
        assert_eq!(err, RejectReason::UnsupportedCommand);
    }

    #[test]
    fn derivative_full_trade_conserves_total() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_perp();
        r.add_user(UserId(1)).unwrap();
        r.add_user(UserId(2)).unwrap();
        deposit(&mut r, 1, CurrencyId(2), 10_000_000_000);
        deposit(&mut r, 2, CurrencyId(2), 10_000_000_000);

        let total_before = r.total_internal(CurrencyId(2));

        r.pre_check_place(
            &po(1, SymbolId(2), Side::Bid, 50_000_000, 50_000_000),
            &spec,
            OrderId(1),
        )
        .unwrap();
        r.pre_check_place(
            &po(2, SymbolId(2), Side::Ask, 50_000_000, 50_000_000),
            &spec,
            OrderId(2),
        )
        .unwrap();
        r.apply_trade(
            &Trade {
                symbol_id: SymbolId(2),
                price: Price(50_000_000),
                size: Size(50_000_000),
                taker_order_id: OrderId(1),
                taker_user_id: UserId(1),
                maker_order_id: OrderId(2),
                maker_user_id: UserId(2),
                taker_side: Side::Bid,
                timestamp: Timestamp(0),
            },
            &spec,
        )
        .unwrap();

        // After the trade: U1 has long position with margin locked, U2 has
        // short position with margin locked, exchange has fees. Money is
        // conserved across all of these buckets.
        assert_eq!(r.total_internal(CurrencyId(2)), total_before);

        let p1 = r.account(UserId(1)).unwrap().position(SymbolId(2)).unwrap();
        let p2 = r.account(UserId(2)).unwrap().position(SymbolId(2)).unwrap();
        assert_eq!(p1.size, Size(50_000_000));
        assert_eq!(p2.size, Size(-50_000_000));
        assert_eq!(p1.entry_price, Price(50_000_000));
        assert_eq!(p2.entry_price, Price(50_000_000));
    }

    #[test]
    fn derivative_open_then_increase_updates_weighted_entry() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_perp();
        r.add_user(UserId(1)).unwrap();
        r.add_user(UserId(2)).unwrap();
        deposit(&mut r, 1, CurrencyId(2), 10_000_000_000);
        deposit(&mut r, 2, CurrencyId(2), 10_000_000_000);

        // User 1 opens long at 50e6 for size 40M.
        r.pre_check_place(&po(1, SymbolId(2), Side::Bid, 50_000_000, 40_000_000), &spec, OrderId(1))
            .unwrap();
        r.pre_check_place(&po(2, SymbolId(2), Side::Ask, 50_000_000, 40_000_000), &spec, OrderId(2))
            .unwrap();
        r.apply_trade(
            &Trade {
                symbol_id: SymbolId(2),
                price: Price(50_000_000),
                size: Size(40_000_000),
                taker_order_id: OrderId(1),
                taker_user_id: UserId(1),
                maker_order_id: OrderId(2),
                maker_user_id: UserId(2),
                taker_side: Side::Bid,
                timestamp: Timestamp(0),
            },
            &spec,
        )
        .unwrap();

        // User 1 increases long at 60e6 for size 60M.
        r.add_user(UserId(3)).unwrap();
        deposit(&mut r, 3, CurrencyId(2), 10_000_000_000);
        r.pre_check_place(&po(1, SymbolId(2), Side::Bid, 60_000_000, 60_000_000), &spec, OrderId(3))
            .unwrap();
        r.pre_check_place(&po(3, SymbolId(2), Side::Ask, 60_000_000, 60_000_000), &spec, OrderId(4))
            .unwrap();
        r.apply_trade(
            &Trade {
                symbol_id: SymbolId(2),
                price: Price(60_000_000),
                size: Size(60_000_000),
                taker_order_id: OrderId(3),
                taker_user_id: UserId(1),
                maker_order_id: OrderId(4),
                maker_user_id: UserId(3),
                taker_side: Side::Bid,
                timestamp: Timestamp(0),
            },
            &spec,
        )
        .unwrap();

        let pos = r.account(UserId(1)).unwrap().position(SymbolId(2)).unwrap();
        assert_eq!(pos.size, Size(100_000_000));
        // Weighted entry = (40M*50e6 + 60M*60e6) / 100M = 56e6
        assert_eq!(pos.entry_price, Price(56_000_000));
    }
}
