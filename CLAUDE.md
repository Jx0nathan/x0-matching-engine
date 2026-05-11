# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repo is

A production-targeted matching engine for spot + perpetual/futures, built as a Cargo workspace. This is a **greenfield rewrite** of an earlier `matching-core` reference design; that legacy code is for comparison only and not depended on.

Status: **M3.1 (persistence) complete**. WAL with bincode framing + snapshot store + crash recovery integration tests all green. M3.2 (Disruptor 3-stage pipeline) is next. See "Milestones" below for what's implemented vs. planned.

## Architecture

### Pipeline shape (target — not all wired yet)

```
client ─► CommandEnvelope ─► Disruptor RingBuffer
                                    │
                          ┌─────────┴──────────┐
                          │ R1: Risk pre-check │   handler 1
                          └─────────┬──────────┘
                                    ▼
                          ┌────────────────────┐
                          │ Matching Engine    │   handler 2
                          └─────────┬──────────┘
                                    ▼
                          ┌────────────────────┐
                          │ R2: Settlement     │   handler 3
                          └─────────┬──────────┘
                                    ▼
                              CommandReceipt
                                    │
                              WAL fsync + result consumer
```

Three handlers run on three threads, linked by `SequenceBarrier`. This is the "真 Disruptor 三段并行" decision — see `crates/me-disruptor` (M3).

### Crate layout

| Crate | Role | Status |
|---|---|---|
| `me-types` | Numeric types, IDs, Command/Event/Receipt, SymbolSpec, conservation trait | ✅ M1 |
| `me-matching` | Order book + matching (Limit; Gtc/Ioc/Fok/PostOnly) | ✅ M2 |
| `me-risk` | Risk engine with paired hold/settle (`UserAccount`, `Hold`, `RiskEngine`) | ✅ M2 |
| `me-core` | Synchronous facade `submit(Command) → CommandReceipt` + persistence wiring | ✅ M2 + M3.1 |
| `me-disruptor` | Lock-free ring buffer + SequenceBarrier | M3.2 (stub) |
| `me-wal` | Write-ahead log + snapshot store | ✅ M3.1 |
| `me-server` | Binary daemon | M5 (stub) |

Conservation property test lives in `crates/me-core/tests/conservation.rs` and is the M2 quality gate.

### Type-layer invariants (enforced by `me-types`)

- **Numerics are i64 newtypes**: `Price`, `Size`, `Amount`. They are **not interchangeable** — `price + size` fails at compile time.
- **Cross-type math widens to i128**: `Price::mul_size(Size) -> i128`. The caller decides how to scale back down. There is no implicit scale factor on `Price` itself — that lives in `SymbolSpec`.
- **Fees are bps-based**: `Amount::mul_bps_ceil` rounds **away from zero** so the exchange never undercharges a sub-minor-unit fee. Use `mul_bps` (truncate toward zero) for non-fee proportional math.
- **Command / Event / Receipt are three separate types.** This is the deliberate departure from the legacy `OrderCommand` god-struct. Commands are input, Events are emissions during processing, Receipts are final outcomes. No mutable struct flows through the pipeline.
- **Conservation is the top-level testable invariant**: `ConservationLedger` (`invariants.rs`) is the contract every property test will assert against. *No input sequence can make `check_balanced()` return `Err`.* This is what makes "对账算得对" testable rather than aspirational.

### Naming conventions

- Side is `Bid`/`Ask`, not Buy/Sell (matches order-book vocabulary).
- `OrderType` is the *shape* (Limit/Market/Stop/Iceberg). `TimeInForce` is the *lifecycle* (Gtc/Ioc/Fok/Day/Gtd/PostOnly). Legacy code conflated these — don't.
- `*_minor_per_major` always means "how many smallest units make up one whole unit" (e.g. `1_000_000` for USDT-6, `100_000_000` for BTC-8).

## Common commands

```bash
# Build everything
cargo build --workspace

# Run all tests
cargo test --workspace

# Run a single crate's tests
cargo test -p me-matching
cargo test -p me-risk
cargo test -p me-core

# Run the conservation property test with more cases
PROPTEST_CASES=10000 cargo test -p me-core --test conservation --release

# Lint (must pass before commits)
cargo clippy --workspace --all-targets -- -D warnings

# Format
cargo fmt --all
```

## Milestones

| ID | Scope | Status |
|---|---|---|
| M1 | Workspace skeleton + `me-types` + conservation framework | ✅ done |
| M2 | Spot matching: order book, R1/R2, synchronous pipeline + conservation property test | ✅ done |
| **M3.1** | WAL (bincode framed) + snapshot store + crash recovery tests | ✅ done |
| M3.2 | Three-handler Disruptor pipeline (R1 → Match → R2), real parallelism | next |
| M3.3 | WAL group commit + CRC checksums + crash-via-kill test | pending |
| M4 | Derivatives: margin engine, perp/future contracts, liquidation queue, funding rate | pending |
| M5 | Productionization: tracing/Prometheus, fuzz suite, CI, stress tests, gray-release config | pending |

Each milestone is independently shippable. Don't start M(n+1) work in M(n) — keep the boundary clean.

## Working agreements

- **No floats anywhere.** Money is i64 minor units; bps for proportional math.
- **No `unsafe`** without a comment explaining the invariant being upheld.
- **No `unwrap()` in non-test code.** Use `?` with `RejectReason` or an `anyhow::Result`.
- **Conservation tests are the gate**: any change to risk or matching logic must keep `crates/me-core/tests/conservation.rs` green.
- **Don't backport from `../matching-core/`.** Read it for reference, but resist copy-paste — its design problems are exactly what we're fixing here.
