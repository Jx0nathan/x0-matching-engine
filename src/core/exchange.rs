use crate::api::*;
use crate::core::pipeline::Pipeline;
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
        }
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
            let handler = move |event: &OrderCommand, sequence: i64, end_of_batch: bool| {
                let mut cmd_mut = event.clone();
                pipeline.handle_event(&mut cmd_mut, sequence, end_of_batch);
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

    /// 启用快照管理
    pub fn enable_snapshotting<P: AsRef<Path>>(&mut self, path: P) -> anyhow::Result<()> {
        self.snapshot_store = Some(SnapshotStore::new(path)?);
        Ok(())
    }

    /// 生成当前状态快照
    pub fn take_snapshot(&self, seq_id: u64) -> anyhow::Result<()> {
        self.ensure_not_started("take_snapshot()")?;
        if let Some(store) = &self.snapshot_store {
            let state = self.serialize_state()?;
            store.save_snapshot(&state, seq_id)?;
        }
        Ok(())
    }

    /// 加载最新的快照并恢复状态
    pub fn load_latest_snapshot(&mut self) -> anyhow::Result<bool> {
        self.ensure_not_started("load_latest_snapshot()")?;

        let loaded = match &self.snapshot_store {
            Some(store) => match store.get_latest_seq_id()? {
                Some(seq_id) => Some(store.load_snapshot(seq_id)?),
                None => None,
            },
            None => None,
        };
        let Some(state) = loaded else {
            return Ok(false);
        };

        // 只替换业务状态，保留已配置的 journaler / snapshot_store。
        // 早先这里整体覆盖 self，会把两者置空，导致恢复后静默停止写 WAL。
        self.config = state.config;
        self.pipeline = Some(Pipeline::from_state(state.pipeline_state));
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
        if let Some(j) = &mut self.journaler {
            let _ = j.write_command(&cmd);
        }

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

        let outcome = Journaler::read_commands(path)?;
        let replayed = outcome.commands.len();

        let pipeline = self
            .pipeline
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ExchangeCore 未就绪，无法重放"))?;
        for mut cmd in outcome.commands {
            pipeline.handle_event(&mut cmd, 0, true);
        }

        Ok(ReplaySummary {
            replayed,
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
        }
    }
}

