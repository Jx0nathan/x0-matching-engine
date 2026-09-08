//! 撮合引擎的服务外壳：把 `ExchangeCore` 包成一个可部署的 HTTP 服务。
//!
//! # 为什么是"单线程引擎 + 通道"
//!
//! `ExchangeCore::submit_command` 需要 `&mut self` —— 整个引擎是一个单线程状态机，
//! 同一批交易对全局只能有一个实例在跑。因此这里的形态是固定的：
//!
//! ```text
//!   HTTP (tokio 多线程)  ──通道──►  引擎线程（独占 ExchangeCore）
//!                        ◄─oneshot─
//! ```
//!
//! 引擎线程是唯一碰状态的地方；HTTP 侧只负责翻译报文和等结果。**不要**试图把
//! ExchangeCore 放进 Mutex 再多线程调用 —— 那只是把串行点挪了个位置，还引入了锁。
//!
//! # 为什么用同步态（不调 startup()）
//!
//! 异步态下 `submit_command` 只返回 `Accepted`，真实结果要从 `set_result_consumer`
//! 回调里捞，得自建一张"请求 → 结果"的关联表；而且 `balance_of` / `l2_depth` /
//! `take_snapshot` 在 startup() 之后一律返回 None 或报错（pipeline 已移交撮合线程）。
//!
//! 同步态下 `submit_command` 直接返回带 `matcher_events` 的完整结果，查询接口也都能用。
//! 由于本服务本来就把所有命令收敛到单线程串行执行，Disruptor 能提供的解耦在这里
//! 拿不到额外好处（它的流水线并行需要多 handler，见 pipeline.rs 的说明）。
//!
//! 将来若要切到异步态：在 `engine_thread` 里 `core.startup()`，并把 `Command` 分支改成
//! "投递 + 用 order_id 关联回调结果"，同时给查询接口另建投影。
//!
//! # 环境变量
//!
//! | 变量 | 默认值 | 说明 |
//! |---|---|---|
//! | `BIND` | `0.0.0.0:8080` | 监听地址 |
//! | `DATA_DIR` | `./data` | WAL 与快照的存放目录 |
//! | `WAL_SYNC` | `64` | 每 N 条 fsync 一次；`always` 表示每条都 fsync |
//! | `SYMBOLS` | `1:0:1` | `symbol_id:base_currency:quote_currency`，逗号分隔 |
//!
//! `WAL_SYNC` 是吞吐与持久性的旋钮：EBS 上单次 fsync 约 0.5–1ms，`always` 会把吞吐
//! 压到千级/秒；`64` 则崩溃最多丢 63 条。按业务容忍度选。

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use matching_core::api::*;
use matching_core::core::exchange::{ExchangeConfig, ExchangeCore};
use matching_core::core::journal::SyncPolicy;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

// ============================================================ 引擎线程

/// 送进引擎线程的请求。所有需要碰状态的操作都必须走这里。
enum EngineReq {
    Command {
        cmd: Box<OrderCommand>,
        reply: oneshot::Sender<Box<OrderCommand>>,
    },
    Balance {
        uid: UserId,
        currency: Currency,
        reply: oneshot::Sender<Option<i64>>,
    },
    Depth {
        symbol: SymbolId,
        depth: usize,
        reply: oneshot::Sender<Option<L2MarketData>>,
    },
    Status {
        reply: oneshot::Sender<StatusResp>,
    },
    Checkpoint {
        reply: oneshot::Sender<Result<u64, String>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), String>>,
    },
}

#[derive(Clone)]
struct AppState {
    tx: mpsc::UnboundedSender<EngineReq>,
}

impl AppState {
    /// 把请求送进引擎线程并等结果。引擎线程已退出时返回 503。
    async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> EngineReq,
    ) -> Result<T, (StatusCode, Json<ErrResp>)> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(make(tx)).map_err(|_| unavailable())?;
        rx.await.map_err(|_| unavailable())
    }
}

