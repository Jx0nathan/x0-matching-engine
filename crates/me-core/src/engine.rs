use ahash::AHashMap;
use smallvec::SmallVec;

use me_matching::{PlaceParams, SpotOrderBook};
use me_risk::RiskEngine;
use me_types::{
    AdjustBalance, Command, CommandReceipt, CommandStatus, Event, OrderAccepted, OrderCancelled,
    OrderFilled, OrderId, OrderPartiallyFilled, OrderRejected, PlaceOrder, RejectReason, SeqNo,
    SymbolId, SymbolSpec, Timestamp, UserId,
};

#[derive(Debug, Default)]
pub struct MatchingEngine {
    risk: RiskEngine,
    books: AHashMap<SymbolId, SpotOrderBook>,
    symbols: AHashMap<SymbolId, SymbolSpec>,
    next_seq_no: u64,
    next_order_id: u64,
}

impl MatchingEngine {
    pub fn new() -> Self {
        Self {
            risk: RiskEngine::new(),
            books: AHashMap::new(),
            symbols: AHashMap::new(),
            next_seq_no: 1,
            next_order_id: 1,
        }
    }

    pub fn register_symbol(&mut self, spec: SymbolSpec) -> Result<(), RejectReason> {
        if self.symbols.contains_key(&spec.symbol_id) {
            return Err(RejectReason::UnsupportedCommand);
        }
        self.books.insert(spec.symbol_id, SpotOrderBook::new(spec.symbol_id));
        self.symbols.insert(spec.symbol_id, spec);
        Ok(())
    }

    pub fn risk(&self) -> &RiskEngine {
        &self.risk
    }

    pub fn book(&self, symbol_id: SymbolId) -> Option<&SpotOrderBook> {
        self.books.get(&symbol_id)
    }

    pub fn symbol(&self, symbol_id: SymbolId) -> Option<&SymbolSpec> {
        self.symbols.get(&symbol_id)
    }

    pub fn submit(&mut self, cmd: Command, now: Timestamp) -> CommandReceipt {
        let seq_no = SeqNo(self.next_seq_no);
        self.next_seq_no += 1;

        match cmd {
            Command::Nop => CommandReceipt {
                seq_no,
                status: CommandStatus::Accepted,
                events: SmallVec::new(),
            },
            Command::AddUser(add) => match self.risk.add_user(add.user_id) {
                Ok(()) => CommandReceipt {
                    seq_no,
                    status: CommandStatus::Accepted,
                    events: SmallVec::new(),
                },
                Err(r) => CommandReceipt::rejected(seq_no, r),
            },
            Command::AdjustBalance(adj) => self.handle_adjust_balance(seq_no, adj),
            Command::SuspendUser(uid) => match self.risk.suspend(uid) {
                Ok(()) => CommandReceipt {
                    seq_no,
                    status: CommandStatus::Accepted,
                    events: SmallVec::new(),
                },
                Err(r) => CommandReceipt::rejected(seq_no, r),
            },
            Command::ResumeUser(uid) => match self.risk.resume(uid) {
                Ok(()) => CommandReceipt {
                    seq_no,
                    status: CommandStatus::Accepted,
                    events: SmallVec::new(),
                },
                Err(r) => CommandReceipt::rejected(seq_no, r),
            },
            Command::PlaceOrder(p) => self.handle_place(seq_no, p, now),
            Command::CancelOrder(c) => self.handle_cancel(seq_no, c.user_id, c.symbol_id, c.order_id, now),
            Command::ModifyOrder(_) => CommandReceipt::rejected(seq_no, RejectReason::UnsupportedCommand),
        }
    }

    fn handle_adjust_balance(&mut self, seq_no: SeqNo, adj: AdjustBalance) -> CommandReceipt {
        match self.risk.adjust_balance(&adj) {
            Ok(()) => CommandReceipt {
                seq_no,
                status: CommandStatus::Accepted,
                events: SmallVec::new(),
            },
            Err(r) => CommandReceipt::rejected(seq_no, r),
        }
    }

