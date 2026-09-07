use crate::api::*;
use ahash::AHashMap;
use slab::Slab;
use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

type OrderIdx = usize;
type BucketIdx = usize;

/// 一次撮合尝试的结果
struct MatchResult {
    /// 本次成交量
    filled: Size,
    /// STP 判定新单必须整单撤销：剩余量不得挂入簿中，需退还冻结资金
    taker_aborted: bool,
}

impl MatchResult {
    fn filled(filled: Size) -> Self {
        Self {
            filled,
            taker_aborted: false,
        }
    }
}

/// 直接订单（使用 Slab 索引实现的双向链表，避免 Rc/RefCell 开销）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DirectOrder {
    order_id: OrderId,
    uid: UserId,
    price: Price,
    size: Size,
    filled: Size,
    action: OrderAction,
    reserve_price: Price,
    timestamp: i64,
    next: Option<OrderIdx>,
    prev: Option<OrderIdx>,
    parent: BucketIdx,
}

/// 价格档位（桶），存储相同价格的一组订单
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bucket {
    price: Price,
    volume: Size,
    num_orders: usize,
    tail: OrderIdx,
}

/// 高性能撮合引擎实现 (Direct 实现)
/// 逻辑参考 exchange-core，使用原始指针/索引链表实现极低延迟
#[derive(Clone, Serialize, Deserialize)]
pub struct DirectOrderBook {
    symbol_spec: CoreSymbolSpecification, // 交易对配置
    
    // 内存池，预分配订单和桶，减少分配开销
    orders: Slab<DirectOrder>,
    buckets: Slab<Bucket>,
    
    // 价格索引，快速定位最优价格
    ask_price_buckets: BTreeMap<Price, BucketIdx>, // 卖单：价格升序
    bid_price_buckets: BTreeMap<Price, BucketIdx>, // 买单：价格降序
    
    // 订单 ID 快速索引
    order_id_index: AHashMap<OrderId, OrderIdx>,
    
    // 最优订单快捷引用，类似 LMAX Disruptor 的快速路径
    best_ask_order: Option<OrderIdx>, // 卖一订单
    best_bid_order: Option<OrderIdx>, // 买一订单
}

impl DirectOrderBook {
    pub fn new(spec: CoreSymbolSpecification) -> Self {
        Self {
            symbol_spec: spec,
            orders: Slab::with_capacity(1024),
            buckets: Slab::with_capacity(128),
            ask_price_buckets: BTreeMap::new(),
            bid_price_buckets: BTreeMap::new(),
            order_id_index: AHashMap::new(),
            best_ask_order: None,
            best_bid_order: None,
        }
    }

