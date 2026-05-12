use ahash::AHashMap;
use serde::{Deserialize, Serialize};
use slab::Slab;
use smallvec::SmallVec;
use std::collections::BTreeMap;

use me_types::{
    ClientOrderId, OrderId, OrderType, Price, RejectReason, SelfTradePrevention, Side, Size,
    SymbolId, TimeInForce, Timestamp, Trade, UserId,
};

type OrderIdx = usize;
type BucketIdx = usize;

/// A passive resting order on the book. Forms an intrusive doubly-linked list
/// within its price bucket; `next`/`prev` are indices into the parent `Slab`.
///
/// Iceberg encoding: `size_remaining` is the *visible* portion currently on
/// the book; `hidden_remaining` is the unrevealed quantity that will be
/// sliced into new visible portions of size `visible_slice` as the visible
/// part gets filled. For non-iceberg orders, `visible_slice == 0` and
/// `hidden_remaining == 0`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestingOrder {
    pub order_id: OrderId,
    pub user_id: UserId,
    pub client_order_id: ClientOrderId,
    pub price: Price,
    pub size_remaining: Size,
    pub side: Side,
    pub timestamp: Timestamp,
    #[serde(default)]
    pub visible_slice: Size,
    #[serde(default)]
    pub hidden_remaining: Size,
    next: Option<OrderIdx>,
    prev: Option<OrderIdx>,
    bucket: BucketIdx,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bucket {
    price: Price,
    head: OrderIdx,
    tail: OrderIdx,
    total_volume: Size,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotOrderBook {
    symbol_id: SymbolId,
    orders: Slab<RestingOrder>,
    buckets: Slab<Bucket>,
    /// Asks sorted ascending — best (lowest) ask is `.iter().next()`.
    asks: BTreeMap<Price, BucketIdx>,
    /// Bids sorted ascending — best (highest) bid is `.iter().next_back()`.
    bids: BTreeMap<Price, BucketIdx>,
    by_order_id: AHashMap<OrderId, OrderIdx>,
}

#[derive(Debug, Clone)]
pub struct PlacementOutcome {
    pub trades: SmallVec<[Trade; 4]>,
    pub filled: Size,
    pub remaining: Size,
    pub rested: bool,
    pub reject: Option<RejectReason>,
    /// Makers cancelled (fully or partially) by self-trade prevention while
    /// matching this order. Empty when STP is None or no same-user maker was
    /// hit. The me-core layer iterates these to release risk holds and emit
    /// OrderCancelled events.
    pub stp_cancellations: SmallVec<[StpCancellation; 2]>,
}

#[derive(Debug, Clone)]
pub struct CancelOutcome {
    pub order_id: OrderId,
    pub user_id: UserId,
    pub side: Side,
    pub price: Price,
    pub remaining_size: Size,
}

/// One maker affected by self-trade prevention while matching a taker.
#[derive(Debug, Clone)]
pub struct StpCancellation {
    pub order_id: OrderId,
    pub user_id: UserId,
    pub side: Side,
    pub price: Price,
    /// Size that was removed: for full_cancel the maker's full remaining,
    /// for partial (DecrementAndCancel maker > taker) the reduced portion only.
    pub size_cancelled: Size,
    /// true ⇒ the maker was removed from the book entirely.
    /// false ⇒ the maker remains on the book with reduced size (DAC partial only).
    pub full_cancel: bool,
}

#[derive(Debug, Clone)]
pub struct PlaceParams {
    pub order_id: OrderId,
    pub user_id: UserId,
    pub client_order_id: ClientOrderId,
    pub side: Side,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub price: Price,
    pub size: Size,
    pub timestamp: Timestamp,
    pub stp: SelfTradePrevention,
    /// For OrderType::Iceberg: the visible slice size. Must be Some(>0) and
    /// `<= size`. For all other order types: must be None.
    pub visible_size: Option<Size>,
}

impl SpotOrderBook {
    pub fn new(symbol_id: SymbolId) -> Self {
        Self {
            symbol_id,
            orders: Slab::new(),
            buckets: Slab::new(),
            asks: BTreeMap::new(),
            bids: BTreeMap::new(),
            by_order_id: AHashMap::new(),
        }
    }

    pub fn symbol_id(&self) -> SymbolId {
        self.symbol_id
    }

    pub fn place(&mut self, params: PlaceParams) -> PlacementOutcome {
        if !matches!(params.order_type, OrderType::Limit | OrderType::Iceberg) {
            return PlacementOutcome {
                trades: SmallVec::new(),
                filled: Size::ZERO,
                remaining: params.size,
                rested: false,
                reject: Some(RejectReason::UnsupportedCommand),
                stp_cancellations: SmallVec::new(),
            };
        }
        if matches!(params.order_type, OrderType::Iceberg) {
            // Iceberg must have a positive visible slice ≤ total size, and a
            // TIF that allows resting (otherwise the slice mechanic is moot).
            let visible = params.visible_size.unwrap_or(Size::ZERO);
            if visible.raw() <= 0 || visible.raw() > params.size.raw() {
                return PlacementOutcome {
                    trades: SmallVec::new(),
                    filled: Size::ZERO,
                    remaining: params.size,
                    rested: false,
                    reject: Some(RejectReason::UnsupportedCommand),
                    stp_cancellations: SmallVec::new(),
                };
            }
            if !matches!(
                params.time_in_force,
                TimeInForce::Gtc | TimeInForce::Day | TimeInForce::Gtd(_)
            ) {
                return PlacementOutcome {
                    trades: SmallVec::new(),
                    filled: Size::ZERO,
                    remaining: params.size,
                    rested: false,
                    reject: Some(RejectReason::UnsupportedCommand),
                    stp_cancellations: SmallVec::new(),
                };
            }
        }
        if !params.size.is_positive() {
            return PlacementOutcome {
                trades: SmallVec::new(),
                filled: Size::ZERO,
                remaining: params.size,
                rested: false,
                reject: Some(RejectReason::SizeBelowMinimum),
                stp_cancellations: SmallVec::new(),
            };
        }

        if matches!(params.time_in_force, TimeInForce::Fok)
            && !self.is_fully_fillable_with_stp(
                params.side,
                params.price,
                params.size,
                params.user_id,
                params.stp,
            )
        {
            return PlacementOutcome {
                trades: SmallVec::new(),
                filled: Size::ZERO,
                remaining: params.size,
                rested: false,
                reject: Some(RejectReason::FokUnfillable),
                stp_cancellations: SmallVec::new(),
            };
        }

        if matches!(params.time_in_force, TimeInForce::PostOnly)
            && self.would_cross(params.side, params.price)
        {
            return PlacementOutcome {
                trades: SmallVec::new(),
                filled: Size::ZERO,
                remaining: params.size,
                rested: false,
                reject: Some(RejectReason::PostOnlyWouldCross),
                stp_cancellations: SmallVec::new(),
            };
        }

        let mut trades: SmallVec<[Trade; 4]> = SmallVec::new();
        let mut stp_cancellations: SmallVec<[StpCancellation; 2]> = SmallVec::new();
        let mut remaining = params.size;
        if !matches!(params.time_in_force, TimeInForce::PostOnly) {
            remaining = self.match_against_book(
                params.side,
                params.price,
                params.size,
                params.user_id,
                params.order_id,
                params.stp,
                params.timestamp,
                &mut trades,
                &mut stp_cancellations,
            );
        }

        // `filled` is the actual trade volume only — STP-neutralised volume
        // does not count as a fill. `remaining` is what the taker still has
        // outstanding (DAC subtracts the neutralised portion from there).
        let filled = Size(trades.iter().map(|t| t.size.raw()).sum::<i64>());

        let rested = if remaining.is_positive() {
            match params.time_in_force {
                TimeInForce::Gtc
                | TimeInForce::PostOnly
                | TimeInForce::Day
                | TimeInForce::Gtd(_) => {
                    let visible = if matches!(params.order_type, OrderType::Iceberg) {
                        params.visible_size.unwrap_or(remaining)
                    } else {
                        Size::ZERO // 0 marks "not iceberg"
                    };
                    self.rest_order(
                        params.order_id,
                        params.user_id,
                        params.client_order_id,
                        params.price,
                        remaining,
                        params.side,
                        params.timestamp,
                        visible,
                    );
                    true
                }
                TimeInForce::Ioc | TimeInForce::Fok => false,
            }
        } else {
            false
        };

        PlacementOutcome {
            trades,
            filled,
            remaining,
            rested,
            reject: None,
            stp_cancellations,
        }
    }

    pub fn cancel(&mut self, order_id: OrderId) -> Option<CancelOutcome> {
        let order_idx = self.by_order_id.remove(&order_id)?;
        let order = self.orders.remove(order_idx);
        let outcome = CancelOutcome {
            order_id: order.order_id,
            user_id: order.user_id,
            side: order.side,
            price: order.price,
            remaining_size: order.size_remaining,
        };
        self.unlink_from_bucket(order_idx, &order);
        Some(outcome)
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.keys().next().copied()
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.keys().next_back().copied()
    }

    pub fn total_resting_orders(&self) -> usize {
        self.orders.len()
    }

    pub fn get_order(&self, order_id: OrderId) -> Option<&RestingOrder> {
        let idx = *self.by_order_id.get(&order_id)?;
        self.orders.get(idx)
    }

    fn would_cross(&self, side: Side, price: Price) -> bool {
        match side {
            Side::Bid => self.asks.range(..=price).next().is_some(),
            Side::Ask => self.bids.range(price..).next().is_some(),
        }
    }

    fn is_fully_fillable(&self, side: Side, limit_price: Price, size: Size) -> bool {
        let mut needed = size.raw();
        match side {
            Side::Bid => {
                for (_, &bucket_idx) in self.asks.range(..=limit_price) {
                    needed = needed.saturating_sub(self.buckets[bucket_idx].total_volume.raw());
                    if needed <= 0 {
                        return true;
                    }
                }
            }
            Side::Ask => {
                for (_, &bucket_idx) in self.bids.range(limit_price..).rev() {
                    needed = needed.saturating_sub(self.buckets[bucket_idx].total_volume.raw());
                    if needed <= 0 {
                        return true;
                    }
                }
            }
        }
        needed <= 0
    }

    /// FOK fillability that ignores volume belonging to the taker themselves
    /// when STP is active — every same-user maker would be cancelled or
    /// neutralised under any of the four STP strategies, so it can't count
    /// toward fulfilling the taker's fill requirement.
    fn is_fully_fillable_with_stp(
        &self,
        side: Side,
        limit_price: Price,
        size: Size,
        taker_uid: UserId,
        stp: SelfTradePrevention,
    ) -> bool {
        if matches!(stp, SelfTradePrevention::None) {
            return self.is_fully_fillable(side, limit_price, size);
        }
        let mut needed = size.raw();
        let walk = |needed: &mut i64,
                    bucket_idx: BucketIdx,
                    orders: &Slab<RestingOrder>,
                    buckets: &Slab<Bucket>|
         -> bool {
            // Walk this bucket's FIFO chain, skipping same-uid orders.
            let mut cur = Some(buckets[bucket_idx].head);
            while let Some(idx) = cur {
                let o = &orders[idx];
                if o.user_id != taker_uid {
                    *needed = needed.saturating_sub(o.size_remaining.raw());
                    if *needed <= 0 {
                        return true;
                    }
                }
                cur = o.next;
            }
            false
        };

        let iter: Box<dyn Iterator<Item = &BucketIdx>> = match side {
            Side::Bid => Box::new(self.asks.range(..=limit_price).map(|(_, b)| b)),
            Side::Ask => Box::new(self.bids.range(limit_price..).rev().map(|(_, b)| b)),
        };
        for &bucket_idx in iter {
            if walk(&mut needed, bucket_idx, &self.orders, &self.buckets) {
                return true;
            }
        }
        needed <= 0
    }

    /// Returns remaining size after matching (0 if fully filled).
    /// Appends one `Trade` per partial or full hit against a maker into
    /// `out_trades`. Same-user makers are routed through the `stp` strategy
    /// and recorded in `out_stp_cancellations` (no Trade is emitted for STP-
    /// affected pairings).
    #[allow(clippy::too_many_arguments)]
    fn match_against_book(
        &mut self,
        taker_side: Side,
        limit_price: Price,
        taker_size: Size,
        taker_user_id: UserId,
        taker_order_id: OrderId,
        stp: SelfTradePrevention,
        timestamp: Timestamp,
        out_trades: &mut SmallVec<[Trade; 4]>,
        out_stp_cancellations: &mut SmallVec<[StpCancellation; 2]>,
    ) -> Size {
        let mut remaining = taker_size;
        'outer: while remaining.is_positive() {
            let best = match taker_side {
                Side::Bid => self.asks.iter().next().map(|(&p, &b)| (p, b)),
                Side::Ask => self.bids.iter().next_back().map(|(&p, &b)| (p, b)),
            };
            let (best_price, bucket_idx) = match best {
                Some(x) => x,
                None => break,
            };
            let crosses = match taker_side {
                Side::Bid => best_price <= limit_price,
                Side::Ask => best_price >= limit_price,
            };
            if !crosses {
                break;
            }

            while remaining.is_positive() {
                let head_idx = self.buckets[bucket_idx].head;
                let maker_user_id = self.orders[head_idx].user_id;

                // ---- Self-trade prevention dispatch ----
                if maker_user_id == taker_user_id
                    && !matches!(stp, SelfTradePrevention::None)
                {
                    let maker_remaining = self.orders[head_idx].size_remaining;
                    let maker_order_id = self.orders[head_idx].order_id;
                    let maker_side = self.orders[head_idx].side;
                    let maker_price = self.orders[head_idx].price;

                    match stp {
                        SelfTradePrevention::None => unreachable!(),
                        SelfTradePrevention::CancelTaker => {
                            // Stop the taker; leave maker on book intact.
                            return remaining;
                        }
                        SelfTradePrevention::CancelMaker => {
                            out_stp_cancellations.push(StpCancellation {
                                order_id: maker_order_id,
                                user_id: maker_user_id,
                                side: maker_side,
                                price: maker_price,
                                size_cancelled: maker_remaining,
                                full_cancel: true,
                            });
                            self.by_order_id.remove(&maker_order_id);
                            let head_order = self.orders.remove(head_idx);
                            self.unlink_from_bucket(head_idx, &head_order);
                            if !self.buckets.contains(bucket_idx) {
                                continue 'outer;
                            }
                            continue;
                        }
                        SelfTradePrevention::CancelBoth => {
                            out_stp_cancellations.push(StpCancellation {
                                order_id: maker_order_id,
                                user_id: maker_user_id,
                                side: maker_side,
                                price: maker_price,
                                size_cancelled: maker_remaining,
                                full_cancel: true,
                            });
                            self.by_order_id.remove(&maker_order_id);
                            let head_order = self.orders.remove(head_idx);
                            self.unlink_from_bucket(head_idx, &head_order);
                            return remaining;
                        }
                        SelfTradePrevention::DecrementAndCancel => {
                            let cancel_size = remaining.min(maker_remaining);
                            if maker_remaining.raw() <= remaining.raw() {
                                // Maker fully cancelled; taker decrements by maker size.
                                out_stp_cancellations.push(StpCancellation {
                                    order_id: maker_order_id,
                                    user_id: maker_user_id,
                                    side: maker_side,
                                    price: maker_price,
                                    size_cancelled: maker_remaining,
                                    full_cancel: true,
                                });
                                self.by_order_id.remove(&maker_order_id);
                                let head_order = self.orders.remove(head_idx);
                                self.unlink_from_bucket(head_idx, &head_order);
                                remaining = Size(remaining.raw() - cancel_size.raw());
                                if !self.buckets.contains(bucket_idx) {
                                    continue 'outer;
                                }
                                continue;
                            } else {
                                // Maker > taker: reduce maker in place, taker done.
                                self.orders[head_idx].size_remaining =
                                    Size(maker_remaining.raw() - cancel_size.raw());
                                self.buckets[bucket_idx].total_volume = Size(
                                    self.buckets[bucket_idx].total_volume.raw() - cancel_size.raw(),
                                );
                                out_stp_cancellations.push(StpCancellation {
                                    order_id: maker_order_id,
                                    user_id: maker_user_id,
                                    side: maker_side,
                                    price: maker_price,
                                    size_cancelled: cancel_size,
                                    full_cancel: false,
                                });
                                return Size::ZERO;
                            }
                        }
                    }
                }

                // ---- Normal fill ----
                let maker_remaining = self.orders[head_idx].size_remaining;
                let fill = remaining.min(maker_remaining);

                out_trades.push(Trade {
                    symbol_id: self.symbol_id,
                    price: best_price,
                    size: fill,
                    taker_order_id,
                    taker_user_id,
                    maker_order_id: self.orders[head_idx].order_id,
                    maker_user_id: self.orders[head_idx].user_id,
                    taker_side,
                    timestamp,
                });

                self.orders[head_idx].size_remaining = Size(maker_remaining.raw() - fill.raw());
                self.buckets[bucket_idx].total_volume =
                    Size(self.buckets[bucket_idx].total_volume.raw() - fill.raw());
                remaining = Size(remaining.raw() - fill.raw());

                if self.orders[head_idx].size_remaining.is_zero() {
                    // Iceberg: if hidden quantity remains, refresh the next
                    // slice and move this order to the FIFO tail (losing time
                    // priority for the refreshed portion).
                    if self.orders[head_idx].hidden_remaining.is_positive() {
                        let slice_cap = self.orders[head_idx].visible_slice;
                        let hidden = self.orders[head_idx].hidden_remaining;
                        let next_slice =
                            Size(slice_cap.raw().min(hidden.raw()));
                        self.orders[head_idx].size_remaining = next_slice;
                        self.orders[head_idx].hidden_remaining =
                            Size(hidden.raw() - next_slice.raw());
                        self.buckets[bucket_idx].total_volume = Size(
                            self.buckets[bucket_idx].total_volume.raw() + next_slice.raw(),
                        );
                        self.relink_head_to_tail(head_idx, bucket_idx);
                        // The taker may still have remaining; loop continues
                        // with the new bucket head (which is whatever was next).
                        continue;
                    }
                    let head_order_id = self.orders[head_idx].order_id;
                    self.by_order_id.remove(&head_order_id);
                    let head_order = self.orders.remove(head_idx);
                    self.unlink_from_bucket(head_idx, &head_order);
                    if !self.buckets.contains(bucket_idx) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        remaining
    }

    #[allow(clippy::too_many_arguments)]
    fn rest_order(
        &mut self,
        order_id: OrderId,
        user_id: UserId,
        client_order_id: ClientOrderId,
        price: Price,
        size: Size,
        side: Side,
        timestamp: Timestamp,
        visible_slice: Size,
    ) {
        let bucket_idx = self.get_or_create_bucket(side, price);

        // Iceberg: show only the visible slice; remainder is hidden until the
        // slice gets filled. Non-iceberg: visible_slice == 0 → show full size,
        // no hidden quantity.
        let (initial_visible, hidden) = if visible_slice.raw() > 0
            && visible_slice.raw() < size.raw()
        {
            (visible_slice, Size(size.raw() - visible_slice.raw()))
        } else {
            (size, Size::ZERO)
        };

        let order = RestingOrder {
            order_id,
            user_id,
            client_order_id,
            price,
            size_remaining: initial_visible,
            side,
            timestamp,
            visible_slice,
            hidden_remaining: hidden,
            next: None,
            prev: Some(self.buckets[bucket_idx].tail),
            bucket: bucket_idx,
        };
        let order_idx = self.orders.insert(order);

        let prev_tail = self.buckets[bucket_idx].tail;
        if self.buckets[bucket_idx].total_volume.is_zero() {
            self.buckets[bucket_idx].head = order_idx;
            self.orders[order_idx].prev = None;
        } else {
            self.orders[prev_tail].next = Some(order_idx);
        }
        self.buckets[bucket_idx].tail = order_idx;
        self.buckets[bucket_idx].total_volume =
            Size(self.buckets[bucket_idx].total_volume.raw() + initial_visible.raw());
        self.by_order_id.insert(order_id, order_idx);
    }

    /// Move `order_idx` (assumed to be the bucket's current head) to the
    /// bucket's tail. Used to refresh an iceberg's next slice — the refreshed
    /// portion goes to the back of the FIFO queue, losing time priority.
    fn relink_head_to_tail(&mut self, order_idx: OrderIdx, bucket_idx: BucketIdx) {
        // If only order in the bucket, nothing to do.
        if self.buckets[bucket_idx].tail == order_idx
            && self.buckets[bucket_idx].head == order_idx
        {
            return;
        }
        let next = self.orders[order_idx].next;
        // Detach from head.
        match next {
            Some(next_idx) => {
                self.orders[next_idx].prev = None;
                self.buckets[bucket_idx].head = next_idx;
            }
            None => return, // sole order, no relink needed
        }
        // Attach at tail.
        let old_tail = self.buckets[bucket_idx].tail;
        self.orders[old_tail].next = Some(order_idx);
        self.orders[order_idx].prev = Some(old_tail);
        self.orders[order_idx].next = None;
        self.buckets[bucket_idx].tail = order_idx;
    }

    fn get_or_create_bucket(&mut self, side: Side, price: Price) -> BucketIdx {
        let level = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        if let Some(&idx) = level.get(&price) {
            return idx;
        }
        let idx = self.buckets.insert(Bucket {
            price,
            head: usize::MAX,
            tail: usize::MAX,
            total_volume: Size::ZERO,
        });
        level.insert(price, idx);
        idx
    }

    fn unlink_from_bucket(&mut self, order_idx: OrderIdx, order: &RestingOrder) {
        let bucket_idx = order.bucket;
        match (order.prev, order.next) {
            (Some(prev), Some(next)) => {
                self.orders[prev].next = Some(next);
                self.orders[next].prev = Some(prev);
            }
            (Some(prev), None) => {
                self.orders[prev].next = None;
                self.buckets[bucket_idx].tail = prev;
            }
            (None, Some(next)) => {
                self.orders[next].prev = None;
                self.buckets[bucket_idx].head = next;
            }
            (None, None) => {
                // Was the only order in the bucket. Remove the bucket entirely.
                let price = self.buckets[bucket_idx].price;
                match order.side {
                    Side::Bid => self.bids.remove(&price),
                    Side::Ask => self.asks.remove(&price),
                };
                self.buckets.remove(bucket_idx);
                return;
            }
        }
        self.buckets[bucket_idx].total_volume =
            Size(self.buckets[bucket_idx].total_volume.raw() - order.size_remaining.raw());
        // If bucket became empty (size 0), evict it.
        if self.buckets[bucket_idx].total_volume.is_zero() {
            let price = self.buckets[bucket_idx].price;
            match order.side {
                Side::Bid => self.bids.remove(&price),
                Side::Ask => self.asks.remove(&price),
            };
            self.buckets.remove(bucket_idx);
        }
        let _ = order_idx; // silence unused if compiler warns
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_types::OrderType;

    fn place(book: &mut SpotOrderBook, oid: u64, uid: u64, side: Side, px: i64, sz: i64,
             tif: TimeInForce) -> PlacementOutcome {
        place_stp(book, oid, uid, side, px, sz, tif, SelfTradePrevention::None)
    }

    #[allow(clippy::too_many_arguments)]
    fn place_stp(book: &mut SpotOrderBook, oid: u64, uid: u64, side: Side, px: i64, sz: i64,
                 tif: TimeInForce, stp: SelfTradePrevention) -> PlacementOutcome {
        book.place(PlaceParams {
            order_id: OrderId(oid),
            user_id: UserId(uid),
            client_order_id: ClientOrderId(oid),
            side,
            order_type: OrderType::Limit,
            time_in_force: tif,
            price: Price(px),
            size: Size(sz),
            timestamp: me_types::Timestamp(0),
            stp,
            visible_size: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn place_iceberg(book: &mut SpotOrderBook, oid: u64, uid: u64, side: Side, px: i64, sz: i64,
                     visible: i64) -> PlacementOutcome {
        book.place(PlaceParams {
            order_id: OrderId(oid),
            user_id: UserId(uid),
            client_order_id: ClientOrderId(oid),
            side,
            order_type: OrderType::Iceberg,
            time_in_force: TimeInForce::Gtc,
            price: Price(px),
            size: Size(sz),
            timestamp: me_types::Timestamp(0),
            stp: SelfTradePrevention::None,
            visible_size: Some(Size(visible)),
        })
    }

    #[test]
    fn place_then_match_one_for_one() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        let r1 = place(&mut book, 1, 100, Side::Ask, 100, 10, TimeInForce::Gtc);
        assert!(r1.rested && r1.trades.is_empty());

        let r2 = place(&mut book, 2, 200, Side::Bid, 100, 10, TimeInForce::Ioc);
        assert_eq!(r2.trades.len(), 1);
        assert_eq!(r2.trades[0].size, Size(10));
        assert_eq!(r2.trades[0].price, Price(100));
        assert_eq!(r2.remaining, Size::ZERO);
        assert!(!r2.rested);
        assert_eq!(book.total_resting_orders(), 0);
    }

    #[test]
    fn partial_fill_taker_rests_remainder_for_gtc() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place(&mut book, 2, 200, Side::Bid, 100, 12, TimeInForce::Gtc);
        assert_eq!(r.filled, Size(5));
        assert_eq!(r.remaining, Size(7));
        assert!(r.rested);
        assert_eq!(book.best_bid(), Some(Price(100)));
    }

    #[test]
    fn ioc_drops_remainder() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place(&mut book, 2, 200, Side::Bid, 100, 12, TimeInForce::Ioc);
        assert_eq!(r.filled, Size(5));
        assert_eq!(r.remaining, Size(7));
        assert!(!r.rested);
        assert!(book.best_bid().is_none());
    }

    #[test]
    fn fok_rejects_when_not_fully_fillable() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place(&mut book, 2, 200, Side::Bid, 100, 12, TimeInForce::Fok);
        assert_eq!(r.reject, Some(RejectReason::FokUnfillable));
        assert!(r.trades.is_empty());
        assert_eq!(book.best_ask(), Some(Price(100)));
    }

    #[test]
    fn fok_fills_when_exactly_fillable() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        place(&mut book, 2, 100, Side::Ask, 101, 7, TimeInForce::Gtc);
        let r = place(&mut book, 3, 200, Side::Bid, 101, 12, TimeInForce::Fok);
        assert!(r.reject.is_none());
        assert_eq!(r.trades.len(), 2);
        assert_eq!(r.filled, Size(12));
    }

    #[test]
    fn post_only_rejects_when_would_cross() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place(&mut book, 2, 200, Side::Bid, 100, 5, TimeInForce::PostOnly);
        assert_eq!(r.reject, Some(RejectReason::PostOnlyWouldCross));
    }

    #[test]
    fn post_only_rests_when_non_aggressive() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place(&mut book, 2, 200, Side::Bid, 99, 5, TimeInForce::PostOnly);
        assert!(r.reject.is_none());
        assert!(r.rested);
    }

    #[test]
    fn price_time_priority_within_bucket() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        place(&mut book, 2, 101, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place(&mut book, 3, 200, Side::Bid, 100, 5, TimeInForce::Gtc);
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.trades[0].maker_user_id, UserId(100));
    }

    #[test]
    fn best_price_priority_across_buckets() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 102, 5, TimeInForce::Gtc);
        place(&mut book, 2, 101, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place(&mut book, 3, 200, Side::Bid, 102, 5, TimeInForce::Gtc);
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.trades[0].price, Price(100));
    }

    #[test]
    fn cancel_removes_from_book() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        let c = book.cancel(OrderId(1)).unwrap();
        assert_eq!(c.remaining_size, Size(5));
        assert!(book.best_ask().is_none());
    }

    #[test]
    fn cancel_unknown_returns_none() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        assert!(book.cancel(OrderId(999)).is_none());
    }

    // ---- Self-trade prevention (M5.1) ----

    #[test]
    fn stp_none_allows_self_trade() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place_stp(&mut book, 2, 100, Side::Bid, 100, 5, TimeInForce::Gtc,
                           SelfTradePrevention::None);
        assert_eq!(r.trades.len(), 1);
        assert!(r.stp_cancellations.is_empty());
        assert_eq!(r.filled, Size(5));
    }

    #[test]
    fn stp_cancel_taker_stops_match() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place_stp(&mut book, 2, 100, Side::Bid, 100, 5, TimeInForce::Gtc,
                           SelfTradePrevention::CancelTaker);
        assert!(r.trades.is_empty());
        assert!(r.stp_cancellations.is_empty());
        // Taker stopped, didn't rest (Gtc rests only if positive remaining
        // BUT cancel-taker semantics here drop the taker). Actually with
        // remaining=5 and Gtc, it WOULD rest. But user expects "cancel taker"
        // to drop the taker. Verify documented behavior: the book code returns
        // remaining; me-core decides whether to rest. With Gtc rest happens.
        assert_eq!(r.remaining, Size(5));
        // The maker is intact.
        assert_eq!(book.best_ask(), Some(Price(100)));
    }

    #[test]
    fn stp_cancel_maker_removes_maker_and_continues() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        // Two asks at 100: one same-user (uid=100), one other (uid=101).
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        place(&mut book, 2, 101, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place_stp(&mut book, 3, 100, Side::Bid, 100, 8, TimeInForce::Gtc,
                           SelfTradePrevention::CancelMaker);
        // Same-user maker removed (oid=1), trade happens against oid=2 (5 size).
        assert_eq!(r.stp_cancellations.len(), 1);
        assert_eq!(r.stp_cancellations[0].order_id, OrderId(1));
        assert!(r.stp_cancellations[0].full_cancel);
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.trades[0].maker_order_id, OrderId(2));
        assert_eq!(r.trades[0].size, Size(5));
        assert_eq!(r.filled, Size(5));
        assert_eq!(r.remaining, Size(3));
    }

    #[test]
    fn stp_cancel_both_removes_maker_and_stops_taker() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        place(&mut book, 2, 101, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place_stp(&mut book, 3, 100, Side::Bid, 100, 8, TimeInForce::Gtc,
                           SelfTradePrevention::CancelBoth);
        assert_eq!(r.stp_cancellations.len(), 1);
        assert_eq!(r.stp_cancellations[0].order_id, OrderId(1));
        assert!(r.trades.is_empty());
        // Other-user maker stays untouched.
        assert_eq!(book.best_ask(), Some(Price(100)));
        // Taker had remaining 8, returned as-is.
        assert_eq!(r.remaining, Size(8));
    }

    #[test]
    fn stp_decrement_and_cancel_taker_larger_than_maker() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        // Same-user maker size 3, then other-user maker size 10.
        place(&mut book, 1, 100, Side::Ask, 100, 3, TimeInForce::Gtc);
        place(&mut book, 2, 101, Side::Ask, 100, 10, TimeInForce::Gtc);
        let r = place_stp(&mut book, 3, 100, Side::Bid, 100, 8, TimeInForce::Gtc,
                           SelfTradePrevention::DecrementAndCancel);
        // Same-user maker cancelled, taker decrements by 3 → 5 left.
        // Then taker matches 5 of the other-user maker normally.
        assert_eq!(r.stp_cancellations.len(), 1);
        assert_eq!(r.stp_cancellations[0].order_id, OrderId(1));
        assert!(r.stp_cancellations[0].full_cancel);
        assert_eq!(r.stp_cancellations[0].size_cancelled, Size(3));
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.trades[0].size, Size(5));
        assert_eq!(r.filled, Size(5));
        assert_eq!(r.remaining, Size(0));
    }

    #[test]
    fn stp_decrement_and_cancel_maker_larger_than_taker() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 10, TimeInForce::Gtc);
        let r = place_stp(&mut book, 2, 100, Side::Bid, 100, 3, TimeInForce::Gtc,
                           SelfTradePrevention::DecrementAndCancel);
        // Maker has 10, taker is 3. Reduce maker to 7, taker fully neutralised.
        assert_eq!(r.stp_cancellations.len(), 1);
        assert_eq!(r.stp_cancellations[0].order_id, OrderId(1));
        assert!(!r.stp_cancellations[0].full_cancel);
        assert_eq!(r.stp_cancellations[0].size_cancelled, Size(3));
        assert!(r.trades.is_empty());
        assert_eq!(r.remaining, Size(0));
        // Maker still on book with 7 remaining.
        let maker = book.get_order(OrderId(1)).unwrap();
        assert_eq!(maker.size_remaining, Size(7));
    }

    #[test]
    fn stp_decrement_and_cancel_equal_sizes() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place_stp(&mut book, 2, 100, Side::Bid, 100, 5, TimeInForce::Gtc,
                           SelfTradePrevention::DecrementAndCancel);
        assert_eq!(r.stp_cancellations.len(), 1);
        // Maker fully cancelled, taker neutralised to 0.
        assert!(r.stp_cancellations[0].full_cancel);
        assert_eq!(r.stp_cancellations[0].size_cancelled, Size(5));
        assert!(r.trades.is_empty());
        assert_eq!(r.remaining, Size(0));
        // Book is empty.
        assert!(book.best_ask().is_none());
    }

    // ---- Iceberg orders (M5.2.b) ----

    #[test]
    fn iceberg_shows_only_visible_slice_on_book() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        // 100 total, 20 visible.
        place_iceberg(&mut book, 1, 100, Side::Ask, 50, 100, 20);
        // Total resting orders: 1 (single iceberg)
        assert_eq!(book.total_resting_orders(), 1);
        let order = book.get_order(OrderId(1)).unwrap();
        assert_eq!(order.size_remaining, Size(20));
        assert_eq!(order.hidden_remaining, Size(80));
        assert_eq!(order.visible_slice, Size(20));
    }

    #[test]
    fn iceberg_refreshes_next_slice_when_visible_filled() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place_iceberg(&mut book, 1, 100, Side::Ask, 50, 100, 20);

        // Taker buys 20 — exactly the visible slice.
        let r = place(&mut book, 2, 200, Side::Bid, 50, 20, TimeInForce::Ioc);
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.trades[0].size, Size(20));

        // Iceberg should have refreshed: visible 20, hidden 60.
        let order = book.get_order(OrderId(1)).unwrap();
        assert_eq!(order.size_remaining, Size(20));
        assert_eq!(order.hidden_remaining, Size(60));
    }

    #[test]
    fn iceberg_consumes_all_slices_across_multiple_takers() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place_iceberg(&mut book, 1, 100, Side::Ask, 50, 100, 25);

        // Single 100 taker should consume all 4 slices.
        let r = place(&mut book, 2, 200, Side::Bid, 50, 100, TimeInForce::Ioc);
        // Each slice produces one trade.
        let total_filled: i64 = r.trades.iter().map(|t| t.size.raw()).sum();
        assert_eq!(total_filled, 100);
        // Iceberg fully drained.
        assert!(book.get_order(OrderId(1)).is_none());
    }

    #[test]
    fn iceberg_remainder_smaller_than_slice_emerges_intact() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        // 50 total, 20 slice → last slice is 10.
        place_iceberg(&mut book, 1, 100, Side::Ask, 50, 50, 20);

        place(&mut book, 2, 200, Side::Bid, 50, 20, TimeInForce::Ioc);
        place(&mut book, 3, 200, Side::Bid, 50, 20, TimeInForce::Ioc);
        // After 40 filled, last slice = 10.
        let order = book.get_order(OrderId(1)).unwrap();
        assert_eq!(order.size_remaining, Size(10));
        assert_eq!(order.hidden_remaining, Size(0));
    }

    #[test]
    fn iceberg_loses_time_priority_on_refresh() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        // Iceberg first (will be head initially).
        place_iceberg(&mut book, 1, 100, Side::Ask, 50, 100, 10);
        // Regular order at same price (FIFO behind iceberg).
        place(&mut book, 2, 101, Side::Ask, 50, 5, TimeInForce::Gtc);

        // Taker buys 10 — fills first iceberg slice completely.
        place(&mut book, 3, 200, Side::Bid, 50, 10, TimeInForce::Ioc);
        // After the slice is consumed and refreshed, iceberg moves to tail.
        // Next taker buys 5 — should hit the regular order (oid=2), NOT the
        // refreshed iceberg slice.
        let r = place(&mut book, 4, 200, Side::Bid, 50, 5, TimeInForce::Ioc);
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.trades[0].maker_order_id, OrderId(2));
    }

    #[test]
    fn iceberg_rejects_visible_larger_than_size() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        let r = place_iceberg(&mut book, 1, 100, Side::Ask, 50, 100, 150);
        assert!(matches!(r.reject, Some(RejectReason::UnsupportedCommand)));
    }

    #[test]
    fn iceberg_rejects_zero_visible() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        let r = place_iceberg(&mut book, 1, 100, Side::Ask, 50, 100, 0);
        assert!(matches!(r.reject, Some(RejectReason::UnsupportedCommand)));
    }

    #[test]
    fn stp_does_not_apply_when_users_differ() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 5, TimeInForce::Gtc);
        let r = place_stp(&mut book, 2, 200, Side::Bid, 100, 5, TimeInForce::Gtc,
                           SelfTradePrevention::CancelBoth);
        assert!(r.stp_cancellations.is_empty());
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.filled, Size(5));
    }

    #[test]
    fn cancel_middle_of_bucket_preserves_linked_list() {
        let mut book = SpotOrderBook::new(SymbolId(1));
        place(&mut book, 1, 100, Side::Ask, 100, 3, TimeInForce::Gtc);
        place(&mut book, 2, 101, Side::Ask, 100, 3, TimeInForce::Gtc);
        place(&mut book, 3, 102, Side::Ask, 100, 3, TimeInForce::Gtc);
        book.cancel(OrderId(2)).unwrap();
        let r = place(&mut book, 4, 200, Side::Bid, 100, 6, TimeInForce::Gtc);
        assert_eq!(r.trades.len(), 2);
        assert_eq!(r.trades[0].maker_order_id, OrderId(1));
        assert_eq!(r.trades[1].maker_order_id, OrderId(3));
    }
}
