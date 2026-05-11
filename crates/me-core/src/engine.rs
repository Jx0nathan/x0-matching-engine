use std::path::Path;

use ahash::AHashMap;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use me_matching::{PlaceParams, SpotOrderBook};
use me_risk::RiskEngine;
use me_types::{
    AdjustBalance, Command, CommandEnvelope, CommandReceipt, CommandStatus, Event, OrderAccepted,
    OrderCancelled, OrderFilled, OrderId, OrderPartiallyFilled, OrderRejected, PlaceOrder,
    RejectReason, SeqNo, SymbolId, SymbolSpec, Timestamp, UserId,
};
use me_wal::{SnapshotStore, WalWriter};

/// Persistent state of the engine — everything that survives a restart.
/// The `MatchingEngine` wraps this plus runtime handles (WAL writer,
/// snapshot store) that are *not* serialized.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineSnapshot {
    pub risk: RiskEngine,
    pub books: AHashMap<SymbolId, SpotOrderBook>,
    pub symbols: AHashMap<SymbolId, SymbolSpec>,
    pub next_seq_no: u64,
    pub next_order_id: u64,
    pub last_applied_seq: SeqNo,
}

#[derive(Debug)]
pub struct MatchingEngine {
    state: EngineSnapshot,
    wal: Option<WalWriter>,
    snapshot_store: Option<SnapshotStore>,
}

impl Default for MatchingEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MatchingEngine {
    pub fn new() -> Self {
        Self {
            state: EngineSnapshot {
                risk: RiskEngine::new(),
                books: AHashMap::new(),
                symbols: AHashMap::new(),
                next_seq_no: 1,
                next_order_id: 1,
                last_applied_seq: SeqNo(0),
            },
            wal: None,
            snapshot_store: None,
        }
    }

    /// Open an engine with persistence backing. On open: load the latest
    /// snapshot (if any), then replay every WAL record with `seq > snapshot.seq`.
    /// After this returns, the engine is at the exact state of `last_applied_seq`
    /// in the WAL, and ready for new commands.
    pub fn with_persistence<P: AsRef<Path>, Q: AsRef<Path>>(
        wal_path: P,
        snapshot_dir: Q,
    ) -> Result<Self> {
        let snapshot_store = SnapshotStore::open(snapshot_dir.as_ref())?;
        let wal = WalWriter::open(wal_path.as_ref())?;

        let mut eng = Self::new();
        eng.snapshot_store = Some(snapshot_store);
        eng.wal = Some(wal);
        eng.restore(wal_path.as_ref())?;
        Ok(eng)
    }

    fn restore(&mut self, wal_path: &Path) -> Result<()> {
        // 1. Load the latest snapshot.
        let mut last_snap_seq = SeqNo(0);
        if let Some(store) = &self.snapshot_store {
            if let Some((snap, seq)) = store.load_latest::<EngineSnapshot>()? {
                self.state = snap;
                last_snap_seq = seq;
            }
        }
        // 2. Replay WAL records strictly after the snapshot.
        let envs = me_wal::read_all(wal_path)?;
        for env in envs {
            if env.seq_no.0 > last_snap_seq.0 {
                let _ = self.apply_envelope(&env);
            }
        }
        Ok(())
    }

    /// Register a tradable symbol. This now routes through `submit` so the
    /// addition is captured in the WAL and reconstructed on replay.
    pub fn register_symbol(&mut self, spec: SymbolSpec) -> Result<(), RejectReason> {
        let receipt = self.submit(Command::RegisterSymbol(spec), Timestamp(0));
        match receipt.status {
            CommandStatus::Accepted => Ok(()),
            CommandStatus::Rejected(r) => Err(r),
            _ => unreachable!("RegisterSymbol can only return Accepted or Rejected"),
        }
    }

    fn handle_register_symbol(&mut self, seq_no: SeqNo, spec: SymbolSpec) -> CommandReceipt {
        if self.state.symbols.contains_key(&spec.symbol_id) {
            return CommandReceipt::rejected(seq_no, RejectReason::UnsupportedCommand);
        }
        self.state.books.insert(spec.symbol_id, SpotOrderBook::new(spec.symbol_id));
        self.state.symbols.insert(spec.symbol_id, spec);
        CommandReceipt {
            seq_no,
            status: CommandStatus::Accepted,
            events: SmallVec::new(),
        }
    }

    pub fn risk(&self) -> &RiskEngine {
        &self.state.risk
    }

    pub fn book(&self, symbol_id: SymbolId) -> Option<&SpotOrderBook> {
        self.state.books.get(&symbol_id)
    }

    pub fn symbol(&self, symbol_id: SymbolId) -> Option<&SymbolSpec> {
        self.state.symbols.get(&symbol_id)
    }

    pub fn last_applied_seq(&self) -> SeqNo {
        self.state.last_applied_seq
    }

    /// Capture the current engine state into a snapshot. Persisted to the
    /// snapshot store under `last_applied_seq`. Returns an error if the engine
    /// has no snapshot store attached.
    pub fn take_snapshot(&mut self) -> Result<SeqNo> {
        let Some(store) = &self.snapshot_store else {
            anyhow::bail!("engine has no snapshot store");
        };
        let seq = self.state.last_applied_seq;
        store.save(&self.state, seq)?;
        Ok(seq)
    }

