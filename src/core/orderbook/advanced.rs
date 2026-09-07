//! 高级订单类型的实验实现（Stop / Iceberg / GTD / PostOnly）。
//!
//! # 现状：未接入引擎，请勿在生产路径使用
//!
//! `MatchingEngineRouter::add_symbol` 一律创建 [`DirectOrderBook`]，本类型从引擎里
//! 到不了。本文件的单测大多直接构造 `AdvancedOrderBook`、绕过 `ExchangeCore`，
//! 因此**测试全绿不代表引擎支持这些订单类型** —— 引擎实际只认
//! Gtc / Ioc / FokBudget 三种，边界见 `tests/advanced_orders_test.rs` 末尾两个用例。
//!
//! # 定位
//!
//! 它是 **NaiveOrderBook + 功能**，不是 DirectOrderBook 的进化：档内仍是
//! `SmallVec<[AdvancedOrder; 8]>`，撤单要 `iter().position()` 线性扫描，
//! 没有任何性能追求。与之相对，`direct_optimized` 走的是纯性能路线（SOA + SIMD）
//! 但功能更少且已废弃。当前局面是"快的没功能，有功能的慢"。
//!
//! # 已核实的缺陷（都会造成资金问题，接入前必须先修）
//!
//! 1. **GTD 过期不退款**：`AdvancedBucket::match_order` 里过期订单只被推进
//!    `to_remove` 就 `continue`，不产生任何 Reject 事件。R2 收不到事件就不退款，
//!    用户为该单冻结的资金被永久锁死。
//!    另外过期检查只在该档**被撮合时**才跑，无人问津的档位上的过期单永远不会被清理。
//! 2. **止损触发后成交事件被丢弃**：`process_stop_orders` 里的 `activate_cmd` 是局部
//!    变量，`place_order_internal` 把成交写进它的 `matcher_events` 后随函数返回一起丢掉，
//!    从未并回外层 `cmd.matcher_events`。结果是订单簿状态变了、钱没结算 —— 账实分离。
//! 3. **冰山单没有切片轮转**：只维护了 `visible_volume`，露出的一片吃完后不会把下一片
//!    重新排到队尾，实际效果是"隐藏单"而非冰山单。
//! 4. **止损扫描是 O(全部止损单)**：`stop_orders: Vec<_>` 每次成交全量遍历。
//!
//! # 若日后要正经支持这些订单类型，建议的方向
//!
//! **不要重做订单簿模型，在 [`DirectOrderBook`] 旁边挂卫星结构即可**：
//!
//! - **冰山单**：核心结构完全不用改。订单照常待在同一条链、同一档、同一排队位置；
//!   只需订单上加 `visible_size`、档位上加 `visible_volume`，行情报后者。
//!   切片轮转用现成的 `remove_order` + `insert_order` 就能实现（露出的一片吃完后
//!   下一片重新排到队尾），正是 `move_order` 用的那条路径。
//! - **GTD / Day**：加一张按过期时间排序的索引 `BTreeMap<Timestamp, Vec<OrderId>>`，
//!   配时钟 tick 扫队首。订单簿本身无需感知。移除时**必须发 Reject 事件**，否则就是
//!   上面第 1 条。
//! - **止损单**：触发前它根本不在簿里，需要独立的触发索引
//!   `BTreeMap<Price, Vec<StopOrder>>`（买止损与卖止损各一），成交后按最新成交价
//!   只取该动的那批，O(被触发数) 而非 O(全部)。
//! - **额外字段旁挂，别塞进 `DirectOrder`**：那个结构体在热路径上每条命令都要读，
//!   为少数特殊单撑大它会拖累绝大多数普通单的缓存局部性。用
//!   `AHashMap<OrderIdx, OrderExtras>` 之类只为特殊单建条目即可
//!   （`direct_optimized` 的冷热分离思路是对的，只是那份实现的链表记账写坏了）。
//!
//! # 真正的难点不在数据结构
//!
//! 止损单会**在撮合过程中凭空产生新的撮合活动**：触发后的单立刻去吃盘口，这次成交
//! 又可能触发别的止损，形成级联。需要定死递归深度上限与处理顺序，并保证所有派生成交的
//! 事件都并回同一条命令，R2 才能正确结算。
//!
//! 还有一个风控决策：本仓库 R1 在**下单时**冻结资金，而止损单触发前不占盘口 ——
//! 它的钱是下单即冻，还是触发才冻？这是 `RiskEngine` 的设计问题，不是订单簿的。
//!
//! [`DirectOrderBook`]: super::DirectOrderBook