fn unavailable() -> (StatusCode, Json<ErrResp>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrResp {
            error: "引擎线程不可用（可能已停机）".into(),
        }),
    )
}

/// 引擎线程主体：独占 ExchangeCore，串行处理所有请求。
fn engine_thread(mut core: ExchangeCore, mut rx: mpsc::UnboundedReceiver<EngineReq>) {
    while let Some(req) = rx.blocking_recv() {
        match req {
            EngineReq::Command { cmd, reply } => {
                let out = core.submit_command(*cmd);
                let _ = reply.send(Box::new(out));
            }
            EngineReq::Balance {
                uid,
                currency,
                reply,
            } => {
                let _ = reply.send(core.balance_of(uid, currency));
            }
            EngineReq::Depth {
                symbol,
                depth,
                reply,
            } => {
                let _ = reply.send(core.l2_depth(symbol, depth));
            }
            EngineReq::Status { reply } => {
                let _ = reply.send(StatusResp {
                    status: if core.is_stopped() {
                        "stopped"
                    } else if core.is_poisoned() {
                        "poisoned"
                    } else {
                        "ok"
                    }
                    .into(),
                    last_seq: core.last_seq(),
                    started: core.is_started(),
                });
            }
            EngineReq::Checkpoint { reply } => {
                let _ = reply.send(core.checkpoint().map_err(|e| e.to_string()));
            }
            EngineReq::Shutdown { reply } => {
                let r = core.shutdown().map_err(|e| e.to_string());
                let _ = reply.send(r);
                break; // 停机后不再接受任何请求
            }
        }
    }
    tracing::info!("引擎线程退出");
}

// ============================================================ 报文

#[derive(Serialize)]
struct ErrResp {
    error: String,
}

#[derive(Serialize)]
struct StatusResp {
    status: String,
    last_seq: u64,
    started: bool,
}

/// 命令执行结果。`result_code` 用字符串回传，避免前端依赖枚举序号。
#[derive(Serialize)]
struct CmdResp {
    result_code: String,
    accepted: bool,
    seq: u64,
    events: Vec<EventResp>,
}

#[derive(Serialize)]
struct EventResp {
    kind: String,
    size: Size,
    price: Price,
    matched_order_id: OrderId,
    matched_order_uid: UserId,
}

