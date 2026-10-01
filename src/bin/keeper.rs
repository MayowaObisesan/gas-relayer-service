//! Day 5 lab: **the keeper** — a watchdog that liquidates on its own authority.
//!
//! Where the relayer (`src/main.rs`) is *reactive* — a user POSTs an intent and the relayer decides
//! whether to forward it — the keeper has no users at all. It watches one contract, and when the
//! contract's own `checkUpkeep()` says an account is underwater, it spends its own gas to
//! `performUpkeep()`. Nobody asks it to. That is the whole difference, and the whole lesson.
//!
//! ```text
//!  watch_logs  ─┐
//!               ├─► mpsc<Trigger> ─► worker ─► checkUpkeep() ─► decide() ─► dry run ─► send
//!  watch_ticks ─┘                                          (state.rs)   (executor.rs)
//! ```
//!
//! Run it:
//! ```text
//! anvil                                                  # terminal 1
//! cargo run --bin keeper -- deploy-pool                  # terminal 2: the pool to watch
//! POOL_ADDRESS=<printed> cargo run --bin keeper           # terminal 2: the keeper
//! cargo run --bin keeper -- crash                        # terminal 3: push it under water
//! curl 127.0.0.1:3001/metrics | grep -v '^#'             # terminal 4: the scoreboard
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tokio::sync::mpsc;

use traffic_simulator::config::load_dotenv;
use traffic_simulator::keeper::abi;
use traffic_simulator::keeper::config::KeeperConfig;
use traffic_simulator::keeper::executor::{self, Execution};
use traffic_simulator::keeper::metrics::KeeperMetrics;
use traffic_simulator::keeper::mock;
use traffic_simulator::keeper::resolver;
use traffic_simulator::keeper::state::{PoolAnswer, Trigger, Upkeep, decide};
use traffic_simulator::keeper::watcher;
use traffic_simulator::secure_key::RelayerKey;

/// HTTP-facing state. Note what is **not** here: the private key. It was wiped before `main`
/// finished wiring the provider, exactly as in the relayer.
struct KeeperState {
    /// Hand-off from the watcher tasks to the single sequential worker.
    tx_sender: mpsc::Sender<Trigger>,
    /// Configured queue depth, so `/metrics` can report how full it is.
    queue_capacity: usize,
    counters: Arc<KeeperMetrics>,
    /// Whether the log subscription is *currently* live. Separate from "has it ever been", because
    /// the gap between those two is exactly the condition an operator needs to see.
    is_subscribed: Arc<AtomicBool>,
    dry_run_enabled: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    load_dotenv();
    let config = KeeperConfig::from_env()?;