use crate::api::*;
use ahash::AHashMap;
use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// 扩展订单（支持所有订单类型）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdvancedOrder {
    order_id: OrderId,
    uid: UserId,
    price: Price,
    size: Size,
    filled: Size,
    action: OrderAction,
    order_type: OrderType,
    reserve_price: Price,
    timestamp: i64,
    
    // 扩展字段
    stop_price: Option<Price>,      // 止损触发价
    visible_size: Option<Size>,     // 冰山单显示数量
    expire_time: Option<i64>,       // 过期时间
    is_triggered: bool,             // 止损单是否已触发
}

/// 价格档位（支持冰山单）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdvancedBucket {
    price: Price,
    orders: SmallVec<[AdvancedOrder; 8]>,
    total_volume: Size,      // 总真实挂单量
    visible_volume: Size,    // 总显示挂单量
}

impl AdvancedBucket {
    fn new(price: Price) -> Self {
        Self {
            price,
            orders: SmallVec::new(),
            total_volume: 0,
            visible_volume: 0,
        }
    }

    fn add(&mut self, order: AdvancedOrder) {
        let remaining = order.size - order.filled;
        self.total_volume += remaining;
        
        // 冰山单只显示部分数量
        if let Some(visible) = order.visible_size {
            self.visible_volume += visible.min(remaining);
        } else {
            self.visible_volume += remaining;
        }
        
        self.orders.push(order);
    }

    /// 该档对外可见的挂单笔数。
    ///
    /// 判据与 `visible_volume` 的累加口径保持一致：显示量为 0 的全隐藏单不计入，
    /// 否则报出的笔数会暴露出「有单但不贡献深度」，等于泄露了隐藏单的存在。
    fn visible_order_count(&self) -> usize {
        self.orders
            .iter()
            .filter(|o| {
                let remaining = o.size - o.filled;
                let visible = match o.visible_size {
                    Some(v) => v.min(remaining),
                    None => remaining,
                };
                visible > 0
            })
            .count()
    }

    fn remove(&mut self, order_id: OrderId) -> Option<AdvancedOrder> {
        if let Some(pos) = self.orders.iter().position(|o| o.order_id == order_id) {
            let order = self.orders.remove(pos);
            let remaining = order.size - order.filled;
            self.total_volume -= remaining;
            
            if let Some(visible) = order.visible_size {
                self.visible_volume -= visible.min(remaining);
            } else {
                self.visible_volume -= remaining;
            }
            
            Some(order)
        } else {
            None
        }
    }

    /// 撮合订单（支持冰山单）
    fn match_order(&mut self, taker_size: Size, _taker_uid: UserId, current_time: i64) 
        -> (Size, SmallVec<[MatcherTradeEvent; 4]>) 
    {
        let mut matched_size = 0;
        let mut events = SmallVec::new();
        let mut to_remove = SmallVec::<[OrderId; 4]>::new();

        for order in &mut self.orders {
            // 检查订单是否过期
            if let Some(expire) = order.expire_time {
                if current_time > expire {
                    to_remove.push(order.order_id);
                    continue;
                }
            }

            let remaining = order.size - order.filled;
            let match_size = remaining.min(taker_size - matched_size);

            if match_size > 0 {
                order.filled += match_size;
                matched_size += match_size;
                
                // 更新总量
                self.total_volume -= match_size;
                
                // 更新显示量（冰山单特殊处理）
                if let Some(visible) = order.visible_size {
                    let old_visible = visible.min(remaining);
                    let new_remaining = order.size - order.filled;
                    let new_visible = visible.min(new_remaining);
                    self.visible_volume = self.visible_volume.saturating_sub(old_visible) + new_visible;
                } else {
                    self.visible_volume -= match_size;
                }

                events.push(MatcherTradeEvent::new_trade(
                    match_size,
                    self.price,
                    order.order_id,
                    order.uid,
                    order.reserve_price,
                ));

                if order.filled >= order.size {
                    to_remove.push(order.order_id);
                }

                if matched_size >= taker_size {
                    break;
                }
            }
        }

        // 移除完成的订单（不更新总量，已在上面更新）
        for oid in to_remove {
            if let Some(pos) = self.orders.iter().position(|o| o.order_id == oid) {
                self.orders.remove(pos);
            }
        }

        (matched_size, events)
    }
}

