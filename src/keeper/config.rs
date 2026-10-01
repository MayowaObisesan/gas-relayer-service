//! Keeper configuration, loaded from the environment.
//!
//! Built on [`crate::config`]'s helpers rather than reimplementing them, and for the same reason
//! that module documents: a variable that is **set but wrong** must be a hard error, because
//! silently defaulting a safety knob is how a keeper ends up pointing at the wrong chain, or
//! broadcasting to a public mempool it thought was private.
//!
//! # The two settings that are not knobs
//!
//! Two fields exist that nobody should be tempted to tune:
//!
//! * **`POOL_ADDRESS` defaults to the zero address, and the keeper refuses to start on it.**
//!   The zero address has no code, so `checkUpkeep()` against it "succeeds" and returns
//!   `false` forever: a keeper that watches nothing and reports perfect health. That is the
//!   silent-failure mode this repo cares most about, so the keeper names it out loud at startup
//!   instead of idling against it.
//! * **`KEEPER_MAX_TX_PER_HOUR` defaults to a small number, not to "unlimited".** A keeper with a
//!   bug in its trigger filter is a loop that spends money; the cap bounds the damage to one hour
//!   of gas, and the counter that trips it is exported on `/metrics`.
//!
//! # No key in this struct
//!
//! Same rule as the relayer, same reason: `Config` gets `Debug`-printed and cloned into closures.
//! The key belongs to [`crate::secure_key::RelayerKey`], which is loaded in `main`, turned into a
//! wallet, and dropped before the first event is ever watched.

use std::time::Duration;

use alloy::primitives::Address;

use crate::config::{DEFAULT_RPC_URL, ZERO_ADDRESS, env_bool, env_opt, env_or, env_parse};

/// Default poll interval. Long enough to be free, short enough that a dropped subscription costs
/// at most one interval of blindness.
pub const DEFAULT_POLL_SECS: u64 = 30;

/// Default cap on broadcasts per hour. Deliberately low: the lab should never need more than a
/// handful, so a runaway is unmistakable.
pub const DEFAULT_MAX_TX_PER_HOUR: u32 = 12;

/// Default HTTP port for the keeper's metrics endpoint. One above the relayer's 3000 so both can
/// run at once — which they must, because a student is meant to compare the two scoreboards.
pub const DEFAULT_KEEPER_BIND_ADDR: &str = "127.0.0.1:3001";

/// Largest payload the keeper will forward, in bytes. Mirrors
/// [`crate::keeper::resolver::MAX_PAYLOAD_BYTES`]; re-declared as a config-visible default so the
/// banner can print the number the keeper is actually enforcing.
pub const DEFAULT_MAX_PAYLOAD_BYTES: usize = 4 * 1024;

