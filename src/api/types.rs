use serde::{Deserialize, Serialize};
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

pub type UserId = u64;
pub type OrderId = u64;
pub type SymbolId = i32;
pub type Currency = i32;
pub type Price = i64;
pub type Size = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum OrderAction {
    Ask,
    Bid,
}

impl OrderAction {
    pub fn opposite(self) -> Self {
        match self {
            OrderAction::Ask => OrderAction::Bid,
            OrderAction::Bid => OrderAction::Ask,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum OrderType {
    Gtc,              // Good-Till-Cancel
    Ioc,              // Immediate-or-Cancel
    Fok,              // Fill-or-Kill
    FokBudget,        // FOK with budget
    IocBudget,        // IOC with budget
    PostOnly,         // 只做 Maker，不吃单
    StopLimit,        // 止损限价单
    StopMarket,       // 止损市价单
    Iceberg,          // 冰山单
    Day,              // 当日有效
    Gtd(i64),         // Good-Till-Date (时间戳)
}

/// 自成交防范策略（Self-Trade Prevention）。
///
/// 同一用户的买卖单互相成交属于洗盘交易，多数司法辖区不允许。
/// 按交易对配置，在撮合时生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum SelfTradePrevention {
    /// 不做任何防范，允许自成交。仅用于测试或明确不需要的场景。
    None,
    /// 撤销新来的订单（taker），保留簿中已有挂单。语义最严格。
    CancelTaker,
    /// 撤销簿中自己的挂单（maker），新单继续与其他对手撮合。
    CancelMaker,
    /// 双方都撤销。
    CancelBoth,
    /// 跳过自己的挂单，继续与更差价位的他人订单撮合，双方订单都保留。
    /// 对用户最无感，且不损耗市场深度。
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum SymbolType {
    CurrencyExchangePair,  // 现货
    FuturesContract,       // 期货
    PerpetualSwap,         // 永续合约
    CallOption,            // 看涨期权
    PutOption,             // 看跌期权
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum CommandResultCode {
    New,
    ValidForMatchingEngine,
    Success,
    Accepted,
    
    // Auth
    AuthInvalidUser,
    
    // Risk
    RiskNsf,
    RiskInvalidReserveBidPrice,
    RiskAskPriceLowerThanFee,
    RiskMarginTradingDisabled,
    /// size / price 非正数等非法入参（负数 size 会让冻结额变成负数）
    RiskInvalidOrderParams,
    /// 金额计算溢出 i64，订单被拒绝而非静默回绕
    RiskArithmeticOverflow,
    
    // Matching
    MatchingInvalidOrderBookId,
    MatchingUnknownOrderId,
    MatchingUnsupportedCommand,
    MatchingMoveFailedPriceOverRiskLimit,
    MatchingReduceFailedWrongSize,
    MatchingInvalidOrderSize,
    /// order_id 已存在于订单簿中，重复下单被拒绝（不会撮合、不会入簿）
    MatchingDuplicateOrderId,
    
    // State
    StatePersistRiskEngineFailed,
    StatePersistMatchingEngineFailed,
    
    // User
    UserMgmtUserAlreadyExists,
    /// 该 transaction_id 已被处理过（WAL 重放 / 消息重投），本次调整被忽略
    UserMgmtDuplicateTransaction,
    
    // Other
    InvalidSymbol,
    UnsupportedSymbolType,
    BinaryCommandFailed,

    /// 撮合线程曾在处理某条命令时 panic，引擎已进入中毒状态、不再执行任何命令。
    ///
    /// 状态可能停在半更新的位置，继续处理只会把错误扩散到资金上，因此后续命令
    /// 一律以此码快速失败。恢复手段是重启进程并从快照 + WAL 重放。
    ///
    /// 追加在枚举末尾：中间插入会改变已有变体的判别值，进而破坏 WAL 中已写入的记录。
    EnginePoisoned,

    /// 引擎已停机（shutdown 已执行完毕），不再受理任何命令。
    ///
    /// 与 EnginePoisoned 的区别：这是正常终态，状态一致且已落最终快照；
    /// 中毒则是异常终态，状态可能停在半更新的位置。
    EngineStopped,
}

#[derive(Debug, Clone, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct CoreSymbolSpecification {
    pub symbol_id: SymbolId,
    pub symbol_type: SymbolType,
    pub base_currency: Currency,
    pub quote_currency: Currency,
    pub base_scale_k: i64,
    pub quote_scale_k: i64,
    pub taker_fee: i64,
    pub maker_fee: i64,
    pub margin_buy: i64,
    pub margin_sell: i64,
    /// 自成交防范策略。默认 `CancelTaker`——合规要求上线必须启用某种策略，
    /// 因此默认取安全值；确实需要允许自成交时显式设为 `None`。
    #[serde(default = "default_stp")]
    pub stp: SelfTradePrevention,
}

fn default_stp() -> SelfTradePrevention {
    SelfTradePrevention::CancelTaker
}

impl Default for CoreSymbolSpecification {
    fn default() -> Self {
        Self {
            symbol_id: 0,
            symbol_type: SymbolType::CurrencyExchangePair,
            base_currency: 0,
            quote_currency: 0,
            base_scale_k: 1,
            quote_scale_k: 1,
            taker_fee: 0,
            maker_fee: 0,
            margin_buy: 0,
            margin_sell: 0,
            stp: SelfTradePrevention::CancelTaker,
        }
    }
}
