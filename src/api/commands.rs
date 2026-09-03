use super::types::*;
use super::events::*;
use serde::{Deserialize, Serialize};
use rkyv::{with::Skip, Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum OrderCommandType {
    PlaceOrder,
    MoveOrder,
    CancelOrder,
    ReduceOrder,
    OrderBookRequest,
    AddUser,
    BalanceAdjustment,
    SuspendUser,
    ResumeUser,
    BinaryDataCommand,
    BinaryDataQuery,
    Reset,
    Nop,
    PersistStateMatching,
    PersistStateRisk,
    GroupingControl,
    ShutdownSignal,
}

#[derive(Debug, Clone, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct OrderCommand {
    pub command: OrderCommandType,
    pub result_code: CommandResultCode,

    /// 全局命令序号，由 ExchangeCore 在提交时打上，随命令一路流到撮合线程。
    ///
    /// 带内快照靠它确定"这份状态包含到第几条命令"；结果回调也能据此给下游定序。
    ///
    /// 刻意不写进 WAL payload（`Skip`）：序号的权威来源是记录头里的那一份，
    /// 存两遍就会有对不上的一天。重放时由 `read_commands_after` 从记录头回填。
    #[with(Skip)]
    pub seq: u64,
    
    pub uid: UserId,
    pub order_id: OrderId,
    pub symbol: SymbolId,
    pub price: Price,
    pub reserve_price: Price,
    pub size: Size,
    pub action: OrderAction,
    pub order_type: OrderType,
    
    pub timestamp: i64,
    pub events_group: u64,
    pub service_flags: i32,
    
    // 扩展字段
    pub stop_price: Option<Price>,      // 止损触发价
    pub visible_size: Option<Size>,     // 冰山单显示数量
    pub expire_time: Option<i64>,       // 过期时间（GTD）
    
    // 撮合事件列表（预分配容量）
    pub matcher_events: Vec<MatcherTradeEvent>,
}

impl Default for OrderCommand {
    fn default() -> Self {
        Self {
            command: OrderCommandType::Nop,
            result_code: CommandResultCode::New,
            seq: 0,
            uid: 0,
            order_id: 0,
            symbol: 0,
            price: 0,
            reserve_price: 0,
            size: 0,
            action: OrderAction::Bid,
            order_type: OrderType::Gtc,
            timestamp: 0,
            events_group: 0,
            service_flags: 0,
            stop_price: None,
            visible_size: None,
            expire_time: None,
            matcher_events: Vec::with_capacity(4), // 预分配 4 个事件容量
        }
    }
}
