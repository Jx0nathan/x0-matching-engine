//! 资金守恒测试：覆盖引擎真实路径（ExchangeCore → RiskEngine → DirectOrderBook）。
//!
//! 核心不变量：任何指令序列执行后，
//!   Σ 用户可用余额 + Σ 订单占用的冻结额 + 手续费归集账户 = Σ 入金
//! 由于冻结额没有单独的账本，测试统一在"撤净所有挂单"后校验，
//! 此时冻结额必然为 0，等式退化为 Σ 余额 + 手续费 = Σ 入金。

use matching_core::api::*;
use matching_core::core::exchange::{ExchangeConfig, ExchangeCore};

const BASE: Currency = 0;
const QUOTE: Currency = 1;
const SYM: SymbolId = 1;

const TAKER_FEE: i64 = 2;
const MAKER_FEE: i64 = 1;

struct Harness {
    core: ExchangeCore,
    /// 每个用户的出入金流水号必须严格递增
    next_tx_id: std::collections::HashMap<UserId, i64>,
    deposited_base: i64,
    deposited_quote: i64,
}

impl Harness {
    fn new() -> Self {
        let mut core = ExchangeCore::new(ExchangeConfig::default());
        core.add_symbol(CoreSymbolSpecification {
            symbol_id: SYM,
            symbol_type: SymbolType::CurrencyExchangePair,
            base_currency: BASE,
            quote_currency: QUOTE,
            base_scale_k: 1,
            quote_scale_k: 1,
            taker_fee: TAKER_FEE,
            maker_fee: MAKER_FEE,
            margin_buy: 0,
            margin_sell: 0,
        })
        .expect("注册交易对失败");
        Self {
            core,
            next_tx_id: std::collections::HashMap::new(),
            deposited_base: 0,
            deposited_quote: 0,
        }
    }

    fn add_user(&mut self, uid: UserId) {
        let code = self
            .core
            .submit_command(OrderCommand {
                command: OrderCommandType::AddUser,
                uid,
                ..Default::default()
            })
            .result_code;
        assert_eq!(code, CommandResultCode::Success, "建用户失败");
    }

    fn deposit(&mut self, uid: UserId, currency: Currency, amount: i64) -> CommandResultCode {
        let tx = self.next_tx_id.entry(uid).or_insert(0);
        *tx += 1;
        let tx_id = *tx;
        self.deposit_with_tx(uid, currency, amount, tx_id)
    }

    fn deposit_with_tx(
        &mut self,
        uid: UserId,
        currency: Currency,
        amount: i64,
        tx_id: i64,
    ) -> CommandResultCode {
        let code = self
            .core
            .submit_command(OrderCommand {
                command: OrderCommandType::BalanceAdjustment,
                uid,
                symbol: currency,
                price: amount,
                order_id: tx_id as u64,
                ..Default::default()
            })
            .result_code;
        if code == CommandResultCode::Success {
            match currency {
                BASE => self.deposited_base += amount,
                QUOTE => self.deposited_quote += amount,
                _ => {}
            }
        }
        code
    }

    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        uid: UserId,
        order_id: OrderId,
        action: OrderAction,
        order_type: OrderType,
        price: Price,
        reserve_price: Price,
        size: Size,
    ) -> CommandResultCode {
        self.core
            .submit_command(OrderCommand {
                command: OrderCommandType::PlaceOrder,
                uid,
                order_id,
                symbol: SYM,
                price,
                reserve_price,
                size,
                action,
                order_type,
                ..Default::default()
            })
            .result_code
    }

    fn cancel(&mut self, uid: UserId, order_id: OrderId) -> CommandResultCode {
        self.core
            .submit_command(OrderCommand {
                command: OrderCommandType::CancelOrder,
                uid,
                order_id,
                symbol: SYM,
                ..Default::default()
            })
            .result_code
    }

    fn move_order(&mut self, uid: UserId, order_id: OrderId, price: Price) -> CommandResultCode {
        self.core
            .submit_command(OrderCommand {
                command: OrderCommandType::MoveOrder,
                uid,
                order_id,
                symbol: SYM,
                price,
                ..Default::default()
            })
            .result_code
    }

    fn balance(&self, uid: UserId, currency: Currency) -> i64 {
        self.core.balance_of(uid, currency).expect("pipeline 可用")
    }

    fn fees(&self, currency: Currency) -> i64 {
        self.core.fees_collected(currency).expect("pipeline 可用")
    }

    /// 在所有挂单都已撤净的前提下校验资金守恒
    fn assert_conserved(&self, uids: &[UserId]) {
        let quote: i64 = uids.iter().map(|&u| self.balance(u, QUOTE)).sum::<i64>() + self.fees(QUOTE);
        let base: i64 = uids.iter().map(|&u| self.balance(u, BASE)).sum::<i64>() + self.fees(BASE);

        assert_eq!(
            quote, self.deposited_quote,
            "quote 不守恒：账面 {} != 入金 {}",
            quote, self.deposited_quote
        );
        assert_eq!(
            base, self.deposited_base,
            "base 不守恒：账面 {} != 入金 {}",
            base, self.deposited_base
        );
    }
}

