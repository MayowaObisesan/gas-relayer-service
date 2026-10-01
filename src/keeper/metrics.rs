//! The keeper's scoreboard.
//!
//! Deliberately a *separate* type from the relayer's [`crate::metrics::Metrics`], in a separate
//! `keeper_` namespace, on a separate port. Two processes watching two different systems should
//! not share a counter struct: a merged one makes it impossible to answer "is the *keeper*
//! healthy?" without also answering questions about the relayer, and the two have different
//! urgencies.
//!
//! # The two numbers that matter
//!
//! | Metric | Healthy value | What a bad value means |
//! |---|---|---|
//! | `keeper_gas_burned_on_reverts_wei` | `0` | the keeper is donating gas to the pool it protects |
//! | `keeper_triggers_total{kind="poll"}` | *(climbing)* | **the log subscription is dead**, only the timer works |
//!
//! The second is the keeper-specific trap, and it is worth reading twice. A relayer with a dead
//! queue gets *no* traffic and idles harmlessly. A keeper with a dead subscription still looks
//! perfectly healthy — the pool reports `upkeepNeeded = false`, the keeper logs "idle", every
//! counter reads zero — while the one job it exists to do quietly stops happening. The only
//! evidence is the *ratio* of event-triggered to poll-triggered work, so both are counted and the
//! render names them.
//!
//! Prometheus text by hand, exactly as the relayer does it: a counter is a name and a number, and
//! that needs no dependency.

use core::sync::atomic::{AtomicU64, Ordering};

/// Outcome labels, in the order they are stored in [`KeeperMetrics::outcomes`].
const OUTCOMES: [&str; 4] = ["idle", "act", "backoff", "refuse"];

/// Trigger labels, in the order they are stored in [`KeeperMetrics::triggers`].
const TRIGGERS: [&str; 5] = ["deposit", "withdraw", "liquidated", "tick", "manual"];

/// Keeper counters. Cheap enough to increment on every trigger.
#[derive(Debug, Default)]
pub struct KeeperMetrics {
    /// Triggers received, by kind, indexed like [`TRIGGERS`].
    pub triggers: [AtomicU64; 5],
    /// Decisions reached, by outcome, indexed like [`OUTCOMES`].
    ///
    /// `backoff` and `refuse` are the two that page a human, and they mean different things:
    /// `backoff` is the network's fault, `refuse` is ours.
    pub outcomes: [AtomicU64; 4],
    /// Transactions actually broadcast.
    pub dispatched: AtomicU64,
    /// Broadcasts that were mined successfully.
    pub confirmed: AtomicU64,
    /// Broadcasts that were mined and reverted. **This should be zero.**
    pub reverted: AtomicU64,
    /// Broadcasts refused by the pre-broadcast `eth_call`, so they never cost anything.
    pub refused_before_sending: AtomicU64,
    /// Broadcasts suppressed by the hourly cap. Non-zero is a bug elsewhere, not an attack.
    pub suppressed_by_cap: AtomicU64,
    /// Wei spent in gas.
    pub gas_spent_wei: AtomicU64,
    /// Wei spent on transactions that reverted. The direct measure of the loss.
    pub gas_burned_on_reverts_wei: AtomicU64,
    /// Times the log subscription was re-established after an error.
    ///
    /// A keeper that reconnects constantly is not watching, and this count is how a student
    /// distinguishes "the pool is fine, our connection is not" from "the pool is in trouble".
    pub subscription_reconnects: AtomicU64,
    /// Consecutive `checkUpkeep` calls that could not be answered. Zero is the healthy value.
    pub consecutive_backoffs: AtomicU64,
    /// Event triggers discarded because the queue was full.
    ///
    /// Non-zero is a *warning*, not a loss: a dropped trigger is recovered by the poll timer, so
    /// nothing was lost — but a large number means events are arriving faster than the keeper can
    /// resolve them, which is a rate problem the poll interval will not solve.
    pub dropped_triggers: AtomicU64,
}

impl KeeperMetrics {
    /// Count one trigger received by the watcher. `index` follows [`TRIGGERS`].
    pub fn record_trigger(&self, index: usize) {
        self.triggers[index].fetch_add(1, Ordering::Relaxed);
    }

    /// Count one decision reached by [`crate::keeper::state::decide`]. `index` follows [`OUTCOMES`].
    pub fn record_outcome(&self, index: usize) {
        self.outcomes[index].fetch_add(1, Ordering::Relaxed);
    }

    /// Record the gas cost of a mined transaction, attributing it to success or to failure.
    ///
    /// Saturating arithmetic, matching the relayer: a runaway counter must never wrap into a
    /// plausible number on a dashboard.
    pub fn record_receipt(&self, success: bool, gas_used: u64, effective_gas_price: u128) {
        let cost = (gas_used as u128).saturating_mul(effective_gas_price) as u64;
        if success {
            self.confirmed.fetch_add(1, Ordering::Relaxed);
        } else {
            self.reverted.fetch_add(1, Ordering::Relaxed);
            self.gas_burned_on_reverts_wei
                .fetch_add(cost, Ordering::Relaxed);
        }
        self.gas_spent_wei.fetch_add(cost, Ordering::Relaxed);
    }

