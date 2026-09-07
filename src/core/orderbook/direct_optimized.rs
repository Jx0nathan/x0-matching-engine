// 本模块自身的 impl 会引用这个已废弃的类型，逐处 allow 太吵，统一在模块层放开。
// 外部使用点仍会收到废弃告警 —— 那正是我们要的。
#![allow(deprecated)]

use crate::api::*;
use crate::core::orderbook::simd_utils::*;
use ahash::AHashMap;
use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

type OrderIdx = usize;

/// `use_simd` 不进快照，但恢复时必须回到"启用"，否则默认值 false 会静默关掉 SIMD
fn simd_enabled_by_default() -> bool {
    true
}

/// SOA 内存布局：订单热数据（缓存友好）
#[derive(Clone, Serialize, Deserialize)]
struct OrderHotData {
    order_ids: Vec<OrderId>,    // 订单 ID
    prices: Vec<Price>,         // 价格
    sizes: Vec<Size>,           // 数量
    filled: Vec<Size>,          // 已成交
    next: Vec<Option<OrderIdx>>, // 链表后继
    prev: Vec<Option<OrderIdx>>, // 链表前驱
    active: Vec<bool>,          // 激活标记
}

/// 订单冷数据（低频访问）
#[derive(Clone, Serialize, Deserialize)]
struct OrderColdData {
    uid: UserId,
    action: OrderAction,
    reserve_price: Price,
    timestamp: i64,
}

/// 预分配订单池（零分配）
#[derive(Clone, Serialize, Deserialize)]
struct OrderPool {
    hot: OrderHotData,
    cold: Vec<OrderColdData>,
    free_list: Vec<OrderIdx>,
    capacity: usize,
}

impl OrderPool {
    fn new(capacity: usize) -> Self {
        let mut free_list = Vec::with_capacity(capacity);
        for i in (0..capacity).rev() {
            free_list.push(i);
        }
        
        Self {
            hot: OrderHotData {
                order_ids: vec![0; capacity],
                prices: vec![0; capacity],
                sizes: vec![0; capacity],
                filled: vec![0; capacity],
                next: vec![None; capacity],
                prev: vec![None; capacity],
                active: vec![false; capacity],
            },
            cold: vec![
                OrderColdData {
                    uid: 0,
                    action: OrderAction::Bid,
                    reserve_price: 0,
                    timestamp: 0,
                };
                capacity
            ],
            free_list,
            capacity,
        }
    }

    #[inline]
    fn alloc(&mut self) -> Option<OrderIdx> {
        self.free_list.pop()
    }

    #[inline]
    fn dealloc(&mut self, idx: OrderIdx) {
        self.hot.active[idx] = false;
        self.free_list.push(idx);
    }
}

/// 价格桶（简化版）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PriceBucket {
    price: Price,
    volume: Size,
    num_orders: usize, // 该档挂单笔数，写点与 volume 严格一一对应
    head: OrderIdx, // 链表头（最早订单）
}

/// 高性能撮合引擎（深度优化版）—— **未完成，请勿使用**
///
/// SOA 布局与 SIMD 批量撮合的实验实现。热路径的想法是对的，但链表与价格桶的
/// 记账整体没有维护起来，作为订单簿是不可用的。已实测确认的缺陷：
///
/// 1. **违反时间优先**：`insert_to_bucket` 把新单挂到 `bucket.head`，而撮合从
///    `head` 沿 `next` 走，导致后到的订单先成交（LIFO）。
/// 2. **价位撮合后永久失联**：撮合吃穿队首订单时只 `dealloc` 槽位，从不推进
///    `bucket.head`。下一次撮合看到 `active[head] == false` 立即退出，该价位
///    即使还有挂单量也再也吃不到。
/// 3. **撤单不回写价格桶**：`cancel_order` 只释放槽位，不摘链、不减 `volume`
///    与 `num_orders`，撤单后盘口深度直接是错的。
/// 4. **无法序列化**：`serialize_state` 没有对应的 `OrderBookState` 变体。
/// 5. `move_order` / `reduce_order` 未实现。
///
/// 修复 1–3 等于重写它的桶与链表层。在此之前不要把它接进 `MatchingEngineRouter`，
/// 也不要拿它的 benchmark 数字与 [`DirectOrderBook`] 对比 —— 它"快"有很大一部分
/// 来自撤单几乎不干活、以及撮合提前退出。
///
/// [`DirectOrderBook`]: super::DirectOrderBook
#[deprecated(
    note = "链表与价格桶记账未维护，存在时间优先、撮合失联、撤单不回写等缺陷；            生产路径请使用 DirectOrderBook"
)]
#[derive(Clone, Serialize, Deserialize)]
pub struct DirectOrderBookOptimized {
    symbol_spec: CoreSymbolSpecification,
    