    // The lab's one-shot subcommands run and exit; the keeper itself is the default.
    match std::env::args().nth(1).unwrap_or_default().as_str() {
        "deploy-pool" => return deploy_pool(&config).await,
        "crash" => return crash(&config).await,
        "status" => return status(&config).await,
        "" => {}
        other => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("usage: cargo run --bin keeper -- [deploy-pool | crash | status]");
            std::process::exit(2);
        }
    }

    // ---------------------------------------------------------------------------------------
    // The God Key, on the stack and then gone.
    //
    // Same discipline as the relayer: loaded, turned into a wallet, explicitly dropped. One live
    // copy remains, inside the provider's signer, and `RelayerKey` guarantees ours is zeroed.
    // ---------------------------------------------------------------------------------------
    let key = match RelayerKey::from_env()? {
        Some(key) => key,
        None => {
            eprintln!(
                "⚠️  PRIVATE_KEY is not set. Generating a THROWAWAY key for this run.\n   \
                 A real keeper's key lives in a KMS/HSM -- it is the only thing standing between \
                 the pool and somebody who finds a bug in this program."
            );
            RelayerKey::generate()
        }
    };
    let keeper_address = key.address();
    let wallet = key.wallet();
    drop(key);

    // ---------------------------------------------------------------------------------------
    // Two providers, for the same reason as the relayer and with more force here.
    //
    //  * `sim_provider` -- a full node. `eth_call` needs real state, and a private-orderflow relay
    //                      cannot serve it.
    //  * `tx_provider`  -- where liquidations are broadcast. A liquidation in the *public* mempool
    //                      is an advertisement: a searcher sees an underwater account and the
    //                      payload naming it, and outbids the keeper for exactly the margin the
    //                      keeper was trying to earn.
    // ---------------------------------------------------------------------------------------
    let sim_provider = ProviderBuilder::new().connect(&config.rpc_url).await?;
    let tx_provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect(&config.broadcast_url)
        .await?;

    // Refuse to run against an address with no code: a keeper watching an empty account decodes
    // nothing, decides nothing, and reports itself healthy forever.
    let has_code = !sim_provider.get_code_at(config.pool).await?.is_empty();
    config.require_real_pool(has_code)?;
    // And that it is *this* pool, not something else that happens to answer.
    mock::pool_is_labelled_correctly(&sim_provider, config.pool).await?;

    let chain_id = sim_provider.get_chain_id().await?;

    // Health factor and liquidation count: printed for the operator, never used to decide.
    let victim_hf = abi::read_health_factor(&sim_provider, config.pool, mock::VICTIM)
        .await
        .unwrap_or_else(|e| format!("unavailable ({e})"));
    let liquidations = read_liquidation_count(&sim_provider, config.pool)
        .await
        .map(|v| v.to_string())
        .unwrap_or_else(|e| format!("unavailable ({e})"));

    // Erase the provider's type only once every direct use of it is done: the concrete type is a
    // stack of filler generics that nothing should have to spell out.
    let sim_for_worker = sim_provider.erased();

    let metrics = Arc::new(KeeperMetrics::default());
    let is_subscribed = Arc::new(AtomicBool::new(false));

    let (tx_sender, mut tx_receiver) = mpsc::channel::<Trigger>(config.queue_capacity);
    let queue_capacity = config.queue_capacity;

    banner(&config, keeper_address, chain_id, &victim_hf, &liquidations);

    // ---------------------------------------------------------------------------------------
    // The two watchers. Both are spawned and never awaited: they are infinite loops, and the
    // worker is the thing that shuts down in an orderly way.
    // ---------------------------------------------------------------------------------------
    let logs_task = {
        let tx = tx_sender.clone();
        let metrics = Arc::clone(&metrics);
        let is_subscribed = Arc::clone(&is_subscribed);
        let ws_url = config.ws_url.clone();
        let pool = config.pool;
        tokio::spawn(async move {
            watcher::watch_logs(&ws_url, pool, tx, metrics, is_subscribed).await
        })
    };
    let ticks_task = {
        let tx = tx_sender.clone();
        let metrics = Arc::clone(&metrics);
        let interval = config.poll_interval;
        tokio::spawn(async move { watcher::watch_ticks(interval, tx, metrics).await })
    };

    // ---------------------------------------------------------------------------------------
    // The worker. Sequential, for the same reason the relayer's is: one key, one nonce, no reason
    // to manage a nonce manager. Each trigger is resolved and acted on *before* the next is read,
    // so a burst of deposits cannot queue up a burst of liquidations.
    // ---------------------------------------------------------------------------------------
    let worker_metrics = Arc::clone(&metrics);
    let worker_config = config.clone();
    let worker = tokio::spawn(async move {
        while let Some(trigger) = tx_receiver.recv().await {
            process(
                trigger,
                &sim_for_worker,
                &tx_provider,
                &worker_config,
                &worker_metrics,
                keeper_address,
            )
            .await;
        }
        println!("📭 trigger queue drained and closed -- worker exiting");
    });

    let state = Arc::new(KeeperState {
        tx_sender,
        queue_capacity,
        counters: metrics,
        is_subscribed,
        dry_run_enabled: config.dry_run,
    });
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/health", get(health_handler))
        .route("/upkeep", post(upkeep_handler))
        .with_state(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .map_err(|e| {
            format!(
                "cannot bind {}: {e}\n   is another keeper already running? \
                 check with: lsof -nP -iTCP:3001 -sTCP:LISTEN",
                config.bind_addr
            )
        })?;
    println!("🚀 keeper metrics on http://{}/metrics\n", config.bind_addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Dropping the last sender closes the channel; the worker drains what is in flight and exits.
    // The two watchers are infinite loops and are simply abandoned when the runtime shuts down.
    drop(state);
    logs_task.abort();
    ticks_task.abort();
    let _ = worker.await;
    println!("👋 keeper stopped cleanly (key material zeroed on drop)");
    Ok(())
}

/// Handle one trigger: ask the pool, decide, and (only if the decision says so) act.
///
/// This is the whole pipeline in one function, and the ordering is the design:
///
/// 1. `checkUpkeep()` — the **contract** decides whether anything is wrong.
/// 2. `decide()` — the pure function turns that answer into one of four states.
/// 3. `should_broadcast()` — the *only* gate in front of spending money.
/// 4. `execute()` — dry run, then send, then account for it.
///
/// Note that step 1 happens on *every* trigger, including a `Deposit` that was obviously healthy.
/// The event is a hint that something changed, not evidence of anything, and re-asking costs one
/// `eth_call` — which is free — while deriving the answer from the log would cost correctness.
async fn process<P, Q>(
    trigger: Trigger,
    sim_provider: &P,
    tx_provider: &Q,
    config: &KeeperConfig,
    metrics: &KeeperMetrics,
    keeper: Address,
) where
    P: Provider,
    Q: Provider,
{
    let answer = resolver::check_upkeep(sim_provider, config.pool, keeper).await;
    let decision = decide(trigger, answer);

    // Count the decision, and maintain the "am I blind right now" gauge. A `backoff` increments
    // it and a success resets it to zero, so the gauge is a *streak* rather than a total — the
    // question it answers is "is the keeper working at this moment", not "has it ever worked".
    match decision.outcome() {
        "idle" => metrics.outcomes[0].fetch_add(1, Ordering::Relaxed),
        "act" => metrics.outcomes[1].fetch_add(1, Ordering::Relaxed),
        "backoff" => {
            metrics.outcomes[2].fetch_add(1, Ordering::Relaxed);
            metrics.consecutive_backoffs.fetch_add(1, Ordering::Relaxed);
            println!(
                "⚠️  could not reach the pool ({}): {}",
                trigger.label(),
                match &decision {
                    Upkeep::Backoff { reason, .. } => reason.as_str(),
                    _ => "unavailable",
                }
            );
            return;
        }
        _ => metrics.outcomes[3].fetch_add(1, Ordering::Relaxed),
    };
    metrics.consecutive_backoffs.store(0, Ordering::Relaxed);

    if !decision.should_broadcast() {
        // The overwhelming majority of triggers end here. One line, no gas.
        println!(
            "·  {} / {} — no action on {}: {}",
            trigger.label(),
            decision.outcome(),
            config.pool,
            match &decision {
                Upkeep::Refuse { reason, .. } => reason.as_str(),
                _ => "the pool says nothing needs doing",
            }
        );
        return;
    }

    // The only branch that can spend money.
    let Some(payload) = decision.payload().cloned() else {
        // Unreachable: `should_broadcast()` is true only for `Act`, which always has a payload.
        // Handled anyway because "unreachable" is a comment, and this guard is free.
        metrics.outcomes[3].fetch_add(1, Ordering::Relaxed);
        return;
    };

    // Log the health factor for context. Read-only, and read *before* the action, so the operator
    // sees the number the contract acted on.
    let hf = abi::read_health_factor(sim_provider, config.pool, mock::VICTIM)
        .await
        .unwrap_or_else(|e| format!("unavailable ({e})"));
    println!(
        "⚠️  checkUpkeep() = TRUE on {} (victim health factor {}) — acting",
        config.pool, hf
    );

    let outcome = executor::execute(
        trigger,
        &payload,
        sim_provider,
        tx_provider,
        config,
        metrics,
        keeper,
    )
    .await;

    match outcome {
        Execution::Confirmed { gas_used, .. } => {
            println!("   → liquidated, {gas_used} gas");
        }
        Execution::Reverted { cost_wei, reason, .. } => {
            println!("   → lost the race: {reason} ({cost_wei} wei)");
        }
        Execution::Skipped(reason) => println!("   → skipped: {reason}"),
        Execution::Failed(reason) => eprintln!("   → failed: {reason}"),
    }
}

/// Startup summary. Prints the keeper's **address**, never its key, and says the two things an
/// operator most needs to know before trusting it: which mempool it is using, and what its poll
/// interval is (because that interval is the keeper's actual reaction-time ceiling).
fn banner(
    config: &KeeperConfig,
    keeper: Address,
    chain_id: u64,
    victim_hf: &str,
    liquidations: &str,
) {
    println!("┌─ liquidation keeper ────────────────────────────────────────");
    println!("│ keeper address   {keeper}");
    println!("│ chain id         {chain_id}");
    println!("│ watching pool    {}", config.pool);
    println!("│ simulate against {}", config.rpc_url);
    println!(
        "│ broadcast to     {}{}",
        config.broadcast_url,
        if config.private_mempool {
            "   ← PRIVATE MEMPOOL (liquidations are not advertised)"
        } else {
            "   ← PUBLIC mempool: a searcher can see and front-run these"
        }
    );
    println!("│ ws subscription  {}", config.ws_url);
    println!(
        "│ poll every       {:?}   ← worst-case reaction time if the socket dies",
        config.poll_interval
    );
    println!("│ priority fee     {} gwei", config.priority_fee_gwei);
    println!(
        "│ tx cap           {}",
        if config.max_tx_per_hour == 0 {
            "UNLIMITED ⚠️  a bug in the trigger filter can drain this key".to_string()
        } else {
            format!("{} per hour", config.max_tx_per_hour)
        }
    );
    println!(
        "│ dry run          {}",
        if config.dry_run {
            "ON  (eth_call before every performUpkeep)"
        } else {
            "OFF ⚠️  the keeper will pay for liquidations that revert"
        }
    );
    if !config.await_receipts {
        println!("│ receipts         OFF ⚠️  gas metrics will be meaningless");
    }
    if let Some(limit) = config.gas_limit {
        println!("│ gas limit        {limit} (explicit: eth_estimateGas is skipped)");
    }
    println!("│ victim health    {victim_hf}    (1.0000 = the liquidation line)");
    println!("│ liquidations     {liquidations}");
    println!("└─────────────────────────────────────────────────────────────");
}

/// `GET /metrics` — the scoreboard. `keeper_gas_burned_on_reverts_wei` must be 0.
async fn metrics_handler(State(state): State<Arc<KeeperState>>) -> Response {
    // `capacity()` is the *remaining* room, so the queue length is the difference.
    let queued = state.queue_capacity.saturating_sub(state.tx_sender.capacity());
    let body = state.counters.render(
        queued,
        state.is_subscribed.load(Ordering::Relaxed),
        state.dry_run_enabled,
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response()
}

/// `GET /health` — liveness only. Says nothing about the node, the pool, or the subscription.
///
/// Deliberately not a readiness signal: a keeper that is running but blind (the subscription dead,
/// the node unreachable) is *alive* and *useless*, and a `/health` that conflated the two would
/// hide the exact failure this design spends so much effort avoiding. The honest liveness signal is
/// `keeper_consecutive_backoffs` in `/metrics`.
async fn health_handler() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// `POST /upkeep` — ask the keeper to look right now.
///
/// The manual trigger, and the reason the keeper is testable from a shell: `curl -X POST
/// 127.0.0.1:3001/upkeep` runs the same pipeline the timer runs, with the same metrics.
///
/// It is a *nudge*, not a command. The keeper still asks the contract, and the contract still
/// decides — which is why this endpoint accepts no arguments. There is no route that says
/// "liquidate this address", because the keeper's authority is one function with one payload.
async fn upkeep_handler(State(state): State<Arc<KeeperState>>) -> Response {
    match state.tx_sender.try_send(Trigger::Manual) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "queued": true,
                "trigger": "manual",
                "note": "the keeper will call checkUpkeep() and act only if the contract says so",
            })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "queued": false,
                "reason": "the trigger queue is full; the poll timer will get there anyway",
            })),
        )
            .into_response(),
    }
}