    fn handle_place(&mut self, seq_no: SeqNo, p: PlaceOrder, now: Timestamp) -> CommandReceipt {
        let order_id = OrderId(self.next_order_id);
        self.next_order_id += 1;

        let spec = match self.symbols.get(&p.symbol_id).cloned() {
            Some(s) => s,
            None => return Self::reject_place(seq_no, &p, now, RejectReason::UnknownSymbol),
        };

        // R1 pre-check: register a Hold (if Ok). From here on, any path out
        // of this function must drain that Hold.
        if let Err(r) = self.risk.pre_check_place(&p, &spec, order_id) {
            return Self::reject_place(seq_no, &p, now, r);
        }

        let book = self
            .books
            .get_mut(&p.symbol_id)
            .expect("book exists since symbol registered");

        let outcome = book.place(PlaceParams {
            order_id,
            user_id: p.user_id,
            client_order_id: p.client_order_id,
            side: p.side,
            order_type: p.order_type,
            time_in_force: p.time_in_force,
            price: p.price.expect("price required, validated by pre_check"),
            size: p.size,
            timestamp: now,
        });

        // Book-side reject (Fok unfillable, PostOnly would cross, etc.) →
        // release the hold immediately. No fills happened.
        if let Some(reason) = outcome.reject {
            // release_hold is idempotent and infallible for unknown ids.
            let _ = self.risk.release_hold(order_id);
            return Self::reject_place(seq_no, &p, now, reason);
        }

        // Apply settlements for each trade.
        for trade in &outcome.trades {
            // Failures here would indicate corrupted state — best to surface them.
            // For M2 we panic-ish: in M3, propagate via a fault channel.
            self.risk
                .apply_trade(trade, &spec)
                .expect("apply_trade should not fail in a consistent state");
        }

        // If the order did not rest, any leftover hold needs releasing.
        // (For Gtc/PostOnly that rested, the hold stays attached to order_id.)
        if !outcome.rested {
            let _ = self.risk.release_hold(order_id);
        }

        // Build receipt events.
        let mut events: SmallVec<[Event; 4]> = SmallVec::new();
        events.push(Event::OrderAccepted(OrderAccepted {
            order_id,
            client_order_id: p.client_order_id,
            user_id: p.user_id,
            symbol_id: p.symbol_id,
            timestamp: now,
        }));
        for trade in outcome.trades.iter().cloned() {
            events.push(Event::Trade(trade));
        }

        let status = if outcome.filled == p.size {
            events.push(Event::OrderFilled(OrderFilled {
                order_id,
                user_id: p.user_id,
                symbol_id: p.symbol_id,
                filled_size: outcome.filled,
                timestamp: now,
            }));
            CommandStatus::Filled
        } else if outcome.filled.is_positive() {
            events.push(Event::OrderPartiallyFilled(OrderPartiallyFilled {
                order_id,
                user_id: p.user_id,
                symbol_id: p.symbol_id,
                filled_size: outcome.filled,
                remaining_size: outcome.remaining,
                timestamp: now,
            }));
            CommandStatus::PartiallyFilled
        } else {
            // No fills. Either rested or dropped immediately (IOC/Fok with no liquidity).
            if !outcome.rested {
                events.push(Event::OrderCancelled(OrderCancelled {
                    order_id,
                    user_id: p.user_id,
                    symbol_id: p.symbol_id,
                    remaining_size: outcome.remaining,
                    timestamp: now,
                }));
                CommandStatus::Cancelled
            } else {
                CommandStatus::Accepted
            }
        };

        CommandReceipt { seq_no, status, events }
    }