    /// GTC 下单。调用方须先确认 order_id 未被占用。
    fn place_gtc(&mut self, cmd: &mut OrderCommand) {
        // 尝试撮合
        let MatchResult {
            filled,
            taker_aborted,
        } = self.try_match(cmd);

        // STP 判定撤销新单：剩余量不挂簿，直接退还冻结资金
        if taker_aborted {
            if filled < cmd.size {
                cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                    cmd.size - filled,
                    cmd.price,
                    cmd.reserve_price,
                ));
            }
            return;
        }

        // 未完全成交，挂单
        if filled < cmd.size {
            let order_idx = self.orders.insert(DirectOrder {
                order_id: cmd.order_id,
                uid: cmd.uid,
                price: cmd.price,
                size: cmd.size,
                filled,
                action: cmd.action,
                reserve_price: cmd.reserve_price,
                timestamp: cmd.timestamp,
                next: None,
                prev: None,
                parent: 0, // 临时值
            });

            self.order_id_index.insert(cmd.order_id, order_idx);
            self.insert_order(order_idx);
        }
    }

    /// IOC 下单
    fn place_ioc(&mut self, cmd: &mut OrderCommand) {
        let filled = self.try_match(cmd).filled;
        let rejected = cmd.size - filled;

        if rejected > 0 {
            cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                rejected,
                cmd.price,
                cmd.reserve_price,
            ));
        }
    }

    /// FOK_BUDGET 下单
    fn place_fok_budget(&mut self, cmd: &mut OrderCommand) {
        let budget = self.check_budget_to_fill(cmd.size, cmd.action);

        if let Some(calculated) = budget {
            if self.is_budget_satisfied(cmd.action, calculated, cmd.price) {
                let MatchResult {
                    filled,
                    taker_aborted,
                } = self.try_match(cmd);
                // FOK 语义要求全成或全撤；STP 中断或未能全额成交时退还未成交部分
                if taker_aborted && filled < cmd.size {
                    cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                        cmd.size - filled,
                        cmd.price,
                        cmd.reserve_price,
                    ));
                }
            } else {
                cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                    cmd.size,
                    cmd.price,
                    cmd.reserve_price,
                ));
            }
        } else {
            cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                cmd.size,
                cmd.price,
                cmd.reserve_price,
            ));
        }
    }

    fn is_budget_satisfied(&self, action: OrderAction, calculated: i64, limit: i64) -> bool {
        calculated != i64::MAX && (calculated == limit || (action == OrderAction::Bid) != (calculated > limit))
    }

    fn check_budget_to_fill(&self, mut size: Size, action: OrderAction) -> Option<i64> {
        let mut maker_idx = match action {
            OrderAction::Bid => self.best_ask_order,
            OrderAction::Ask => self.best_bid_order,
        };

        let mut budget: i64 = 0;

        while let Some(idx) = maker_idx {
            let order = &self.orders[idx];
            let bucket = &self.buckets[order.parent];
            let available = bucket.volume;
            let price = order.price;

            if size > available {
                size -= available;
                budget += (available * price) as i64;
                // 移动到桶尾部订单的前一个订单
                let tail_idx = bucket.tail;
                maker_idx = self.orders[tail_idx].prev;
            } else {
                return Some(budget + (size * price) as i64);
            }
        }

        None
    }

    /// 尝试撮合
    fn try_match(&mut self, cmd: &mut OrderCommand) -> MatchResult {
        let is_bid = cmd.action == OrderAction::Bid;
        let limit_price = cmd.price;

        let mut maker_idx = if is_bid {
            self.best_ask_order
        } else {
            self.best_bid_order
        };

        // 检查是否有可撮合订单
        if let Some(idx) = maker_idx {
            let maker_price = self.orders[idx].price;
            if is_bid && maker_price > limit_price {
                return MatchResult::filled(0);
            }
            if !is_bid && maker_price < limit_price {
                return MatchResult::filled(0);
            }
        } else {
            return MatchResult::filled(0);
        }

        let mut filled = 0;
        let mut taker_aborted = false;
        let taker_size = cmd.size;
        let taker_reserve = cmd.reserve_price;
        let stp = self.symbol_spec.stp;

        // 最优价指针只能推进到"队首被连续移除"的位置。STP 跳过或保留任何一单后，
        // 它就必须停在那一单上，否则被保留的订单会脱离最优价链而变得不可达。
        let mut new_best = maker_idx;
        let mut front_intact = true;

        while let Some(idx) = maker_idx {
            let remaining = taker_size - filled;
            if remaining == 0 {
                break;
            }

            let (maker_price, maker_filled, maker_size, maker_parent, maker_prev) = {
                let order = &self.orders[idx];
                if is_bid && order.price > limit_price {
                    break;
                }
                if !is_bid && order.price < limit_price {
                    break;
                }
                (order.price, order.filled, order.size, order.parent, order.prev)
            };

            // ---- 自成交防范 ----
            if self.orders[idx].uid == cmd.uid && stp != SelfTradePrevention::None {
                let (cancel_maker, abort_taker) = match stp {
                    SelfTradePrevention::CancelTaker => (false, true),
                    SelfTradePrevention::CancelMaker => (true, false),
                    SelfTradePrevention::CancelBoth => (true, true),
                    SelfTradePrevention::Skip => (false, false),
                    SelfTradePrevention::None => unreachable!(),
                };

                if cancel_maker {
                    // 释放该挂单剩余量的冻结资金：方向是 taker 的对手方
                    let maker_remaining = maker_size - maker_filled;
                    cmd.matcher_events.push(MatcherTradeEvent::new_reject_maker(
                        maker_remaining,
                        maker_price,
                        self.orders[idx].reserve_price,
                        self.orders[idx].order_id,
                        self.orders[idx].uid,
                    ));
                    self.order_id_index.remove(&self.orders[idx].order_id);
                    self.remove_order(idx);
                    self.orders.remove(idx);
                    if front_intact {
                        new_best = maker_prev;
                    }
                } else {
                    // 这一单被保留下来，最优价指针不能再越过它
                    front_intact = false;
                }

                if abort_taker {
                    taker_aborted = true;
                    break;
                }

                // Skip / CancelMaker：跳过这一单，继续向后撮合
                maker_idx = maker_prev;
                continue;
            }

            let trade_size = remaining.min(maker_size - maker_filled);

            // 更新 maker 订单
            {
                let order = &mut self.orders[idx];
                order.filled += trade_size;
            }

            // 更新桶
            self.buckets[maker_parent].volume -= trade_size;
            filled += trade_size;

            let maker_completed = maker_filled + trade_size == maker_size;

            // 生成事件
            let event = MatcherTradeEvent::new_trade(
                trade_size,
                maker_price,
                self.orders[idx].order_id,
                self.orders[idx].uid,
                if is_bid { taker_reserve } else { self.orders[idx].reserve_price },
            );
            cmd.matcher_events.push(event);

            if !maker_completed {
                // 挂单仍有剩余，留在簿中。此时 new_best 尚未越过它，正好停在它上面。
                break;
            }

            // 移除完全成交的 maker 订单。必须走 remove_order：它会修复链表邻居
            // 与桶的记账——STP 保留过前面的订单时，这一单可能已不在队首，
            // 直接从 slab 删除会留下悬空的 prev/next 指针。
            self.order_id_index.remove(&self.orders[idx].order_id);
            self.remove_order(idx);
            self.orders.remove(idx);
            if front_intact {
                new_best = maker_prev;
            }

            maker_idx = maker_prev;
        }

        // 更新最优订单
        if is_bid {
            self.best_ask_order = new_best;
        } else {
            self.best_bid_order = new_best;
        }

        MatchResult {
            filled,
            taker_aborted,
        }
    }

    /// 全簿不变量自检。仅在 debug 构建下运行，release 里被完全编译掉。
    ///
    /// 挂一张单要协同修改约 10 处状态（链表指针、桶记账、BTreeMap 档位、best 指针、
    /// 哈希索引、槽位），漏掉任何一处都不会崩溃 —— 只是数字悄悄错了，或者某张订单
    /// 从链上脱落变得既撮合不到也撤不掉。这个函数的作用就是把这类静默故障
    /// **变成写错的那一条命令上的即时崩溃**，而不是三小时后对账时才发现。
    #[cfg(debug_assertions)]
    fn assert_invariants(&self) {
        use std::collections::HashSet;

        // ---- 1. 索引与槽位一一对应 ----
        assert_eq!(
            self.order_id_index.len(),
            self.orders.len(),
            "order_id_index({}) 与活跃订单数({}) 不符：有订单泄漏或索引残留",
            self.order_id_index.len(),
            self.orders.len()
        );
        for (&order_id, &idx) in &self.order_id_index {
            let order = self
                .orders
                .get(idx)
                .unwrap_or_else(|| panic!("order_id {order_id} 指向已释放的槽位 {idx}"));
            assert_eq!(
                order.order_id, order_id,
                "槽位 {idx} 上的订单是 {}，索引却认为是 {order_id}（槽位被复用后未回填）",
                order.order_id
            );
        }

        // ---- 2. 逐桶核对：volume / num_orders / tail ----
        for (is_ask, map) in [
            (true, &self.ask_price_buckets),
            (false, &self.bid_price_buckets),
        ] {
            for (&price, &bucket_idx) in map {
                let bucket = &self.buckets[bucket_idx];
                assert_eq!(bucket.price, price, "BTreeMap 键 {price} 与桶内价格不符");

                // 沿 prev 走完本档，累计剩余量与笔数
                let mut volume = 0;
                let mut count = 0;
                let mut cursor = self.head_of_bucket(bucket_idx, is_ask);
                let mut last = None;
                while let Some(idx) = cursor {
                    let o = &self.orders[idx];
                    if o.parent != bucket_idx {
                        break; // 已跨出本档
                    }
                    assert_eq!(o.price, price, "订单 {} 挂在 {price} 档却记着价格 {}", o.order_id, o.price);
                    assert!(o.filled <= o.size, "订单 {} 成交量超过下单量", o.order_id);
                    volume += o.size - o.filled;
                    count += 1;
                    last = Some(idx);
                    cursor = o.prev;
                }

                assert_eq!(
                    bucket.volume, volume,
                    "{}档 {price} 的 volume 记账错：记着 {}，实际 {volume}",
                    if is_ask { "卖" } else { "买" }, bucket.volume
                );
                assert_eq!(
                    bucket.num_orders, count,
                    "{}档 {price} 的 num_orders 记账错：记着 {}，实际 {count}",
                    if is_ask { "卖" } else { "买" }, bucket.num_orders
                );
                assert!(count > 0, "{price} 档为空却没有被删除");
                assert_eq!(
                    Some(bucket.tail), last,
                    "{price} 档的 tail 不是沿 prev 走到的最后一单"
                );
            }
        }

        // ---- 3. 从 best 出发能遍历到全部订单，且价格单调 ----
        let mut seen = HashSet::new();
        for (is_ask, best) in [(true, self.best_ask_order), (false, self.best_bid_order)] {
            let mut cursor = best;
            let mut prev_price: Option<Price> = None;
            while let Some(idx) = cursor {
                assert!(
                    seen.insert(idx),
                    "优先级链上出现环：槽位 {idx} 被访问了两次"
                );
                let o = &self.orders[idx];
                assert_eq!(
                    o.action == OrderAction::Ask,
                    is_ask,
                    "订单 {} 出现在了对手方向的链上",
                    o.order_id
                );
                if let Some(pp) = prev_price {
                    // 卖盘沿 prev 价格递增，买盘递减
                    if is_ask {
                        assert!(o.price >= pp, "卖盘优先级链价格非递增：{pp} -> {}", o.price);
                    } else {
                        assert!(o.price <= pp, "买盘优先级链价格非递减：{pp} -> {}", o.price);
                    }
                }
                prev_price = Some(o.price);
                cursor = o.prev;
            }
        }
        assert_eq!(
            seen.len(),
            self.orders.len(),
            "有 {} 张订单从 best 指针出发不可达 —— 它们既撮合不到也撤不掉",
            self.orders.len() - seen.len()
        );
    }

    #[cfg(not(debug_assertions))]
    #[inline(always)]
    fn assert_invariants(&self) {}

    /// 找到某个价格档在优先级链上的队首（自检用，O(链长)，不在热路径上）
    #[cfg(debug_assertions)]
    fn head_of_bucket(&self, bucket_idx: BucketIdx, is_ask: bool) -> Option<OrderIdx> {
        let mut cursor = if is_ask { self.best_ask_order } else { self.best_bid_order };
        while let Some(idx) = cursor {
            if self.orders[idx].parent == bucket_idx {
                return Some(idx);
            }
            cursor = self.orders[idx].prev;
        }
        None
    }

    /// 插入订单到链表
    fn insert_order(&mut self, order_idx: OrderIdx) {
        let (price, action) = {
            let order = &self.orders[order_idx];
            (order.price, order.action)
        };

        let is_ask = action == OrderAction::Ask;
        let buckets_map = if is_ask { &mut self.ask_price_buckets } else { &mut self.bid_price_buckets };

        if let Some(&bucket_idx) = buckets_map.get(&price) {
            // 桶已存在，添加到尾部
            let old_tail = self.buckets[bucket_idx].tail;
            let prev_order = self.orders[old_tail].prev;

            self.buckets[bucket_idx].tail = order_idx;
            self.buckets[bucket_idx].volume += self.orders[order_idx].size - self.orders[order_idx].filled;
            self.buckets[bucket_idx].num_orders += 1;

            self.orders[old_tail].prev = Some(order_idx);
            if let Some(prev_idx) = prev_order {
                self.orders[prev_idx].next = Some(order_idx);
            }

            self.orders[order_idx].next = Some(old_tail);
            self.orders[order_idx].prev = prev_order;
            self.orders[order_idx].parent = bucket_idx;
        } else {
            // 创建新桶
            let bucket_idx = self.buckets.insert(Bucket {
                price,
                volume: self.orders[order_idx].size - self.orders[order_idx].filled,
                num_orders: 1,
                tail: order_idx,
            });

            buckets_map.insert(price, bucket_idx);
            self.orders[order_idx].parent = bucket_idx;

            // 链接到链表
            let lower_bucket_idx = if is_ask {
                buckets_map.range(..price).next_back().map(|(_, &idx)| idx)
            } else {
                buckets_map.range((price + 1)..).next().map(|(_, &idx)| idx)
            };

            if let Some(lower_idx) = lower_bucket_idx {
                let lower_tail = self.buckets[lower_idx].tail;
                let prev_order = self.orders[lower_tail].prev;

                self.orders[lower_tail].prev = Some(order_idx);
                if let Some(prev_idx) = prev_order {
                    self.orders[prev_idx].next = Some(order_idx);
                }

                self.orders[order_idx].next = Some(lower_tail);
                self.orders[order_idx].prev = prev_order;
            } else {
                // 更新最优订单
                let old_best = if is_ask { self.best_ask_order } else { self.best_bid_order };
                if let Some(old_idx) = old_best {
                    self.orders[old_idx].next = Some(order_idx);
                }

                if is_ask {
                    self.best_ask_order = Some(order_idx);
                } else {
                    self.best_bid_order = Some(order_idx);
                }

                self.orders[order_idx].next = None;
                self.orders[order_idx].prev = old_best;
            }
        }
    }
    /// 移除订单
    fn remove_order(&mut self, order_idx: OrderIdx) -> bool {
        let (bucket_idx, remaining, next, prev, price, action) = {
            let order = &self.orders[order_idx];
            (
                order.parent,
                order.size - order.filled,
                order.next,
                order.prev,
                order.price,
                order.action,
            )
        };

        // 更新桶
        self.buckets[bucket_idx].volume -= remaining;
        self.buckets[bucket_idx].num_orders -= 1;

        let mut should_remove_bucket = false;

        // 如果是尾部订单
        if self.buckets[bucket_idx].tail == order_idx {
            if let Some(next_idx) = next {
                if self.orders[next_idx].parent != bucket_idx {
                    should_remove_bucket = true;
                } else {
                    self.buckets[bucket_idx].tail = next_idx;
                }
            } else {
                should_remove_bucket = true;
            }
        }

        // 更新邻居订单
        if let Some(next_idx) = next {
            self.orders[next_idx].prev = prev;
        }
        if let Some(prev_idx) = prev {
            self.orders[prev_idx].next = next;
        }

        // 更新最优订单
        if Some(order_idx) == self.best_ask_order {
            self.best_ask_order = prev;
        } else if Some(order_idx) == self.best_bid_order {
            self.best_bid_order = prev;
        }

        // 移除桶
        if should_remove_bucket {
            if action == OrderAction::Ask {
                self.ask_price_buckets.remove(&price);
            } else {
                self.bid_price_buckets.remove(&price);
            }
            self.buckets.remove(bucket_idx);
        }

        should_remove_bucket
    }
}

