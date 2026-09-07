use crate::api::*;
use crate::core::exchange::{ExchangeConfig, ExchangeState, ResultConsumer};
use crate::core::snapshot::SnapshotStore;
use crate::core::processors::{matching_engine::{MatchingEngineRouter, MatchingEngineState}, risk_engine::RiskEngine};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct PipelineState {
    pub risk_engines: Vec<RiskEngine>,
    pub matching_engines: Vec<MatchingEngineState>,
}

/// 带内快照的落盘出口。
///
/// 启动后 pipeline 归 Disruptor 消费者线程所有，外部再也拿不到状态，
/// 所以快照只能由"持有状态的那个线程"自己写。这个 sink 就跟着 pipeline
/// 一起被移交过去。config 是快照文件的一部分，随之带上一份副本。
struct SnapshotSink {
    store: SnapshotStore,
    config: ExchangeConfig,
}

/// 流水线 - 组织各个处理器
pub struct Pipeline {
    risk_engines: Vec<RiskEngine>,
    matching_engines: Vec<MatchingEngineRouter>,
    result_consumer: Option<ResultConsumer>,
    snapshot_sink: Option<SnapshotSink>,
}

impl Pipeline {
    /// 处理单个命令（完整流水线）
    pub fn handle_event(&mut self, cmd: &mut OrderCommand, _sequence: i64, _end_of_batch: bool) {
        // 0. 带内控制命令：不经风控与撮合，就地处理后直接交给结果消费者。
        //    快照必须走这条路，因为只有本线程能读到状态。
        if matches!(
            cmd.command,
            OrderCommandType::PersistStateMatching | OrderCommandType::PersistStateRisk
        ) {
            self.persist_state(cmd);
            if let Some(consumer) = &self.result_consumer {
                consumer(cmd);
            }
            return;
        }

        // 停机信号：本线程即将随 Disruptor 一起退出，而 pipeline 就活在那个闭包里 ——
        // 线程一走状态就没了。所以这里必须趁最后的机会落一份快照，否则下次启动
        // 只能从上一个检查点重放整段 WAL。
        if cmd.command == OrderCommandType::ShutdownSignal {
            if self.snapshot_sink.is_some() {
                self.persist_state(cmd);
            } else {
                // 没启用快照不算错：无状态可存，停机本身仍然成立
                cmd.result_code = CommandResultCode::Success;
            }
            if let Some(consumer) = &self.result_consumer {
                consumer(cmd);
            }
            return;
        }

        // 1. Risk R1 (预处理)
        for engine in &mut self.risk_engines {
            engine.pre_process(cmd);
        }

        // 2. Matching Engine
        for engine in &mut self.matching_engines {
            engine.process_order(cmd);
        }

        // 3. Risk R2 (后处理)
        for engine in &mut self.risk_engines {
            engine.post_process(cmd);
        }

        // 4. Result Consumer
        if let Some(consumer) = &self.result_consumer {
            consumer(cmd);
        }
    }
    /// 就地把整条流水线的状态写成快照，档案号取命令自带的全局序号。
    ///
    /// 该序号是"这条命令之前已应用的最后一条命令"的序号（控制命令本身不入 WAL、
    /// 不改状态），因此快照内容与档案号严格对应，恢复时按它跳过日志前缀即可。
    ///
    /// 注意：序列化在撮合线程内同步完成，期间该线程不消费新命令 —— 这是一次
    /// stop-the-world 停顿，时长随订单簿规模增长。
    fn persist_state(&self, cmd: &mut OrderCommand) {
        let Some(sink) = &self.snapshot_sink else {
            tracing::error!(seq = cmd.seq, "收到快照命令，但未启用快照存储");
            cmd.result_code = CommandResultCode::StatePersistMatchingEngineFailed;
            return;
        };

        let state = ExchangeState {
            config: sink.config.clone(),
            pipeline_state: self.serialize_state(),
        };

        match sink.store.save_snapshot(&state, cmd.seq) {
            Ok(_) => cmd.result_code = CommandResultCode::Success,
            Err(e) => {
                tracing::error!(seq = cmd.seq, error = %e, "带内快照写入失败");
                cmd.result_code = CommandResultCode::StatePersistMatchingEngineFailed;
            }
        }
    }

    /// 装配快照出口。必须在 `startup()` 之前调用 —— 之后 pipeline 就不在本地了。
    pub fn set_snapshot_sink(&mut self, store: SnapshotStore, config: ExchangeConfig) {
        self.snapshot_sink = Some(SnapshotSink { store, config });
    }

    pub fn serialize_state(&self) -> PipelineState {
        PipelineState {
            risk_engines: self.risk_engines.clone(),
            matching_engines: self.matching_engines.iter().map(|e| e.serialize_state()).collect(),
        }
    }

    pub fn from_state(state: PipelineState) -> Self {
        Self {
            risk_engines: state.risk_engines,
            matching_engines: state.matching_engines.into_iter().map(MatchingEngineRouter::from_state).collect(),
            result_consumer: None,
            snapshot_sink: None,
        }
    }
    pub fn new(config: &ExchangeConfig) -> Self {
        // 创建风险引擎分片
        let risk_engines = (0..config.risk_engines_num)
            .map(|shard_id| RiskEngine::new(shard_id, config.risk_engines_num))
            .collect();

        // 创建撮合引擎分片
        let matching_engines = (0..config.matching_engines_num)
            .map(|shard_id| MatchingEngineRouter::new(shard_id, config.matching_engines_num))
            .collect();

        Self {
            risk_engines,
            matching_engines,
            result_consumer: None,
            snapshot_sink: None,
        }
    }

    pub fn set_result_consumer(&mut self, consumer: ResultConsumer) {
        self.result_consumer = Some(consumer);
    }

    /// 查询用户可用余额。每个 uid 只归属一个风控分片，其余分片返回 0。
    pub fn balance_of(&self, uid: UserId, currency: Currency) -> i64 {
        self.risk_engines.iter().map(|e| e.balance_of(uid, currency)).sum()
    }

    /// 已归集的手续费总额（对账用）
    pub fn fees_collected(&self, currency: Currency) -> i64 {
        self.risk_engines.iter().map(|e| e.fees_collected(currency)).sum()
    }

    pub fn add_symbol(&mut self, spec: CoreSymbolSpecification) {
        for engine in &mut self.risk_engines {
            engine.add_symbol(spec.clone());
        }
        for engine in &mut self.matching_engines {
            engine.add_symbol(spec.clone());
        }
    }
}