/// 高级订单簿（支持所有订单类型）
#[derive(Clone, Serialize, Deserialize)]
pub struct AdvancedOrderBook {
    symbol_spec: CoreSymbolSpecification,
    
    // 活跃订单
    ask_buckets: BTreeMap<Price, AdvancedBucket>,
    bid_buckets: BTreeMap<Price, AdvancedBucket>,
    order_map: AHashMap<OrderId, (Price, OrderAction)>,
    
    // 止损单池（未触发）
    stop_orders: Vec<AdvancedOrder>,
    
    // 最新成交价（用于触发止损单）
    last_trade_price: Option<Price>,
    
    // 最优价格缓存
    best_ask_price: Option<Price>,
    best_bid_price: Option<Price>,
}

impl AdvancedOrderBook {
    pub fn new(spec: CoreSymbolSpecification) -> Self {
        Self {
            symbol_spec: spec,
            ask_buckets: BTreeMap::new(),
            bid_buckets: BTreeMap::new(),
            order_map: AHashMap::with_capacity(1024),
            stop_orders: Vec::new(),
            last_trade_price: None,
            best_ask_price: None,
            best_bid_price: None,
        }
    }

    #[inline]
    fn update_best_prices(&mut self) {
        self.best_ask_price = self.ask_buckets.keys().next().copied();
        self.best_bid_price = self.bid_buckets.keys().next_back().copied();
    }

    /// 检查订单是否会立即成交
    fn would_match(&self, cmd: &OrderCommand) -> bool {
        match cmd.action {
            OrderAction::Bid => {
                if let Some(best_ask) = self.best_ask_price {
                    cmd.price >= best_ask
                } else {
                    false
                }
            }
            OrderAction::Ask => {
                if let Some(best_bid) = self.best_bid_price {
                    cmd.price <= best_bid
                } else {
                    false
                }
            }
        }
    }

    /// Post-Only 检查
    fn check_post_only(&self, cmd: &OrderCommand) -> CommandResultCode {
        if self.would_match(cmd) {
            CommandResultCode::MatchingUnsupportedCommand // Post-Only 拒绝
        } else {
            CommandResultCode::ValidForMatchingEngine
        }
    }

    /// 处理止损单
    fn process_stop_orders(&mut self, cmd: &mut OrderCommand) {
        if let Some(last_price) = self.last_trade_price {
            let mut triggered = Vec::new();
            
            for (idx, stop_order) in self.stop_orders.iter_mut().enumerate() {
                if let Some(stop_price) = stop_order.stop_price {
                    let should_trigger = match stop_order.action {
                        OrderAction::Bid => last_price >= stop_price,  // 买止损
                        OrderAction::Ask => last_price <= stop_price,  // 卖止损
                    };

                    if should_trigger && !stop_order.is_triggered {
                        stop_order.is_triggered = true;
                        triggered.push(idx);
                    }
                }
            }

            // 激活触发的止损单
            for idx in triggered.iter().rev() {
                let order = self.stop_orders.remove(*idx);
                let mut activate_cmd = OrderCommand {
                    uid: order.uid,
                    order_id: order.order_id,
                    symbol: cmd.symbol,
                    price: order.price,
                    size: order.size,
                    action: order.action,
                    order_type: order.order_type,
                    reserve_price: order.reserve_price,
                    timestamp: order.timestamp,
                    ..Default::default()
                };
                
                self.place_order_internal(&mut activate_cmd);
            }
        }
    }