impl super::OrderBook for DirectOrderBook {
    fn new_order(&mut self, cmd: &mut OrderCommand) -> CommandResultCode {
        // 重复 order_id 一律拒绝。早先的做法是拿重复命令去撮合再拒绝剩余量，
        // 既可能产生非预期成交，又会让调用方收到"成功"。
        if self.order_id_index.contains_key(&cmd.order_id) {
            return CommandResultCode::MatchingDuplicateOrderId;
        }

        let code = match cmd.order_type {
            OrderType::Gtc => {
                self.place_gtc(cmd);
                CommandResultCode::Success
            }
            OrderType::Ioc => {
                self.place_ioc(cmd);
                CommandResultCode::Success
            }
            OrderType::FokBudget => {
                self.place_fok_budget(cmd);
                CommandResultCode::Success
            }
            _ => CommandResultCode::MatchingUnsupportedCommand,
        };
        self.assert_invariants();
        code
    }

    fn cancel_order(&mut self, cmd: &mut OrderCommand) -> CommandResultCode {
        let Some(&order_idx) = self.order_id_index.get(&cmd.order_id) else {
            return CommandResultCode::MatchingUnknownOrderId;
        };

        let (action, remaining, price, reserve_price) = {
            let order = &self.orders[order_idx];
            if order.uid != cmd.uid {
                return CommandResultCode::MatchingUnknownOrderId;
            }
            (order.action, order.size - order.filled, order.price, order.reserve_price)
        };

        self.order_id_index.remove(&cmd.order_id);
        self.remove_order(order_idx);
        self.orders.remove(order_idx);

        cmd.action = action;
        cmd.matcher_events.push(MatcherTradeEvent::new_reject(remaining, price, reserve_price));

        self.assert_invariants();
        CommandResultCode::Success
    }