    // SOA 订单池（预分配）
    order_pool: OrderPool,
    
    // 价格索引（ART 可替换 BTreeMap）
    ask_buckets: BTreeMap<Price, PriceBucket>,
    bid_buckets: BTreeMap<Price, PriceBucket>,
    
    // SIMD 优化开关。不进快照（与业务状态无关），但默认值必须是 true —— 
    // 光写 #[serde(skip)] 会让反序列化取 bool::default()，即快照恢复后
    // SIMD 被静默关闭，性能悄悄掉一截且无任何提示。
    #[serde(skip, default = "simd_enabled_by_default")]
    use_simd: bool,
    
    // 订单 ID 索引
    order_index: AHashMap<OrderId, OrderIdx>,
    
    // 最优价格缓存
    best_ask: Option<Price>,
    best_bid: Option<Price>,
}

impl DirectOrderBookOptimized {
    pub fn new(spec: CoreSymbolSpecification) -> Self {
        Self {
            symbol_spec: spec,
            order_pool: OrderPool::new(100_000), // 预分配 10 万订单
            ask_buckets: BTreeMap::new(),
            bid_buckets: BTreeMap::new(),
            order_index: AHashMap::with_capacity(100_000),
            best_ask: None,
            best_bid: None,
            use_simd: true, // 默认启用 SIMD
        }
    }
    
    /// 设置 SIMD 优化开关
    pub fn set_simd_enabled(&mut self, enabled: bool) {
        self.use_simd = enabled;
    }

    /// GTC 下单。调用方须先确认 order_id 未被占用。
    fn place_gtc(&mut self, cmd: &mut OrderCommand) {
        let filled = if self.use_simd {
            self.try_match_simd_batch(cmd)
        } else {
            self.try_match(cmd)
        };

        if filled < cmd.size {
            if let Some(idx) = self.order_pool.alloc() {
                // 写入热数据
                self.order_pool.hot.order_ids[idx] = cmd.order_id;
                self.order_pool.hot.prices[idx] = cmd.price;
                self.order_pool.hot.sizes[idx] = cmd.size;
                self.order_pool.hot.filled[idx] = filled;
                self.order_pool.hot.active[idx] = true;
                
                // 写入冷数据
                self.order_pool.cold[idx] = OrderColdData {
                    uid: cmd.uid,
                    action: cmd.action,
                    reserve_price: cmd.reserve_price,
                    timestamp: cmd.timestamp,
                };

                self.order_index.insert(cmd.order_id, idx);
                self.insert_to_bucket(idx, cmd.price, cmd.action);
            }
        }
    }

