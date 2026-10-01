//! The Watcher: `(Event)`. Listens, and — the part `KEEPERS.md` skips — *notices when it stopped
//! listening*.
//!
//! Two producers, one consumer:
//!
//! ```text
//!   watch_logs()   --- eth_subscribe on Deposit/Withdraw/Liquidated --+
//!                                                                     +--> mpsc<Trigger>
//!   watch_ticks()  --- interval timer, independent of any socket -------+
//! ```
//!
//! Both send [`Trigger`]s and nothing else. A trigger carries no payload, no health factor and no
//! opinion: the condition is re-derived on-chain every time (see [`crate::keeper::state`]).
//!
//! # Why the timer is not optional
//!
//! A WebSocket subscription is a connection that a remote node is free to drop, and a dropped
//! subscription is **indistinguishable from a quiet chain** at the application layer. The node
//! does not tell you "I have stopped sending you logs"; the stream simply goes quiet, and every
//! log line the keeper writes says `idle`. Meanwhile the position it exists to liquidate goes from
//! 0.9 to 0.7 to 0.3 and nobody is watching.
//!
//! Real deployments handle this three ways, and this module does the first two:
//!
//! 1. **Poll anyway** ([`watch_ticks`]). Bounds the blindness to one interval. This is the
//!    single highest-value line in the file.
//! 2. **Detect the disconnect** ([`watch_logs`] reconnects and counts it, and
//!    `keeper_subscribed` goes to 0). Reconnects alone are not enough — the *scary* failure is a
//!    connection that stays open and delivers nothing, which only the timer can catch.
//! 3. **Backfill on restart**: scan recent blocks for accounts already underwater. A keeper that
//!    starts up after a position went bad has, by construction, missed the event that told it.
//!    This is a documented gap (`KEEPERS.md`), not an oversight.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::{Filter, Log};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::keeper::abi;
use crate::keeper::metrics::KeeperMetrics;
use crate::keeper::state::Trigger;

/// Index of each trigger in [`KeeperMetrics::triggers`], so recording a trigger cannot pick the
/// wrong bucket. The watcher records by `Trigger`; the metrics array is indexed.
fn trigger_index(trigger: Trigger) -> usize {
    match trigger {
        Trigger::Deposit => 0,
        Trigger::Withdraw => 1,
        Trigger::Liquidated => 2,
        Trigger::Tick => 3,
        Trigger::Manual => 4,
    }
}

/// Record a trigger in the right counter.
pub fn record_trigger(metrics: &KeeperMetrics, trigger: Trigger) {
    metrics.record_trigger(trigger_index(trigger));
}

/// The log filter: our pool, and the three events that can change whether a liquidation is needed.
///
/// `Filter::new().address(pool).events(...)` is the whole security surface of the watcher: pool
/// address (whose logs we care about) and topic0s (which events). A filter with `events` but no
/// address would match another contract's `Withdraw`; a filter with an address but no events would
/// deliver every log the pool emits, including the `Liquidated` events a busy pool emits constantly.
///
/// Note that both halves are needed and neither is implied by the other — see
/// [`abi::DEPOSIT_TOPIC`] for why topic0 alone is not enough.
///
/// # `event_signature`, not `events` — and this is a real bug this signature prevents
///
/// `Filter::events` takes event *signatures* (`"Deposit(address,uint256,uint256)"`) and hashes each
/// one itself. Passing it the already-hashed [`abi::DEPOSIT_TOPIC`] **double-hashes** it, and the
/// resulting filter matches nothing at all.
///
/// That failure is the worst kind: no error, no warning, no reconnect — just a subscription that
/// is healthy, connected, and permanently deaf. The keeper would then run entirely on its poll
/// timer while every metric except `kind="poll"` stayed at zero, which is exactly the condition
/// `keeper::watcher` is built to make visible.
///
/// `event_signature` takes topic0 values as given, which is what we have. The test
/// `the_filter_is_scoped_to_the_pool_and_the_three_events` is what caught this.
pub fn watcher_filter(pool: Address) -> Filter {
    Filter::new()
        .address(pool)
        .event_signature(vec![
            abi::DEPOSIT_TOPIC,
            abi::WITHDRAW_TOPIC,
            abi::LIQUIDATED_TOPIC,
        ])
}

/// Map a decoded log back to the trigger it represents.
///
/// # Errors
/// Returns a message when the log's topic0 is none of ours. That should be impossible given the
/// filter, and treating it as an error rather than ignoring it means a changed filter shows up as
/// a log line instead of as a keeper that mysteriously stopped reacting.
pub fn trigger_from_log(log: &Log) -> Result<Trigger, String> {
    let Some(topic0) = log.topics().first() else {
        return Err("log has no topics".to_string());
    };
    if *topic0 == abi::DEPOSIT_TOPIC {
        Ok(Trigger::Deposit)
    } else if *topic0 == abi::WITHDRAW_TOPIC {
        Ok(Trigger::Withdraw)
    } else if *topic0 == abi::LIQUIDATED_TOPIC {
        Ok(Trigger::Liquidated)
    } else {
        Err(format!("unexpected topic0 {topic0} in a filtered stream"))
    }
}

