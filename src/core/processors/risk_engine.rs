use crate::api::*;
use crate::core::users::UserProfileService;
use ahash::AHashMap;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct RiskEngine {
    shard_id: usize,
    shard_mask: u64,
    user_service: UserProfileService,
    symbols: AHashMap<SymbolId, CoreSymbolSpecification>, // 运行时使用 AHashMap
    /// 手续费归集账户，按币种累计。没有它，手续费从用户扣除后就凭空消失，系统资金不守恒。
    #[serde(default)]
    fees_collected: AHashMap<Currency, i64>,
}

impl RiskEngine {
    pub fn new(shard_id: usize, num_shards: usize) -> Self {
        assert!(num_shards.is_power_of_two());
        Self {
            shard_id,
            shard_mask: (num_shards - 1) as u64,
            user_service: UserProfileService::new(),
            symbols: AHashMap::new(),
            fees_collected: AHashMap::new(),
        }
    }

    /// 已归集的手续费（对账用）
    pub fn fees_collected(&self, currency: Currency) -> i64 {
        self.fees_collected.get(&currency).copied().unwrap_or(0)
    }

    /// 查询用户余额（对账 / 测试用）
    pub fn balance_of(&self, uid: UserId, currency: Currency) -> i64 {
        self.user_service
            .get_user(uid)
            .and_then(|p| p.accounts.get(&currency).copied())
            .unwrap_or(0)
    }

    fn uid_for_this_shard(&self, uid: UserId) -> bool {
        self.shard_mask == 0 || (uid & self.shard_mask) == self.shard_id as u64
    }

    pub fn add_symbol(&mut self, spec: CoreSymbolSpecification) {
        self.symbols.insert(spec.symbol_id, spec);
    }

    // R1: Pre-process
    pub fn pre_process(&mut self, cmd: &mut OrderCommand) {
        match cmd.command {
            OrderCommandType::PlaceOrder => {
                if self.uid_for_this_shard(cmd.uid) {
                    // 先把 reserve_price 归一成"实际冻结价"，使冻结与退款用同一个价格。
                    // 撮合层产生的 Reject 事件回传的正是 reserve_price。
                    Self::normalize_reserve_price(cmd);
                    cmd.result_code = self.place_order_risk_check(cmd);
                }
            }
            OrderCommandType::AddUser => {
                if self.uid_for_this_shard(cmd.uid) {
                    cmd.result_code = if self.user_service.add_user(cmd.uid) {
                        CommandResultCode::Success
                    } else {
                        CommandResultCode::UserMgmtUserAlreadyExists
                    };
                }
            }
            OrderCommandType::BalanceAdjustment => {
                if self.uid_for_this_shard(cmd.uid) {
                    cmd.result_code = self.user_service.balance_adjustment(
                        cmd.uid,
                        cmd.symbol,
                        cmd.price,
                        cmd.order_id as i64,
                    );
                }
            }
            _ => {}
        }
    }

    /// 把 reserve_price 统一成本单实际冻结所用的价格。
    ///
    /// 冻结（R1）与退款（R2）必须引用同一个价格，否则撤单会多退或少退。
    /// 预算型订单按委托价冻结，卖单不使用该字段。
    fn normalize_reserve_price(cmd: &mut OrderCommand) {
        cmd.reserve_price = match cmd.action {
            OrderAction::Bid => {
                if matches!(cmd.order_type, OrderType::FokBudget | OrderType::IocBudget) {
                    cmd.price
                } else {
                    cmd.reserve_price
                }
            }
            OrderAction::Ask => cmd.price,
        };
    }

    /// 计算下单需冻结的金额，溢出返回 None。
    fn hold_amount(cmd: &OrderCommand, spec: &CoreSymbolSpecification) -> Option<i64> {
        match cmd.action {
            // 买单冻结 quote：数量 × 冻结价 × 计价精度 + 数量 × taker 费率
            OrderAction::Bid => {
                let notional = cmd
                    .size
                    .checked_mul(cmd.reserve_price)?
                    .checked_mul(spec.quote_scale_k)?;
                let fee = cmd.size.checked_mul(spec.taker_fee)?;
                notional.checked_add(fee)
            }
            // 卖单冻结 base
            OrderAction::Ask => cmd.size.checked_mul(spec.base_scale_k),
        }
    }

    fn place_order_risk_check(&mut self, cmd: &OrderCommand) -> CommandResultCode {
        // 入参校验必须在任何金额计算之前：负数 size 会让冻结额变成负数，
        // 即"扣款"反而给用户加钱。
        if cmd.size <= 0 || cmd.price <= 0 || cmd.reserve_price <= 0 {
            return CommandResultCode::RiskInvalidOrderParams;
        }

        // 买单的冻结价不得低于委托价，否则成交时退款会超过冻结额
        if cmd.action == OrderAction::Bid && cmd.reserve_price < cmd.price {
            return CommandResultCode::RiskInvalidReserveBidPrice;
        }

        let Some(spec) = self.symbols.get(&cmd.symbol) else {
            return CommandResultCode::InvalidSymbol;
        };

        let currency = match cmd.action {
            OrderAction::Bid => spec.quote_currency,
            OrderAction::Ask => spec.base_currency,
        };

        let Some(hold_amount) = Self::hold_amount(cmd, spec) else {
            return CommandResultCode::RiskArithmeticOverflow;
        };

        let Some(profile) = self.user_service.get_user_mut(cmd.uid) else {
            return CommandResultCode::AuthInvalidUser;
        };

        let balance = profile.accounts.entry(currency).or_insert(0);
        if *balance >= hold_amount {
            *balance -= hold_amount;
            CommandResultCode::ValidForMatchingEngine
        } else {
            CommandResultCode::RiskNsf
        }
    }

