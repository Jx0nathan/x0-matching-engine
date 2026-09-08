# Matching Core

高性能撮合引擎核心库，使用 Rust 构建，支持多种订单类型和交易品种。

## 特性

### 核心功能
- **高性能撮合引擎**：支持毫秒级延迟的订单撮合
- **多种订单类型**：GTC、IOC、FOK、Post-Only、Stop Order、Iceberg、GTD、Day
- **多交易品种**：现货、期货、永续合约、看涨期权、看跌期权
- **内存优化**：SOA 内存布局、订单池预分配、SmallVec 减少堆分配
- **零拷贝序列化**：使用 rkyv 实现高性能 WAL

### 技术亮点
- **LMAX Disruptor 模式**：无锁环形缓冲区，实现高吞吐量
- **分片架构**：支持多风险引擎和撮合引擎分片
- **持久化**：WAL 日志和快照机制
- **SIMD 优化**：批量撮合优化
- **ART 索引**：自适应基数树用于价格索引

## 快速开始

### 安装

```bash
git clone <repository-url>
cd matching-core
cargo build --release
```

### 基础使用

```rust
use matching_core::api::*;
use matching_core::core::orderbook::{OrderBook, AdvancedOrderBook};

// 创建交易对配置
let spec = CoreSymbolSpecification {
    symbol_id: 1,
    symbol_type: SymbolType::CurrencyExchangePair,
    base_currency: 0,
    quote_currency: 1,
    base_scale_k: 1,
    quote_scale_k: 1,
    taker_fee: 0,
    maker_fee: 0,
    margin_buy: 0,
    margin_sell: 0,
};

// 创建订单簿
let mut book = AdvancedOrderBook::new(spec);

// 挂卖单
let mut ask = OrderCommand {
    uid: 1,
    order_id: 1,
    symbol: 1,
    price: 10000,
    size: 100,
    action: OrderAction::Ask,
    order_type: OrderType::Gtc,
    reserve_price: 10000,
    timestamp: 1000,
    ..Default::default()
};
book.new_order(&mut ask);

// 买单成交
let mut bid = OrderCommand {
    uid: 2,
    order_id: 2,
    symbol: 1,
    price: 10000,
    size: 50,
    action: OrderAction::Bid,
    order_type: OrderType::Ioc,
    reserve_price: 10000,
    timestamp: 1001,
    ..Default::default()
};
book.new_order(&mut bid);

// 查看成交事件
for event in bid.matcher_events {
    println!("成交: {} @ {}", event.size, event.price);
}
```

### 高级订单类型示例

#### Post-Only 订单（只做 Maker）

```rust
let mut post_only = OrderCommand {
    uid: 1,
    order_id: 1,
    symbol: 1,
    price: 9999,
    size: 10,
    action: OrderAction::Bid,
    order_type: OrderType::PostOnly,
    reserve_price: 9999,
    timestamp: 1000,
    ..Default::default()
};
book.new_order(&mut post_only);
```

#### 冰山单（Iceberg Order）

```rust
let mut iceberg = OrderCommand {
    uid: 1,
    order_id: 1,
    symbol: 1,
    price: 10000,
    size: 1000,        // 总数量
    action: OrderAction::Ask,
    order_type: OrderType::Iceberg,
    reserve_price: 10000,
    timestamp: 1000,
    visible_size: Some(100),  // 显示数量
    ..Default::default()
};
book.new_order(&mut iceberg);
```

#### 止损单（Stop Order）

```rust
let mut stop = OrderCommand {
    uid: 1,
    order_id: 1,
    symbol: 1,
    price: 11000,      // 限价
    size: 10,
    action: OrderAction::Bid,
    order_type: OrderType::StopLimit,
    reserve_price: 11000,
    timestamp: 1000,
    stop_price: Some(10500),  // 触发价
    ..Default::default()
};
book.new_order(&mut stop);
```

#### GTD 订单（Good-Till-Date）

```rust
let mut gtd = OrderCommand {
    uid: 1,
    order_id: 1,
    symbol: 1,
    price: 10000,
    size: 100,
    action: OrderAction::Ask,
    order_type: OrderType::Gtd(2000),
    reserve_price: 10000,
    timestamp: 1000,
    expire_time: Some(2000),  // 过期时间戳
    ..Default::default()
};
book.new_order(&mut gtd);
```

## 性能指标

