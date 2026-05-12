use ahash::AHashMap;
use serde::{Deserialize, Serialize};

use me_types::{
    AdjustBalance, Amount, Bps, CurrencyId, OrderId, PerpParams, PlaceOrder, Price, RejectReason,
    Side, Size, SymbolId, SymbolKindParams, SymbolSpec, Trade, UserId,
};

/// Description of one underwater position that the engine force-closed.
#[derive(Debug, Clone)]
pub struct LiquidationReport {
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    pub size_closed: Size,
    pub close_price: Price,
    pub realized_pnl: Amount,
    pub margin_released: Amount,
}

/// One user's funding payment for the latest settlement.
#[derive(Debug, Clone)]
pub struct FundingReport {
    pub user_id: UserId,
    pub symbol_id: SymbolId,
    /// Signed: positive = user paid, negative = user received.
    pub payment: Amount,
}

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

    /// Sum across all users of free + held + position.margin_locked in this
    /// currency. Realized-only — does NOT account for the mark-to-market
    /// value of open positions. For spot-only systems this is enough; for
    /// derivatives use `total_internal_with_marks` to also include unrealized
    /// PnL, otherwise closes that realize profit appear as money created from
    /// thin air (the counterparty's loss isn't booked until they close too).
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

    /// Conservation total including mark-to-market unrealized PnL.
    /// `mark_for(symbol)` returns `Some((mark_price, base_minor_per_major))`
    /// for each symbol with a current mark, `None` otherwise.
    ///
    /// Invariant: for any fixed mark mapping, this total stays constant
    /// across all engine operations (trades, fills, fees) modulo external
    /// deposits/withdrawals.
    ///
    /// Implementation note: we accumulate per-symbol unrealized PnL as the
    /// pre-divided integer `(mark − entry) × size` sum, dividing by `scale`
    /// once at the end. Truncating per-position would lose sub-unit precision
    /// asymmetrically between long and short legs and break conservation by
    /// ±1 unit per imbalance — exactly the off-by-one that the property test
    /// would otherwise catch.
    pub fn total_internal_with_marks<F>(&self, currency: CurrencyId, mark_for: F) -> i128
    where
        F: Fn(SymbolId) -> Option<(Price, i64)>,
    {
        let mut total: i128 = 0;
        // (symbol_id) -> (sum of (mark-entry)×size, scale)
        let mut per_symbol_unrealized_raw: ahash::AHashMap<SymbolId, (i128, i128)> =
            ahash::AHashMap::new();

        for account in self.accounts.values() {
            total += account.free(currency).raw() as i128;
            total += account.held(currency).raw() as i128;
            for (symbol_id, position) in &account.positions {
                if position.margin_currency != currency {
                    continue;
                }
                total += position.margin_locked.raw() as i128;
                if position.size.raw() == 0 {
                    continue;
                }
                if let Some((mark, scale)) = mark_for(*symbol_id) {
                    if scale <= 0 {
                        continue;
                    }
                    let diff =
                        (mark.raw() as i128) - (position.entry_price.raw() as i128);
                    let raw = diff * (position.size.raw() as i128);
                    let entry = per_symbol_unrealized_raw
                        .entry(*symbol_id)
                        .or_insert((0, scale as i128));
                    entry.0 += raw;
                }
            }
        }

        for (_, (raw_sum, scale)) in &per_symbol_unrealized_raw {
            total += raw_sum / scale;
        }

        total
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
        let account = self
            .accounts
            .get_mut(&cmd.user_id)
            .expect("user existence checked above");
        let current_signed = account
            .positions
            .get(&cmd.symbol_id)
            .map(|p| p.size.raw())
            .unwrap_or(0);
        let current_margin = account
            .positions
            .get(&cmd.symbol_id)
            .map(|p| p.margin_locked.raw())
            .unwrap_or(0);

        let order_signed = match cmd.side {
            Side::Bid => cmd.size.raw(),
            Side::Ask => -cmd.size.raw(),
        };
        let new_signed = current_signed + order_signed;
        let new_abs = new_signed.unsigned_abs() as i64;

        // Worst-case required margin assuming the full order fills at the
        // order's limit price. Handles open/increase, reduce/close, AND flip
        // uniformly: net_margin_delta is zero when the post-fill position is
        // already covered by the existing margin.
        let new_notional = Amount::from_scaled_i128(
            (price.raw() as i128) * (new_abs as i128),
            spec.base_minor_per_major as i128,
        )
        .ok_or(RejectReason::ArithmeticOverflow)?;
        let new_required_margin = new_notional
            .mul_bps_ceil(params.initial_margin_bps)
            .ok_or(RejectReason::ArithmeticOverflow)?;
        let reserve_margin = Amount(new_required_margin.raw().saturating_sub(current_margin).max(0));

        let total_notional = quote_amount(price, cmd.size, spec)
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

    /// Release a portion of an order's hold corresponding to `size_to_release`
    /// of the order's remaining size. Used by STP `DecrementAndCancel` when a
    /// maker's order is partially reduced — its remaining hold shrinks
    /// proportionally and the freed amount moves back to free balance. The
    /// Hold record stays in the engine (with reduced `remaining_size` and
    /// `remaining_amount`) since the maker is still on the book.
    pub fn partial_release_hold(
        &mut self,
        order_id: OrderId,
        size_to_release: Size,
    ) -> Result<(), RejectReason> {
        let Some(hold) = self.holds.get_mut(&order_id) else {
            return Ok(());
        };
        if size_to_release.raw() <= 0 || hold.remaining_size.raw() <= 0 {
            return Ok(());
        }
        // Cap release to what the hold actually still has.
        let release_size = size_to_release.raw().min(hold.remaining_size.raw());
        let release_amount = (hold.remaining_amount.raw() as i128) * (release_size as i128)
            / (hold.remaining_size.raw() as i128);
        let release_amount = release_amount as i64;

        hold.remaining_size = Size(hold.remaining_size.raw() - release_size);
        hold.remaining_amount = Amount(hold.remaining_amount.raw() - release_amount);

        let user_id = hold.user_id;
        let currency = hold.currency;
        let acct = self
            .accounts
            .get_mut(&user_id)
            .ok_or(RejectReason::UnknownUser)?;
        acct.sub_from_hold(currency, Amount(release_amount));
        acct.credit_free(currency, Amount(release_amount));
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

        let contribution_signed: i64 = match hold.side {
            Side::Bid => trade.size.raw(),
            Side::Ask => -trade.size.raw(),
        };

        let prev_signed = self
            .accounts
            .get(&hold.user_id)
            .ok_or(RejectReason::UnknownUser)?
            .positions
            .get(&hold.symbol_id)
            .map(|p| p.size.raw())
            .unwrap_or(0);
        let abs_contribution = contribution_signed.unsigned_abs() as i64;
        let abs_prev = prev_signed.unsigned_abs() as i64;
        let is_opposite =
            prev_signed != 0 && (prev_signed > 0) != (contribution_signed > 0);
        let is_flip = is_opposite && abs_contribution > abs_prev;
        let is_reducing = is_opposite && !is_flip;

        // Proportional drawdown of fee from this order's reserve.
        let reserved_fee_for_fill = Amount(
            ((reserve_fee.raw() as i128) * (trade.size.raw() as i128)
                / (original_size.raw() as i128)) as i64,
        );
        let reserved_margin_for_fill = Amount(
            ((reserve_margin.raw() as i128) * (trade.size.raw() as i128)
                / (original_size.raw() as i128)) as i64,
        );
        let reserved_total_for_fill = reserved_margin_for_fill
            .checked_add(reserved_fee_for_fill)
            .ok_or(RejectReason::ArithmeticOverflow)?;

        let fee_bps = if is_taker {
            spec.fee_schedule.taker_bps
        } else {
            spec.fee_schedule.maker_bps
        };
        let actual_notional = quote_amount(trade.price, trade.size, spec)
            .ok_or(RejectReason::ArithmeticOverflow)?;
        let actual_fee = actual_notional
            .mul_bps_ceil(fee_bps)
            .ok_or(RejectReason::ArithmeticOverflow)?;

        let imr_bps = match &spec.kind_params {
            SymbolKindParams::PerpetualSwap(p) => p.initial_margin_bps,
            SymbolKindParams::Future(f) => f.initial_margin_bps,
            SymbolKindParams::Spot => unreachable!("settle_derivative on spot symbol"),
        };

        if is_flip {
            self.settle_derivative_flip(
                trade,
                &mut hold,
                contribution_signed,
                actual_notional,
                actual_fee,
                reserved_total_for_fill,
                imr_bps,
                spec,
            )?;
        } else if is_reducing {
            self.settle_derivative_reduce(
                trade,
                &mut hold,
                contribution_signed,
                actual_notional,
                actual_fee,
                reserved_total_for_fill,
                imr_bps,
                spec,
            )?;
        } else {
            self.settle_derivative_open(
                trade,
                &mut hold,
                contribution_signed,
                actual_notional,
                actual_fee,
                reserved_total_for_fill,
                imr_bps,
                spec,
            )?;
        }

        // Hold accounting.
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

    #[allow(clippy::too_many_arguments)]
    fn settle_derivative_open(
        &mut self,
        trade: &Trade,
        hold: &mut Hold,
        contribution_signed: i64,
        actual_notional: Amount,
        actual_fee: Amount,
        reserved_total_for_fill: Amount,
        imr_bps: Bps,
        spec: &SymbolSpec,
    ) -> Result<(), RejectReason> {
        let actual_margin = actual_notional
            .mul_bps_ceil(imr_bps)
            .ok_or(RejectReason::ArithmeticOverflow)?;
        let actual_consumed = actual_margin
            .checked_add(actual_fee)
            .ok_or(RejectReason::ArithmeticOverflow)?;
        let slack = reserved_total_for_fill
            .raw()
            .saturating_sub(actual_consumed.raw());

        let acct = self
            .accounts
            .get_mut(&hold.user_id)
            .ok_or(RejectReason::UnknownUser)?;
        acct.sub_from_hold(hold.currency, reserved_total_for_fill);
        if slack > 0 {
            acct.credit_free(hold.currency, Amount(slack));
        }
        // Ask filling above the limit can leave actual_consumed > reserved.
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

        let revenue = self
            .accounts
            .get_mut(&EXCHANGE_ACCOUNT)
            .expect("exchange account missing");
        revenue.credit_free(spec.quote_currency, actual_fee);
        Ok(())
    }

    /// Settle a fill that closes the existing position AND opens a new one
    /// in the opposite direction (flip). prev's margin is fully released to
    /// free, PnL on the closed portion is realized at fill price, and a new
    /// position is created at fill_price for the opened remainder.
    #[allow(clippy::too_many_arguments)]
    fn settle_derivative_flip(
        &mut self,
        trade: &Trade,
        hold: &mut Hold,
        contribution_signed: i64,
        _actual_notional: Amount,
        actual_fee: Amount,
        reserved_total_for_fill: Amount,
        imr_bps: Bps,
        spec: &SymbolSpec,
    ) -> Result<(), RejectReason> {
        let acct = self
            .accounts
            .get_mut(&hold.user_id)
            .ok_or(RejectReason::UnknownUser)?;

        // Read existing position snapshot (drop borrow before mutating).
        let position = acct
            .positions
            .get(&hold.symbol_id)
            .cloned()
            .unwrap_or_default();
        let prev_signed = position.size.raw();
        let prev_abs = prev_signed.unsigned_abs() as i64;
        let prev_entry = position.entry_price.raw();
        let prev_margin = position.margin_locked;

        let abs_contribution = contribution_signed.unsigned_abs() as i64;
        let closed_size = prev_abs;
        let opened_size = abs_contribution - closed_size;
        let new_signed = contribution_signed.signum() * opened_size;

        // Realized PnL on closed portion (full close of prev) at fill price.
        let pnl_per_unit_i128 = if prev_signed > 0 {
            trade.price.raw() as i128 - prev_entry as i128
        } else {
            prev_entry as i128 - trade.price.raw() as i128
        };
        let pnl_i128 =
            pnl_per_unit_i128 * (closed_size as i128) / (spec.base_minor_per_major as i128);
        let realized_pnl = pnl_i128 as i64;

        // Margin required for the new (opened) portion at fill price.
        let opened_notional = Amount::from_scaled_i128(
            (trade.price.raw() as i128) * (opened_size as i128),
            spec.base_minor_per_major as i128,
        )
        .ok_or(RejectReason::ArithmeticOverflow)?;
        let new_margin = opened_notional
            .mul_bps_ceil(imr_bps)
            .ok_or(RejectReason::ArithmeticOverflow)?;

        // Hold drawdown for this fill's combined (margin + fee) need.
        acct.sub_from_hold(hold.currency, reserved_total_for_fill);
        let target = new_margin
            .checked_add(actual_fee)
            .ok_or(RejectReason::ArithmeticOverflow)?;
        if reserved_total_for_fill.raw() >= target.raw() {
            let slack = reserved_total_for_fill.raw() - target.raw();
            if slack > 0 {
                acct.credit_free(hold.currency, Amount(slack));
            }
        } else {
            let shortfall = target.raw() - reserved_total_for_fill.raw();
            if !acct.debit_free(hold.currency, Amount(shortfall)) {
                return Err(RejectReason::InsufficientMargin);
            }
        }

        // Release prev margin and apply PnL (signed). PnL can be negative.
        let net_to_balance = prev_margin.raw() + realized_pnl;
        let quote_bal = acct.balances.entry(hold.currency).or_insert(Amount::ZERO);
        *quote_bal = Amount(quote_bal.raw() + net_to_balance);

        // Set up new position with the opened remainder at fill_price.
        let position = acct.position_mut_or_insert(hold.symbol_id);
        position.size = Size(new_signed);
        position.entry_price = trade.price;
        position.margin_locked = new_margin;
        position.margin_currency = hold.currency;

        let revenue = self
            .accounts
            .get_mut(&EXCHANGE_ACCOUNT)
            .expect("exchange account missing");
        revenue.credit_free(spec.quote_currency, actual_fee);
        Ok(())
    }

    /// Settle a fill that reduces (or closes) a user's existing position.
    /// Computes realized PnL at the fill price, releases proportional margin
    /// from the position back to free, and pays the fee from the order's
    /// reserve. M4.2 invariant: the order does not flip — pre_check rejects
    /// orders whose size would exceed the current position magnitude.
    #[allow(clippy::too_many_arguments)]
    fn settle_derivative_reduce(
        &mut self,
        trade: &Trade,
        hold: &mut Hold,
        contribution_signed: i64,
        _actual_notional: Amount,
        actual_fee: Amount,
        reserved_total_for_fill: Amount,
        _imr_bps: Bps,
        spec: &SymbolSpec,
    ) -> Result<(), RejectReason> {
        let acct = self
            .accounts
            .get_mut(&hold.user_id)
            .ok_or(RejectReason::UnknownUser)?;

        // Drain the fee reserve from holds. Pay actual fee; slack to free;
        // shortfall pulled from free.
        acct.sub_from_hold(hold.currency, reserved_total_for_fill);
        let slack = reserved_total_for_fill.raw().saturating_sub(actual_fee.raw());
        if slack > 0 {
            acct.credit_free(hold.currency, Amount(slack));
        }
        if actual_fee.raw() > reserved_total_for_fill.raw() {
            let shortfall = Amount(actual_fee.raw() - reserved_total_for_fill.raw());
            if !acct.debit_free(hold.currency, shortfall) {
                return Err(RejectReason::InsufficientFunds);
            }
        }

        let position = acct.position_mut_or_insert(hold.symbol_id);
        let prev_signed = position.size.raw();
        let prev_abs = prev_signed.unsigned_abs() as i64;
        let prev_entry = position.entry_price.raw();
        let prev_margin = position.margin_locked.raw();

        // Closed amount is the contribution magnitude (validated <= |prev| at pre-check).
        let closed = trade.size.raw();
        let new_signed = prev_signed + contribution_signed;

        // Realized PnL on closed_size:
        //   long close (prev > 0): pnl = (fill_price - entry) × closed / scale
        //   short close (prev < 0): pnl = (entry - fill_price) × closed / scale
        let pnl_per_unit_i128: i128 = if prev_signed > 0 {
            trade.price.raw() as i128 - prev_entry as i128
        } else {
            prev_entry as i128 - trade.price.raw() as i128
        };
        let pnl_i128 = pnl_per_unit_i128 * (closed as i128) / (spec.base_minor_per_major as i128);
        let realized_pnl = pnl_i128 as i64;

        // Release proportional margin from position back to free.
        // released = prev_margin × closed / prev_abs.
        let released = if prev_abs == 0 {
            0
        } else {
            ((prev_margin as i128) * (closed as i128) / (prev_abs as i128)) as i64
        };
        let new_margin = prev_margin.saturating_sub(released);

        position.size = Size(new_signed);
        position.margin_locked = Amount(new_margin);
        if new_signed == 0 {
            position.entry_price = Price(0);
        }
        // entry_price unchanged for partial reduce.

        // Apply PnL (signed) to free balance. May go negative (bankruptcy →
        // liquidation in M4.3); for M4.2 we allow it and let conservation
        // tests confirm money is still conserved at the engine level.
        let acct = self
            .accounts
            .get_mut(&hold.user_id)
            .ok_or(RejectReason::UnknownUser)?;
        let quote_bal = acct.balances.entry(spec.quote_currency).or_insert(Amount::ZERO);
        *quote_bal = Amount(quote_bal.raw() + released + realized_pnl);

        let revenue = self
            .accounts
            .get_mut(&EXCHANGE_ACCOUNT)
            .expect("exchange account missing");
        revenue.credit_free(spec.quote_currency, actual_fee);
        Ok(())
    }
}

/// `Price × Size / base_minor_per_major`, fitted into Amount (i64).
fn quote_amount(price: Price, size: Size, spec: &SymbolSpec) -> Option<Amount> {
    Amount::from_scaled_i128(price.mul_size(size), spec.base_minor_per_major as i128)
}

// ---- M4.3.b/c: mark-driven liquidation, funding, settle ----

impl RiskEngine {
    /// Scan all open positions on `spec.symbol_id`. For each position whose
    /// (margin_locked + unrealized_pnl_at_mark) < MMR × notional_at_mark,
    /// force-close at `mark` and return a report.
    pub fn scan_liquidations(
        &mut self,
        spec: &SymbolSpec,
        mark: Price,
    ) -> Vec<LiquidationReport> {
        let mmr_bps = match &spec.kind_params {
            SymbolKindParams::PerpetualSwap(p) => p.maintenance_margin_bps,
            SymbolKindParams::Future(f) => f.maintenance_margin_bps,
            SymbolKindParams::Spot => return Vec::new(),
        };

        let users: Vec<UserId> = self.accounts.keys().copied().collect();
        let mut reports = Vec::new();
        for user_id in users {
            if user_id == EXCHANGE_ACCOUNT {
                continue;
            }
            let underwater = {
                let acct = self.accounts.get(&user_id).unwrap();
                let Some(position) = acct.positions.get(&spec.symbol_id) else {
                    continue;
                };
                if position.size.is_zero() {
                    continue;
                }
                let abs_size = position.size.raw().unsigned_abs() as i128;
                let notional = (mark.raw() as i128) * abs_size / (spec.base_minor_per_major as i128);
                let pnl_per_unit = if position.size.raw() > 0 {
                    mark.raw() as i128 - position.entry_price.raw() as i128
                } else {
                    position.entry_price.raw() as i128 - mark.raw() as i128
                };
                let unrealized = pnl_per_unit * abs_size / (spec.base_minor_per_major as i128);
                let equity = position.margin_locked.raw() as i128 + unrealized;
                let mmr = notional * (mmr_bps.raw() as i128) / 10_000;
                equity < mmr
            };
            if underwater {
                if let Some(report) = self.force_close_position(user_id, spec, mark) {
                    reports.push(report);
                }
            }
        }
        reports
    }

    /// Internal force-close: realize PnL at `close_price`, release margin to
    /// free, reset position to flat. Used by liquidation and futures expiry.
    fn force_close_position(
        &mut self,
        user_id: UserId,
        spec: &SymbolSpec,
        close_price: Price,
    ) -> Option<LiquidationReport> {
        let acct = self.accounts.get_mut(&user_id)?;
        let position = acct.positions.get(&spec.symbol_id).cloned()?;
        if position.size.is_zero() {
            return None;
        }

        let size = position.size.raw();
        let abs_size = size.unsigned_abs() as i64;
        let entry = position.entry_price.raw();

        let pnl_per_unit = if size > 0 {
            close_price.raw() as i128 - entry as i128
        } else {
            entry as i128 - close_price.raw() as i128
        };
        let pnl_total = pnl_per_unit * (abs_size as i128) / (spec.base_minor_per_major as i128);
        let pnl = pnl_total as i64;

        let released_margin = position.margin_locked.raw();

        // Apply: release margin + realize PnL into free balance (may go negative).
        let quote_bal = acct.balances.entry(spec.quote_currency).or_insert(Amount::ZERO);
        *quote_bal = Amount(quote_bal.raw() + released_margin + pnl);

        // Reset position.
        let position_mut = acct.position_mut_or_insert(spec.symbol_id);
        position_mut.size = Size(0);
        position_mut.entry_price = Price(0);
        position_mut.margin_locked = Amount(0);

        Some(LiquidationReport {
            user_id,
            symbol_id: spec.symbol_id,
            size_closed: Size(abs_size),
            close_price,
            realized_pnl: Amount(pnl),
            margin_released: Amount(released_margin),
        })
    }

    /// Apply a funding rate to every open position on `spec.symbol_id` at the
    /// current `mark` price. Returns per-user reports. Long pays short when
    /// `rate_bps > 0`. Funding flows are user-to-user; the engine takes no
    /// fee on funding.
    pub fn apply_funding(
        &mut self,
        spec: &SymbolSpec,
        mark: Price,
        rate_bps: i32,
    ) -> Vec<FundingReport> {
        let users: Vec<UserId> = self.accounts.keys().copied().collect();
        let mut reports = Vec::new();
        for user_id in users {
            if user_id == EXCHANGE_ACCOUNT {
                continue;
            }
            let payment = {
                let Some(acct) = self.accounts.get(&user_id) else { continue; };
                let Some(position) = acct.positions.get(&spec.symbol_id) else { continue; };
                if position.size.is_zero() {
                    continue;
                }
                // payment = signed_size × mark × rate_bps / scale / 10_000
                let signed_size = position.size.raw() as i128;
                let raw = signed_size * (mark.raw() as i128) * (rate_bps as i128)
                    / (spec.base_minor_per_major as i128)
                    / 10_000;
                raw as i64
            };
            if payment == 0 {
                continue;
            }
            let acct = self.accounts.get_mut(&user_id).expect("user just observed");
            let quote_bal = acct.balances.entry(spec.quote_currency).or_insert(Amount::ZERO);
            *quote_bal = Amount(quote_bal.raw() - payment);
            reports.push(FundingReport {
                user_id,
                symbol_id: spec.symbol_id,
                payment: Amount(payment),
            });
        }
        reports
    }

    /// Force-close every open position on `spec.symbol_id` at the given
    /// settlement price. Used by futures expiry. Returns reports analogous
    /// to liquidations.
    pub fn settle_all_positions(
        &mut self,
        spec: &SymbolSpec,
        settlement_price: Price,
    ) -> Vec<LiquidationReport> {
        let users: Vec<UserId> = self.accounts.keys().copied().collect();
        let mut reports = Vec::new();
        for user_id in users {
            if user_id == EXCHANGE_ACCOUNT {
                continue;
            }
            if let Some(report) = self.force_close_position(user_id, spec, settlement_price) {
                reports.push(report);
            }
        }
        reports
    }
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
    fn derivative_flip_closes_then_opens_opposite() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_perp();
        open_50m_long_at_50(&mut r, &spec);

        // Flip: ask 80M at price 55e6 → close 50M long + open 30M short.
        r.add_user(UserId(3)).unwrap();
        deposit(&mut r, 3, CurrencyId(2), 10_000_000_000);
        r.pre_check_place(&po(1, SymbolId(2), Side::Ask, 55_000_000, 80_000_000), &spec, OrderId(3))
            .unwrap();
        r.pre_check_place(&po(3, SymbolId(2), Side::Bid, 55_000_000, 80_000_000), &spec, OrderId(4))
            .unwrap();
        r.apply_trade(
            &Trade {
                symbol_id: SymbolId(2),
                price: Price(55_000_000),
                size: Size(80_000_000),
                taker_order_id: OrderId(3),
                taker_user_id: UserId(1),
                maker_order_id: OrderId(4),
                maker_user_id: UserId(3),
                taker_side: Side::Ask,
                timestamp: Timestamp(0),
            },
            &spec,
        )
        .unwrap();

        let pos = r.account(UserId(1)).unwrap().position(SymbolId(2)).unwrap();
        // Long 50M closes at +5M profit; new position is short 30M with entry at 55e6.
        assert_eq!(pos.size, Size(-30_000_000));
        assert_eq!(pos.entry_price, Price(55_000_000));
        // New margin = 30M × 55e6 × 5% / 1e8 = 825_000
        assert_eq!(pos.margin_locked, Amount(825_000));
    }

    #[test]
    fn derivative_flip_conserves_with_marks() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_perp();
        open_50m_long_at_50(&mut r, &spec);

        r.add_user(UserId(3)).unwrap();
        deposit(&mut r, 3, CurrencyId(2), 10_000_000_000);

        let mark = |sid: SymbolId| {
            if sid == SymbolId(2) {
                Some((Price(55_000_000), 100_000_000))
            } else {
                None
            }
        };
        let total_before = r.total_internal_with_marks(CurrencyId(2), mark);

        r.pre_check_place(&po(1, SymbolId(2), Side::Ask, 55_000_000, 80_000_000), &spec, OrderId(3))
            .unwrap();
        r.pre_check_place(&po(3, SymbolId(2), Side::Bid, 55_000_000, 80_000_000), &spec, OrderId(4))
            .unwrap();
        r.apply_trade(
            &Trade {
                symbol_id: SymbolId(2),
                price: Price(55_000_000),
                size: Size(80_000_000),
                taker_order_id: OrderId(3),
                taker_user_id: UserId(1),
                maker_order_id: OrderId(4),
                maker_user_id: UserId(3),
                taker_side: Side::Ask,
                timestamp: Timestamp(0),
            },
            &spec,
        )
        .unwrap();

        let total_after = r.total_internal_with_marks(CurrencyId(2), mark);
        assert_eq!(total_before, total_after);
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

    /// Helper: open a 50M long for U1 at price 50e6 by trading with U2.
    fn open_50m_long_at_50(r: &mut RiskEngine, spec: &SymbolSpec) {
        r.add_user(UserId(1)).unwrap();
        r.add_user(UserId(2)).unwrap();
        deposit(r, 1, CurrencyId(2), 10_000_000_000);
        deposit(r, 2, CurrencyId(2), 10_000_000_000);
        r.pre_check_place(&po(1, SymbolId(2), Side::Bid, 50_000_000, 50_000_000), spec, OrderId(1))
            .unwrap();
        r.pre_check_place(&po(2, SymbolId(2), Side::Ask, 50_000_000, 50_000_000), spec, OrderId(2))
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
            spec,
        )
        .unwrap();
    }

    #[test]
    fn derivative_full_close_at_profit_realizes_pnl_and_resets_entry() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_perp();
        open_50m_long_at_50(&mut r, &spec);

        let u1_before_quote = r.account(UserId(1)).unwrap().free(CurrencyId(2)).raw();
        let u1_before_position_margin =
            r.account(UserId(1)).unwrap().position(SymbolId(2)).unwrap().margin_locked.raw();

        // U3 takes U1's close at higher price (60e6) → U1 closes at profit.
        r.add_user(UserId(3)).unwrap();
        deposit(&mut r, 3, CurrencyId(2), 10_000_000_000);
        r.pre_check_place(&po(1, SymbolId(2), Side::Ask, 60_000_000, 50_000_000), &spec, OrderId(3))
            .unwrap();
        r.pre_check_place(&po(3, SymbolId(2), Side::Bid, 60_000_000, 50_000_000), &spec, OrderId(4))
            .unwrap();
        r.apply_trade(
            &Trade {
                symbol_id: SymbolId(2),
                price: Price(60_000_000),
                size: Size(50_000_000),
                taker_order_id: OrderId(3),
                taker_user_id: UserId(1),
                maker_order_id: OrderId(4),
                maker_user_id: UserId(3),
                taker_side: Side::Ask,
                timestamp: Timestamp(0),
            },
            &spec,
        )
        .unwrap();

        let pos = r.account(UserId(1)).unwrap().position(SymbolId(2)).unwrap();
        assert_eq!(pos.size, Size(0));
        assert_eq!(pos.margin_locked, Amount(0));
        assert_eq!(pos.entry_price, Price(0));

        // Profit per unit at (60e6 - 50e6) × 50M / 100M = 5_000_000.
        let u1_after_quote = r.account(UserId(1)).unwrap().free(CurrencyId(2)).raw();
        let gained = u1_after_quote - u1_before_quote;
        // Expected = margin returned + realized profit - fee.
        // margin returned ≈ u1_before_position_margin
        // realized profit = 5_000_000
        // fee on notional 30_000_000 at 20bps (ceil) = 60_000
        assert_eq!(gained, u1_before_position_margin + 5_000_000 - 60_000);
    }

    #[test]
    fn derivative_partial_reduce_keeps_entry_and_releases_proportional_margin() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_perp();
        open_50m_long_at_50(&mut r, &spec);

        let pos_before = r.account(UserId(1)).unwrap().position(SymbolId(2)).unwrap().clone();
        let prev_margin = pos_before.margin_locked.raw();

        // Reduce 20M of 50M long at price 55e6.
        r.add_user(UserId(3)).unwrap();
        deposit(&mut r, 3, CurrencyId(2), 10_000_000_000);
        r.pre_check_place(&po(1, SymbolId(2), Side::Ask, 55_000_000, 20_000_000), &spec, OrderId(3))
            .unwrap();
        r.pre_check_place(&po(3, SymbolId(2), Side::Bid, 55_000_000, 20_000_000), &spec, OrderId(4))
            .unwrap();
        r.apply_trade(
            &Trade {
                symbol_id: SymbolId(2),
                price: Price(55_000_000),
                size: Size(20_000_000),
                taker_order_id: OrderId(3),
                taker_user_id: UserId(1),
                maker_order_id: OrderId(4),
                maker_user_id: UserId(3),
                taker_side: Side::Ask,
                timestamp: Timestamp(0),
            },
            &spec,
        )
        .unwrap();

        let pos_after = r.account(UserId(1)).unwrap().position(SymbolId(2)).unwrap();
        // Position size dropped to 30M, entry unchanged at 50e6.
        assert_eq!(pos_after.size, Size(30_000_000));
        assert_eq!(pos_after.entry_price, Price(50_000_000));
        // Margin released proportionally: 20M / 50M = 40% of prev.
        let expected_remaining_margin = prev_margin * 30 / 50;
        // Allow a 1-unit rounding tolerance (integer truncation on division).
        assert!(
            (pos_after.margin_locked.raw() - expected_remaining_margin).abs() <= 1,
            "expected ~{}, got {}",
            expected_remaining_margin,
            pos_after.margin_locked.raw()
        );
    }

    #[test]
    fn derivative_reduce_conserves_total_internal_with_marks() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_perp();
        open_50m_long_at_50(&mut r, &spec);

        // Hold the mark fixed throughout — conservation must hold at any
        // single mark price across the trade.
        let mark = |sid: SymbolId| {
            if sid == SymbolId(2) {
                Some((Price(55_000_000), 100_000_000))
            } else {
                None
            }
        };

        r.add_user(UserId(3)).unwrap();
        deposit(&mut r, 3, CurrencyId(2), 10_000_000_000);

        let total_before = r.total_internal_with_marks(CurrencyId(2), mark);

        r.pre_check_place(&po(1, SymbolId(2), Side::Ask, 55_000_000, 20_000_000), &spec, OrderId(3))
            .unwrap();
        r.pre_check_place(&po(3, SymbolId(2), Side::Bid, 55_000_000, 20_000_000), &spec, OrderId(4))
            .unwrap();
        r.apply_trade(
            &Trade {
                symbol_id: SymbolId(2),
                price: Price(55_000_000),
                size: Size(20_000_000),
                taker_order_id: OrderId(3),
                taker_user_id: UserId(1),
                maker_order_id: OrderId(4),
                maker_user_id: UserId(3),
                taker_side: Side::Ask,
                timestamp: Timestamp(0),
            },
            &spec,
        )
        .unwrap();

        // At a fixed mark, total (realized + unrealized + margin) is invariant
        // across the trade. U1 realized +1M on the close, exactly offset by
        // U2 (still open short) gaining an extra 1M of mark-to-market loss
        // on the symmetric side.
        let total_after = r.total_internal_with_marks(CurrencyId(2), mark);
        assert_eq!(total_before, total_after);
    }

    #[test]
    fn derivative_close_at_loss_debits_balance() {
        let mut r = RiskEngine::new();
        let spec = spec_btc_perp();
        open_50m_long_at_50(&mut r, &spec);

        let u1_before_quote = r.account(UserId(1)).unwrap().free(CurrencyId(2)).raw();
        let u1_before_position_margin =
            r.account(UserId(1)).unwrap().position(SymbolId(2)).unwrap().margin_locked.raw();

        // Close at 40e6 (worse than 50e6 entry) → loss of 5M.
        r.add_user(UserId(3)).unwrap();
        deposit(&mut r, 3, CurrencyId(2), 10_000_000_000);
        r.pre_check_place(&po(1, SymbolId(2), Side::Ask, 40_000_000, 50_000_000), &spec, OrderId(3))
            .unwrap();
        r.pre_check_place(&po(3, SymbolId(2), Side::Bid, 40_000_000, 50_000_000), &spec, OrderId(4))
            .unwrap();
        r.apply_trade(
            &Trade {
                symbol_id: SymbolId(2),
                price: Price(40_000_000),
                size: Size(50_000_000),
                taker_order_id: OrderId(3),
                taker_user_id: UserId(1),
                maker_order_id: OrderId(4),
                maker_user_id: UserId(3),
                taker_side: Side::Ask,
                timestamp: Timestamp(0),
            },
            &spec,
        )
        .unwrap();

        let u1_after_quote = r.account(UserId(1)).unwrap().free(CurrencyId(2)).raw();
        // Net = margin returned + (-5_000_000 loss) - fee.
        // fee on notional 20_000_000 at 20bps = 40_000.
        let net_change = u1_after_quote - u1_before_quote;
        assert_eq!(net_change, u1_before_position_margin - 5_000_000 - 40_000);
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
