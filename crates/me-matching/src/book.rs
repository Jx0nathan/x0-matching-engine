use ahash::AHashMap;
use serde::{Deserialize, Serialize};
use slab::Slab;
use smallvec::SmallVec;
use std::collections::BTreeMap;

use me_types::{
    ClientOrderId, OrderId, OrderType, Price, RejectReason, Side, Size, SymbolId, TimeInForce,
    Timestamp, Trade, UserId,
};

type OrderIdx = usize;
type BucketIdx = usize;

/// A passive resting order on the book. Forms an intrusive doubly-linked list
/// within its price bucket; `next`/`prev` are indices into the parent `Slab`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestingOrder {
    pub order_id: OrderId,
    pub user_id: UserId,
    pub client_order_id: ClientOrderId,
    pub price: Price,
    pub size_remaining: Size,
    pub side: Side,
    pub timestamp: Timestamp,
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
}

#[derive(Debug, Clone)]
pub struct CancelOutcome {
    pub order_id: OrderId,
    pub user_id: UserId,
    pub side: Side,
    pub price: Price,
    pub remaining_size: Size,
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
        if !matches!(params.order_type, OrderType::Limit) {
            return PlacementOutcome {
                trades: SmallVec::new(),
                filled: Size::ZERO,
                remaining: params.size,
                rested: false,
                reject: Some(RejectReason::UnsupportedCommand),
            };
        }
        if !params.size.is_positive() {
            return PlacementOutcome {
                trades: SmallVec::new(),
                filled: Size::ZERO,
                remaining: params.size,
                rested: false,
                reject: Some(RejectReason::SizeBelowMinimum),
            };
        }

        if matches!(params.time_in_force, TimeInForce::Fok)
            && !self.is_fully_fillable(params.side, params.price, params.size)
        {
            return PlacementOutcome {
                trades: SmallVec::new(),
                filled: Size::ZERO,
                remaining: params.size,
                rested: false,
                reject: Some(RejectReason::FokUnfillable),
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
            };
        }

        let mut trades: SmallVec<[Trade; 4]> = SmallVec::new();
        let mut remaining = params.size;
        if !matches!(params.time_in_force, TimeInForce::PostOnly) {
            remaining = self.match_against_book(
                params.side,
                params.price,
                params.size,
                params.user_id,
                params.order_id,
                params.timestamp,
                &mut trades,
            );
        }

        let filled = Size(params.size.raw() - remaining.raw());

        let rested = if remaining.is_positive() {
            match params.time_in_force {
                TimeInForce::Gtc
                | TimeInForce::PostOnly
                | TimeInForce::Day
                | TimeInForce::Gtd(_) => {
                    self.rest_order(
                        params.order_id,
                        params.user_id,
                        params.client_order_id,
                        params.price,
                        remaining,
                        params.side,
                        params.timestamp,
                    );
                    true
                }
                TimeInForce::Ioc | TimeInForce::Fok => false,
            }
        } else {
            false
        };

        PlacementOutcome { trades, filled, remaining, rested, reject: None }
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

    /// Returns remaining size after matching (0 if fully filled).
    /// Appends one `Trade` per partial or full hit against a maker into `out_trades`.
    #[allow(clippy::too_many_arguments)]
    fn match_against_book(
        &mut self,
        taker_side: Side,
        limit_price: Price,
        taker_size: Size,
        taker_user_id: UserId,
        taker_order_id: OrderId,
        timestamp: Timestamp,
        out_trades: &mut SmallVec<[Trade; 4]>,
    ) -> Size {
        let mut remaining = taker_size;
        while remaining.is_positive() {
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

            // Walk the FIFO head of this bucket, consuming orders.
            while remaining.is_positive() {
                let head_idx = self.buckets[bucket_idx].head;
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
                    let head_order_id = self.orders[head_idx].order_id;
                    self.by_order_id.remove(&head_order_id);
                    let head_order = self.orders.remove(head_idx);
                    self.unlink_from_bucket(head_idx, &head_order);
                    // unlink may have removed the bucket too; if so, leave the inner loop.
                    if !self.buckets.contains(bucket_idx) {
                        break;
                    }
                } else {
                    // Bucket head still has remaining; if so, taker is done.
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
    ) {
        let bucket_idx = self.get_or_create_bucket(side, price);

        let order = RestingOrder {
            order_id,
            user_id,
            client_order_id,
            price,
            size_remaining: size,
            side,
            timestamp,
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
            Size(self.buckets[bucket_idx].total_volume.raw() + size.raw());
        self.by_order_id.insert(order_id, order_idx);
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