### 吞吐量
- **TPS (Transactions Per Second)**: 支持数百万级订单处理
  - 10,000 订单: **7,247,910 TPS**
  - 100,000 订单: **4,968,213 TPS**
- **QPS (Queries Per Second)**: 高并发撮合查询
  - 10,000 订单: **3,623,955 QPS**
  - 100,000 订单: **2,484,106 QPS**

### 延迟
- **平均延迟**: < 1 微秒（单订单处理）
- **批量处理**: 10,000 订单约 1.38 毫秒
- **P99 延迟**: < 10 微秒

### 内存
- **内存占用**: 优化的 SOA 布局，减少内存碎片
  - 10,000 订单: **1.91 MB**
  - 100,000 订单: **19.07 MB**
- **订单池**: 预分配机制，减少动态分配

### 性能数据表

| 订单数量 | TPS | QPS | 内存 (MB) | 延迟 (ms) |
|---------|-----|-----|-----------|----------|
| 1,000 | 6,559,183 | 3,279,591 | 0.19 | 0.15 |
| 5,000 | 7,242,000 | 3,621,000 | 0.95 | 0.69 |
| 10,000 | 7,247,910 | 3,623,955 | 1.91 | 1.38 |
| 50,000 | 3,834,037 | 1,917,018 | 9.54 | 13.04 |
| 100,000 | 4,968,213 | 2,484,106 | 19.07 | 20.13 |

### 基准测试

生成性能数据：

```bash
cargo run --example generate_benchmark_data --release
```

生成性能图表（需要安装 matplotlib 和 pandas）：

```bash
pip3 install matplotlib pandas
python3 scripts/plot_benchmark.py
```

查看综合测试报告：

```bash
cargo run --example comprehensive_test --release
```

运行 Criterion 基准测试：

```bash
cargo bench --bench comprehensive_bench
```

## 项目结构

```
matching-core/
├── src/
│   ├── api/              # API 类型定义
│   │   ├── types.rs      # 基础类型
│   │   ├── commands.rs   # 订单命令
│   │   └── events.rs     # 撮合事件
│   ├── core/             # 核心引擎
│   │   ├── exchange.rs   # 交易所核心
│   │   ├── pipeline.rs   # 处理流水线
│   │   ├── orderbook/    # 订单簿实现
│   │   │   ├── naive.rs           # 基础实现
│   │   │   ├── direct.rs          # 高性能实现
│   │   │   ├── direct_optimized.rs # 深度优化
│   │   │   └── advanced.rs        # 高级订单类型
│   │   ├── processors/   # 处理器
│   │   │   ├── risk_engine.rs     # 风控引擎
│   │   │   └── matching_engine.rs # 撮合引擎
│   │   ├── journal.rs    # WAL 日志
│   │   └── snapshot.rs   # 快照
│   └── lib.rs
├── examples/             # 示例代码
│   ├── advanced_demo.rs      # 高级订单演示
│   ├── comprehensive_test.rs # 综合测试
│   └── load_test.rs          # 压力测试
├── benches/              # 基准测试
│   ├── comprehensive_bench.rs # 综合基准测试
│   └── advanced_orderbook_bench.rs
└── tests/                # 单元测试
    ├── advanced_orders_test.rs
    └── edge_cases_test.rs
```

## 订单类型支持

| 订单类型 | 说明 | 状态 |
|---------|------|------|
| GTC | Good-Till-Cancel，直到取消 | ✅ |
| IOC | Immediate-or-Cancel，立即成交或取消 | ✅ |
| FOK | Fill-or-Kill，全部成交或全部取消 | ✅ |
| Post-Only | 只做 Maker，拒绝会立即成交的订单 | ✅ |
| Stop Limit | 止损限价单 | ✅ |
| Stop Market | 止损市价单 | ✅ |
| Iceberg | 冰山单，隐藏真实挂单量 | ✅ |
| Day | 当日有效 | ✅ |
| GTD | Good-Till-Date，指定日期过期 | ✅ |

## 交易品种支持

| 品种类型 | 说明 | 状态 |
|---------|------|------|
| CurrencyExchangePair | 现货交易对 | ✅ |
| FuturesContract | 期货合约 | ✅ |
| PerpetualSwap | 永续合约 | ✅ |
| CallOption | 看涨期权 | ✅ |
| PutOption | 看跌期权 | ✅ |

## 项目地址
```text
https://github.com/llc-993/matching-core
```


## 运行示例