    fn move_order(&mut self, cmd: &mut OrderCommand) -> CommandResultCode {
        let Some(&order_idx) = self.order_id_index.get(&cmd.order_id) else {
            return CommandResultCode::MatchingUnknownOrderId;
        };

        if cmd.price <= 0 {
            return CommandResultCode::RiskInvalidOrderParams;
        }

        let (uid, action, reserve_price, size, filled_before) = {
            let order = &self.orders[order_idx];
            if order.uid != cmd.uid {
                return CommandResultCode::MatchingUnknownOrderId;
            }
            (
                order.uid,
                order.action,
                order.reserve_price,
                order.size,
                order.filled,
            )
        };

        // 风险检查
        if self.symbol_spec.symbol_type == SymbolType::CurrencyExchangePair
            && action == OrderAction::Bid
            && cmd.price > reserve_price
        {
            return CommandResultCode::RiskInvalidReserveBidPrice;
        }

        // 移除订单
        self.remove_order(order_idx);

        // 更新价格
        self.orders[order_idx].price = cmd.price;
        cmd.action = action;

        // 尝试撮合。只能拿"剩余未成交量"去撮合：
        // 用原始 size 会把已成交部分再成交一遍，凭空造出多余成交量。
        let mut temp_cmd = OrderCommand {
            uid,
            order_id: cmd.order_id,
            symbol: cmd.symbol,
            price: cmd.price,
            size: size - filled_before,
            action,
            reserve_price,
            ..Default::default()
        };

        let MatchResult {
            filled: newly_filled,
            taker_aborted,
        } = self.try_match(&mut temp_cmd);
        cmd.matcher_events.extend(temp_cmd.matcher_events);

        // 累加而非覆盖，否则改价会抹掉此前的成交记录
        let total_filled = filled_before + newly_filled;
        self.orders[order_idx].filled = total_filled;

        // STP 判定撤销：改价后的订单不再挂回簿中，剩余量退还冻结资金
        if taker_aborted && total_filled < size {
            cmd.matcher_events.push(MatcherTradeEvent::new_reject(
                size - total_filled,
                cmd.price,
                reserve_price,
            ));
            self.order_id_index.remove(&cmd.order_id);
            self.orders.remove(order_idx);
            self.assert_invariants();
            return CommandResultCode::Success;
        }

        if total_filled >= size {
            // 完全成交
            self.order_id_index.remove(&cmd.order_id);
            self.orders.remove(order_idx);
        } else {
            // 部分成交，重新挂单
            self.insert_order(order_idx);
        }

        self.assert_invariants();
        CommandResultCode::Success
    }

