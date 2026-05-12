use std::path::Path;

use ahash::AHashMap;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use tracing::{debug, trace};

/// Returns true if the mark price has crossed the stop trigger for an order
/// of the given side. Buy stop (Bid): triggers when mark >= stop. Sell stop
/// (Ask): triggers when mark <= stop.
fn stop_trigger_met(side: Side, mark: Price, stop: Price) -> bool {
    match side {
        Side::Bid => mark.raw() >= stop.raw(),
        Side::Ask => mark.raw() <= stop.raw(),
    }
}

use me_matching::{PlaceParams, SpotOrderBook};
use me_risk::RiskEngine;
use me_types::{
    AdjustBalance, ApplyFunding, Command, CommandEnvelope, CommandReceipt, CommandStatus,
    CurrencyId, Event, ModifyOrder, OrderAccepted, OrderCancelled, OrderFilled, OrderId,
    OrderPartiallyFilled, OrderRejected, OrderType, PlaceOrder, Price, RejectReason, SeqNo,
    SelfTradePrevention, SetMarkPrice, SettleFuture, Side, Size, SymbolId, SymbolKindParams,
    SymbolSpec, TimeInForce, Timestamp, UserId,
};
use me_wal::{SnapshotStore, WalWriter};

/// A Stop order awaiting its trigger price. Lives in `EngineSnapshot.pending_stops`
/// until `SetMarkPrice` moves the mark through the trigger; then the underlying
/// Limit/Market portion is submitted to the book under the same `order_id`
/// (the risk hold is already in place from the placement-time pre-check).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingStop {
    pub order_id: OrderId,
    /// The order to submit when triggered. For StopMarket variants the price
    /// field has already been lowered to the reserve_price (Bid) or zero (Ask).
    pub place_order: PlaceOrder,
}

