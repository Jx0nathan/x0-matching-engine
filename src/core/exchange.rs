use crate::api::*;
use crate::core::pipeline::Pipeline;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use serde::{Deserialize, Serialize};

/// 交易所核心配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeConfig {
    pub ring_buffer_size: usize,
    pub matching_engines_num: usize,
    pub risk_engines_num: usize,
    pub producer_type: ProducerType,
    pub wait_strategy: WaitStrategyType,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ProducerType {
    Single,
    Multi,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum WaitStrategyType {
    BusySpin,
    Yielding,
    Blocking,
    Sleeping,
}

impl ExchangeConfig {
    // 这里不再需要同步转换方法，因为 startup 内部已经处理了配置到具体类型的映射
}

#[derive(Serialize, Deserialize)]
pub struct ExchangeState {
    pub config: ExchangeConfig,
    pub pipeline_state: crate::core::pipeline::PipelineState,
}

impl Default for ExchangeConfig {
    fn default() -> Self {
        Self {
            ring_buffer_size: 64 * 1024,
            matching_engines_num: 1,
            risk_engines_num: 1,
            producer_type: ProducerType::Single,
            wait_strategy: WaitStrategyType::BusySpin,
        }
    }
}

/// 结果消费者回调
pub type ResultConsumer = Arc<dyn Fn(&OrderCommand) + Send + Sync>;

use crate::core::journal::{Journaler, SyncPolicy};
use std::path::Path;

/// WAL 重放结果概要
#[derive(Debug, Clone)]
pub struct ReplaySummary {
    /// 成功重放的命令条数
    pub replayed: usize,
    /// 重放结束后的全局序号（= 日志中最后一条完整记录的序号）
    pub last_seq: u64,
    /// 日志尾部从该偏移起损坏；None 表示日志完整
    pub truncated_at: Option<u64>,
}

use crate::core::snapshot::SnapshotStore;

/// 内部接口，用于类型抹除 Disruptor 的泛型 Producer
trait Publisher {
    fn publish(&mut self, cmd: OrderCommand);
}

struct ProducerWrapper<P: disruptor::Producer<OrderCommand>>(P);

impl<P: disruptor::Producer<OrderCommand>> Publisher for ProducerWrapper<P> {
    fn publish(&mut self, cmd: OrderCommand) {
        self.0.publish(|event| {
            *event = cmd;
        });
    }
}

/// 交易所核心
pub struct ExchangeCore {
    config: ExchangeConfig,
    // 使用 Publisher trait 对象隐藏具体的扰乱器生产者类型
    producer: Option<Box<dyn Publisher>>,
    pipeline: Option<Pipeline>,
    journaler: Option<Journaler>,
    snapshot_store: Option<SnapshotStore>,
    /// 已应用到状态机的最后一条命令的全局序号。
    /// 快照按它存档，重放按它跳过前缀 —— 两者对齐，恢复才不会重复执行。
    last_seq: u64,
    /// 撮合线程是否已中毒（曾 panic）。生产者与消费者线程共享。
    poisoned: Arc<AtomicBool>,
}

impl ExchangeCore {
    pub fn new(config: ExchangeConfig) -> Self {
        let pipeline = Pipeline::new(&config);
        Self { 
            config, 
            pipeline: Some(pipeline),
            producer: None,
            journaler: None,
            snapshot_store: None,
            last_seq: 0,
            poisoned: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 撮合线程是否已因 panic 进入中毒状态。
    ///
    /// 只在异步态下有意义：同步态的 panic 会直接沿调用栈抛给调用方，那是正确行为，
    /// 不需要也不应该被吞掉。
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// 已应用到状态机的最后一条命令的全局序号
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// 是否已进入异步模式。`startup()` 会把 pipeline 移交给 Disruptor 消费者线程，
    /// 此后一切依赖直接访问 pipeline 的操作都无法进行。
    pub fn is_started(&self) -> bool {
        self.producer.is_some()
    }

    fn ensure_not_started(&self, op: &str) -> anyhow::Result<()> {
        if self.is_started() {
            anyhow::bail!(
                "{op} 必须在 startup() 之前调用：启动后 pipeline 已移交 Disruptor 消费者线程"
            );
        }
        Ok(())
    }

    /// 启动 Disruptor 流水线
    pub fn startup(&mut self) {
        if self.producer.is_some() {
            return;
        }

        if let Some(mut pipeline) = self.pipeline.take() {
            let ring_size = self.config.ring_buffer_size;
            
            // 封装事件处理逻辑
            // Disruptor 3.6.1 的 handler 接收的是 &E (不可变)
            // 为了维持原有 Pipeline 的可变逻辑，我们在处理前进行克隆
            //
            // 这里必须兜住 panic。实测（去掉本捕获后跑
            // panicking_consumer_poisons_engine_instead_of_aborting）：panic 逃出
            // handler 后会在 disruptor 的析构路径上引发二次 panic，整个进程被
            // SIGABRT 掉 —— "panic in a destructor during cleanup /
            // thread caused non-unwinding panic. aborting."
            //
            // 后果不只是进程没了，而是**原始 panic 的上下文被掩盖**：最终打出来的
            // 是析构期二次 panic 的信息，看不出是哪条命令、哪个用户触发的；
            // WAL 也没有机会收尾。
            //
            // 捕获之后：线程存活并继续排空缓冲（但不再碰状态机），日志里留下
            // 出事命令的 seq / order_id / uid，提交侧通过 poisoned 标记快速失败。
            // 要不要进一步主动停机，交给上层按运维策略决定。
            let poisoned = self.poisoned.clone();
            let handler = move |event: &OrderCommand, sequence: i64, end_of_batch: bool| {
                // 已中毒：状态可能停在半更新的位置，只排空缓冲，绝不再碰状态机
                if poisoned.load(Ordering::Acquire) {
                    return;
                }

                let mut cmd_mut = event.clone();
                // AssertUnwindSafe：pipeline 跨 catch_unwind 边界被可变借用。
                // 这正是 UnwindSafe 要拦的情况 —— panic 后状态可能不自洽。
                // 我们的应对不是"继续用"，而是就此中毒、不再处理任何命令。
                let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    pipeline.handle_event(&mut cmd_mut, sequence, end_of_batch);
                }));

                if outcome.is_err() {
                    poisoned.store(true, Ordering::Release);
                    tracing::error!(
                        seq = cmd_mut.seq,
                        order_id = cmd_mut.order_id,
                        uid = cmd_mut.uid,
                        "撮合线程处理命令时 panic，引擎已中毒：后续命令一律拒绝，\
                         请重启进程并从快照 + WAL 重放恢复"
                    );
                }
            };

            // 使用 build_single_producer / build_multi_producer
            // 目前 3.6.1 仅显式支持 BusySpin 等几种策略在 wait_strategies 下
            let producer: Box<dyn Publisher> = match self.config.producer_type {
                ProducerType::Single => {
                    Box::new(ProducerWrapper(disruptor::build_single_producer(ring_size, || OrderCommand::default(), disruptor::wait_strategies::BusySpin)
                        .handle_events_with(handler)
                        .build()))
                },
                ProducerType::Multi => {
                    Box::new(ProducerWrapper(disruptor::build_multi_producer(ring_size, || OrderCommand::default(), disruptor::wait_strategies::BusySpin)
                        .handle_events_with(handler)
                        .build()))
                }
            };

            self.producer = Some(producer);
        }
    }

    /// 启用快照管理。必须在 `startup()` 之前调用。
    ///
    /// 除了自己留一份（同步态直接落快照、以及压缩时查最新档案号），还要把出口装进
    /// pipeline —— 启动后 pipeline 归撮合线程所有，那时只有它能读到状态。
    pub fn enable_snapshotting<P: AsRef<Path>>(&mut self, path: P) -> anyhow::Result<()> {
        self.ensure_not_started("enable_snapshotting()")?;
        let store = SnapshotStore::new(path)?;
        if let Some(p) = &mut self.pipeline {
            p.set_snapshot_sink(store.clone(), self.config.clone());
        }
        self.snapshot_store = Some(store);
        Ok(())
    }

    /// 请求一次快照。**同步态与异步态通用**。
    ///
    /// 走的是带内控制命令：命令排在正常命令流里进入撮合线程，由持有状态的那一方
    /// 就地落盘。这样异步态下也能做检查点 —— `take_snapshot()` 做不到，因为
    /// 启动后 `ExchangeCore` 已经读不到状态了。
    ///
    /// 异步态下返回 `Accepted`，快照是否写成要看结果回调里的 `result_code`。
    pub fn request_snapshot(&mut self) -> OrderCommand {
        self.submit_command(OrderCommand {
            command: OrderCommandType::PersistStateMatching,
            ..Default::default()
        })
    }

    /// 生成当前状态快照，以「已应用的最后一条命令序号」为档案号。
    ///
    /// 序号由内部维护而非调用方指定：快照内容与序号必须严格对应，
    /// 否则恢复时会以错误的位置重放日志 —— 号小了重复执行，号大了丢命令。
    pub fn take_snapshot(&self) -> anyhow::Result<()> {
        self.ensure_not_started("take_snapshot()")?;
        if let Some(store) = &self.snapshot_store {
            let state = self.serialize_state()?;
            store.save_snapshot(&state, self.last_seq)?;
        }
        Ok(())
    }

    /// 丢弃已被最新快照覆盖的那段 WAL 前缀，返回回收的字节数。
    ///
    /// 截断上界取自**最新快照的序号**，而不是 `last_seq`——这样在设计上就不可能
    /// 丢掉尚未被任何快照覆盖的日志。未启用快照时直接拒绝。
    pub fn compact_journal(&mut self) -> anyhow::Result<u64> {
        let Some(store) = &self.snapshot_store else {
            anyhow::bail!("未启用快照，压缩 WAL 会导致这段命令再也无法恢复");
        };
        let Some(snapshot_seq) = store.get_latest_seq_id()? else {
            return Ok(0); // 还没有任何快照，什么都不能丢
        };
        match &mut self.journaler {
            Some(j) => j.compact_before(snapshot_seq),
            None => Ok(0),
        }
    }

    /// 检查点：先落快照，再丢弃被它覆盖的日志前缀。返回回收的字节数。
    ///
    /// 顺序不可颠倒，且 `save_snapshot` 内部已 fsync + 原子替换——
    /// 先截断后落盘的话，中间崩溃就是永久的数据丢失。
    pub fn checkpoint(&mut self) -> anyhow::Result<u64> {
        self.take_snapshot()?;
        self.compact_journal()
    }

    /// 加载最新的快照并恢复状态
    pub fn load_latest_snapshot(&mut self) -> anyhow::Result<bool> {
        self.ensure_not_started("load_latest_snapshot()")?;

        let loaded = match &self.snapshot_store {
            Some(store) => match store.get_latest_seq_id()? {
                Some(seq_id) => Some((seq_id, store.load_snapshot(seq_id)?)),
                None => None,
            },
            None => None,
        };
        let Some((seq_id, state)) = loaded else {
            return Ok(false);
        };

        // 只替换业务状态，保留已配置的 journaler / snapshot_store。
        // 早先这里整体覆盖 self，会把两者置空，导致恢复后静默停止写 WAL。
        self.config = state.config;
        self.pipeline = Some(Pipeline::from_state(state.pipeline_state));
        // 快照的档案号就是它所包含的最后一条命令的序号，重放据此跳过前缀
        self.last_seq = seq_id;
        Ok(true)
    }

    /// 启用日志持久化（默认每条命令 fsync）
    pub fn enable_journaling<P: AsRef<Path>>(&mut self, path: P) -> anyhow::Result<()> {
        self.journaler = Some(Journaler::new(path)?);
        Ok(())
    }

    /// 启用日志持久化并指定落盘策略。吞吐敏感的场景可用 `SyncPolicy::EveryN`
    /// 做组提交，代价是崩溃时最多丢失最后 N-1 条命令。
    pub fn enable_journaling_with_policy<P: AsRef<Path>>(
        &mut self,
        path: P,
        policy: SyncPolicy,
    ) -> anyhow::Result<()> {
        self.journaler = Some(Journaler::with_sync_policy(path, policy)?);
        Ok(())
    }

    /// 强制把 WAL 落盘
    pub fn sync_journal(&mut self) -> anyhow::Result<()> {
        if let Some(j) = &mut self.journaler {
            j.sync()?;
        }
        Ok(())
    }

    /// 注册结果消费者回调。必须在 `startup()` 之前调用。
    pub fn set_result_consumer(&mut self, consumer: ResultConsumer) -> anyhow::Result<()> {
        self.ensure_not_started("set_result_consumer()")?;
        if let Some(p) = &mut self.pipeline {
            p.set_result_consumer(consumer);
        }
        Ok(())
    }

    /// 注册交易对。必须在 `startup()` 之前调用。
    pub fn add_symbol(&mut self, spec: CoreSymbolSpecification) -> anyhow::Result<()> {
        self.ensure_not_started("add_symbol()")?;
        if let Some(p) = &mut self.pipeline {
            p.add_symbol(spec);
        }
        Ok(())
    }

    /// 查询用户可用余额。`startup()` 之后 pipeline 移交给 Disruptor 线程，返回 None。
    pub fn balance_of(&self, uid: UserId, currency: Currency) -> Option<i64> {
        self.pipeline.as_ref().map(|p| p.balance_of(uid, currency))
    }

    /// 查询已归集的手续费。`startup()` 之后返回 None。
    pub fn fees_collected(&self, currency: Currency) -> Option<i64> {
        self.pipeline.as_ref().map(|p| p.fees_collected(currency))
    }

    /// 提交命令。
    ///
    /// 同步模式（未 `startup()`）：命令就地执行完毕，返回值里的 `result_code`
    /// 与 `matcher_events` 即最终结果。
    ///
    /// 异步模式（已 `startup()`）：命令只是投递进环形缓冲区，返回值的 `result_code`
    /// 为 `Accepted`，**真正的撮合结果只能通过 `set_result_consumer` 注册的回调获取**。
    pub fn submit_command(&mut self, mut cmd: OrderCommand) -> OrderCommand {
        // 中毒后立刻拒绝：不写 WAL（这条命令不会被执行，写进去反而会在重放时
        // 凭空多做一次），也不投递。
        if self.is_poisoned() {
            cmd.result_code = CommandResultCode::EnginePoisoned;
            return cmd;
        }

        // 控制命令不改变状态机，不写 WAL：写进去只会让每次重放都凭空多做一次快照。
        let is_control = matches!(
            cmd.command,
            OrderCommandType::PersistStateMatching | OrderCommandType::PersistStateRisk
        );

        if !is_control {
            if let Some(j) = &mut self.journaler {
                match j.write_command(&cmd) {
                    Ok(seq) => self.last_seq = seq,
                    // 不推进 last_seq：这条命令没能进日志，就不能被算进任何快照的覆盖范围
                    Err(e) => tracing::error!(
                        order_id = cmd.order_id,
                        uid = cmd.uid,
                        error = %e,
                        "WAL 写入失败，该命令将无法被重放"
                    ),
                }
            }
        }

        // 序号随命令一路流到撮合线程。普通命令带的是自己的序号；控制命令带的是
        // 它之前那条命令的序号 —— 正好等于"快照将覆盖到第几条"。
        cmd.seq = self.last_seq;

        if let Some(producer) = &mut self.producer {
            producer.publish(cmd.clone());
            // 不要回传 New：那会让调用方误以为命令未被处理。
            // Accepted 明确表示"已受理，结果走回调"。
            cmd.result_code = CommandResultCode::Accepted;
            cmd
        } else if let Some(pipeline) = &mut self.pipeline {
            pipeline.handle_event(&mut cmd, 0, true);
            cmd
        } else {
            panic!("ExchangeCore 未就绪");
        }
    }

    /// 从日志重放。必须在 `startup()` 之前调用：重放走的是同步管线，
    /// 且不会把命令重新写回 WAL。
    ///
    /// 返回值里的 `truncated_at` 非 None 表示日志尾部有损坏或半条记录
    /// （典型成因是掉电）。此时已重放的部分是完整可信的，但**继续追加之前
    /// 必须调用 `Journaler::truncate_to_last_valid` 把尾巴裁掉**。
    pub fn replay_journal<P: AsRef<Path>>(&mut self, path: P) -> anyhow::Result<ReplaySummary> {
        self.ensure_not_started("replay_journal()")?;

        // 只重放快照之后的命令。快照已经包含了 last_seq 及之前所有命令的效果，
        // 再放一遍就是重复下单、重复扣款。
        let outcome = Journaler::read_commands_after(path, self.last_seq)?;
        let replayed = outcome.commands.len();

        let pipeline = self
            .pipeline
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ExchangeCore 未就绪，无法重放"))?;
        for mut cmd in outcome.commands {
            pipeline.handle_event(&mut cmd, 0, true);
        }

        if let Some(seq) = outcome.last_seq {
            self.last_seq = self.last_seq.max(seq);
        }

        Ok(ReplaySummary {
            replayed,
            last_seq: self.last_seq,
            truncated_at: outcome.truncated_at,
        })
    }

    pub fn serialize_state(&self) -> anyhow::Result<ExchangeState> {
        self.ensure_not_started("serialize_state()")?;
        let pipeline = self
            .pipeline
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ExchangeCore 未就绪，无法序列化状态"))?;
        Ok(ExchangeState {
            config: self.config.clone(),
            pipeline_state: pipeline.serialize_state(),
        })
    }

    pub fn from_state(state: ExchangeState) -> Self {
        Self {
            config: state.config,
            pipeline: Some(Pipeline::from_state(state.pipeline_state)),
            producer: None,
            journaler: None,
            snapshot_store: None,
            last_seq: 0,
            poisoned: Arc::new(AtomicBool::new(false)),
        }
    }
}