/// Resolve on Ctrl-C so the worker can drain and the process can exit in order.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\n🛑 shutdown signal: refusing new triggers, draining the queue...");
}

/// Send a state-changing call and wait for its receipt, failing loudly.
///
/// The subcommands are the demo's control surface, so a revert here has to stop the script: a
/// `crash` that silently failed would leave the student watching a healthy pool and concluding the
/// keeper is broken.
async fn send_and_settle<Q: Provider>(
    provider: &Q,
    config: &KeeperConfig,
    from: Address,
    input: Bytes,
    what: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use alloy::network::TransactionBuilder;

    let mut tx = alloy::rpc::types::TransactionRequest::default()
        .with_from(from)
        .with_to(config.pool)
        .with_input(input);
    if let Some(limit) = config.gas_limit {
        tx = tx.with_gas_limit(limit);
    }

    let pending = provider
        .send_transaction(tx)
        .await
        .map_err(|e| format!("{what}: send failed: {e}"))?;
    let receipt = pending
        .get_receipt()
        .await
        .map_err(|e| format!("{what}: no receipt: {e}"))?;

    if !receipt.status() {
        return Err(format!(
            "{what}: transaction REVERTED in block {:?} ({} gas). Is the victim already liquidated? \
             Reset it with: cast send {pool} 'reset(address)(address)' {victim}",
            receipt.block_number,
            receipt.gas_used,
            pool = config.pool,
            victim = mock::VICTIM,
        )
        .into());
    }
    println!("   ✓ {what} mined in block {:?}", receipt.block_number);
    Ok(())
}

