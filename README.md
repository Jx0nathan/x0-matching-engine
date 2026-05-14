# matching-engine

[English](./README_EN.md) | 中文

面向生产的撮合引擎，覆盖现货 + 永续/期货，使用 Rust 编写。

> **当前状态：M5.5（Prometheus metrics + CI）完成。** `MatchingEngine::metrics_snapshot()` 直接渲染 Prometheus 文本格式（counters：commands / trades / liquidations / WAL fsyncs / stops triggered 等；gauges：last_applied_seq / 保险基金余额 / 交易所手续费收入 / 各 symbol 挂单深度 / 各币种 total_internal）。GitHub Actions CI 上 push 和 PR 跑 fmt + clippy -D warnings + 全量 test + release build + 1000-case 守恒属性测试。路线图详见 `CLAUDE.md`。

## 设计目标

- **正确性优先。** 每一次改动都由"资金守恒"属性测试守门：在任意命令序列下，每种货币的"用户余额之和"必须等于"净存款"。任何输入序列都不允许凭空增减资金。
- **确定性。** 流水线每个阶段一次只处理一条事件，WAL 回放能 bit-for-bit 复现引擎状态。
- **先性能，再可移植性。** Disruptor 风格的三段流水线（R1 → 撮合 → R2），每段独占一个核心。全程 i64 minor-unit；中间运算扩位到 i128，从乘法层面就消除溢出可能。

## 仓库布局

```
matching-engine/
├── crates/
│   ├── me-types/        类型、命令、事件、守恒不变量
│   ├── me-disruptor/    无锁环形缓冲区（M3）
│   ├── me-wal/          预写日志 + 快照（M3）
│   ├── me-risk/         R1 前置风控 + R2 结算（M2）
│   ├── me-matching/     订单簿 + 撮合（M2）
│   ├── me-core/         流水线 facade（M2/M3）
│   └── me-server/       binary daemon（M5）
└── tests/
    └── invariants/      跨 crate 的守恒不变量测试
```

## 构建 / 测试

```bash
cargo build --workspace
cargo test -p me-types        # M1 阶段的测试
cargo clippy --workspace --all-targets -- -D warnings
```

需要 Rust 1.75+。

## 这不是什么

- **不是** 早期参考设计 `matching-core` 的 fork。后者仅作为对照，不被依赖。
- **不是** 完成品。详见 `CLAUDE.md` 中的 milestone 划分。
- **不能** 直接用于未经审计的生产环境。守恒测试能拦下算术 bug，拦不住业务逻辑层面的设计错误——那需要专门的审计流程。

## 许可证

MIT。