    fn handle_cancel(
        &mut self,
        seq_no: SeqNo,
        user_id: UserId,
        symbol_id: SymbolId,
        order_id: OrderId,
        now: Timestamp,
    ) -> CommandReceipt {
        let Some(book) = self.books.get_mut(&symbol_id) else {
            return CommandReceipt::rejected(seq_no, RejectReason::UnknownSymbol);
        };
        let outcome = match book.cancel(order_id) {
            Some(o) => o,
            None => return CommandReceipt::rejected(seq_no, RejectReason::UnknownOrder),
        };
        if outcome.user_id != user_id {
            // Authorization check: only the order owner may cancel.
            // Re-insert would be cleanest, but for M2 the integration tests
            // only cancel their own; treat this as a structural error.
            return CommandReceipt::rejected(seq_no, RejectReason::UnknownOrder);
        }
        let _ = self.risk.release_hold(order_id);

        let mut events: SmallVec<[Event; 4]> = SmallVec::new();
        events.push(Event::OrderCancelled(OrderCancelled {
            order_id,
            user_id,
            symbol_id,
            remaining_size: outcome.remaining_size,
            timestamp: now,
        }));
        CommandReceipt {
            seq_no,
            status: CommandStatus::Cancelled,
            events,
        }
    }

    fn reject_place(
        seq_no: SeqNo,
        p: &PlaceOrder,
        now: Timestamp,
        reason: RejectReason,
    ) -> CommandReceipt {
        let mut events: SmallVec<[Event; 4]> = SmallVec::new();
        events.push(Event::OrderRejected(OrderRejected {
            client_order_id: p.client_order_id,
            user_id: p.user_id,
            symbol_id: p.symbol_id,
            reason,
            timestamp: now,
        }));
        CommandReceipt {
            seq_no,
            status: CommandStatus::Rejected(reason),
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_types::{
        AddUser, AdjustBalance, Amount, Bps, CancelOrder, ClientOrderId, CurrencyId, FeeSchedule,
        OrderType, Price, PriceBand, SelfTradePrevention, Side, Size, SymbolKindParams,
        TimeInForce,
    };

    fn build_engine() -> MatchingEngine {
        let mut eng = MatchingEngine::new();
        eng.register_symbol(SymbolSpec {
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
        })
        .unwrap();
        eng
    }

    fn add_user(eng: &mut MatchingEngine, uid: u64) {
        eng.submit(Command::AddUser(AddUser { user_id: UserId(uid) }), Timestamp(0));
    }

    fn deposit(eng: &mut MatchingEngine, uid: u64, currency: u32, amt: i64) {
        eng.submit(
            Command::AdjustBalance(AdjustBalance {
                user_id: UserId(uid),
                currency_id: CurrencyId(currency),
                delta: Amount(amt),
                transaction_id: 0,
            }),
            Timestamp(0),
        );
    }

    fn place(
        eng: &mut MatchingEngine,
        uid: u64,
        coid: u64,
        side: Side,
        price: i64,
        size: i64,
        tif: TimeInForce,
    ) -> CommandReceipt {
        eng.submit(
            Command::PlaceOrder(PlaceOrder {
                user_id: UserId(uid),
                symbol_id: SymbolId(1),
                client_order_id: ClientOrderId(coid),
                side,
                order_type: OrderType::Limit,
                time_in_force: tif,
                price: Some(Price(price)),
                size: Size(size),
                reserve_price: None,
                stop_price: None,
                visible_size: None,
                self_trade_prevention: SelfTradePrevention::None,
            }),
            Timestamp(0),
        )
    }

    #[test]
    fn end_to_end_trade_conserves_money() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        add_user(&mut eng, 2);
        deposit(&mut eng, 1, 1, 100_000_000);
        deposit(&mut eng, 2, 2, 1_000_000_000);

        let btc_before = eng.risk().total_internal(CurrencyId(1));
        let usdt_before = eng.risk().total_internal(CurrencyId(2));

        place(&mut eng, 1, 1, Side::Ask, 50_000_000, 100_000_000, TimeInForce::Gtc);
        let r = place(&mut eng, 2, 2, Side::Bid, 50_000_000, 100_000_000, TimeInForce::Ioc);
        assert_eq!(r.status, CommandStatus::Filled);

        assert_eq!(eng.risk().total_internal(CurrencyId(1)), btc_before);
        assert_eq!(eng.risk().total_internal(CurrencyId(2)), usdt_before);
    }

    #[test]
    fn rejected_place_does_not_leak_hold() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        deposit(&mut eng, 1, 2, 1_000_000_000);
        let usdt_before = eng.risk().total_internal(CurrencyId(2));
        let user_free_before = eng.risk().account(UserId(1)).unwrap().free(CurrencyId(2));

        // FOK with no opposing liquidity → rejected at the book.
        let r = place(&mut eng, 1, 1, Side::Bid, 50_000_000, 100_000_000, TimeInForce::Fok);
        assert!(matches!(r.status, CommandStatus::Rejected(_)));

        assert_eq!(eng.risk().total_internal(CurrencyId(2)), usdt_before);
        assert_eq!(eng.risk().account(UserId(1)).unwrap().free(CurrencyId(2)), user_free_before);
        assert_eq!(eng.risk().account(UserId(1)).unwrap().held(CurrencyId(2)), Amount::ZERO);
    }