/// Everything the keeper needs except the key.
#[derive(Clone, Debug)]
pub struct KeeperConfig {
    /// The pool to watch. Must have code on chain; verified at startup.
    pub pool: Address,
    /// Full node used for `eth_call` (`checkUpkeep`, the dry run, `healthFactor` reads).
    pub rpc_url: String,
    /// Where transactions are broadcast. Equal to `rpc_url` unless a private endpoint is set.
    pub broadcast_url: String,
    /// `true` when `PRIVATE_RPC_URL` / `FLASHBOTS_RPC_URL` is set.
    ///
    /// This matters *more* for a keeper than for a relayer. A liquidation in the public mempool
    /// is an advertisement: every searcher sees an account with health factor 0.85 and a payload
    /// naming it, and bids the gas fee up until the keeper's transaction loses. The keeper's
    /// entire revenue is the difference between the bonus and the gas, and a searcher takes
    /// exactly that difference.
    pub private_mempool: bool,
    /// WebSocket endpoint for the log subscription. Falls back to `rpc_url` with the scheme
    /// rewritten, which is correct for anvil.
    pub ws_url: String,
    /// How often to ask `checkUpkeep()` even when no event arrived. The safety net.
    pub poll_interval: Duration,
    /// Depth of the in-process trigger queue.
    ///
    /// A keeper's queue holds *nudges*, not intents, so this can stay small: a full queue drops a
    /// trigger, and the poll timer will re-ask regardless. Back-pressure here is a courtesy to the
    /// watcher, not a correctness mechanism.
    pub queue_capacity: usize,
    /// Hard cap on broadcasts per rolling hour. `0` disables the cap (and the banner says so).
    pub max_tx_per_hour: u32,
    /// EIP-1559 priority fee in wei. A liquidation is a race, so this sits above the node's
    /// suggested value — but it is a fixed number, not a multiplier on an unbounded base fee.
    pub priority_fee_gwei: u128,
    /// Explicit gas limit for `performUpkeep`. `None` means "let the node estimate".
    ///
    /// `performUpkeep` touches storage, so estimation is the honest choice and the default. The
    /// field exists because the relayer showed what happens when a gas cap is the *only* thing
    /// bounding your worst case.
    pub gas_limit: Option<u64>,
    /// Wait for receipts so gas actually spent can be counted. Turning this off makes
    /// `keeper_gas_burned_on_reverts_wei` meaningless, so the banner warns.
    pub await_receipts: bool,
    /// **Lab switch.** `KEEPER_DRY_RUN=0` skips the pre-broadcast `eth_call` and buys reverting
    /// transactions, exactly like the relayer's `DRY_RUN=0`.
    pub dry_run: bool,
    /// HTTP bind address for `/metrics` and `/health`.
    pub bind_addr: String,
}

impl KeeperConfig {
    /// Read the whole configuration from the environment.
    ///
    /// # Errors
    /// Returns a human-readable message if a variable is present but unparseable. Note it does
    /// **not** error on the zero-address pool: that check needs a node, so it lives in
    /// [`Self::require_real_pool`] and runs after the provider connects.
    pub fn from_env() -> Result<Self, String> {
        let rpc_url = env_or("RPC_URL", DEFAULT_RPC_URL);

        let private = env_opt("PRIVATE_RPC_URL").or_else(|| env_opt("FLASHBOTS_RPC_URL"));
        let (broadcast_url, private_mempool) = match &private {
            Some(url) => (url.clone(), true),
            None => (rpc_url.clone(), false),
        };

        let pool_raw = env_or("POOL_ADDRESS", ZERO_ADDRESS);
        let pool = pool_raw
            .parse::<Address>()
            .map_err(|e| format!("POOL_ADDRESS={pool_raw:?} is not an address: {e}"))?;

        let ws_url = env_opt("WS_RPC_URL").unwrap_or_else(|| derive_ws_url(&rpc_url));

        let poll_secs = env_parse("KEEPER_POLL_SECS", DEFAULT_POLL_SECS)?;
        if poll_secs == 0 {
            return Err(
                "KEEPER_POLL_SECS=0 would disable the only trigger that does not depend on the \
                 log subscription. Use a large interval if you mean 'rarely'."
                    .to_string(),
            );
        }

        let priority_fee_gwei = env_parse("KEEPER_PRIORITY_FEE_GWEI", 2u128)?;

        Ok(Self {
            pool,
            ws_url,
            broadcast_url,
            private_mempool,
            rpc_url,
            poll_interval: Duration::from_secs(poll_secs),
            queue_capacity: env_parse("KEEPER_QUEUE_CAPACITY", 100)?,
            max_tx_per_hour: env_parse("KEEPER_MAX_TX_PER_HOUR", DEFAULT_MAX_TX_PER_HOUR)?,
            priority_fee_gwei,
            gas_limit: env_opt("KEEPER_GAS_LIMIT")
                .map(|raw| {
                    raw.parse::<u64>()
                        .map_err(|e| format!("KEEPER_GAS_LIMIT={raw:?} is not a number: {e}"))
                })
                .transpose()?,
            await_receipts: env_bool("KEEPER_AWAIT_RECEIPTS", true)?,
            dry_run: env_bool("KEEPER_DRY_RUN", true)?,
            bind_addr: env_or("KEEPER_BIND_ADDR", DEFAULT_KEEPER_BIND_ADDR),
        })
    }