    fn reduce_order(&mut self, cmd: &mut OrderCommand) -> CommandResultCode {
        let Some(&order_idx) = self.order_id_index.get(&cmd.order_id) else {
            return CommandResultCode::MatchingUnknownOrderId;
        };

        if cmd.size <= 0 {
            return CommandResultCode::MatchingInvalidOrderSize;
        }

        let (action, remaining, price, parent_idx, reserve_price) = {
            let order = &self.orders[order_idx];
            if order.uid != cmd.uid {
                return CommandResultCode::MatchingUnknownOrderId;
            }
            (order.action, order.size - order.filled, order.price, order.parent, order.reserve_price)
        };

        let reduce_by = remaining.min(cmd.size);
        let can_remove = reduce_by == remaining;

        if can_remove {
            self.order_id_index.remove(&cmd.order_id);
            self.remove_order(order_idx);
            self.orders.remove(order_idx);
        } else {
            let order = &mut self.orders[order_idx];
            order.size -= reduce_by;
            self.buckets[parent_idx].volume -= reduce_by;
        }

        cmd.action = action;
        cmd.matcher_events.push(MatcherTradeEvent::new_reject(reduce_by, price, reserve_price));

        self.assert_invariants();
        CommandResultCode::Success
    }

    fn get_symbol_spec(&self) -> &CoreSymbolSpecification {
        &self.symbol_spec
    }

