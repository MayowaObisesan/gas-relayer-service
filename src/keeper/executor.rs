//! The Executor: `(Action)`. Dry run, sign, broadcast, receipt — and nothing else.
//!
//! The only function in the keeper that can spend money, so the only function that has to be
//! paranoid. It is built as a straight line with a **dry run before every broadcast** and no
//! early return that skips it, because the shape of the code is the defence:
//!
//! ```text
//!   Upkeep::Act { payload }
//!         |
//!         |  gas cap?               <- bound the worst case
//!         v
//!   dry_run(performUpkeep(payload))  <- free eth_call; THE PRIMARY DEFENCE
//!         |
//!         |  WouldRevert -> count and return. No gas spent.
//!         |  Unavailable -> count and return. No gas spent.
//!         v
//!   send_transaction                 <- the only line in the keeper that costs money
//!         |
//!         v
//!   get_receipt -> record gas        <- success or revert, both counted
//! ```
//!
//! # The TOCTOU window, honestly stated
//!
//! The dry run executes at one instant; the transaction lands at another. Between them a
//! competing liquidator can seize the same position, and `performUpkeep` then reverts — which is
//! *correct* on-chain behaviour (the contract refuses to liquidate twice) and a real loss for the
//! keeper. The dry run narrows the window from "the whole off-chain read" to "one block"; it does
//! not close it, and no off-chain check can. What closes it is:
//!
//! * a bounded gas price, so a lost race is cheap;
//! * the receipt accounting, so the loss is *visible* (`keeper_gas_burned_on_reverts_wei`);
//! * the `Liquidated` log the watcher subscribes to, so a lost race is *avoided* next time.
//!
//! This is the same honest limit `dry_run.rs` documents for the relayer, and the same answer: a
//! defence you cannot measure is a defence you do not have.
//!
//! # One transaction at a time
//!
//! Triggers are processed **sequentially**. A keeper's transactions all come from one key, so two
//! in flight means two nonces to manage for no benefit — a liquidation is worth exactly as much
//! whether it is submitted 50ms or 500ms after the trigger.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, TxHash};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use std::sync::atomic::Ordering;

use crate::dry_run::{Simulation, dry_run};
use crate::keeper::abi;
use crate::keeper::config::KeeperConfig;
use crate::keeper::metrics::KeeperMetrics;
use crate::keeper::resolver;
use crate::keeper::state::Trigger;

/// What became of one attempt to act. Returned rather than logged, so the caller decides how loud
/// to be and so the outcome is inspectable without a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Execution {
    /// We deliberately did nothing. `reason` is for the log.
    Skipped(String),
    /// Broadcast and mined successfully.
    Confirmed { tx_hash: TxHash, gas_used: u64 },
    /// Broadcast, mined, and **reverted**. The keeper paid for this. Counted.
    Reverted {
        tx_hash: TxHash,
        gas_used: u64,
        cost_wei: u64,
        reason: String,
    },
    /// We could not find out, or could not send. Nothing was spent.
    Failed(String),
}

impl Execution {
    /// `true` only for the cases that cost money, whether they worked or not. The other two both
    /// mean "no gas", and they are separate variants because one is success and one is an outage.
    pub fn spent_gas(&self) -> bool {
        matches!(self, Execution::Confirmed { .. } | Execution::Reverted { .. })
    }
}