    /// Event-triggered work: the number that should be non-zero on a working subscription.
    pub fn event_triggers(&self) -> u64 {
        self.triggers[..3]
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .sum()
    }

    /// Total work done, from any source.
    pub fn total_triggers(&self) -> u64 {
        self.triggers.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    /// Render the counters as Prometheus text exposition format.
    ///
    /// `subscribed` is passed in rather than stored because it is a fact about the *connection*,
    /// not a counter: it is on or off. The interesting case is the gap between "the subscription is
    /// live" and "the subscription is *delivering*", which the two trigger kinds expose between
    /// them.
    pub fn render(&self, queue_len: usize, subscribed: bool, dry_run_enabled: bool) -> String {
        let mut out = String::new();

        out.push_str("# HELP keeper_dry_run_enabled Whether the pre-broadcast eth_call is active. 0 means the keeper buys failing liquidations.\n");
        out.push_str("# TYPE keeper_dry_run_enabled gauge\n");
        out.push_str(&format!(
            "keeper_dry_run_enabled {}\n",
            u8::from(dry_run_enabled)
        ));

        out.push_str("# HELP keeper_subscribed Whether the log subscription is currently live. A keeper running on its poll timer alone is degraded, not healthy.\n");
        out.push_str("# TYPE keeper_subscribed gauge\n");
        out.push_str(&format!("keeper_subscribed {}\n", u8::from(subscribed)));

        out.push_str("# HELP keeper_triggers_total Triggers received, by kind. If kind=\"poll\" is the only one moving, the subscription is dead and the keeper is running blind.\n");
        out.push_str("# TYPE keeper_triggers_total counter\n");
        for (label, counter) in TRIGGERS.iter().zip(self.triggers.iter()) {
            out.push_str(&format!(
                "keeper_triggers_total{{kind=\"{label}\"}} {}\n",
                counter.load(Ordering::Relaxed)
            ));
        }
        out.push_str(&format!(
            "keeper_triggers_from_events_total {}\n",
            self.event_triggers()
        ));

        out.push_str("# HELP keeper_outcomes_total Decisions reached, by outcome. \"backoff\" means we could not find out; \"refuse\" means the answer was unusable.\n");
        out.push_str("# TYPE keeper_outcomes_total counter\n");
        for (label, counter) in OUTCOMES.iter().zip(self.outcomes.iter()) {
            out.push_str(&format!(
                "keeper_outcomes_total{{outcome=\"{label}\"}} {}\n",
                counter.load(Ordering::Relaxed)
            ));
        }

        out.push_str("# HELP keeper_consecutive_backoffs Unanswered checkUpkeep calls in a row. Non-zero means the keeper is blind right now.\n");
        out.push_str("# TYPE keeper_consecutive_backoffs gauge\n");
        out.push_str(&format!(
            "keeper_consecutive_backoffs {}\n",
            self.consecutive_backoffs.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP keeper_transactions_total Broadcast outcomes.\n");
        out.push_str("# TYPE keeper_transactions_total counter\n");
        for (label, value) in [
            ("dispatched", self.dispatched.load(Ordering::Relaxed)),
            ("confirmed", self.confirmed.load(Ordering::Relaxed)),
            ("reverted", self.reverted.load(Ordering::Relaxed)),
            (
                "refused_before_sending",
                self.refused_before_sending.load(Ordering::Relaxed),
            ),
            (
                "suppressed_by_cap",
                self.suppressed_by_cap.load(Ordering::Relaxed),
            ),
        ] {
            out.push_str(&format!(
                "keeper_transactions_total{{status=\"{label}\"}} {value}\n"
            ));
        }

        out.push_str("# HELP keeper_gas_spent_wei Total wei paid in gas.\n");
        out.push_str("# TYPE keeper_gas_spent_wei counter\n");
        out.push_str(&format!(
            "keeper_gas_spent_wei {}\n",
            self.gas_spent_wei.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP keeper_gas_burned_on_reverts_wei Wei paid for transactions that reverted. Must stay 0.\n");
        out.push_str("# TYPE keeper_gas_burned_on_reverts_wei counter\n");
        out.push_str(&format!(
            "keeper_gas_burned_on_reverts_wei {}\n",
            self.gas_burned_on_reverts_wei.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP keeper_subscription_reconnects_total Times the log subscription was re-established after an error.\n");
        out.push_str(&format!(
            "keeper_subscription_reconnects_total {}\n",
            self.subscription_reconnects.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP keeper_dropped_triggers_total Event triggers discarded because the queue was full. Recovered by the poll timer, so no liquidation is lost.\n");
        out.push_str("# TYPE keeper_dropped_triggers_total counter\n");
        out.push_str(&format!(
            "keeper_dropped_triggers_total {}\n",
            self.dropped_triggers.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP keeper_queue_len Triggers waiting to be resolved.\n");
        out.push_str("# TYPE keeper_queue_len gauge\n");
        out.push_str(&format!("keeper_queue_len {queue_len}\n"));
        out
    }
}