/// 撤单必须原额退还冻结资金（原 bug：Reject 事件的 bidder_hold_price 恒为 0，退款为 0）
#[test]
fn cancel_refunds_exact_hold() {
    let mut h = Harness::new();
    h.add_user(1);
    h.deposit(1, QUOTE, 10_000);
    let before = h.balance(1, QUOTE);

    assert_eq!(
        h.place(1, 1, OrderAction::Bid, OrderType::Gtc, 100, 100, 10),
        CommandResultCode::Success
    );
    // 冻结 = 10*100 + 10*2 = 1020
    assert_eq!(h.balance(1, QUOTE), before - 1020, "冻结额与公式不符");

    assert_eq!(h.cancel(1, 1), CommandResultCode::Success);
    assert_eq!(h.balance(1, QUOTE), before, "撤单后余额未回到原值");
    h.assert_conserved(&[1]);
}

/// IOC 未成交部分必须退款
#[test]
fn unfilled_ioc_refunds() {
    let mut h = Harness::new();
    h.add_user(1);
    h.deposit(1, QUOTE, 10_000);
    let before = h.balance(1, QUOTE);

    // 簿子是空的，IOC 全额未成交
    h.place(1, 1, OrderAction::Bid, OrderType::Ioc, 100, 100, 10);

    assert_eq!(h.balance(1, QUOTE), before, "IOC 未成交部分没有退款");
    h.assert_conserved(&[1]);
}

/// 非正数 size / price 必须在冻结前被拒（原 bug：负数 size 让冻结额为负 = 凭空发钱）
#[test]
fn non_positive_params_rejected() {
    let mut h = Harness::new();
    h.add_user(1);
    h.deposit(1, QUOTE, 1_000);

    for (price, size) in [(100, -10), (100, 0), (-100, 10), (0, 10)] {
        let code = h.place(1, 1, OrderAction::Bid, OrderType::Gtc, price, price.max(1), size);
        assert_eq!(
            code,
            CommandResultCode::RiskInvalidOrderParams,
            "price={price} size={size} 未被拒绝"
        );
    }

    assert_eq!(h.balance(1, QUOTE), 1_000, "非法订单改动了余额");
    h.assert_conserved(&[1]);
}

/// 买单的冻结价不得低于委托价
#[test]
fn reserve_below_price_rejected() {
    let mut h = Harness::new();
    h.add_user(1);
    h.deposit(1, QUOTE, 10_000);

    let code = h.place(1, 1, OrderAction::Bid, OrderType::Gtc, 100, 90, 10);
    assert_eq!(code, CommandResultCode::RiskInvalidReserveBidPrice);
    assert_eq!(h.balance(1, QUOTE), 10_000);
}