    pub fn submit(&mut self, cmd: Command, now: Timestamp) -> CommandReceipt {
        let seq_no = SeqNo(self.state.next_seq_no);
        self.state.next_seq_no += 1;
        let env = CommandEnvelope { seq_no, received_at: now, command: cmd };

        if let Some(wal) = &mut self.wal {
            // Synchronous durability. M3.2 will batch sync via group commit.
            wal.append(&env).expect("WAL append failed");
            wal.sync().expect("WAL sync failed");
        }

        self.apply_envelope(&env)
    }

    /// Apply an envelope to engine state. Used both for live submissions
    /// (after WAL append) and for replay during `restore` (no WAL append).
    fn apply_envelope(&mut self, env: &CommandEnvelope) -> CommandReceipt {
        let seq_no = env.seq_no;
        let now = env.received_at;

        // Keep next_seq_no monotonic even when replay skips ahead.
        if seq_no.0 + 1 > self.state.next_seq_no {
            self.state.next_seq_no = seq_no.0 + 1;
        }

        let receipt = match &env.command {
            Command::Nop => CommandReceipt {
                seq_no,
                status: CommandStatus::Accepted,
                events: SmallVec::new(),
            },
            Command::AddUser(add) => match self.state.risk.add_user(add.user_id) {
                Ok(()) => CommandReceipt {
                    seq_no,
                    status: CommandStatus::Accepted,
                    events: SmallVec::new(),
                },
                Err(r) => CommandReceipt::rejected(seq_no, r),
            },
            Command::AdjustBalance(adj) => self.handle_adjust_balance(seq_no, adj.clone()),
            Command::SuspendUser(uid) => match self.state.risk.suspend(*uid) {
                Ok(()) => CommandReceipt {
                    seq_no,
                    status: CommandStatus::Accepted,
                    events: SmallVec::new(),
                },
                Err(r) => CommandReceipt::rejected(seq_no, r),
            },
            Command::ResumeUser(uid) => match self.state.risk.resume(*uid) {
                Ok(()) => CommandReceipt {
                    seq_no,
                    status: CommandStatus::Accepted,
                    events: SmallVec::new(),
                },
                Err(r) => CommandReceipt::rejected(seq_no, r),
            },
            Command::PlaceOrder(p) => self.handle_place(seq_no, p.clone(), now),
            Command::CancelOrder(c) => {
                self.handle_cancel(seq_no, c.user_id, c.symbol_id, c.order_id, now)
            }
            Command::RegisterSymbol(spec) => self.handle_register_symbol(seq_no, spec.clone()),
            Command::ModifyOrder(_) => {
                CommandReceipt::rejected(seq_no, RejectReason::UnsupportedCommand)
            }
        };

        self.state.last_applied_seq = seq_no;
        receipt
    }

    fn handle_adjust_balance(&mut self, seq_no: SeqNo, adj: AdjustBalance) -> CommandReceipt {
        match self.state.risk.adjust_balance(&adj) {
            Ok(()) => CommandReceipt {
                seq_no,
                status: CommandStatus::Accepted,
                events: SmallVec::new(),
            },
            Err(r) => CommandReceipt::rejected(seq_no, r),
        }
    }

    fn handle_place(&mut self, seq_no: SeqNo, p: PlaceOrder, now: Timestamp) -> CommandReceipt {
        let order_id = OrderId(self.state.next_order_id);
        self.state.next_order_id += 1;

        let spec = match self.state.symbols.get(&p.symbol_id).cloned() {
            Some(s) => s,
            None => return Self::reject_place(seq_no, &p, now, RejectReason::UnknownSymbol),
        };

        if let Err(r) = self.state.risk.pre_check_place(&p, &spec, order_id) {
            return Self::reject_place(seq_no, &p, now, r);
        }

        let book = self
            .state
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

        if let Some(reason) = outcome.reject {
            let _ = self.state.risk.release_hold(order_id);
            return Self::reject_place(seq_no, &p, now, reason);
        }

        for trade in &outcome.trades {
            self.state
                .risk
                .apply_trade(trade, &spec)
                .expect("apply_trade should not fail in a consistent state");
        }

        if !outcome.rested {
            let _ = self.state.risk.release_hold(order_id);
        }

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
        } else if !outcome.rested {
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
        let Some(book) = self.state.books.get_mut(&symbol_id) else {
            return CommandReceipt::rejected(seq_no, RejectReason::UnknownSymbol);
        };
        let outcome = match book.cancel(order_id) {
            Some(o) => o,
            None => return CommandReceipt::rejected(seq_no, RejectReason::UnknownOrder),
        };
        if outcome.user_id != user_id {
            return CommandReceipt::rejected(seq_no, RejectReason::UnknownOrder);
        }
        let _ = self.state.risk.release_hold(order_id);

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

        assert_eq!(eng.risk().total_internal(CurrencyId(1)), btc_before);
        assert_eq!(eng.risk().total_internal(CurrencyId(2)), usdt_before);
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
        let r = place(&mut eng, 1, 1, Side::Bid, 50_000_000, 100_000_000, TimeInForce::Gtc);
        assert!(matches!(r.status, CommandStatus::Rejected(RejectReason::InsufficientFunds)));
    }

    #[test]
    fn last_applied_seq_advances() {
        let mut eng = MatchingEngine::new();
        assert_eq!(eng.last_applied_seq(), SeqNo(0));
        add_user(&mut eng, 1);
        let last = eng.last_applied_seq();
        assert_eq!(last, SeqNo(1));
        eng.submit(Command::Nop, Timestamp(0));
        assert_eq!(eng.last_applied_seq(), SeqNo(2));
    }
}
