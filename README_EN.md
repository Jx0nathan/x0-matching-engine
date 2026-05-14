# matching-engine

English | [中文](./README.md)

Production-targeted matching engine for spot + perpetual/futures, written in Rust.

> **Status: M5.5 (Prometheus metrics + CI) complete.** `MatchingEngine::metrics_snapshot()` renders Prometheus text format directly (counters for commands/trades/liquidations/WAL syncs/stops; gauges for `last_applied_seq`, insurance fund balance, exchange revenue, per-symbol resting orders, per-currency `total_internal`). GitHub Actions CI runs fmt, clippy `-D warnings`, full workspace tests, release build, and a 1000-case conservation property test on every push and PR. See `CLAUDE.md`.

## Design goals

- **Correctness first.** Every change is gated by a conservation-of-money property test: across any sequence of commands, the sum of user balances must equal net deposits in every currency. No input sequence can mint or destroy money.
- **Determinism.** The pipeline is single-event-at-a-time per stage. Replay of the WAL reproduces engine state bit-for-bit.
- **Performance, then portability.** Disruptor-style three-stage pipeline (R1 → Matching → R2) with one core per stage. i64 minor-units throughout; intermediate math widens to i128 to make overflow impossible at multiplications.

## Layout

```
matching-engine/
├── crates/
│   ├── me-types/        types, commands, events, conservation invariant
│   ├── me-disruptor/    lock-free ring buffer (M3)
│   ├── me-wal/          write-ahead log + snapshots (M3)
│   ├── me-risk/         R1 pre-check + R2 settlement (M2)
│   ├── me-matching/     order book + matching (M2)
│   ├── me-core/         pipeline facade (M2/M3)
│   └── me-server/       binary daemon (M5)
└── tests/
    └── invariants/      cross-crate conservation tests
```

## Build / test

```bash
cargo build --workspace
cargo test -p me-types        # M1 tests
cargo clippy --workspace --all-targets -- -D warnings
```

Requires Rust 1.75+.

## What this is not

- Not a fork of the earlier `matching-core` reference design. That code is for comparison; we don't depend on it.
- Not a finished system. See `CLAUDE.md` milestones.
- Not for unaudited production use. Conservation tests catch arithmetic bugs; they don't catch business-logic mistakes a real audit would.

## License

MIT.