    /// 下单（所有类型）
    fn place_order(&mut self, cmd: &mut OrderCommand) {
        // Post-Only 检查
        if cmd.order_type == OrderType::PostOnly {
            if self.check_post_only(cmd) != CommandResultCode::ValidForMatchingEngine {
                cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                    cmd.size,
                    cmd.price,
                    cmd.reserve_price,
                ));
                return;
            }
        }

        // 止损单：暂存到止损池
        if matches!(cmd.order_type, OrderType::StopLimit | OrderType::StopMarket) {
            let order = AdvancedOrder {
                order_id: cmd.order_id,
                uid: cmd.uid,
                price: cmd.price,
                size: cmd.size,
                filled: 0,
                action: cmd.action,
                order_type: cmd.order_type,
                reserve_price: cmd.reserve_price,
                timestamp: cmd.timestamp,
                stop_price: cmd.stop_price,
                visible_size: cmd.visible_size,
                expire_time: cmd.expire_time,
                is_triggered: false,
            };
            self.stop_orders.push(order);
            return;
        }

        self.place_order_internal(cmd);
    }

    /// 内部下单逻辑。调用方须先确认 order_id 未被占用。
    fn place_order_internal(&mut self, cmd: &mut OrderCommand) {
        // FOK: 全部成交或全部取消
        if cmd.order_type == OrderType::Fok {
            if !self.can_fill_completely(cmd) {
                cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                    cmd.size,
                    cmd.price,
                    cmd.reserve_price,
                ));
                return;
            }
        }

        let filled = self.try_match(cmd);

        // 更新最新成交价
        if filled > 0 {
            self.last_trade_price = Some(cmd.price);
            self.process_stop_orders(cmd);
        }

        // IOC/FOK: 不挂单
        if matches!(cmd.order_type, OrderType::Ioc | OrderType::Fok) {
            if filled < cmd.size {
                cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                    cmd.size - filled,
                    cmd.price,
                    cmd.reserve_price,
                ));
            }
            return;
        }

        // GTC/Day/GTD/PostOnly/Iceberg: 挂单
        if filled < cmd.size {
            let order = AdvancedOrder {
                order_id: cmd.order_id,
                uid: cmd.uid,
                price: cmd.price,
                size: cmd.size,
                filled,
                action: cmd.action,
                order_type: cmd.order_type,
                reserve_price: cmd.reserve_price,
                timestamp: cmd.timestamp,
                stop_price: None,
                visible_size: cmd.visible_size,
                expire_time: cmd.expire_time,
                is_triggered: false,
            };

            self.order_map.insert(cmd.order_id, (cmd.price, cmd.action));

            match cmd.action {
                OrderAction::Ask => {
                    self.ask_buckets
                        .entry(cmd.price)
                        .or_insert_with(|| AdvancedBucket::new(cmd.price))
                        .add(order);
                    if self.best_ask_price.is_none() || cmd.price < self.best_ask_price.unwrap() {
                        self.best_ask_price = Some(cmd.price);
                    }
                }
                OrderAction::Bid => {
                    self.bid_buckets
                        .entry(cmd.price)
                        .or_insert_with(|| AdvancedBucket::new(cmd.price))
                        .add(order);
                    if self.best_bid_price.is_none() || cmd.price > self.best_bid_price.unwrap() {
                        self.best_bid_price = Some(cmd.price);
                    }
                }
            }
        }
    }

    /// 检查是否可以完全成交（FOK）
    fn can_fill_completely(&self, cmd: &OrderCommand) -> bool {
        let buckets = match cmd.action {
            OrderAction::Bid => &self.ask_buckets,
            OrderAction::Ask => &self.bid_buckets,
        };

        let mut available = 0;
        for (price, bucket) in buckets.iter() {
            if (cmd.action == OrderAction::Bid && *price > cmd.price) ||
               (cmd.action == OrderAction::Ask && *price < cmd.price) {
                break;
            }
            available += bucket.total_volume;
            if available >= cmd.size {
                return true;
            }
        }
        false
    }

    /// 尝试撮合
    fn try_match(&mut self, cmd: &mut OrderCommand) -> Size {
        let mut filled = 0;

        // 快速路径检查
        if (cmd.action == OrderAction::Bid && self.best_ask_price.map_or(true, |p| p > cmd.price)) ||
           (cmd.action == OrderAction::Ask && self.best_bid_price.map_or(true, |p| p < cmd.price)) {
            return 0;
        }

        let current_time = cmd.timestamp;

        match cmd.action {
            OrderAction::Bid => {
                let prices: Vec<Price> = self.ask_buckets.range(..=cmd.price).map(|(p, _)| *p).collect();
                
                for price in prices {
                    if filled >= cmd.size {
                        break;
                    }

                    if let Some(bucket) = self.ask_buckets.get_mut(&price) {
                        let (matched, events) = bucket.match_order(cmd.size - filled, cmd.uid, current_time);
                        filled += matched;
                        cmd.matcher_events.extend(events);

                        if bucket.total_volume == 0 {
                            self.ask_buckets.remove(&price);
                        }
                    }
                }
                self.update_best_prices();
            }
            OrderAction::Ask => {
                let prices: Vec<Price> = self.bid_buckets.range(cmd.price..).rev().map(|(p, _)| *p).collect();
                
                for price in prices {
                    if filled >= cmd.size {
                        break;
                    }

                    if let Some(bucket) = self.bid_buckets.get_mut(&price) {
                        let (matched, events) = bucket.match_order(cmd.size - filled, cmd.uid, current_time);
                        filled += matched;
                        cmd.matcher_events.extend(events);

                        if bucket.total_volume == 0 {
                            self.bid_buckets.remove(&price);
                        }
                    }
                }
                self.update_best_prices();
            }
        }

        filled
    }

    /// 取消订单
    fn cancel_order(&mut self, cmd: &mut OrderCommand) -> CommandResultCode {
        // 检查活跃订单
        if let Some((price, action)) = self.order_map.remove(&cmd.order_id) {
            let buckets = match action {
                OrderAction::Ask => &mut self.ask_buckets,
                OrderAction::Bid => &mut self.bid_buckets,
            };

            if let Some(bucket) = buckets.get_mut(&price) {
                if let Some(order) = bucket.remove(cmd.order_id) {
                    cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                        order.size - order.filled,
                        price,
                        order.reserve_price,
                    ));
                    cmd.action = action;

                    if bucket.total_volume == 0 {
                        buckets.remove(&price);
                        self.update_best_prices();
                    }

                    return CommandResultCode::Success;
                }
            }
        }

        // 检查止损单池
        if let Some(pos) = self.stop_orders.iter().position(|o| o.order_id == cmd.order_id) {
            let order = self.stop_orders.remove(pos);
            cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                order.size - order.filled,
                order.price,
                order.reserve_price,
            ));
            cmd.action = order.action;
            return CommandResultCode::Success;
        }

        CommandResultCode::MatchingUnknownOrderId
    }
}