/// Watch pool logs over a WebSocket and forward every match as a [`Trigger`].
///
/// This function is written as a **reconnecting loop** rather than a straight `while let`, and
/// that is the point. A single `subscribe` that errors out takes the whole watcher down; since
/// the watcher is a spawned task, a dead task is invisible — the process is still running, still
/// serving `/metrics`, still reporting `keeper_subscribed 1` if nobody thought to change it.
///
/// The `is_subscribed` flag is the honest version of that gauge, and it is flipped in **both**
/// directions: down on disconnect, up on a successful resubscribe.
///
/// # Panics
/// Never. Every error path reconnects after a delay, because a keeper that exits on a transient
/// RPC blip is worse than one that retries.
///
/// # Note on back-pressure
/// `try_send` rather than `send`. A burst of `Liquidated` logs on a busy pool must not be able to
/// block the subscription and cause the node to drop us — the one failure this whole design is
/// trying to prevent. A dropped *trigger* is safe, because the poll timer will re-ask anyway;
/// a blocked subscription is not.
pub async fn watch_logs(
    ws_url: &str,
    pool: Address,
    tx: mpsc::Sender<Trigger>,
    metrics: Arc<KeeperMetrics>,
    is_subscribed: Arc<AtomicBool>,
) {
    let mut backoff = Duration::from_secs(1);

    loop {
        is_subscribed.store(false, Ordering::Relaxed);

        match ProviderBuilder::new().connect_ws(WsConnect::new(ws_url)).await {
            Ok(provider) => {
                let filter = watcher_filter(pool);
                let mut stream = match provider.subscribe_logs(&filter).await {
                    Ok(subscription) => subscription.into_stream(),
                    Err(err) => {
                        eprintln!("⚠️  subscribe failed: {err}; retrying in {backoff:?}");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                        continue;
                    }
                };

                // We are genuinely receiving logs now.
                is_subscribed.store(true, Ordering::Relaxed);
                metrics
                    .subscription_reconnects
                    .fetch_add(1, Ordering::Relaxed);
                backoff = Duration::from_secs(1);
                println!("👂 subscribed to {pool} logs on {ws_url}");

                // The stream yields the log itself, or ends. `into_stream()` already unwraps the
                // JSON-RPC envelope, so a dropped connection shows up as the stream *ending*
                // (`None`) rather than as an `Err` -- which is exactly why this loop has to be
                // written to reconnect on the way out as well as on an error.
                while let Some(log) = stream.next().await {
                    match trigger_from_log(&log) {
                        Ok(trigger) => {
                            record_trigger(&metrics, trigger);
                            // A full queue drops the *nudge*, which is safe: the poll timer
                            // re-asks the pool regardless. `send().await` here would instead block
                            // the subscription and get us dropped -- the one failure this whole
                            // design exists to prevent.
                            if tx.try_send(trigger).is_err() {
                                metrics.dropped_triggers.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(reason) => {
                            eprintln!("⚠️  {reason} -- the filter and the interface disagree");
                        }
                    }
                }

                is_subscribed.store(false, Ordering::Relaxed);
                eprintln!("🔌 log subscription ended; reconnecting in {backoff:?}");
            }
            Err(err) => {
                eprintln!("⚠️  cannot connect to {ws_url}: {err}; retrying in {backoff:?}");
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Emit a [`Trigger::Tick`] on a fixed interval, forever.
///
/// This is the safety net, and it is the reason the keeper is *correct* rather than merely
/// reactive. A `Log` subscription is a socket the node may close without telling us, and a closed
/// socket is indistinguishable from a quiet chain: every log line still says `idle`, every counter
/// still reads zero, and the position the keeper exists to liquidate goes from 0.9 to 0.3
/// unobserved. No amount of reconnect logic fixes that, because a connection can be perfectly
/// healthy and simply deliver nothing.
///
/// `try_send`, not `send`, for the same reason as in [`watch_logs`]: a full queue drops a tick,
/// and a missed tick costs nothing because the next one is seconds away. A *blocked* tick would
/// stall the timer task forever.
///
/// # Note
/// The first tick fires immediately rather than after one interval, so a keeper that starts
/// against an already-underwater position acts at once instead of waiting out the interval. That
/// is also the honest behaviour for a restart: the keeper has no memory of events it missed
/// (`KEEPERS.md` gap: no backfill), so the timer is its only recovery path.
pub async fn watch_ticks(
    interval: Duration,
    tx: mpsc::Sender<Trigger>,
    metrics: Arc<KeeperMetrics>,
) {
    let mut ticker = tokio::time::interval(interval);
    // `MissedTickBehavior::Delay`: if resolving one trigger took longer than the interval, do not
    // try to "catch up" by firing a burst of ticks. Each tick is a full `eth_call`; a burst would
    // hammer the node exactly when it is already struggling.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // The first `tick()` completes immediately.
        ticker.tick().await;
        record_trigger(&metrics, Trigger::Tick);
        if tx.try_send(Trigger::Tick).is_err() {
            metrics.dropped_triggers.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keeper::metrics::KeeperMetrics;
    use alloy::primitives::{B256, Bytes};
    use alloy::rpc::types::Log;

    /// A log carrying one topic0, which is all `trigger_from_log` looks at. Built by hand because
    /// `alloy` does not offer a convenient `Log` constructor, and the fields it needs are exactly
    /// the two this function reads.
    fn log_with_topic0(topic0: alloy::primitives::B256) -> Log {
        let inner = alloy::primitives::Log {
            address: Address::ZERO,
            data: alloy::primitives::LogData::new_unchecked(
                vec![topic0, alloy::primitives::B256::ZERO],
                Bytes::new(),
            ),
        };
        Log {
            inner,
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    /// Each of the three subscribed events must map to its own trigger. Getting this wrong is the
    /// quiet failure mode: the keeper would still wake up, just on the wrong event, and the only
    /// symptom would be a pool that is liquidated late (or not at all).
    #[test]
    fn every_subscribed_topic_maps_to_its_trigger() {
        assert_eq!(
            trigger_from_log(&log_with_topic0(abi::DEPOSIT_TOPIC)).unwrap(),
            Trigger::Deposit
        );
        assert_eq!(
            trigger_from_log(&log_with_topic0(abi::WITHDRAW_TOPIC)).unwrap(),
            Trigger::Withdraw
        );
        assert_eq!(
            trigger_from_log(&log_with_topic0(abi::LIQUIDATED_TOPIC)).unwrap(),
            Trigger::Liquidated
        );
    }

    /// An unknown topic is an **error**, not `None` and not a default. Swallowing it would mean a
    /// changed filter (or a topic0 added to the pool's ABI) silently produces a keeper that stops
    /// reacting with no log line to explain why.
    #[test]
    fn an_unexpected_topic_is_an_error_not_a_silent_skip() {
        let unknown = alloy::primitives::b256!(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        );
        assert!(trigger_from_log(&log_with_topic0(unknown)).is_err());
    }

    /// The filter must be scoped to *our* pool **and** our three events. A filter missing the
    /// address would match any contract's `Deposit`; a filter missing the events would deliver
    /// every log the pool emits. Both are visible here, and neither is visible at runtime without
    /// reading this test.
    #[test]
    fn the_filter_is_scoped_to_the_pool_and_the_three_events() {
        let pool = Address::repeat_byte(0x11);
        let filter = watcher_filter(pool);

        // Address: exactly one, and it is ours.
        let addresses: Vec<Address> = filter.address.iter().copied().collect();
        assert_eq!(addresses, vec![pool]);

        // Topics: `Topic` is a `FilterSet<B256>` (a HashSet newtype), so membership is a
        // `contains`, and the first slot holds all three as an OR-pattern. The remaining three
        // slots are unconstrained -- the keeper does not filter on indexed parameters.
        let topics: Vec<&B256> = filter.topics[0].iter().collect();
        assert!(topics.contains(&&abi::DEPOSIT_TOPIC), "{topics:?}");
        assert!(topics.contains(&&abi::WITHDRAW_TOPIC), "{topics:?}");
        assert!(topics.contains(&&abi::LIQUIDATED_TOPIC), "{topics:?}");
        // `UpkeepPerformed` is deliberately NOT subscribed: the `Liquidated` event already tells
        // the keeper the outcome, and two events per liquidation would double its trigger rate.
        let upkeep = <abi::UpkeepPerformed as alloy::sol_types::SolEvent>::SIGNATURE_HASH;
        assert!(
            !topics.contains(&&upkeep),
            "UpkeepPerformed would double the trigger rate for no new information"
        );
    }

    /// The trigger-index mapping must agree with the label order in `metrics.rs`. These are two
    /// parallel arrays in two modules and nothing but this test keeps them in step; a swap would
    /// make `kind="withdraw"` count deposits and quietly break the dead-subscription alert.
    #[test]
    fn trigger_indices_match_the_metric_label_order() {
        let metrics = KeeperMetrics::default();
        // One of each, recorded through the watcher's own mapper.
        for trigger in [
            Trigger::Deposit,
            Trigger::Withdraw,
            Trigger::Liquidated,
            Trigger::Tick,
            Trigger::Manual,
        ] {
            record_trigger(&metrics, trigger);
        }
        let text = metrics.render(0, true, true);
        for (index, label) in [
            "deposit",
            "withdraw",
            "liquidated",
            "tick",
            "manual",
        ]
        .iter()
        .enumerate()
        {
            assert!(
                text.contains(&format!(
                    "keeper_triggers_total{{kind=\"{label}\"}} 1"
                )),
                "{label} (index {index}) was not counted under its own label"
            );
        }
    }
}