impl From<Box<OrderCommand>> for CmdResp {
    fn from(cmd: Box<OrderCommand>) -> Self {
        Self {
            result_code: format!("{:?}", cmd.result_code),
            accepted: matches!(
                cmd.result_code,
                CommandResultCode::Success | CommandResultCode::Accepted
            ),
            seq: cmd.seq,
            events: cmd
                .matcher_events
                .iter()
                .map(|e| EventResp {
                    kind: format!("{:?}", e.event_type),
                    size: e.size,
                    price: e.price,
                    matched_order_id: e.matched_order_id,
                    matched_order_uid: e.matched_order_uid,
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
struct AddUserReq {
    uid: UserId,
}

/// 出入金。**刻意用显式字段名**：底层 OrderCommand 把 currency 塞在 `symbol`、
/// amount 塞在 `price`、transaction_id 塞在 `order_id`，直接暴露那套字段复用
/// 极易用错，这里做一次翻译。
#[derive(Deserialize)]
struct BalanceReq {
    uid: UserId,
    currency: Currency,
    /// 正数入金、负数出金
    amount: i64,
    /// 幂等流水号，**必须按用户严格递增**，否则会被判为重复而忽略
    transaction_id: i64,
}

#[derive(Deserialize)]
struct PlaceOrderReq {
    uid: UserId,
    order_id: OrderId,
    symbol: SymbolId,
    price: Price,
    size: Size,
    /// "buy" / "bid" 或 "sell" / "ask"
    side: String,
    /// 缺省 Gtc。引擎只支持 gtc / ioc / fok_budget
    #[serde(default)]
    order_type: Option<String>,
    /// 买单的冻结价，缺省等于 price
    #[serde(default)]
    reserve_price: Option<Price>,
}

#[derive(Deserialize)]
struct CancelReq {
    uid: UserId,
    symbol: SymbolId,
}

#[derive(Deserialize)]
struct ReduceReq {
    uid: UserId,
    symbol: SymbolId,
    size: Size,
}

#[derive(Deserialize)]
struct DepthQuery {
    symbol: SymbolId,
    #[serde(default = "default_depth")]
    limit: usize,
}
fn default_depth() -> usize {
    10
}

#[derive(Serialize)]
struct DepthResp {
    symbol: SymbolId,
    asks: Vec<LevelResp>,
    bids: Vec<LevelResp>,
}

#[derive(Serialize)]
struct LevelResp {
    price: Price,
    size: Size,
    orders: usize,
}

#[derive(Serialize)]
struct BalanceResp {
    uid: UserId,
    currency: Currency,
    available: i64,
}

#[derive(Serialize)]
struct CheckpointResp {
    reclaimed_bytes: u64,
}

fn parse_side(s: &str) -> Result<OrderAction, (StatusCode, Json<ErrResp>)> {
    match s.to_ascii_lowercase().as_str() {
        "buy" | "bid" => Ok(OrderAction::Bid),
        "sell" | "ask" => Ok(OrderAction::Ask),
        other => Err(bad_request(format!(
            "side 只能是 buy/bid 或 sell/ask，收到 {other:?}"
        ))),
    }
}

fn parse_order_type(s: Option<&str>) -> Result<OrderType, (StatusCode, Json<ErrResp>)> {
    match s.map(|v| v.to_ascii_lowercase()).as_deref() {
        None | Some("gtc") => Ok(OrderType::Gtc),
        Some("ioc") => Ok(OrderType::Ioc),
        Some("fok_budget") => Ok(OrderType::FokBudget),
        Some(other) => Err(bad_request(format!(
            "引擎只支持 gtc / ioc / fok_budget，收到 {other:?}"
        ))),
    }
}

fn bad_request(msg: String) -> (StatusCode, Json<ErrResp>) {
    (StatusCode::BAD_REQUEST, Json(ErrResp { error: msg }))
}

// ============================================================ 路由

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ErrResp>)>;

async fn health(State(st): State<AppState>) -> ApiResult<StatusResp> {
    Ok(Json(st.ask(|reply| EngineReq::Status { reply }).await?))
}

async fn add_user(State(st): State<AppState>, Json(req): Json<AddUserReq>) -> ApiResult<CmdResp> {
    let cmd = OrderCommand {
        command: OrderCommandType::AddUser,
        uid: req.uid,
        ..Default::default()
    };
    submit(st, cmd).await
}

async fn adjust_balance(
    State(st): State<AppState>,
    Json(req): Json<BalanceReq>,
) -> ApiResult<CmdResp> {
    let cmd = OrderCommand {
        command: OrderCommandType::BalanceAdjustment,
        uid: req.uid,
        symbol: req.currency,          // 底层用 symbol 字段承载币种
        price: req.amount,             // 底层用 price 字段承载金额
        order_id: req.transaction_id as u64, // 底层用 order_id 承载幂等流水号
        ..Default::default()
    };
    submit(st, cmd).await
}

async fn place_order(
    State(st): State<AppState>,
    Json(req): Json<PlaceOrderReq>,
) -> ApiResult<CmdResp> {
    let action = parse_side(&req.side)?;
    let order_type = parse_order_type(req.order_type.as_deref())?;
    let cmd = OrderCommand {
        command: OrderCommandType::PlaceOrder,
        uid: req.uid,
        order_id: req.order_id,
        symbol: req.symbol,
        price: req.price,
        reserve_price: req.reserve_price.unwrap_or(req.price),
        size: req.size,
        action,
        order_type,
        ..Default::default()
    };
    submit(st, cmd).await
}

async fn cancel_order(
    State(st): State<AppState>,
    Path(order_id): Path<OrderId>,
    Json(req): Json<CancelReq>,
) -> ApiResult<CmdResp> {
    let cmd = OrderCommand {
        command: OrderCommandType::CancelOrder,
        uid: req.uid,
        order_id,
        symbol: req.symbol,
        ..Default::default()
    };
    submit(st, cmd).await
}

async fn reduce_order(
    State(st): State<AppState>,
    Path(order_id): Path<OrderId>,
    Json(req): Json<ReduceReq>,
) -> ApiResult<CmdResp> {
    let cmd = OrderCommand {
        command: OrderCommandType::ReduceOrder,
        uid: req.uid,
        order_id,
        symbol: req.symbol,
        size: req.size,
        ..Default::default()
    };
    submit(st, cmd).await
}

async fn submit(st: AppState, cmd: OrderCommand) -> ApiResult<CmdResp> {
    let out = st
        .ask(|reply| EngineReq::Command {
            cmd: Box::new(cmd),
            reply,
        })
        .await?;
    Ok(Json(CmdResp::from(out)))
}

async fn depth(State(st): State<AppState>, Query(q): Query<DepthQuery>) -> ApiResult<DepthResp> {
    let l2 = st
        .ask(|reply| EngineReq::Depth {
            symbol: q.symbol,
            depth: q.limit.min(1000),
            reply,
        })
        .await?
        .ok_or_else(|| bad_request(format!("交易对 {} 未注册", q.symbol)))?;

    let zip = |p: Vec<Price>, s: Vec<Size>, c: Vec<usize>| {
        p.into_iter()
            .zip(s)
            .zip(c)
            .map(|((price, size), orders)| LevelResp {
                price,
                size,
                orders,
            })
            .collect::<Vec<_>>()
    };
    Ok(Json(DepthResp {
        symbol: q.symbol,
        asks: zip(l2.ask_prices, l2.ask_volumes, l2.ask_order_counts),
        bids: zip(l2.bid_prices, l2.bid_volumes, l2.bid_order_counts),
    }))
}

async fn get_balance(
    State(st): State<AppState>,
    Path((uid, currency)): Path<(UserId, Currency)>,
) -> ApiResult<BalanceResp> {
    let available = st
        .ask(|reply| EngineReq::Balance {
            uid,
            currency,
            reply,
        })
        .await?
        .ok_or_else(unavailable)?;
    Ok(Json(BalanceResp {
        uid,
        currency,
        available,
    }))
}

async fn checkpoint(State(st): State<AppState>) -> ApiResult<CheckpointResp> {
    let reclaimed = st
        .ask(|reply| EngineReq::Checkpoint { reply })
        .await?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrResp { error: e })))?;
    Ok(Json(CheckpointResp {
        reclaimed_bytes: reclaimed,
    }))
}

// ============================================================ 启动

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// `SYMBOLS=1:0:1,2:0:2` → 交易对列表
fn parse_symbols(raw: &str) -> anyhow::Result<Vec<CoreSymbolSpecification>> {
    let mut out = Vec::new();
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = item.split(':').collect();
        if parts.len() != 3 {
            anyhow::bail!("SYMBOLS 项格式应为 symbol_id:base:quote，收到 {item:?}");
        }
        out.push(CoreSymbolSpecification {
            symbol_id: parts[0].parse()?,
            symbol_type: SymbolType::CurrencyExchangePair,
            base_currency: parts[1].parse()?,
            quote_currency: parts[2].parse()?,
            base_scale_k: 1,
            quote_scale_k: 1,
            taker_fee: 0,
            maker_fee: 0,
            margin_buy: 0,
            margin_sell: 0,
            stp: SelfTradePrevention::CancelBoth,
        });
    }
    if out.is_empty() {
        anyhow::bail!("SYMBOLS 不能为空");
    }
    Ok(out)
}