/// 撮合层不支持的订单类型必须回滚资金，且不得回报 Success
/// （原 bug：返回码被丢弃并硬置 Success，资金永久冻结）
#[test]
fn unsupported_order_type_refunds_and_reports_failure() {
    let mut h = Harness::new();
    h.add_user(1);
    h.deposit(1, QUOTE, 10_000);
    let before = h.balance(1, QUOTE);

    // DirectOrderBook 目前只支持 Gtc / Ioc / FokBudget
    for (i, ot) in [
        OrderType::Fok,
        OrderType::PostOnly,
        OrderType::Iceberg,
        OrderType::Day,
        OrderType::StopLimit,
        OrderType::Gtd(9999),
    ]
    .into_iter()
    .enumerate()
    {
        let code = h.place(1, 100 + i as u64, OrderAction::Bid, ot, 100, 100, 10);
        assert_eq!(
            code,
            CommandResultCode::MatchingUnsupportedCommand,
            "{ot:?} 应回报不支持而非成功"
        );
        assert_eq!(h.balance(1, QUOTE), before, "{ot:?} 被拒后资金未退还");
    }

    h.assert_conserved(&[1]);
}

/// 完整成交后，双方余额与手续费账户之和必须等于入金
#[test]
fn full_trade_conserves_funds_with_fees() {
    let mut h = Harness::new();
    h.add_user(1); // maker，卖 base
    h.add_user(2); // taker，买 base
    h.deposit(1, BASE, 1_000);
    h.deposit(2, QUOTE, 10_000);

    // maker 挂卖 10 @ 100
    assert_eq!(
        h.place(1, 1, OrderAction::Ask, OrderType::Gtc, 100, 100, 10),
        CommandResultCode::Success
    );
    // taker 吃掉全部
    assert_eq!(
        h.place(2, 2, OrderAction::Bid, OrderType::Gtc, 100, 100, 10),
        CommandResultCode::Success
    );

    // taker 付出 10*100 + 10*2(taker费) = 1020
    assert_eq!(h.balance(2, QUOTE), 10_000 - 1020);
    assert_eq!(h.balance(2, BASE), 10);
    // maker 收到 10*100 - 10*1(maker费) = 990
    assert_eq!(h.balance(1, QUOTE), 990);
    assert_eq!(h.balance(1, BASE), 1_000 - 10);
    // 手续费归集 = 20 + 10
    assert_eq!(h.fees(QUOTE), 30);

    h.assert_conserved(&[1, 2]);
}

/// taker 卖、maker 买：maker 冻结时按 taker 费率预留，成交后须退还费率差
#[test]
fn maker_is_charged_maker_fee_not_taker_fee() {
    let mut h = Harness::new();
    h.add_user(1); // maker，挂买
    h.add_user(2); // taker，卖
    h.deposit(1, QUOTE, 10_000);
    h.deposit(2, BASE, 1_000);

    assert_eq!(
        h.place(1, 1, OrderAction::Bid, OrderType::Gtc, 100, 100, 10),
        CommandResultCode::Success
    );
    assert_eq!(
        h.place(2, 2, OrderAction::Ask, OrderType::Gtc, 100, 100, 10),
        CommandResultCode::Success
    );

    // maker 实付 10*100 + 10*1(maker费) = 1010，而非按 taker 费率的 1020
    assert_eq!(
        h.balance(1, QUOTE),
        10_000 - 1010,
        "maker 被按 taker 费率收费了"
    );
    assert_eq!(h.balance(1, BASE), 10);
    // taker 收到 10*100 - 10*2 = 980
    assert_eq!(h.balance(2, QUOTE), 980);
    assert_eq!(h.fees(QUOTE), 30);

    h.assert_conserved(&[1, 2]);
}