/// Read the pool's `liquidationCount()`.
///
/// Built from the `sol!` interface in `keeper::abi` rather than from a hand-written selector: the
/// whole argument for a single declared interface is that there is exactly one place where a
/// signature is written down, and a hand-rolled `keccak("liquidationCount()")[0..4]` here would be
/// a second one that could silently drift.
async fn read_liquidation_count<P: Provider>(provider: &P, pool: Address) -> Result<U256, String> {
    use alloy::network::TransactionBuilder;
    use alloy::sol_types::{SolCall, SolValue};

    let call = alloy::rpc::types::TransactionRequest::default()
        .with_to(pool)
        .with_input(Bytes::from(abi::liquidationCountCall {}.abi_encode()));
    let out = provider
        .call(call)
        .await
        .map_err(|e| format!("liquidationCount() failed: {e}"))?;
    U256::abi_decode_validate(&out).map_err(|e| format!("liquidationCount() reply: {e}"))
}

/// `keeper deploy-pool` — deploy the mock and print its address.
///
/// The bytecode is embedded (`keeper::mock::CREATION_BYTECODE`), so this works on a bare `anvil`
/// with no Solidity toolchain. The printed address is what you export as `POOL_ADDRESS`.
async fn deploy_pool(config: &KeeperConfig) -> Result<(), Box<dyn std::error::Error>> {
    let key = RelayerKey::from_env()?.unwrap_or_else(RelayerKey::generate);
    let deployer = key.address();
    let wallet = key.wallet();
    drop(key);

    println!("📦 deploying LendingPoolMock from {deployer}...");
    let sim = ProviderBuilder::new().connect(&config.rpc_url).await?;
    let tx = ProviderBuilder::new()
        .wallet(wallet)
        .connect(&config.broadcast_url)
        .await?;

    let pool = mock::deploy(&sim, &tx, deployer).await?;
    println!("\n✅ pool deployed at {pool}\n");
    println!("next:");
    println!("   POOL_ADDRESS={pool} cargo run --bin keeper");
    println!("   POOL_ADDRESS={pool} cargo run --bin keeper -- status");
    Ok(())
}