    /// Refuse to run against an address with no code.
    ///
    /// The zero address accepts a call and returns zero bytes, so a keeper pointed at it decodes
    /// nothing, decides nothing, and reports itself healthy forever. The relayer has the same trap
    /// (`FORWARDER_ADDRESS`) and only *warns*, because its zero-address default is a deliberate
    /// lab convenience. A keeper's default is not: there is no useful thing for a keeper to do
    /// against an empty account, so this one is fatal.
    ///
    /// # Errors
    /// Returns a message naming the address and how to deploy the mock.
    pub fn require_real_pool(&self, has_code: bool) -> Result<(), String> {
        if has_code {
            return Ok(());
        }
        Err(format!(
            "POOL_ADDRESS {} has NO CODE on this chain.\n   \
             checkUpkeep() against an empty account returns nothing, so the keeper would watch \
             nothing forever and still report itself healthy.\n   \
             deploy the mock:  cargo run --bin keeper -- deploy-pool",
            self.pool
        ))
    }
}

/// Guess the WebSocket URL from the HTTP one.
///
/// `http://` → `ws://`, `https://` → `wss://`. The port is deliberately **not** guessed: anvil
/// and geth both use 8545 for HTTP and 8546 for WS, and silently rewriting a port would produce a
/// keeper that subscribes to nothing while reporting a healthy subscription. Better to leave it and
/// let the connect fail loudly with the URL printed, making `WS_RPC_URL` the obvious fix.
pub fn derive_ws_url(rpc_url: &str) -> String {
    if let Some(rest) = rpc_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = rpc_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        // Already a `ws://`/`wss://` URL, or something unrecognised. Use it as given and let the
        // connection error speak for itself.
        rpc_url.to_string()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The scheme rewrite has to be right in both directions, because getting it wrong produces a
    /// subscription error at startup rather than a wrong answer later — which is the good case,
    /// but only if it is actually an error and not a connection to something else.
    #[test]
    fn the_ws_url_is_derived_from_the_http_url() {
        assert_eq!(
            derive_ws_url("http://127.0.0.1:8545"),
            "ws://127.0.0.1:8545"
        );
        assert_eq!(
            derive_ws_url("https://eth.example.com/v2"),
            "wss://eth.example.com/v2"
        );
        // Already a websocket URL: passed through untouched, not double-prefixed.
        assert_eq!(
            derive_ws_url("ws://127.0.0.1:8546"),
            "ws://127.0.0.1:8546"
        );
    }

    /// The port is *not* rewritten. Silently swapping 8545 for 8546 would look helpful and be
    /// actively dangerous: it would hide the fact that the operator's `WS_RPC_URL` is unset, which
    /// is the thing they need to know.
    #[test]
    fn the_port_is_left_alone() {
        let derived = derive_ws_url("http://127.0.0.1:8545");
        assert!(derived.contains("8545"));
        assert!(!derived.contains("8546"));
    }

    /// The zero-address pool is the failure this whole repo is built to avoid: a keeper that
    /// watches an empty account decodes nothing, decides nothing, and looks perfectly healthy.
    /// It must be fatal, and the error must tell you how to fix it.
    #[test]
    fn a_pool_with_no_code_is_fatal_and_says_how_to_fix_it() {
        let config = KeeperConfig {
            pool: Address::ZERO,
            ws_url: "ws://127.0.0.1:8545".into(),
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
            bind_addr: DEFAULT_KEEPER_BIND_ADDR.into(),
        };

        let err = config
            .require_real_pool(false)
            .expect_err("a pool with no code must be rejected");
        assert!(err.contains("NO CODE"), "the error must name the fault: {err}");
        assert!(
            err.contains("deploy-pool"),
            "the error must be actionable: {err}"
        );

        // And a pool *with* code is accepted, obviously.
        assert!(config.require_real_pool(true).is_ok());
    }
}