    /// IOC 下单
    fn place_ioc(&mut self, cmd: &mut OrderCommand) {
        let filled = if self.use_simd {
            self.try_match_simd_batch(cmd)
        } else {
            self.try_match(cmd)
        };
        if filled < cmd.size {
            cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                cmd.size - filled,
                cmd.price,
                cmd.reserve_price,
            ));
        }
    }

    /// SIMD 批量撮合（优化版）
    #[cfg(target_arch = "aarch64")]
    fn try_match(&mut self, cmd: &mut OrderCommand) -> Size {
        let is_bid = cmd.action == OrderAction::Bid;
        let limit_price = cmd.price;
        let mut filled = 0;

        // 快速路径：检查最优价格
        let best_price = if is_bid { self.best_ask } else { self.best_bid };
        if let Some(best) = best_price {
            if (is_bid && best > limit_price) || (!is_bid && best < limit_price) {
                return 0;
            }
        } else {
            return 0;
        }

        let prices_to_match: Vec<Price> = if is_bid {
            self.ask_buckets.range(..=limit_price).map(|(p, _)| *p).collect()
        } else {
            self.bid_buckets.range(limit_price..).rev().map(|(p, _)| *p).collect()
        };

        let mut need_update_best = false;

        for price in prices_to_match {
            if filled >= cmd.size {
                break;
            }

            let buckets = if is_bid { &mut self.ask_buckets } else { &mut self.bid_buckets };
            
            if let Some(bucket) = buckets.get_mut(&price) {
                let mut current_idx = bucket.head;
                
                while filled < cmd.size && self.order_pool.hot.active[current_idx] {
                    let remaining = cmd.size - filled;
                    let order_remaining = self.order_pool.hot.sizes[current_idx] - self.order_pool.hot.filled[current_idx];
                    let trade_size = remaining.min(order_remaining);

                    // 更新成交
                    self.order_pool.hot.filled[current_idx] += trade_size;
                    bucket.volume -= trade_size;
                    filled += trade_size;

                    // 生成事件
                    let maker_uid = self.order_pool.cold[current_idx].uid;
                    let reserve = if is_bid {
                        cmd.reserve_price
                    } else {
                        self.order_pool.cold[current_idx].reserve_price
                    };
                    
                    cmd.matcher_events.push(MatcherTradeEvent::new_trade(
                        trade_size,
                        price,
                        self.order_pool.hot.order_ids[current_idx],
                        maker_uid,
                        reserve,
                    ));

                    // 订单完成
                    if self.order_pool.hot.filled[current_idx] >= self.order_pool.hot.sizes[current_idx] {
                        let order_id = self.order_pool.hot.order_ids[current_idx];
                        self.order_index.remove(&order_id);
                        self.order_pool.dealloc(current_idx);
                        bucket.num_orders -= 1;
                    }

                    if let Some(next) = self.order_pool.hot.next[current_idx] {
                        current_idx = next;
                    } else {
                        break;
                    }
                }

                if bucket.volume == 0 {
                    buckets.remove(&price);
                    need_update_best = true;
                }
            }
        }

        if need_update_best {
            self.update_best_price(is_bid);
        }

        filled
    }

    /// 非 ARM 架构回退
    #[cfg(not(target_arch = "aarch64"))]
    fn try_match(&mut self, cmd: &mut OrderCommand) -> Size {
        let is_bid = cmd.action == OrderAction::Bid;
        let limit_price = cmd.price;
        let mut filled = 0;

        let best_price = if is_bid { self.best_ask } else { self.best_bid };
        if let Some(best) = best_price {
            if (is_bid && best > limit_price) || (!is_bid && best < limit_price) {
                return 0;
            }
        } else {
            return 0;
        }

        let prices_to_match: Vec<Price> = if is_bid {
            self.ask_buckets.range(..=limit_price).map(|(p, _)| *p).collect()
        } else {
            self.bid_buckets.range(limit_price..).rev().map(|(p, _)| *p).collect()
        };

        let mut need_update_best = false;

        for price in prices_to_match {
            if filled >= cmd.size {
                break;
            }

            let buckets = if is_bid { &mut self.ask_buckets } else { &mut self.bid_buckets };
            
            if let Some(bucket) = buckets.get_mut(&price) {
                let mut current_idx = bucket.head;
                
                while filled < cmd.size && self.order_pool.hot.active[current_idx] {
                    let remaining = cmd.size - filled;
                    let order_remaining = self.order_pool.hot.sizes[current_idx] - self.order_pool.hot.filled[current_idx];
                    let trade_size = remaining.min(order_remaining);

                    self.order_pool.hot.filled[current_idx] += trade_size;
                    bucket.volume -= trade_size;
                    filled += trade_size;

                    let maker_uid = self.order_pool.cold[current_idx].uid;
                    let reserve = if is_bid {
                        cmd.reserve_price
                    } else {
                        self.order_pool.cold[current_idx].reserve_price
                    };
                    
                    cmd.matcher_events.push(MatcherTradeEvent::new_trade(
                        trade_size,
                        price,
                        self.order_pool.hot.order_ids[current_idx],
                        maker_uid,
                        reserve,
                    ));

                    if self.order_pool.hot.filled[current_idx] >= self.order_pool.hot.sizes[current_idx] {
                        let order_id = self.order_pool.hot.order_ids[current_idx];
                        self.order_index.remove(&order_id);
                        self.order_pool.dealloc(current_idx);
                        bucket.num_orders -= 1;
                    }

                    if let Some(next) = self.order_pool.hot.next[current_idx] {
                        current_idx = next;
                    } else {
                        break;
                    }
                }

                if bucket.volume == 0 {
                    buckets.remove(&price);
                    need_update_best = true;
                }
            }
        }

        if need_update_best {
            self.update_best_price(is_bid);
        }

        filled
    }

    /// SIMD 批量撮合优化（高性能版本）
    fn try_match_simd_batch(&mut self, cmd: &mut OrderCommand) -> Size {
        let is_bid = cmd.action == OrderAction::Bid;
        let limit_price = cmd.price;
        let mut filled = 0;

        // 快速路径：检查最优价格
        let best_price = if is_bid { self.best_ask } else { self.best_bid };
        if let Some(best) = best_price {
            if (is_bid && best > limit_price) || (!is_bid && best < limit_price) {
                return 0;
            }
        } else {
            return 0;
        }

        // 收集价格档位
        let prices_to_match: Vec<Price> = if is_bid {
            self.ask_buckets.range(..=limit_price).map(|(p, _)| *p).collect()
        } else {
            self.bid_buckets.range(limit_price..).rev().map(|(p, _)| *p).collect()
        };

        let mut need_update_best = false;
        let mut prices_to_remove = Vec::new();

        for price in prices_to_match {
            if filled >= cmd.size {
                break;
            }

            // 收集该价格档的所有活跃订单
            let mut order_indices = Vec::new();
            {
                let buckets = if is_bid { &self.ask_buckets } else { &self.bid_buckets };
                if let Some(bucket) = buckets.get(&price) {
                    let mut current_idx = bucket.head;
                    
                    while self.order_pool.hot.active[current_idx] {
                        order_indices.push(current_idx);
                        if let Some(next) = self.order_pool.hot.next[current_idx] {
                            current_idx = next;
                        } else {
                            break;
                        }
                    }
                }
            }

            if order_indices.is_empty() {
                continue;
            }

            // SIMD 批量处理（如果订单数量 >= 4）
            if order_indices.len() >= 4 {
                let matched = self.simd_match_orders_internal(
                    &order_indices,
                    cmd.size - filled,
                    price,
                    cmd.action,
                    cmd.reserve_price,
                    &mut cmd.matcher_events,
                );
                filled += matched;
            } else {
                // 少量订单使用标准处理
                for &idx in &order_indices {
                    if filled >= cmd.size {
                        break;
                    }
                    
                    let order_remaining = self.order_pool.hot.sizes[idx] - self.order_pool.hot.filled[idx];
                    let trade_size = (cmd.size - filled).min(order_remaining);

                    self.order_pool.hot.filled[idx] += trade_size;
                    filled += trade_size;

                    let maker_uid = self.order_pool.cold[idx].uid;
                    let reserve = if is_bid {
                        cmd.reserve_price
                    } else {
                        self.order_pool.cold[idx].reserve_price
                    };
                    
                    cmd.matcher_events.push(MatcherTradeEvent::new_trade(
                        trade_size,
                        price,
                        self.order_pool.hot.order_ids[idx],
                        maker_uid,
                        reserve,
                    ));

                    if self.order_pool.hot.filled[idx] >= self.order_pool.hot.sizes[idx] {
                        let order_id = self.order_pool.hot.order_ids[idx];
                        self.order_index.remove(&order_id);
                        self.order_pool.dealloc(idx);
                    }
                }
            }

            // 更新桶信息
            {
                let buckets = if is_bid { &mut self.ask_buckets } else { &mut self.bid_buckets };
                if let Some(bucket) = buckets.get_mut(&price) {
                    // 重新计算桶的总量
                    let mut new_volume = 0;
                    let mut new_count = 0;
                    for &idx in &order_indices {
                        if self.order_pool.hot.active[idx] {
                            new_volume += self.order_pool.hot.sizes[idx] - self.order_pool.hot.filled[idx];
                            new_count += 1;
                        }
                    }
                    bucket.volume = new_volume;
                    bucket.num_orders = new_count;
                    
                    if bucket.volume == 0 {
                        prices_to_remove.push(price);
                        need_update_best = true;
                    }
                }
            }
        }

        // 清理空桶
        for price in prices_to_remove {
            if is_bid {
                self.ask_buckets.remove(&price);
            } else {
                self.bid_buckets.remove(&price);
            }
        }

        if need_update_best {
            self.update_best_price(is_bid);
        }

        filled
    }

    /// SIMD 批量处理订单
    #[inline]
    fn simd_match_orders_internal(
        &mut self,
        order_indices: &[OrderIdx],
        need_size: Size,
        price: Price,
        taker_action: OrderAction,
        taker_reserve: Price,
        events: &mut Vec<MatcherTradeEvent>,
    ) -> Size {
        // 收集订单数据（SOA 优势）
        let sizes: Vec<i64> = order_indices.iter()
            .map(|&idx| self.order_pool.hot.sizes[idx])
            .collect();
        
        let filled: Vec<i64> = order_indices.iter()
            .map(|&idx| self.order_pool.hot.filled[idx])
            .collect();

        // SIMD 批量计算匹配量
        let (matched_sizes, _total_matched) = simd_batch_match_prepare(&sizes, &filled, need_size);

        // 应用匹配结果
        let mut actual_filled = 0i64;
        for (i, &idx) in order_indices.iter().enumerate() {
            let match_size = matched_sizes[i];
            if match_size > 0 {
                self.order_pool.hot.filled[idx] += match_size;
                actual_filled += match_size;

                let maker_uid = self.order_pool.cold[idx].uid;
                let reserve = if taker_action == OrderAction::Bid {
                    taker_reserve
                } else {
                    self.order_pool.cold[idx].reserve_price
                };

                events.push(MatcherTradeEvent::new_trade(
                    match_size,
                    price,
                    self.order_pool.hot.order_ids[idx],
                    maker_uid,
                    reserve,
                ));
            }
        }

        actual_filled
    }

    /// 插入订单到价格桶
    fn insert_to_bucket(&mut self, order_idx: OrderIdx, price: Price, action: OrderAction) {
        let size = self.order_pool.hot.sizes[order_idx] - self.order_pool.hot.filled[order_idx];
        let is_ask = action == OrderAction::Ask;

        let buckets = if is_ask {
            &mut self.ask_buckets
        } else {
            &mut self.bid_buckets
        };

        let is_new = !buckets.contains_key(&price);

        buckets
            .entry(price)
            .and_modify(|bucket| {
                bucket.volume += size;
                bucket.num_orders += 1;
                let old_head = bucket.head;
                self.order_pool.hot.next[order_idx] = Some(old_head);
                self.order_pool.hot.prev[old_head] = Some(order_idx);
                bucket.head = order_idx;
            })
            .or_insert_with(|| {
                PriceBucket {
                    price,
                    volume: size,
                    num_orders: 1,
                    head: order_idx,
                }
            });

        if is_new {
            self.update_best_price(is_ask);
        }
    }

    /// 更新最优价格缓存
    fn update_best_price(&mut self, is_ask: bool) {
        if is_ask {
            self.best_ask = self.ask_buckets.keys().next().copied();
        } else {
            self.best_bid = self.bid_buckets.keys().next_back().copied();
        }
    }

    /// 取消订单
    fn cancel_order(&mut self, cmd: &mut OrderCommand) -> CommandResultCode {
        if let Some(&order_idx) = self.order_index.get(&cmd.order_id) {
            let price = self.order_pool.hot.prices[order_idx];
            let action = self.order_pool.cold[order_idx].action;
            let remaining = self.order_pool.hot.sizes[order_idx] - self.order_pool.hot.filled[order_idx];
            let reserve_price = self.order_pool.cold[order_idx].reserve_price;

            cmd.matcher_events.push(MatcherTradeEvent::new_reject(remaining, price, reserve_price));
            cmd.action = action;

            self.order_index.remove(&cmd.order_id);
            self.order_pool.dealloc(order_idx);

            CommandResultCode::Success
        } else {
            CommandResultCode::MatchingUnknownOrderId
        }
    }
}