    fn get_l2_data(&self, depth: usize) -> L2MarketData {
        let mut data = L2MarketData::new(depth);

        for (price, &bucket_idx) in self.ask_price_buckets.iter().take(depth) {
            data.ask_prices.push(*price);
            data.ask_volumes.push(self.buckets[bucket_idx].volume);
            data.ask_order_counts.push(self.buckets[bucket_idx].num_orders);
        }

        for (price, &bucket_idx) in self.bid_price_buckets.iter().rev().take(depth) {
            data.bid_prices.push(*price);
            data.bid_volumes.push(self.buckets[bucket_idx].volume);
            data.bid_order_counts.push(self.buckets[bucket_idx].num_orders);
        }

        data
    }

    fn get_order_by_id(&self, order_id: OrderId) -> Option<(Price, OrderAction)> {
        self.order_id_index.get(&order_id).map(|&idx| {
            let order = &self.orders[idx];
            (order.price, order.action)
        })
    }

    fn get_total_ask_volume(&self) -> Size {
        self.ask_price_buckets.values().map(|&idx| self.buckets[idx].volume).sum()
    }

    fn get_total_bid_volume(&self) -> Size {
        self.bid_price_buckets.values().map(|&idx| self.buckets[idx].volume).sum()
    }

    fn get_ask_buckets_count(&self) -> usize {
        self.ask_price_buckets.len()
    }

    fn get_bid_buckets_count(&self) -> usize {
        self.bid_price_buckets.len()
    }

    fn serialize_state(&self) -> crate::core::orderbook::OrderBookState {
        crate::core::orderbook::OrderBookState::Direct(self.clone())
    }
}