    // R2: Post-process 结算
    pub fn post_process(&mut self, cmd: &mut OrderCommand) {
        if cmd.matcher_events.is_empty() {
            return;
        }

        let Some(spec) = self.symbols.get(&cmd.symbol).cloned() else {
            return;
        };

        let taker_sell = cmd.action == OrderAction::Ask;

        for event in &cmd.matcher_events {
            match event.event_type {
                MatcherEventType::Trade => {
                    self.handle_trade_event(cmd, event, &spec, taker_sell);
                }
                MatcherEventType::Reject | MatcherEventType::Reduce => {
                    self.handle_reject_event(cmd, event, &spec, taker_sell);
                }
                // STP 撤掉的是簿中挂单，其冻结资金在对手方向上，退款币种相反
                MatcherEventType::RejectMaker => {
                    self.handle_reject_event(cmd, event, &spec, !taker_sell);
                }
            }
        }
        // 不再无条件置 Success：撮合层的拒绝码（如 MatchingUnsupportedCommand）
        // 必须原样回传给调用方，否则失败会被伪装成成功。
    }

    /// 结算金额计算。溢出属于不变量被破坏（冻结额已校验可放入 i64，
    /// 结算额恒不超过冻结额），此时继续记账只会写坏余额，故直接 panic 停机。
    #[inline]
    fn mul3(a: i64, b: i64, c: i64) -> i64 {
        a.checked_mul(b)
            .and_then(|v| v.checked_mul(c))
            .expect("结算金额溢出 i64：资金不变量已被破坏")
    }

    /// 处理成交事件
    fn handle_trade_event(
        &mut self,
        cmd: &OrderCommand,
        event: &MatcherTradeEvent,
        spec: &CoreSymbolSpecification,
        taker_sell: bool,
    ) {
        // 成交额与双方手续费。冻结时买方一律按 taker 费率预留，
        // 若其实际成为 maker，需在此退还 taker 与 maker 的费率差。
        let notional = Self::mul3(event.size, event.price, spec.quote_scale_k);
        let base_amount = event
            .size
            .checked_mul(spec.base_scale_k)
            .expect("结算金额溢出 i64：资金不变量已被破坏");
        let taker_fee = event
            .size
            .checked_mul(spec.taker_fee)
            .expect("结算金额溢出 i64：资金不变量已被破坏");
        let maker_fee = event
            .size
            .checked_mul(spec.maker_fee)
            .expect("结算金额溢出 i64：资金不变量已被破坏");
        let fee_rebate = taker_fee - maker_fee; // 买方冻结按 taker 计，成为 maker 后退差额
        let price_diff = event.bidder_hold_price - event.price;
        let bid_refund = Self::mul3(event.size, price_diff, spec.quote_scale_k);

        // Taker 结算。手续费归集也绑定在 taker 所属分片：每个 uid 只属于一个分片，
        // 这样一笔成交的手续费在全局恰好入账一次，多分片下不会重复计。
        if self.uid_for_this_shard(cmd.uid) {
            *self.fees_collected.entry(spec.quote_currency).or_insert(0) += taker_fee + maker_fee;

            if let Some(taker) = self.user_service.get_user_mut(cmd.uid) {
                if taker_sell {
                    // 卖单：收入 quote 币，扣 taker 手续费
                    *taker.accounts.entry(spec.quote_currency).or_insert(0) += notional - taker_fee;
                } else {
                    // 买单：返还冻结价与成交价的差额 + 收入 base 币（手续费已在冻结时按 taker 扣除）
                    *taker.accounts.entry(spec.quote_currency).or_insert(0) += bid_refund;
                    *taker.accounts.entry(spec.base_currency).or_insert(0) += base_amount;
                }
            }
        }

        // Maker 结算
        if self.uid_for_this_shard(event.matched_order_uid) {
            if let Some(maker) = self.user_service.get_user_mut(event.matched_order_uid) {
                if taker_sell {
                    // Taker 卖 => Maker 买：退差价 + 费率差，收入 base 币
                    *maker.accounts.entry(spec.quote_currency).or_insert(0) +=
                        bid_refund + fee_rebate;
                    *maker.accounts.entry(spec.base_currency).or_insert(0) += base_amount;
                } else {
                    // Taker 买 => Maker 卖：收入 quote 币，扣 maker 手续费
                    *maker.accounts.entry(spec.quote_currency).or_insert(0) += notional - maker_fee;
                }
            }
        }
    }

    /// 处理拒绝/取消事件：把未成交部分的冻结资金原样退回
    fn handle_reject_event(
        &mut self,
        cmd: &OrderCommand,
        event: &MatcherTradeEvent,
        spec: &CoreSymbolSpecification,
        taker_sell: bool,
    ) {
        if !self.uid_for_this_shard(cmd.uid) {
            return;
        }

        // 退款金额必须与 R1 的冻结公式逐项对应，否则会多退或少退
        let refund_base = event
            .size
            .checked_mul(spec.base_scale_k)
            .expect("退款金额溢出 i64：资金不变量已被破坏");
        let refund_quote = Self::mul3(event.size, event.bidder_hold_price, spec.quote_scale_k)
            + event
                .size
                .checked_mul(spec.taker_fee)
                .expect("退款金额溢出 i64：资金不变量已被破坏");

        let Some(profile) = self.user_service.get_user_mut(cmd.uid) else {
            return;
        };

        if taker_sell {
            *profile.accounts.entry(spec.base_currency).or_insert(0) += refund_base;
        } else {
            *profile.accounts.entry(spec.quote_currency).or_insert(0) += refund_quote;
        }
    }
}