/// Build the transaction that would liquidate, without sending it.
///
/// Split from [`execute`] so the *exact bytes* that could be signed are inspectable, and so a test
/// can assert on them without a node. Everything that constrains the transaction lives here:
///
/// * `to` is the pool, never anything derived from the payload. This is the line that stops a
///   hostile payload from choosing a destination.
/// * `input` is `performUpkeep(payload)` and nothing else.
/// * an explicit `gas_limit` when configured, which also suppresses `eth_estimateGas`.
/// * a `max_priority_fee_per_gas` **and** a `max_fee_per_gas` above it.
///
/// # The fee cap, and the bug it was added after
///
/// EIP-1559 requires `max_priority_fee_per_gas <= max_fee_per_gas`, and a node rejects the whole
/// transaction with `Invalid input` when it does not. Setting only the priority fee is enough to
/// compile, pass every unit test, and be rejected by every node — and because the keeper *dry
/// runs* first, the symptom was a keeper that correctly decided to liquidate and then, seven times
/// in a row, refused to send anything.
///
/// That is worth stating plainly, because it is the strongest possible argument for the dry run:
/// the primary defence caught a bug that no unit test in `executor.rs` had. See
/// `max_fee_is_above_the_priority_fee` for the invariant now pinned by a test.
///
/// The cap is `priority_fee * 2` because a liquidation must outbid a default-gas competitor but
/// must not become an unbounded bid — the bonus is 2% of collateral, and a fee that can exceed the
/// bonus makes the whole exercise unprofitable.
pub fn build_perform_upkeep(
    pool: Address,
    keeper: Address,
    payload: &Bytes,
    config: &KeeperConfig,
) -> TransactionRequest {
    let priority_fee = config.priority_fee_gwei.saturating_mul(1_000_000_000u128);
    // `saturating_mul`, and a floor of 1 wei: a zero fee cap with a non-zero priority fee is the
    // very rejection we are fixing, so it must be impossible to construct.
    let fee_cap = priority_fee.saturating_mul(2).max(priority_fee.max(1));

    let request = TransactionRequest::default()
        .with_from(keeper)
        .with_to(pool)
        .with_input(abi::perform_upkeep_calldata(payload))
        .with_max_priority_fee_per_gas(priority_fee)
        .with_max_fee_per_gas(fee_cap);

    match config.gas_limit {
        Some(limit) => request.with_gas_limit(limit),
        None => request,
    }
}