/// `keeper crash` — push the victim under the liquidation line.
///
/// The whole point of the two steps is that **neither looks alarming in isolation**, which is why a
/// keeper cannot decide from logs and has to ask the contract:
///
/// 1. `armVictim(200, 100)` opens a position with health factor **2.0** — comfortably healthy. The
///    pool emits one `Deposit` and the keeper correctly does nothing.
/// 2. `withdraw(121)` removes collateral, leaving 79/100 = **0.79**. A second event arrives, and
///    *this* time the keeper acts.
///
/// Withdrawing exactly 100 would leave health factor precisely 1.0 and still **not** be
/// liquidatable, because the contract compares with `<`, not `<=`. Crossing the line matters, and
/// the arithmetic in the printed output shows it.
async fn crash(config: &KeeperConfig) -> Result<(), Box<dyn std::error::Error>> {
    let key = RelayerKey::from_env()?.unwrap_or_else(RelayerKey::generate);
    let sender = key.address();
    let wallet = key.wallet();
    drop(key);

    let sim = ProviderBuilder::new().connect(&config.rpc_url).await?;
    let tx = ProviderBuilder::new()
        .wallet(wallet)
        .connect(&config.broadcast_url)
        .await?;

    let has_code = !sim.get_code_at(config.pool).await?.is_empty();
    config.require_real_pool(has_code)?;

    println!("🌊 crashing the pool at {}", config.pool);
    println!("   victim           {}", mock::VICTIM);
    println!(
        "   before           {}",
        abi::read_health_factor(&sim, config.pool, mock::VICTIM)
            .await
            .unwrap_or_else(|e| format!("unavailable ({e})"))
    );

    // Step 1: a healthy position. `armVictim` overwrites, so this is idempotent.
    let arm = mock::arm_victim_calldata(mock::HEALTHY_COLLATERAL, mock::HEALTHY_DEBT);
    send_and_settle(&tx, config, sender, arm, "arm a HEALTHY position").await?;
    println!(
        "   armed            collateral {} / debt {}  →  health factor {}",
        mock::HEALTHY_COLLATERAL,
        mock::HEALTHY_DEBT,
        hf_label(mock::HEALTHY_COLLATERAL, mock::HEALTHY_DEBT)
    );
    println!("   (the keeper should log 'no action' here — nothing is wrong yet)");

    // Step 2: withdraw past the 1.0 line.
    let withdraw = mock::HEALTHY_COLLATERAL - mock::UNDERWATER_COLLATERAL + 1;
    let after = mock::HEALTHY_COLLATERAL - withdraw;
    send_and_settle(
        &tx,
        config,
        sender,
        mock::withdraw_victim_calldata(withdraw),
        "withdraw collateral",
    )
    .await?;

    println!(
        "   withdrew         {withdraw}  →  collateral {after} / debt {}  →  health factor {}",
        mock::HEALTHY_DEBT,
        hf_label(after, mock::HEALTHY_DEBT)
    );
    println!(
        "\n✅ the victim is now under water. If the keeper is running it will liquidate within {:?}.",
        config.poll_interval
    );
    println!(
        "   scoreboard: curl -s {}:3001/metrics | grep -E 'act|confirmed|gas_burned'",
        config.bind_addr.rsplit(':').next().unwrap_or("127.0.0.1")
    );
    Ok(())
}