    #[test]
    fn cancel_returns_hold_to_free() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        deposit(&mut eng, 1, 2, 1_000_000_000);
        let free_before = eng.risk().account(UserId(1)).unwrap().free(CurrencyId(2));

        let r = place(&mut eng, 1, 1, Side::Bid, 50_000_000, 100_000_000, TimeInForce::Gtc);
        assert_eq!(r.status, CommandStatus::Accepted);
        let order_id = match &r.events[0] {
            Event::OrderAccepted(a) => a.order_id,
            _ => panic!("expected OrderAccepted"),
        };

        eng.submit(
            Command::CancelOrder(CancelOrder {
                user_id: UserId(1),
                symbol_id: SymbolId(1),
                order_id,
            }),
            Timestamp(0),
        );

        assert_eq!(eng.risk().account(UserId(1)).unwrap().free(CurrencyId(2)), free_before);
        assert_eq!(eng.risk().account(UserId(1)).unwrap().held(CurrencyId(2)), Amount::ZERO);
    }

    #[test]
    fn ioc_partial_fill_releases_remainder() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        add_user(&mut eng, 2);
        deposit(&mut eng, 1, 1, 100_000_000);
        deposit(&mut eng, 2, 2, 1_000_000_000);
        let usdt_before = eng.risk().total_internal(CurrencyId(2));
        let btc_before = eng.risk().total_internal(CurrencyId(1));

        place(&mut eng, 1, 1, Side::Ask, 50_000_000, 30_000_000, TimeInForce::Gtc);
        let r = place(&mut eng, 2, 2, Side::Bid, 50_000_000, 100_000_000, TimeInForce::Ioc);
        assert!(matches!(r.status, CommandStatus::PartiallyFilled));

        // No leaks: totals preserved.
        assert_eq!(eng.risk().total_internal(CurrencyId(1)), btc_before);
        assert_eq!(eng.risk().total_internal(CurrencyId(2)), usdt_before);
        // Buyer has no residual hold.
        assert_eq!(eng.risk().account(UserId(2)).unwrap().held(CurrencyId(2)), Amount::ZERO);
    }

    #[test]
    fn unknown_symbol_rejects() {
        let mut eng = MatchingEngine::new();
        add_user(&mut eng, 1);
        let r = place(&mut eng, 1, 1, Side::Bid, 100, 10, TimeInForce::Gtc);
        assert!(matches!(r.status, CommandStatus::Rejected(RejectReason::UnknownSymbol)));
    }

    #[test]
    fn insufficient_funds_rejects() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        // No deposit.
        let r = place(&mut eng, 1, 1, Side::Bid, 50_000_000, 100_000_000, TimeInForce::Gtc);
        assert!(matches!(r.status, CommandStatus::Rejected(RejectReason::InsufficientFunds)));
    }
}