/// Persistent state of the engine — everything that survives a restart.
/// The `MatchingEngine` wraps this plus runtime handles (WAL writer,
/// snapshot store) that are *not* serialized.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineSnapshot {
    pub risk: RiskEngine,
    pub books: AHashMap<SymbolId, SpotOrderBook>,
    pub symbols: AHashMap<SymbolId, SymbolSpec>,
    /// Latest mark prices per derivative symbol. Set via Command::SetMarkPrice;
    /// used by liquidation, funding, and mark-aware conservation totals.
    pub mark_prices: AHashMap<SymbolId, Price>,
    /// Pending Stop orders, keyed by symbol. Each entry was pre-checked at
    /// placement (the user's hold is already locked) and is just waiting for
    /// the mark price to cross its `stop_price`.
    #[serde(default)]
    pub pending_stops: AHashMap<SymbolId, Vec<PendingStop>>,
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
                mark_prices: AHashMap::new(),
                pending_stops: AHashMap::new(),
                next_seq_no: 1,
                next_order_id: 1,
                last_applied_seq: SeqNo(0),
            },
            wal: None,
            snapshot_store: None,
        }
    }

    pub fn mark_price(&self, symbol_id: SymbolId) -> Option<Price> {
        self.state.mark_prices.get(&symbol_id).copied()
    }

    /// Conservation total in a currency, including unrealized PnL from every
    /// open derivative position evaluated at its stored mark price.
    /// Symbols without a mark contribute zero unrealized PnL (positions in
    /// them just count their margin_locked).
    pub fn total_internal_with_marks(&self, currency: CurrencyId) -> i128 {
        self.state.risk.total_internal_with_marks(currency, |sid| {
            let mark = self.state.mark_prices.get(&sid).copied()?;
            let scale = self.state.symbols.get(&sid).map(|s| s.base_minor_per_major)?;
            Some((mark, scale))
        })
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

    fn handle_set_mark_price(
        &mut self,
        seq_no: SeqNo,
        cmd: SetMarkPrice,
        now: Timestamp,
    ) -> CommandReceipt {
        // Mark price applies to derivative symbols for liquidation logic, but
        // also to spot symbols for stop-order triggering. Update the mark
        // unconditionally; only run the derivative-specific liquidation sweep
        // if the symbol is a derivative.
        let Some(spec) = self.state.symbols.get(&cmd.symbol_id).cloned() else {
            return CommandReceipt::rejected(seq_no, RejectReason::UnknownSymbol);
        };
        self.state.mark_prices.insert(cmd.symbol_id, cmd.mark_price);

        if !matches!(spec.kind_params, SymbolKindParams::Spot) {
            let reports = self.state.risk.scan_liquidations(&spec, cmd.mark_price);
            if !reports.is_empty() {
                debug!(
                    symbol_id = cmd.symbol_id.0,
                    liquidations = reports.len(),
                    "mark price update triggered liquidations"
                );
            }
        }

        // Trigger any pending stops whose condition is satisfied at the new mark.
        let triggered = self.drain_triggered_stops(cmd.symbol_id, cmd.mark_price);
        let mut events: SmallVec<[Event; 4]> = SmallVec::new();
        for mut stop in triggered {
            // Lower order_type to Limit; the price/TIF were already set at
            // placement time. The hold is still in place from pre_check.
            stop.place_order.order_type = OrderType::Limit;
            let (_status, stop_events) =
                self.apply_to_book(stop.order_id, &stop.place_order, &spec, now);
            // Fold each triggered-stop's lifecycle into this SetMarkPrice
            // receipt. Consumers learn from these events that the stop fired.
            for ev in stop_events {
                events.push(ev);
            }
        }

        CommandReceipt {
            seq_no,
            status: CommandStatus::Accepted,
            events,
        }
    }

    /// Drain pending stops on `symbol_id` whose trigger condition is now met.
    fn drain_triggered_stops(&mut self, symbol_id: SymbolId, mark: Price) -> Vec<PendingStop> {
        let Some(pending) = self.state.pending_stops.get_mut(&symbol_id) else {
            return Vec::new();
        };
        let mut triggered = Vec::new();
        let mut still_pending = Vec::with_capacity(pending.len());
        for stop in pending.drain(..) {
            let Some(stop_price) = stop.place_order.stop_price else {
                continue; // invariant: only orders with stop_price land here
            };
            if stop_trigger_met(stop.place_order.side, mark, stop_price) {
                triggered.push(stop);
            } else {
                still_pending.push(stop);
            }
        }
        *pending = still_pending;
        triggered
    }

    fn handle_apply_funding(&mut self, seq_no: SeqNo, cmd: ApplyFunding) -> CommandReceipt {
        let Some(spec) = self.state.symbols.get(&cmd.symbol_id).cloned() else {
            return CommandReceipt::rejected(seq_no, RejectReason::UnknownSymbol);
        };
        if !matches!(spec.kind_params, SymbolKindParams::PerpetualSwap(_)) {
            return CommandReceipt::rejected(seq_no, RejectReason::UnsupportedCommand);
        }
        let Some(mark) = self.state.mark_prices.get(&cmd.symbol_id).copied() else {
            // Need a mark to compute funding.
            return CommandReceipt::rejected(seq_no, RejectReason::InvalidOrderBookState);
        };
        let _reports = self.state.risk.apply_funding(&spec, mark, cmd.rate_bps);
        CommandReceipt {
            seq_no,
            status: CommandStatus::Accepted,
            events: SmallVec::new(),
        }
    }

    fn handle_settle_future(&mut self, seq_no: SeqNo, cmd: SettleFuture) -> CommandReceipt {
        let spec = match self.state.symbols.get(&cmd.symbol_id).cloned() {
            Some(s) => s,
            None => return CommandReceipt::rejected(seq_no, RejectReason::UnknownSymbol),
        };
        if !matches!(spec.kind_params, SymbolKindParams::Future(_)) {
            return CommandReceipt::rejected(seq_no, RejectReason::UnsupportedCommand);
        }
        let _reports = self.state.risk.settle_all_positions(&spec, cmd.settlement_price);
        // Suspend the expired symbol so no new orders are accepted.
        if let Some(s) = self.state.symbols.get_mut(&cmd.symbol_id) {
            s.is_suspended = true;
        }
        self.state.mark_prices.insert(cmd.symbol_id, cmd.settlement_price);
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

    #[tracing::instrument(skip_all, fields(seq_no))]
    pub fn submit(&mut self, cmd: Command, now: Timestamp) -> CommandReceipt {
        let seq_no = SeqNo(self.state.next_seq_no);
        self.state.next_seq_no += 1;
        tracing::Span::current().record("seq_no", seq_no.0);
        let env = CommandEnvelope { seq_no, received_at: now, command: cmd };

        if let Some(wal) = &mut self.wal {
            wal.append(&env).expect("WAL append failed");
            wal.sync().expect("WAL sync failed");
            trace!(seq = seq_no.0, "WAL synced");
        }

        self.apply_envelope(&env)
    }

    /// Batched variant of `submit`: append all envelopes to the WAL, fsync
    /// ONCE for the whole batch, then apply each in order. This amortises
    /// the fsync cost over the batch — the canonical group-commit pattern.
    ///
    /// Durability invariant preserved: by the time any receipt is returned,
    /// every record in the batch is on durable storage.
    #[tracing::instrument(skip_all, fields(batch_size = cmds.len()))]
    pub fn submit_batch(
        &mut self,
        cmds: Vec<(Command, Timestamp)>,
    ) -> Vec<CommandReceipt> {
        if cmds.is_empty() {
            return Vec::new();
        }
        let mut envs = Vec::with_capacity(cmds.len());
        for (cmd, now) in cmds {
            let seq_no = SeqNo(self.state.next_seq_no);
            self.state.next_seq_no += 1;
            envs.push(CommandEnvelope { seq_no, received_at: now, command: cmd });
        }

        if let Some(wal) = &mut self.wal {
            for env in &envs {
                wal.append(env).expect("WAL append failed");
            }
            wal.sync().expect("WAL sync failed");
            debug!(batch_size = envs.len(), "WAL group-commit synced");
        }

        envs.iter().map(|env| self.apply_envelope(env)).collect()
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
            Command::SetMarkPrice(m) => self.handle_set_mark_price(seq_no, m.clone(), now),
            Command::ApplyFunding(f) => self.handle_apply_funding(seq_no, f.clone()),
            Command::SettleFuture(s) => self.handle_settle_future(seq_no, s.clone()),
            Command::ModifyOrder(m) => self.handle_modify(seq_no, m.clone(), now),
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

        let mut p = p;
        // Lower Market / StopMarket to a Limit-at-bound. A Market Bid needs
        // *some* upper bound for the risk hold; we use the caller's reserve
        // (or price if reserve is omitted). Ask side accepts any positive
        // bid, so the floor drops to 0. Time-in-force is forced to Ioc so
        // the Market portion never rests.
        let is_market_like = matches!(
            p.order_type,
            OrderType::Market | OrderType::StopMarket
        );
        if is_market_like {
            match p.side {
                Side::Bid => {
                    let bound = p.reserve_price.or(p.price);
                    match bound {
                        Some(price) => {
                            p.price = Some(price);
                            p.reserve_price = Some(price);
                        }
                        None => {
                            return Self::reject_place(seq_no, &p, now, RejectReason::MissingPrice);
                        }
                    }
                }
                Side::Ask => {
                    p.price = Some(Price(0));
                }
            }
            if matches!(p.order_type, OrderType::Market) {
                p.order_type = OrderType::Limit;
                p.time_in_force = TimeInForce::Ioc;
            } else {
                // StopMarket — keep order_type to signal "pend until trigger",
                // but at trigger time we'll lower further to Limit + IOC.
                p.time_in_force = TimeInForce::Ioc;
            }
        }

        if let Err(r) = self.state.risk.pre_check_place(&p, &spec, order_id) {
            return Self::reject_place(seq_no, &p, now, r);
        }

        // Stop dispatch. If the stop's trigger condition isn't currently
        // satisfied by the mark, register it in pending_stops; otherwise
        // fall through to the book using the lowered limit semantics.
        if matches!(p.order_type, OrderType::StopLimit | OrderType::StopMarket) {
            let Some(stop_price) = p.stop_price else {
                let _ = self.state.risk.release_hold(order_id);
                return Self::reject_place(seq_no, &p, now, RejectReason::MissingPrice);
            };
            let mark = self.state.mark_prices.get(&p.symbol_id).copied();
            let triggered = matches!(mark, Some(m) if stop_trigger_met(p.side, m, stop_price));
            if !triggered {
                self.state
                    .pending_stops
                    .entry(p.symbol_id)
                    .or_default()
                    .push(PendingStop {
                        order_id,
                        place_order: p.clone(),
                    });
                let mut events: SmallVec<[Event; 4]> = SmallVec::new();
                events.push(Event::OrderAccepted(OrderAccepted {
                    order_id,
                    client_order_id: p.client_order_id,
                    user_id: p.user_id,
                    symbol_id: p.symbol_id,
                    timestamp: now,
                }));
                return CommandReceipt {
                    seq_no,
                    status: CommandStatus::Accepted,
                    events,
                };
            }
            // Already past the trigger — lower to Limit and place now.
            p.order_type = OrderType::Limit;
        }

        let (status, events) = self.apply_to_book(order_id, &p, &spec, now);
        CommandReceipt { seq_no, status, events }
    }

    /// Run book.place + settlement for an order that already has a registered
    /// risk hold (allocated at pre_check_place time). Used by both
    /// fresh-placement handle_place and pending-stop triggers, so the
    /// downstream settlement (apply_trade, release_hold on STP/IOC, event
    /// emission) lives in exactly one place.
    fn apply_to_book(
        &mut self,
        order_id: OrderId,
        p: &PlaceOrder,
        spec: &SymbolSpec,
        now: Timestamp,
    ) -> (CommandStatus, SmallVec<[Event; 4]>) {
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
            stp: p.self_trade_prevention,
            visible_size: p.visible_size,
        });

        if let Some(reason) = outcome.reject {
            let _ = self.state.risk.release_hold(order_id);
            let mut events: SmallVec<[Event; 4]> = SmallVec::new();
            events.push(Event::OrderRejected(OrderRejected {
                client_order_id: p.client_order_id,
                user_id: p.user_id,
                symbol_id: p.symbol_id,
                reason,
                timestamp: now,
            }));
            return (CommandStatus::Rejected(reason), events);
        }

        for trade in &outcome.trades {
            self.state
                .risk
                .apply_trade(trade, spec)
                .expect("apply_trade should not fail in a consistent state");
        }

        for stp_cancel in &outcome.stp_cancellations {
            if stp_cancel.full_cancel {
                let _ = self.state.risk.release_hold(stp_cancel.order_id);
            } else {
                let _ = self
                    .state
                    .risk
                    .partial_release_hold(stp_cancel.order_id, stp_cancel.size_cancelled);
            }
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
        for stp_cancel in &outcome.stp_cancellations {
            if stp_cancel.full_cancel {
                events.push(Event::OrderCancelled(OrderCancelled {
                    order_id: stp_cancel.order_id,
                    user_id: stp_cancel.user_id,
                    symbol_id: p.symbol_id,
                    remaining_size: stp_cancel.size_cancelled,
                    timestamp: now,
                }));
            }
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

        (status, events)
    }

    /// ModifyOrder implements cancel-and-replace. The existing resting order
    /// is read for its side, price, client_order_id, and any iceberg metadata;
    /// `new_price` and `new_size` override the relevant fields (each defaults
    /// to the current value if None). The old order is removed from the book
    /// and its hold released; a fresh `order_id` is allocated and the
    /// synthetic replacement runs the normal pre_check + apply_to_book path.
    /// Time priority is lost — that's the trade-off for not building a
    /// preserving-amend code path (which only works in narrow cases).
    fn handle_modify(
        &mut self,
        seq_no: SeqNo,
        cmd: ModifyOrder,
        now: Timestamp,
    ) -> CommandReceipt {
        // Snapshot the existing order (cloned so we can drop the borrow before
        // mutating the book on the cancel/replace below).
        let existing = {
            let Some(book) = self.state.books.get(&cmd.symbol_id) else {
                return CommandReceipt::rejected(seq_no, RejectReason::UnknownSymbol);
            };
            match book.get_order(cmd.order_id) {
                Some(o) => o.clone(),
                None => return CommandReceipt::rejected(seq_no, RejectReason::UnknownOrder),
            }
        };
        if existing.user_id != cmd.user_id {
            return CommandReceipt::rejected(seq_no, RejectReason::UnknownOrder);
        }

        let new_price = cmd.new_price.unwrap_or(existing.price);
        // For iceberg orders the "remaining order" is visible + hidden.
        let current_total =
            existing.size_remaining.raw() + existing.hidden_remaining.raw();
        let new_size = cmd.new_size.unwrap_or(Size(current_total));
        if !new_size.is_positive() {
            return CommandReceipt::rejected(seq_no, RejectReason::SizeBelowMinimum);
        }

        let is_iceberg = existing.visible_slice.raw() > 0;
        let order_type = if is_iceberg { OrderType::Iceberg } else { OrderType::Limit };
        let visible_size = if is_iceberg { Some(existing.visible_slice) } else { None };

        let new_place = PlaceOrder {
            user_id: cmd.user_id,
            symbol_id: cmd.symbol_id,
            client_order_id: existing.client_order_id,
            side: existing.side,
            order_type,
            time_in_force: TimeInForce::Gtc,
            price: Some(new_price),
            size: new_size,
            reserve_price: None,
            stop_price: None,
            visible_size,
            self_trade_prevention: SelfTradePrevention::None,
        };

        let new_order_id = OrderId(self.state.next_order_id);
        self.state.next_order_id += 1;

        let Some(spec) = self.state.symbols.get(&cmd.symbol_id).cloned() else {
            return CommandReceipt::rejected(seq_no, RejectReason::UnknownSymbol);
        };

        // Cancel the old order: remove from book and release its risk hold.
        let cancel_outcome = {
            let book = self
                .state
                .books
                .get_mut(&cmd.symbol_id)
                .expect("symbol registered");
            book.cancel(cmd.order_id).expect("just verified by get_order")
        };
        let _ = self.state.risk.release_hold(cmd.order_id);

        // Pre-check the replacement. If pre-check rejects (e.g. user can't
        // cover the increased size), the old order is already cancelled and
        // its hold returned to free — the user is now flat on this order_id.
        if let Err(r) = self.state.risk.pre_check_place(&new_place, &spec, new_order_id) {
            let mut events: SmallVec<[Event; 4]> = SmallVec::new();
            events.push(Event::OrderCancelled(OrderCancelled {
                order_id: cmd.order_id,
                user_id: cmd.user_id,
                symbol_id: cmd.symbol_id,
                remaining_size: cancel_outcome.remaining_size,
                timestamp: now,
            }));
            events.push(Event::OrderRejected(OrderRejected {
                client_order_id: new_place.client_order_id,
                user_id: new_place.user_id,
                symbol_id: new_place.symbol_id,
                reason: r,
                timestamp: now,
            }));
            return CommandReceipt {
                seq_no,
                status: CommandStatus::Rejected(r),
                events,
            };
        }

        let (status, mut new_events) = self.apply_to_book(new_order_id, &new_place, &spec, now);

        let mut events: SmallVec<[Event; 4]> = SmallVec::new();
        events.push(Event::OrderCancelled(OrderCancelled {
            order_id: cmd.order_id,
            user_id: cmd.user_id,
            symbol_id: cmd.symbol_id,
            remaining_size: cancel_outcome.remaining_size,
            timestamp: now,
        }));
        for ev in new_events.drain(..) {
            events.push(ev);
        }
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

    #[test]
    fn submit_batch_matches_n_individual_submits() {
        // Build the same scenario both ways and compare end state.
        fn run<F: FnMut(&mut MatchingEngine, Vec<(Command, Timestamp)>)>(mut driver: F) -> MatchingEngine {
            let mut eng = build_engine();
            let t = Timestamp(0);
            let cmds = vec![
                (Command::AddUser(AddUser { user_id: UserId(1) }), t),
                (Command::AddUser(AddUser { user_id: UserId(2) }), t),
                (
                    Command::AdjustBalance(AdjustBalance {
                        user_id: UserId(1),
                        currency_id: CurrencyId(1),
                        delta: Amount(100_000_000),
                        transaction_id: 0,
                    }),
                    t,
                ),
                (
                    Command::AdjustBalance(AdjustBalance {
                        user_id: UserId(2),
                        currency_id: CurrencyId(2),
                        delta: Amount(1_000_000_000),
                        transaction_id: 1,
                    }),
                    t,
                ),
                (
                    Command::PlaceOrder(PlaceOrder {
                        user_id: UserId(1),
                        symbol_id: SymbolId(1),
                        client_order_id: ClientOrderId(1),
                        side: Side::Ask,
                        order_type: OrderType::Limit,
                        time_in_force: TimeInForce::Gtc,
                        price: Some(Price(50_000_000)),
                        size: Size(100_000_000),
                        reserve_price: None,
                        stop_price: None,
                        visible_size: None,
                        self_trade_prevention: SelfTradePrevention::None,
                    }),
                    t,
                ),
                (
                    Command::PlaceOrder(PlaceOrder {
                        user_id: UserId(2),
                        symbol_id: SymbolId(1),
                        client_order_id: ClientOrderId(2),
                        side: Side::Bid,
                        order_type: OrderType::Limit,
                        time_in_force: TimeInForce::Ioc,
                        price: Some(Price(50_000_000)),
                        size: Size(100_000_000),
                        reserve_price: None,
                        stop_price: None,
                        visible_size: None,
                        self_trade_prevention: SelfTradePrevention::None,
                    }),
                    t,
                ),
            ];
            driver(&mut eng, cmds);
            eng
        }

        let one_by_one = run(|eng, cmds| {
            for (cmd, ts) in cmds {
                eng.submit(cmd, ts);
            }
        });
        let batched = run(|eng, cmds| {
            eng.submit_batch(cmds);
        });

        for cur in [CurrencyId(1), CurrencyId(2)] {
            assert_eq!(
                one_by_one.risk().total_internal(cur),
                batched.risk().total_internal(cur)
            );
        }
        assert_eq!(one_by_one.last_applied_seq(), batched.last_applied_seq());
    }

    // ---- Market orders (M5.2.a) ----

    fn place_market(
        eng: &mut MatchingEngine,
        uid: u64,
        coid: u64,
        side: Side,
        reserve_price: Option<i64>,
        size: i64,
    ) -> CommandReceipt {
        eng.submit(
            Command::PlaceOrder(PlaceOrder {
                user_id: UserId(uid),
                symbol_id: SymbolId(1),
                client_order_id: ClientOrderId(coid),
                side,
                order_type: OrderType::Market,
                time_in_force: TimeInForce::Ioc,
                price: None,
                size: Size(size),
                reserve_price: reserve_price.map(Price),
                stop_price: None,
                visible_size: None,
                self_trade_prevention: SelfTradePrevention::None,
            }),
            Timestamp(0),
        )
    }

    #[test]
    fn market_bid_fills_available_liquidity_up_to_reserve() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        add_user(&mut eng, 2);
        deposit(&mut eng, 1, 1, 100_000_000);
        deposit(&mut eng, 2, 2, 10_000_000_000);

        place(&mut eng, 1, 1, Side::Ask, 50_000_000, 100_000_000, TimeInForce::Gtc);
        let r = place_market(&mut eng, 2, 2, Side::Bid, Some(60_000_000), 50_000_000);
        // Filled at maker price 50e6, not at the 60e6 reserve.
        assert_eq!(r.status, CommandStatus::Filled);
    }

    #[test]
    fn market_ask_sells_at_any_bid_price() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        add_user(&mut eng, 2);
        deposit(&mut eng, 1, 1, 100_000_000);
        deposit(&mut eng, 2, 2, 10_000_000_000);

        // Buyer has a bid at 30e6 (much lower than 50e6 mid).
        place(&mut eng, 2, 1, Side::Bid, 30_000_000, 50_000_000, TimeInForce::Gtc);
        // Seller market-asks 50M — should hit the 30e6 bid regardless of "fair" price.
        let r = place_market(&mut eng, 1, 2, Side::Ask, None, 50_000_000);
        assert_eq!(r.status, CommandStatus::Filled);
    }

    #[test]
    fn market_bid_without_reserve_rejects() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        deposit(&mut eng, 1, 2, 10_000_000_000);

        let r = place_market(&mut eng, 1, 1, Side::Bid, None, 50_000_000);
        assert!(matches!(r.status, CommandStatus::Rejected(RejectReason::MissingPrice)));
    }

    // ---- Stop orders (M5.2.c) ----

    fn place_stop_limit(
        eng: &mut MatchingEngine,
        uid: u64,
        coid: u64,
        side: Side,
        stop_price: i64,
        limit_price: i64,
        size: i64,
    ) -> CommandReceipt {
        eng.submit(
            Command::PlaceOrder(PlaceOrder {
                user_id: UserId(uid),
                symbol_id: SymbolId(1),
                client_order_id: ClientOrderId(coid),
                side,
                order_type: OrderType::StopLimit,
                time_in_force: TimeInForce::Gtc,
                price: Some(Price(limit_price)),
                size: Size(size),
                reserve_price: None,
                stop_price: Some(Price(stop_price)),
                visible_size: None,
                self_trade_prevention: SelfTradePrevention::None,
            }),
            Timestamp(0),
        )
    }

    fn set_mark(eng: &mut MatchingEngine, price: i64) {
        eng.submit(
            Command::SetMarkPrice(me_types::SetMarkPrice {
                symbol_id: SymbolId(1),
                mark_price: Price(price),
            }),
            Timestamp(0),
        );
    }

    #[test]
    fn stop_pending_until_mark_crosses() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        add_user(&mut eng, 2);
        deposit(&mut eng, 1, 1, 100_000_000);
        deposit(&mut eng, 2, 2, 10_000_000_000);

        // Resting ask at 50e6 so a triggered buy stop has liquidity.
        place(&mut eng, 1, 1, Side::Ask, 50_000_000, 100_000_000, TimeInForce::Gtc);
        // Buy stop @ 55e6 with limit 60e6. Mark unset/below 55 → pending.
        let r = place_stop_limit(&mut eng, 2, 2, Side::Bid, 55_000_000, 60_000_000, 50_000_000);
        assert_eq!(r.status, CommandStatus::Accepted);
        // No trade yet — the resting ask is untouched.
        assert_eq!(eng.book(SymbolId(1)).unwrap().best_ask(), Some(Price(50_000_000)));
    }

    #[test]
    fn buy_stop_triggers_when_mark_reaches_stop_price() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        add_user(&mut eng, 2);
        deposit(&mut eng, 1, 1, 100_000_000);
        deposit(&mut eng, 2, 2, 10_000_000_000);

        place(&mut eng, 1, 1, Side::Ask, 50_000_000, 100_000_000, TimeInForce::Gtc);
        place_stop_limit(&mut eng, 2, 2, Side::Bid, 55_000_000, 60_000_000, 50_000_000);

        // Mark rises to 55e6 → stop triggers.
        let r = eng.submit(
            Command::SetMarkPrice(me_types::SetMarkPrice {
                symbol_id: SymbolId(1),
                mark_price: Price(55_000_000),
            }),
            Timestamp(0),
        );
        // The triggered stop's events fold into the SetMarkPrice receipt.
        let trade = r.events.iter().find_map(|e| match e {
            Event::Trade(t) => Some(t.clone()),
            _ => None,
        });
        let trade = trade.expect("expected a trade from the triggered stop");
        // Bought 50M of the resting 100M ask at fill price 50e6 (maker's price).
        assert_eq!(trade.size, Size(50_000_000));
        assert_eq!(trade.price, Price(50_000_000));
        // 50M of the original 100M ask remains on the book.
        assert_eq!(eng.book(SymbolId(1)).unwrap().best_ask(), Some(Price(50_000_000)));
    }

    #[test]
    fn sell_stop_triggers_when_mark_falls() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        add_user(&mut eng, 2);
        deposit(&mut eng, 1, 1, 100_000_000);
        deposit(&mut eng, 2, 2, 10_000_000_000);

        // Resting bid at 40e6.
        place(&mut eng, 2, 1, Side::Bid, 40_000_000, 50_000_000, TimeInForce::Gtc);
        // Sell stop @ 45e6 with limit 39e6. Mark not set → pending.
        place_stop_limit(&mut eng, 1, 2, Side::Ask, 45_000_000, 39_000_000, 50_000_000);

        // Set mark BELOW the stop price → triggers.
        set_mark(&mut eng, 44_000_000);
        // Bid consumed.
        assert!(eng.book(SymbolId(1)).unwrap().best_bid().is_none());
    }

    #[test]
    fn stop_does_not_trigger_if_mark_doesnt_cross() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        add_user(&mut eng, 2);
        deposit(&mut eng, 1, 1, 100_000_000);
        deposit(&mut eng, 2, 2, 10_000_000_000);

        place(&mut eng, 1, 1, Side::Ask, 50_000_000, 100_000_000, TimeInForce::Gtc);
        place_stop_limit(&mut eng, 2, 2, Side::Bid, 55_000_000, 60_000_000, 50_000_000);

        // Mark moves but doesn't reach 55e6.
        set_mark(&mut eng, 54_999_999);
        assert!(eng.book(SymbolId(1)).unwrap().best_ask().is_some());
    }

    // ---- ModifyOrder (M5.3) ----

    fn modify_order(
        eng: &mut MatchingEngine,
        uid: u64,
        order_id: u64,
        new_price: Option<i64>,
        new_size: Option<i64>,
    ) -> CommandReceipt {
        eng.submit(
            Command::ModifyOrder(me_types::ModifyOrder {
                user_id: UserId(uid),
                symbol_id: SymbolId(1),
                order_id: OrderId(order_id),
                new_price: new_price.map(Price),
                new_size: new_size.map(Size),
            }),
            Timestamp(0),
        )
    }

    fn place_get_order_id(
        eng: &mut MatchingEngine,
        uid: u64,
        coid: u64,
        side: Side,
        price: i64,
        size: i64,
    ) -> OrderId {
        let r = place(eng, uid, coid, side, price, size, TimeInForce::Gtc);
        match r.events.iter().find(|e| matches!(e, Event::OrderAccepted(_))) {
            Some(Event::OrderAccepted(a)) => a.order_id,
            _ => panic!("expected OrderAccepted"),
        }
    }

    #[test]
    fn modify_changes_price_loses_priority() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        deposit(&mut eng, 1, 2, 10_000_000_000);

        let oid = place_get_order_id(&mut eng, 1, 1, Side::Bid, 50_000_000, 10_000_000);

        let r = modify_order(&mut eng, 1, oid.0, Some(48_000_000), None);
        // Old order cancelled, new order placed at new price.
        let cancelled = r.events.iter().any(|e| matches!(e, Event::OrderCancelled(c) if c.order_id == oid));
        let accepted_new = r.events.iter().any(|e| matches!(e, Event::OrderAccepted(a) if a.order_id != oid));
        assert!(cancelled, "old order should be cancelled");
        assert!(accepted_new, "new order should be accepted");
        assert_eq!(eng.book(SymbolId(1)).unwrap().best_bid(), Some(Price(48_000_000)));
    }

    #[test]
    fn modify_reduces_size() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        deposit(&mut eng, 1, 2, 10_000_000_000);

        let oid = place_get_order_id(&mut eng, 1, 1, Side::Bid, 50_000_000, 10_000_000);
        let r = modify_order(&mut eng, 1, oid.0, None, Some(4_000_000));
        assert!(matches!(r.status, CommandStatus::Accepted));
        // New order's size is 4M, at original price.
        let new_oid = match r.events.iter().find(|e| matches!(e, Event::OrderAccepted(a) if a.order_id != oid)) {
            Some(Event::OrderAccepted(a)) => a.order_id,
            _ => panic!("no new accepted"),
        };
        let new_order = eng.book(SymbolId(1)).unwrap().get_order(new_oid).unwrap();
        assert_eq!(new_order.size_remaining, Size(4_000_000));
        assert_eq!(new_order.price, Price(50_000_000));
    }

    #[test]
    fn modify_increases_size_with_sufficient_funds() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        deposit(&mut eng, 1, 2, 10_000_000_000);

        let oid = place_get_order_id(&mut eng, 1, 1, Side::Bid, 50_000_000, 5_000_000);
        let r = modify_order(&mut eng, 1, oid.0, None, Some(20_000_000));
        assert!(matches!(r.status, CommandStatus::Accepted));
        let new_oid = match r.events.iter().find(|e| matches!(e, Event::OrderAccepted(a) if a.order_id != oid)) {
            Some(Event::OrderAccepted(a)) => a.order_id,
            _ => panic!("no new accepted"),
        };
        let new_order = eng.book(SymbolId(1)).unwrap().get_order(new_oid).unwrap();
        assert_eq!(new_order.size_remaining, Size(20_000_000));
    }

    #[test]
    fn modify_unknown_order_rejects() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        deposit(&mut eng, 1, 2, 10_000_000_000);

        let r = modify_order(&mut eng, 1, 999, Some(48_000_000), None);
        assert!(matches!(r.status, CommandStatus::Rejected(RejectReason::UnknownOrder)));
    }

    #[test]
    fn modify_by_non_owner_rejects() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        add_user(&mut eng, 2);
        deposit(&mut eng, 1, 2, 10_000_000_000);

        let oid = place_get_order_id(&mut eng, 1, 1, Side::Bid, 50_000_000, 10_000_000);
        // User 2 tries to modify user 1's order.
        let r = modify_order(&mut eng, 2, oid.0, Some(48_000_000), None);
        assert!(matches!(r.status, CommandStatus::Rejected(RejectReason::UnknownOrder)));
        // Original order still on the book.
        assert_eq!(eng.book(SymbolId(1)).unwrap().best_bid(), Some(Price(50_000_000)));
    }

    #[test]
    fn modify_returns_full_hold_when_replacement_fails_pre_check() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        // Deposit just enough for the initial order plus tiny buffer.
        deposit(&mut eng, 1, 2, 600_000_000);

        // 10M at 50e6 = notional 5_000_000, + 0.2% fee = 10_000 → hold ~5_010_000.
        let oid = place_get_order_id(&mut eng, 1, 1, Side::Bid, 50_000_000, 10_000_000);
        // Try to grow to 200M (notional 100_000_000) — too expensive.
        let r = modify_order(&mut eng, 1, oid.0, None, Some(2_000_000_000));
        assert!(matches!(r.status, CommandStatus::Rejected(RejectReason::InsufficientFunds)));
        // Old order is gone (cancelled atomically), held funds returned.
        assert!(eng.book(SymbolId(1)).unwrap().get_order(oid).is_none());
        let user1 = eng.risk().account(UserId(1)).unwrap();
        assert_eq!(user1.held(CurrencyId(2)).raw(), 0);
    }

    #[test]
    fn stop_without_stop_price_rejects() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        deposit(&mut eng, 1, 2, 10_000_000_000);
        let r = eng.submit(
            Command::PlaceOrder(PlaceOrder {
                user_id: UserId(1),
                symbol_id: SymbolId(1),
                client_order_id: ClientOrderId(1),
                side: Side::Bid,
                order_type: OrderType::StopLimit,
                time_in_force: TimeInForce::Gtc,
                price: Some(Price(60_000_000)),
                size: Size(50_000_000),
                reserve_price: None,
                stop_price: None,
                visible_size: None,
                self_trade_prevention: SelfTradePrevention::None,
            }),
            Timestamp(0),
        );
        assert!(matches!(r.status, CommandStatus::Rejected(RejectReason::MissingPrice)));
    }

    #[test]
    fn market_with_no_liquidity_cancels() {
        let mut eng = build_engine();
        add_user(&mut eng, 1);
        deposit(&mut eng, 1, 2, 10_000_000_000);

        // Empty ask side — Market Bid finds nothing.
        let r = place_market(&mut eng, 1, 1, Side::Bid, Some(50_000_000), 50_000_000);
        // No fill, IOC drops it → Cancelled.
        assert!(matches!(r.status, CommandStatus::Cancelled));
    }

    #[test]
    fn submit_batch_empty_is_noop() {
        let mut eng = build_engine();
        let before = eng.last_applied_seq();
        let receipts = eng.submit_batch(vec![]);
        assert!(receipts.is_empty());
        assert_eq!(eng.last_applied_seq(), before);
    }
}