/// `keeper status` — ask the pool what it thinks, without acting.
///
/// The read-only companion to `crash`, and the fastest way to answer "is the pool healthy?" when
/// the keeper is not running. It prints the contract's *own* verdict, so it is a check on the
/// chain rather than on our code.
async fn status(config: &KeeperConfig) -> Result<(), Box<dyn std::error::Error>> {
    let sim = ProviderBuilder::new().connect(&config.rpc_url).await?;
    let has_code = !sim.get_code_at(config.pool).await?.is_empty();
    config.require_real_pool(has_code)?;

    println!("pool              {}", config.pool);
    println!("victim            {}", mock::VICTIM);
    println!(
        "  health factor   {}",
        abi::read_health_factor(&sim, config.pool, mock::VICTIM)
            .await
            .unwrap_or_else(|e| format!("unavailable ({e})"))
    );
    // `from = ZERO`: a read-only call, so the sender only affects any `msg.sender` branch, and this
    // pool's `checkUpkeep` has none.
    match resolver::check_upkeep(&sim, config.pool, Address::ZERO).await {
        PoolAnswer::NoAction => println!("checkUpkeep()     false — nothing to do"),
        PoolAnswer::Action(payload) => println!(
            "checkUpkeep()     TRUE — a liquidation is available ({} byte payload)",
            payload.len()
        ),
        PoolAnswer::Unavailable(reason) => {
            println!("checkUpkeep()     unavailable: {reason}")
        }
        PoolAnswer::Malformed(reason) => {
            println!("checkUpkeep()     unusable reply: {reason}")
        }
    }
    println!(
        "liquidations      {}",
        read_liquidation_count(&sim, config.pool)
            .await
            .map(|v| v.to_string())
            .unwrap_or_else(|e| format!("unavailable ({e})"))
    );
    Ok(())
}

/// Render a health factor from plain integers, so `crash` can print what it is *about* to create
/// before the transaction lands.
fn hf_label(collateral: u128, debt: u128) -> String {
    abi::format_health_factor(
        U256::from(collateral) * U256::from(10u64).pow(U256::from(18)) / U256::from(debt),
    )
}