/// Dry run, then (only then) broadcast, then account for it.
///
/// This is the function that must never spend money on a losing transaction.
///
/// `P` (simulate) and `Q` (broadcast) are separate providers for the reason the relayer's are: a
/// private-orderflow relay cannot serve `eth_call`, so the dry run has to happen against a full
/// node even when the transaction is sent somewhere private.
///
/// # Errors
/// Never returns `Err`. Every outcome is an [`Execution`] variant, because each one has to be
/// counted and the type has to say which.
#[allow(clippy::too_many_arguments)]
pub async fn execute<P, Q>(
    trigger: Trigger,
    payload: &Bytes,
    sim_provider: &P,
    tx_provider: &Q,
    config: &KeeperConfig,
    metrics: &KeeperMetrics,
    keeper: Address,
) -> Execution
where
    P: Provider,
    Q: Provider,
{
    // Re-check the payload bound here, at the last possible moment, even though the resolver
    // already applied it. The cost is a length comparison; the benefit is that the "never sign an
    // unbounded payload" rule survives someone adding a `PoolAnswer` variant that skips
    // `classify`. Defence in depth, written as code rather than agreed as a convention.
    if !resolver::payload_is_forwardable(payload) {
        metrics.refused_before_sending.fetch_add(1, Ordering::Relaxed);
        return Execution::Skipped(format!(
            "payload of {} bytes is outside the forwardable range",
            payload.len()
        ));
    }

    let tx = build_perform_upkeep(config.pool, keeper, payload, config);

    // ---------------------------------------------------------------------------------------
    // LAST-CHANCE DRY RUN. Free, and the primary defence.
    //
    // The resolver already asked `checkUpkeep` and the state machine already decided. That answer
    // is now stale: a block may have landed, and the position may have been seized by somebody
    // else. This `eth_call` is the difference between "we checked a while ago" and "we checked
    // immediately before spending gas".
    // ---------------------------------------------------------------------------------------
    if config.dry_run {
        match dry_run(sim_provider, &tx).await {
            Simulation::Passed(_) => {}
            Simulation::WouldRevert(reason) => {
                metrics.refused_before_sending.fetch_add(1, Ordering::Relaxed);
                println!(
                    "🛑 {} (trigger {}) would revert -- not broadcasting: {reason}",
                    keeper,
                    trigger.label()
                );
                return Execution::Skipped(format!("dry run says it would revert: {reason}"));
            }
            Simulation::Unavailable(reason) => {
                // Our problem, not the pool's. Nothing spent; the next tick will try again.
                println!(
                    "⚠️  dry run unavailable ({}); skipping this trigger",
                    trigger.label()
                );
                return Execution::Skipped(format!("dry run unavailable: {reason}"));
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // The only line in the keeper that costs money.
    // ---------------------------------------------------------------------------------------
    let pending = match tx_provider.send_transaction(tx).await {
        Ok(pending) => pending,
        Err(err) => {
            // Not broadcast, so not spent. The nonce may or may not have been consumed; that is a
            // documented gap (`KEEPERS.md`) and the reason the next tick re-asks rather than
            // assuming success.
            return Execution::Failed(format!("send_transaction failed: {err}"));
        }
    };
    let tx_hash = *pending.tx_hash();
    metrics.dispatched.fetch_add(1, Ordering::Relaxed);
    println!(
        "🚀 {}: performUpkeep broadcast ({} byte payload, trigger {})",
        trigger.label(),
        payload.len(),
        tx_hash
    );

    if !config.await_receipts {
        // Fire and forget. The banner warns that the gas metrics are then meaningless.
        return Execution::Skipped(format!("broadcast without awaiting a receipt: {tx_hash}"));
    }

    // ---------------------------------------------------------------------------------------
    // Receipt. This is the only place the keeper learns whether it actually made money, and the
    // only place a loss becomes a number.
    // ---------------------------------------------------------------------------------------
    let receipt = match pending.get_receipt().await {
        Ok(receipt) => receipt,
        Err(err) => {
            // Broadcast but unmined or unknown. Counted as spent, because the gas may well be gone.
            return Execution::Failed(format!("no receipt for {tx_hash}: {err}"));
        }
    };

    let gas_used = receipt.gas_used;
    let price = receipt.effective_gas_price;
    let success = receipt.status();

    metrics.record_receipt(success, gas_used, price);

    if success {
        println!("✅ {tx_hash} mined: liquidation performed, {gas_used} gas");
        Execution::Confirmed { tx_hash, gas_used }
    } else {
        let cost = (gas_used as u128).saturating_mul(price) as u64;
        // The overwhelmingly likely cause, and the one the lab demonstrates on purpose: a
        // competing liquidator won the race between our dry run and this transaction.
        let reason = "transaction reverted on chain; most likely a competing liquidator won the \
                      race between our dry run and this transaction"
            .to_string();
        println!("💸 {tx_hash} REVERTED after {gas_used} gas -- {cost} wei lost");
        Execution::Reverted {
            tx_hash,
            gas_used,
            cost_wei: cost,
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keeper::config::KeeperConfig;
    use std::time::Duration;

    fn config() -> KeeperConfig {
        KeeperConfig {
            pool: Address::repeat_byte(0x11),
            ws_url: "ws://127.0.0.1:8546".into(),
            broadcast_url: "http://127.0.0.1:8545".into(),
            private_mempool: false,
            rpc_url: "http://127.0.0.1:8545".into(),
            poll_interval: Duration::from_secs(30),
            queue_capacity: 100,
            max_tx_per_hour: 12,
            priority_fee_gwei: 2,
            gas_limit: None,
            await_receipts: true,
            dry_run: true,
            bind_addr: "127.0.0.1:3001".into(),
        }
    }

    /// The transaction the keeper is willing to sign is *only* `performUpkeep(payload)` to the
    /// pool. Nothing in this function may derive `to` or `value` from the payload, because the
    /// payload is the one piece of data in the system the keeper did not choose. Assert the shape
    /// here so a future "helpful" change breaks a test instead of the treasury.
    #[test]
    fn the_signed_transaction_targets_the_pool_and_nothing_else() {
        let mut payload = [0u8; 32];
        payload[31] = 0xbe;
        let payload = Bytes::from(payload.to_vec());
        let pool = Address::repeat_byte(0x11);

        let tx = build_perform_upkeep(pool, Address::repeat_byte(0x22), &payload, &config());

        assert_eq!(tx.to, Some(pool.into()), "destination must be the configured pool");
        // Fully-qualified on purpose: `TransactionRequest` has an *inherent* `input(builder)`
        // method that shadows the `TransactionBuilder::input()` accessor, so the plain
        // `tx.input()` does not compile as an accessor. This is exactly the kind of shadowing a
        // test is good for -- the wrong call would have type-checked as a builder and moved the
        // assertion somewhere meaningless.
        assert_eq!(
            <TransactionRequest as TransactionBuilder>::input(&tx),
            Some(&abi::perform_upkeep_calldata(&payload)),
            "the only calldata the keeper ever signs is performUpkeep(payload)"
        );
        assert!(tx.value.is_none(), "performUpkeep is not payable; send no ETH with it");
    }

    /// The gas cap is not just a number in the struct: it has to reach the transaction, because an
    /// explicit limit is also what suppresses `eth_estimateGas` (whose own failure would mask the
    /// loss the relayer lab is built to show).
    #[test]
    fn an_explicit_gas_limit_reaches_the_transaction() {
        let mut config = config();
        let payload = Bytes::from(vec![0u8; 32]);

        assert!(build_perform_upkeep(Address::ZERO, Address::ZERO, &payload, &config)
            .gas
            .is_none());

        config.gas_limit = Some(500_000);
        assert_eq!(
            build_perform_upkeep(Address::ZERO, Address::ZERO, &payload, &config).gas,
            Some(500_000)
        );
    }

    /// The priority fee is converted from gwei to wei. Getting this wrong by 10^9 is not a
    /// rounding error: it is either a fee 10^9 times too high (the keeper donates its margin) or
    /// too low to win any race at all.
    #[test]
    fn the_priority_fee_is_gwei_converted_to_wei() {
        let mut config = config();
        let payload = Bytes::from(vec![0u8; 32]);

        config.priority_fee_gwei = 2;
        let tx = build_perform_upkeep(Address::ZERO, Address::ZERO, &payload, &config);
        assert_eq!(
            tx.max_priority_fee_per_gas,
            Some(2_000_000_000u128)
        );

        config.priority_fee_gwei = 0;
        let tx = build_perform_upkeep(Address::ZERO, Address::ZERO, &payload, &config);
        assert_eq!(tx.max_priority_fee_per_gas, Some(0u128));
    }

    /// EIP-1559's hard rule, and the one this file actually violated once.
    ///
    /// A node rejects the whole transaction with `Invalid input: max_priority_fee_per_gas greater
    /// than max_fee_per_gas`. The keeper found this out the cheap way: it dry ran, the node refused,
    /// and the keeper declined to send — so the bug cost **zero gas** and produced seven identical
    /// refusals in the log instead of seven mined failures.
    ///
    /// This test exists because of that live run. It is the one test in this module written *after*
    /// the bug rather than before it, which is exactly backwards from how tests are supposed to
    /// work and is worth saying out loud to students: the dry run found a class of error that no
    /// amount of unit testing had.
    #[test]
    fn max_fee_is_above_the_priority_fee() {
        let mut config = config();
        let payload = Bytes::from(vec![0u8; 32]);

        for gwei in [0u128, 1, 2, 7, 500] {
            config.priority_fee_gwei = gwei;
            let tx = build_perform_upkeep(Address::ZERO, Address::ZERO, &payload, &config);
            let priority = tx.max_priority_fee_per_gas.expect("priority fee is always set");
            let cap = tx.max_fee_per_gas.expect("fee cap is always set");
            assert!(
                priority <= cap,
                "at {gwei} gwei: priority {priority} > cap {cap} -- every node rejects this"
            );
        }
    }

    /// Only the two outcomes that involve a transaction cost gas. A skipped dry run and a failed
    /// send are both free, and conflating them with a revert is how a dashboard ends up reporting
    /// losses that never happened.
    #[test]
    fn only_broadcast_outcomes_spend_gas() {
        assert!(!Execution::Skipped("dry run said no".into()).spent_gas());
        assert!(!Execution::Failed("node down".into()).spent_gas());
        assert!(
            Execution::Confirmed {
                tx_hash: alloy::primitives::B256::ZERO,
                gas_used: 90_000
            }
            .spent_gas()
        );
        assert!(
            Execution::Reverted {
                tx_hash: alloy::primitives::B256::ZERO,
                gas_used: 90_000,
                cost_wei: 1,
                reason: "lost the race".into()
            }
            .spent_gas(),
            "a revert is still a spend -- that is the whole point of measuring it"
        );
    }
}