impl super::OrderBook for AdvancedOrderBook {
    fn new_order(&mut self, cmd: &mut OrderCommand) -> CommandResultCode {
        // 活跃簿与止损单池都要查重
        if self.order_map.contains_key(&cmd.order_id)
            || self.stop_orders.iter().any(|o| o.order_id == cmd.order_id)
        {
            return CommandResultCode::MatchingDuplicateOrderId;
        }

        self.place_order(cmd);
        CommandResultCode::Success
    }

    fn cancel_order(&mut self, cmd: &mut OrderCommand) -> CommandResultCode {
        self.cancel_order(cmd)
    }

    fn move_order(&mut self, cmd: &mut OrderCommand) -> CommandResultCode {
        // 简化：先取消再下单
        let cancel_result = self.cancel_order(cmd);
        if cancel_result == CommandResultCode::Success {
            self.place_order(cmd);
        }
        cancel_result
    }

    fn reduce_order(&mut self, _cmd: &mut OrderCommand) -> CommandResultCode {
        CommandResultCode::MatchingUnsupportedCommand
    }

    fn get_symbol_spec(&self) -> &CoreSymbolSpecification {
        &self.symbol_spec
    }

    fn get_l2_data(&self, depth: usize) -> L2MarketData {
        let mut data = L2MarketData::new(depth);

        for (price, bucket) in self.ask_buckets.iter().take(depth) {
            data.ask_prices.push(*price);
            data.ask_volumes.push(bucket.visible_volume); // 显示量
            data.ask_order_counts.push(bucket.visible_order_count());
        }

        for (price, bucket) in self.bid_buckets.iter().rev().take(depth) {
            data.bid_prices.push(*price);
            data.bid_volumes.push(bucket.visible_volume); // 显示量
            data.bid_order_counts.push(bucket.visible_order_count());
        }

        data
    }

    fn get_order_by_id(&self, order_id: OrderId) -> Option<(Price, OrderAction)> {
        self.order_map.get(&order_id).copied()
    }

    fn get_total_ask_volume(&self) -> Size {
        self.ask_buckets.values().map(|b| b.total_volume).sum()
    }

    fn get_total_bid_volume(&self) -> Size {
        self.bid_buckets.values().map(|b| b.total_volume).sum()
    }

    fn get_ask_buckets_count(&self) -> usize {
        self.ask_buckets.len()
    }

    fn get_bid_buckets_count(&self) -> usize {
        self.bid_buckets.len()
    }

    fn serialize_state(&self) -> crate::core::orderbook::OrderBookState {
        crate::core::orderbook::OrderBookState::Advanced(self.clone())
    }
}

