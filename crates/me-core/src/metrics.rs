use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use me_risk::{RiskEngine, EXCHANGE_ACCOUNT, INSURANCE_FUND};
use me_types::CurrencyId;

/// Instrumentation counters incremented on hot paths. All counters are
/// monotonically increasing `u64`. Snapshot-time gauges (insurance fund
/// balance, exchange revenue, last_applied_seq) are computed by reading
/// engine state directly — they don't need atomics because they're
/// already protected by the engine's single-consumer execution model.
#[derive(Debug, Default)]
pub struct Metrics {
    pub commands_total: AtomicU64,
    pub place_orders_total: AtomicU64,
    pub cancel_orders_total: AtomicU64,
    pub modify_orders_total: AtomicU64,
    pub trades_total: AtomicU64,
    pub liquidations_total: AtomicU64,
    pub stp_cancellations_total: AtomicU64,
    pub rejected_total: AtomicU64,
    pub wal_syncs_total: AtomicU64,
    pub funding_settlements_total: AtomicU64,
    pub future_settlements_total: AtomicU64,
    pub stops_triggered_total: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn inc(&self, counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn add(&self, counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

/// Render counters + on-demand state gauges as Prometheus text-format.
/// Caller (typically the binary daemon) serves this from an HTTP endpoint
/// scraped by Prometheus.
pub fn render_prometheus(
    metrics: &Metrics,
    risk: &RiskEngine,
    last_applied_seq: u64,
    order_book_depths: &[(u32, usize)], // (symbol_id, resting_orders)
) -> String {
    let mut out = String::new();

    // ---- Counters ----
    write_counter(
        &mut out,
        "me_commands_total",
        "Total commands processed (any type)",
        metrics.commands_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "me_place_orders_total",
        "PlaceOrder commands accepted by dispatch (incl. later-rejected)",
        metrics.place_orders_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "me_cancel_orders_total",
        "CancelOrder commands accepted by dispatch",
        metrics.cancel_orders_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "me_modify_orders_total",
        "ModifyOrder commands accepted by dispatch",
        metrics.modify_orders_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "me_trades_total",
        "Trade events emitted (one per maker × taker fill)",
        metrics.trades_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "me_stp_cancellations_total",
        "Maker cancellations triggered by self-trade prevention",
        metrics.stp_cancellations_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "me_liquidations_total",
        "Force-closed positions across all symbols",
        metrics.liquidations_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "me_rejected_total",
        "CommandStatus::Rejected outcomes from submit",
        metrics.rejected_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "me_wal_syncs_total",
        "Successful WAL fsync calls",
        metrics.wal_syncs_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "me_funding_settlements_total",
        "ApplyFunding commands processed",
        metrics.funding_settlements_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "me_future_settlements_total",
        "SettleFuture commands processed",
        metrics.future_settlements_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "me_stops_triggered_total",
        "Stop orders triggered by mark-price updates",
        metrics.stops_triggered_total.load(Ordering::Relaxed),
    );

    // ---- Gauges ----
    write_gauge(
        &mut out,
        "me_last_applied_seq",
        "Sequence number of the last applied command",
        last_applied_seq as i64,
    );

    // Insurance fund balance per currency.
    if let Some(insurance) = risk.account(INSURANCE_FUND) {
        gauge_header(
            &mut out,
            "me_insurance_fund_balance",
            "Insurance fund balance per currency (signed; negative = uncovered deficit)",
        );
        for (currency, amount) in &insurance.balances {
            let _ = writeln!(
                out,
                "me_insurance_fund_balance{{currency=\"{}\"}} {}",
                currency.0,
                amount.raw()
            );
        }
    }

    // Exchange revenue per currency.
    if let Some(revenue) = risk.account(EXCHANGE_ACCOUNT) {
        gauge_header(
            &mut out,
            "me_exchange_revenue",
            "Accumulated trading-fee revenue per currency",
        );
        for (currency, amount) in &revenue.balances {
            let _ = writeln!(
                out,
                "me_exchange_revenue{{currency=\"{}\"}} {}",
                currency.0,
                amount.raw()
            );
        }
    }

    // Per-symbol order book depth (count of resting orders).
    if !order_book_depths.is_empty() {
        gauge_header(
            &mut out,
            "me_order_book_resting_orders",
            "Number of resting orders per symbol",
        );
        for (symbol_id, count) in order_book_depths {
            let _ = writeln!(
                out,
                "me_order_book_resting_orders{{symbol=\"{}\"}} {}",
                symbol_id, count
            );
        }
    }

    // Currency-aware free + held conservation totals. Each registered
    // currency surfaces as a separate series so dashboards can detect
    // drift over time.
    gauge_header(
        &mut out,
        "me_total_internal",
        "Sum of free + held + position.margin_locked across all accounts (mark-free, realized only)",
    );
    let mut seen = ahash::AHashSet::new();
    for currency in collect_currencies(risk) {
        if !seen.insert(currency) {
            continue;
        }
        let total = risk.total_internal(currency);
        let _ = writeln!(
            out,
            "me_total_internal{{currency=\"{}\"}} {}",
            currency.0, total
        );
    }

    out
}

fn collect_currencies(risk: &RiskEngine) -> Vec<CurrencyId> {
    let mut v = Vec::new();
    // Walk both insurance and exchange to catch every currency the engine
    // has touched; conservation should be checked for each.
    if let Some(a) = risk.account(INSURANCE_FUND) {
        v.extend(a.balances.keys().copied());
        v.extend(a.holds.keys().copied());
    }
    if let Some(a) = risk.account(EXCHANGE_ACCOUNT) {
        v.extend(a.balances.keys().copied());
        v.extend(a.holds.keys().copied());
    }
    v
}

fn write_counter(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {} {}", name, help);
    let _ = writeln!(out, "# TYPE {} counter", name);
    let _ = writeln!(out, "{} {}", name, value);
}

fn write_gauge(out: &mut String, name: &str, help: &str, value: i64) {
    gauge_header(out, name, help);
    let _ = writeln!(out, "{} {}", name, value);
}

fn gauge_header(out: &mut String, name: &str, help: &str) {
    let _ = writeln!(out, "# HELP {} {}", name, help);
    let _ = writeln!(out, "# TYPE {} gauge", name);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_renders() {
        let m = Metrics::new();
        let risk = RiskEngine::new();
        let s = render_prometheus(&m, &risk, 0, &[]);
        assert!(s.contains("me_commands_total 0"));
        assert!(s.contains("# TYPE me_commands_total counter"));
        assert!(s.contains("me_last_applied_seq 0"));
    }

    #[test]
    fn counters_increment() {
        let m = Metrics::new();
        m.inc(&m.commands_total);
        m.inc(&m.commands_total);
        m.add(&m.trades_total, 5);
        let risk = RiskEngine::new();
        let s = render_prometheus(&m, &risk, 42, &[]);
        assert!(s.contains("me_commands_total 2"));
        assert!(s.contains("me_trades_total 5"));
        assert!(s.contains("me_last_applied_seq 42"));
    }
}