fn parse_sync_policy(raw: &str) -> anyhow::Result<SyncPolicy> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "always" => Ok(SyncPolicy::EveryCommand),
        "never" => Ok(SyncPolicy::Never),
        n => Ok(SyncPolicy::EveryN(n.parse().map_err(|_| {
            anyhow::anyhow!("WAL_SYNC 应为 always / never / 正整数，收到 {n:?}")
        })?)),
    }
}

/// 组装引擎：注册交易对 → 从快照与 WAL 恢复。顺序不可调换，
/// 所有配置类操作都必须在 startup() 之前（本服务不调 startup，但保持同样的顺序纪律）。
fn build_engine(data_dir: &str, sync_policy: SyncPolicy) -> anyhow::Result<(ExchangeCore, String)> {
    std::fs::create_dir_all(data_dir)?;
    let wal = format!("{data_dir}/journal.bin");
    let snaps = format!("{data_dir}/snapshots");

    let mut core = ExchangeCore::new(ExchangeConfig::default());
    core.enable_snapshotting(&snaps)?;
    core.enable_journaling_with_policy(&wal, sync_policy)?;

    for spec in parse_symbols(&env_or("SYMBOLS", "1:0:1"))? {
        tracing::info!(symbol = spec.symbol_id, "注册交易对");
        core.add_symbol(spec)?;
    }

    // 先加载快照，再重放快照之后的日志 —— 顺序颠倒会重复执行命令
    let restored = core.load_latest_snapshot()?;
    let summary = core.replay_journal(&wal)?;
    tracing::info!(
        snapshot = restored,
        replayed = summary.replayed,
        last_seq = summary.last_seq,
        "状态恢复完成"
    );
    if let Some(at) = summary.truncated_at {
        tracing::warn!(
            offset = at,
            "WAL 尾部损坏（通常是掉电）。已重放的部分完整可信，但继续追加前\
             应调用 Journaler::truncate_to_last_valid 裁掉尾巴"
        );
    }
    Ok((core, wal))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let bind = env_or("BIND", "0.0.0.0:8080");
    let data_dir = env_or("DATA_DIR", "./data");
    let sync_policy = parse_sync_policy(&env_or("WAL_SYNC", "64"))?;
    tracing::info!(%bind, %data_dir, ?sync_policy, "启动撮合服务");

    let (core, _wal) = build_engine(&data_dir, sync_policy)?;

    let (tx, rx) = mpsc::unbounded_channel();
    let engine = std::thread::Builder::new()
        .name("matching-engine".into())
        .spawn(move || engine_thread(core, rx))?;

    let state = AppState { tx: tx.clone() };
    let app = Router::new()
        .route("/health", get(health))
        .route("/users", post(add_user))
        .route("/balances", post(adjust_balance))
        .route("/balances/{uid}/{currency}", get(get_balance))
        .route("/orders", post(place_order))
        .route("/orders/{order_id}", delete(cancel_order))
        .route("/orders/{order_id}/reduce", post(reduce_order))
        .route("/depth", get(depth))
        .route("/admin/checkpoint", post(checkpoint))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("监听 {bind}");

    axum::serve(listener, app)
        .with_graceful_shutdown(wait_for_signal())
        .await?;

    // HTTP 已停止收新请求，此时才让引擎收尾：排空在途命令 + 落最终快照 + WAL fsync。
    // 顺序不能反 —— 先停机再关 HTTP 的话，中间进来的请求会全部被拒。
    tracing::info!("HTTP 已停止，开始引擎停机");
    let (done_tx, done_rx) = oneshot::channel();
    if tx.send(EngineReq::Shutdown { reply: done_tx }).is_ok() {
        match done_rx.await {
            Ok(Ok(())) => tracing::info!("引擎停机完成，状态已落盘"),
            Ok(Err(e)) => tracing::error!(error = %e, "引擎停机报错"),
            Err(_) => tracing::error!("引擎线程在停机前已退出"),
        }
    }
    drop(tx);
    let _ = engine.join();
    tracing::info!("已退出");
    Ok(())
}

/// 同时接 SIGTERM（容器/systemd 停止）与 Ctrl-C
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("注册 SIGTERM 失败");
        tokio::select! {
            _ = term.recv() => tracing::info!("收到 SIGTERM"),
            _ = tokio::signal::ctrl_c() => tracing::info!("收到 Ctrl-C"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
