# matching-engine

English | [中文](./README.md)

Production-targeted matching engine for spot + perpetual/futures, written in Rust.

> **Status: M5.3 (ModifyOrder) complete.** Modify is implemented as cancel-and-replace: snapshot the existing resting order (side / price / size_remaining / visible_slice), build a synthetic PlaceOrder with `new_price` / `new_size` overrides, atomically remove the old + release hold, then pre_check and apply_to_book the replacement under a fresh order_id (time priority is lost). M5.2 order types, M5.1 STP, M4 derivatives, M3 persistence all preserved. See `CLAUDE.md`.

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