impl super::OrderBook for DirectOrderBookOptimized {
    fn new_order(&mut self, cmd: &mut OrderCommand) -> CommandResultCode {
        if self.order_index.contains_key(&cmd.order_id) {
            return CommandResultCode::MatchingDuplicateOrderId;
        }

        match cmd.order_type {
            OrderType::Gtc => {
                self.place_gtc(cmd);
                CommandResultCode::Success
            }
            OrderType::Ioc => {
                self.place_ioc(cmd);
                CommandResultCode::Success
            }
            _ => CommandResultCode::MatchingUnsupportedCommand,
        }
    }

    fn cancel_order(&mut self, cmd: &mut OrderCommand) -> CommandResultCode {
        self.cancel_order(cmd)
    }

    fn move_order(&mut self, _cmd: &mut OrderCommand) -> CommandResultCode {
        CommandResultCode::MatchingUnsupportedCommand // 简化实现
    }

    fn reduce_order(&mut self, _cmd: &mut OrderCommand) -> CommandResultCode {
        CommandResultCode::MatchingUnsupportedCommand // 简化实现
    }

    fn get_symbol_spec(&self) -> &CoreSymbolSpecification {
        &self.symbol_spec
    }

    fn get_l2_data(&self, depth: usize) -> L2MarketData {
        let mut data = L2MarketData::new(depth);

        for (price, bucket) in self.ask_buckets.iter().take(depth) {
            data.ask_prices.push(*price);
            data.ask_volumes.push(bucket.volume);
            data.ask_order_counts.push(bucket.num_orders);
        }

        for (price, bucket) in self.bid_buckets.iter().rev().take(depth) {
            data.bid_prices.push(*price);
            data.bid_volumes.push(bucket.volume);
            data.bid_order_counts.push(bucket.num_orders);
        }

        data
    }

    fn get_order_by_id(&self, order_id: OrderId) -> Option<(Price, OrderAction)> {
        self.order_index.get(&order_id).map(|&idx| {
            let price = self.order_pool.hot.prices[idx];
            let action = self.order_pool.cold[idx].action;
            (price, action)
        })
    }

    fn get_total_ask_volume(&self) -> Size {
        self.ask_buckets.values().map(|b| b.volume).sum()
    }

    fn get_total_bid_volume(&self) -> Size {
        self.bid_buckets.values().map(|b| b.volume).sum()
    }

    fn get_ask_buckets_count(&self) -> usize {
        self.ask_buckets.len()
    }

    fn get_bid_buckets_count(&self) -> usize {
        self.bid_buckets.len()
    }

    fn serialize_state(&self) -> crate::core::orderbook::OrderBookState {
        // 早先这里返回一个空的 DirectOrderBook —— 快照会静默丢掉全部挂单，
        // 没有任何报错，恢复后订单簿凭空清零。而 OrderBookState 一直就有
        // DirectOptimized 变体，MatchingEngineRouter::from_state 也早已接得住，
        // 缺的只是这一行。
        crate::core::orderbook::OrderBookState::DirectOptimized(self.clone())
    }
}