/// 部分成交后撤单：已成交部分正常结算，剩余部分原额退还
#[test]
fn partial_fill_then_cancel_conserves() {
    let mut h = Harness::new();
    h.add_user(1);
    h.add_user(2);
    h.deposit(1, QUOTE, 10_000);
    h.deposit(2, BASE, 1_000);

    // maker 挂买 100 @ 50
    h.place(1, 1, OrderAction::Bid, OrderType::Gtc, 50, 50, 100);
    // taker 只卖 40
    h.place(2, 2, OrderAction::Ask, OrderType::Gtc, 50, 50, 40);
    // maker 撤掉剩余 60
    assert_eq!(h.cancel(1, 1), CommandResultCode::Success);

    assert_eq!(h.balance(1, BASE), 40);
    assert_eq!(h.balance(2, BASE), 1_000 - 40);
    h.assert_conserved(&[1, 2]);
}

/// 改价不得让订单成交出超过原始数量的量
/// （原 bug：move 用原始 size 重新撮合且覆盖 filled）
#[test]
fn move_order_does_not_overfill() {
    let mut h = Harness::new();
    h.add_user(1);
    h.add_user(2);
    h.deposit(1, QUOTE, 100_000);
    h.deposit(2, BASE, 1_000);

    // maker 买 10 @ 90，冻结价 100
    h.place(1, 1, OrderAction::Bid, OrderType::Gtc, 90, 100, 10);
    // taker 卖 4 @ 90 => maker 成交 4，剩 6
    h.place(2, 2, OrderAction::Ask, OrderType::Gtc, 90, 90, 4);
    assert_eq!(h.balance(1, BASE), 4);

    // 对手方在 95 挂 10 手卖单
    h.place(2, 3, OrderAction::Ask, OrderType::Gtc, 95, 95, 10);
    // maker 改价到 95：只应成交剩余的 6，而不是原始的 10
    assert_eq!(h.move_order(1, 1, 95), CommandResultCode::Success);

    assert_eq!(
        h.balance(1, BASE),
        10,
        "改价后成交量超过了订单原始数量"
    );

    // 撤净剩余挂单后校验守恒
    h.cancel(2, 3);
    h.assert_conserved(&[1, 2]);
}

/// 重复流水号不得二次入账（原 bug：transaction_id 被忽略，WAL 重放会双倍充值）
#[test]
fn duplicate_balance_adjustment_is_ignored() {
    let mut h = Harness::new();
    h.add_user(1);

    assert_eq!(
        h.deposit_with_tx(1, QUOTE, 1_000, 1),
        CommandResultCode::Success
    );
    // 同一条流水重投
    assert_eq!(
        h.deposit_with_tx(1, QUOTE, 1_000, 1),
        CommandResultCode::UserMgmtDuplicateTransaction
    );
    // 过期流水
    assert_eq!(
        h.deposit_with_tx(1, QUOTE, 1_000, 0),
        CommandResultCode::UserMgmtDuplicateTransaction
    );

    assert_eq!(h.balance(1, QUOTE), 1_000, "重复流水被重复入账");
    h.assert_conserved(&[1]);
}

/// 出金不得把余额做成负数
#[test]
fn withdrawal_cannot_go_negative() {
    let mut h = Harness::new();
    h.add_user(1);
    h.deposit(1, QUOTE, 100);

    assert_eq!(
        h.deposit_with_tx(1, QUOTE, -500, 99),
        CommandResultCode::RiskNsf
    );
    assert_eq!(h.balance(1, QUOTE), 100);
    h.assert_conserved(&[1]);
}

/// 冻结额溢出 i64 时必须拒单，而不是回绕成小数字放行
#[test]
fn hold_overflow_is_rejected() {
    let mut h = Harness::new();
    h.add_user(1);
    h.deposit(1, QUOTE, i64::MAX);

    let code = h.place(
        1,
        1,
        OrderAction::Bid,
        OrderType::Gtc,
        i64::MAX / 2,
        i64::MAX / 2,
        1_000_000,
    );
    assert_eq!(code, CommandResultCode::RiskArithmeticOverflow);
    assert_eq!(h.balance(1, QUOTE), i64::MAX);
}