### 基础撮合演示

```bash
cargo run --example advanced_demo --release
```

### 综合测试套件

```bash
cargo run --example comprehensive_test --release
```

### 压力测试

```bash
cargo run --example load_test --release
```

## 测试

运行所有测试：

```bash
cargo test --release
```

运行特定测试：

```bash
cargo test --test advanced_orders_test --release
cargo test --test edge_cases_test --release
```

## 依赖

主要依赖：
- `disruptor`: LMAX Disruptor 模式实现
- `rkyv`: 零拷贝序列化
- `ahash`: 快速哈希算法
- `slab`: 对象池
- `smallvec`: 小向量优化
- `serde`: 序列化框架

## 提示
- 这个项目是从生产上copy出来的，只有大致的逻辑，并没有经过生产环境的检验，仅供学习。


## 服务外壳（matching-server）

`ExchangeCore` 是一个库，本身没有网络层。`src/bin/server.rs` 把它包成可部署的 HTTP 服务。

```bash
cargo run --release --bin matching-server
```

| 环境变量 | 默认值 | 说明 |
|---|---|---|
| `BIND` | `0.0.0.0:8080` | 监听地址 |
| `DATA_DIR` | `./data` | WAL 与快照目录 |
| `WAL_SYNC` | `64` | 每 N 条 fsync 一次；`always` 每条都刷，`never` 不主动刷 |
| `SYMBOLS` | `1:0:1` | `symbol_id:base:quote`，逗号分隔 |
| `RUST_LOG` | `info` | 日志级别 |

接口：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 引擎状态（ok / poisoned / stopped）与当前序号 |
| POST | `/users` | `{uid}` 开户 |
| POST | `/balances` | `{uid, currency, amount, transaction_id}` 出入金，金额可负 |
| GET | `/balances/{uid}/{currency}` | 查余额 |
| POST | `/orders` | `{uid, order_id, symbol, price, size, side, order_type?, reserve_price?}` |
| DELETE | `/orders/{order_id}` | `{uid, symbol}` 撤单 |
| POST | `/orders/{order_id}/reduce` | `{uid, symbol, size}` 减量 |
| GET | `/depth?symbol=1&limit=10` | L2 盘口（含每档笔数） |
| POST | `/admin/checkpoint` | 落快照并压缩 WAL 前缀 |

部署要点：

- **单写者**：撮合是单线程状态机，同一批交易对全局只能跑一个实例。不要放在负载均衡后面多副本，那会把订单簿劈成两份。扩容靠按交易对拆实例。
- **`transaction_id` 必须按用户严格递增**，它是出入金的幂等键；小于等于历史值会被判为重复并忽略。
- **WAL_SYNC 是吞吐旋钮**：EBS 上单次 fsync 约 0.5–1ms，`always` 会把吞吐压到千级/秒；`64` 则崩溃最多丢 63 条。
- **必须让 SIGTERM 走到停机流程**：进程被硬杀时，在途命令与整个内存状态一起丢失。容器/systemd 的停止超时要留够写快照的时间。
- 数据卷不可随实例销毁（WAL 与快照在上面），重建后重新挂载即可自动恢复。

### 容器部署

```bash
docker build -t matching-server .
docker run -d --name matching \
  -p 8080:8080 \
  -v matching-data:/data \
  --stop-timeout 120 \
  matching-server
```

`--stop-timeout` 不能省。Docker 默认只给 10 秒，而停机要排空在途命令并写最终快照；
超时会被 SIGKILL，届时在途命令与整个内存状态一起丢失。订单簿越大越要放宽。

数据卷 `/data` 承载 WAL 与快照，容器重建后必须挂回同一个卷才能恢复状态。

### systemd 部署

```bash
sudo useradd --system --create-home --home-dir /var/lib/matching matching
sudo install -m755 target/release/matching-server /usr/local/bin/
sudo install -m644 deploy/matching-server.service /etc/systemd/system/
sudo install -m600 -o matching deploy/matching-server.env.example /etc/matching-server.env
sudo systemctl daemon-reload && sudo systemctl enable --now matching-server
```

unit 里几个关键项：`TimeoutStopSec=120` 给停机留足写快照的时间；
`ProtectSystem=strict` 下文件系统只读，靠 `ReadWritePaths=/var/lib/matching` 放开数据目录；
`CPUAffinity` 默认注释掉，需要把撮合线程钉在专用核上时解开。
