//! # Day 5 — "The Keeper: From Listening to Acting"
//!
//! Days 2–4 built a **relayer**: something that watches a queue of user intents and forwards
//! them. A relayer is *reactive to requests*. A **keeper** is something else entirely — it holds
//! a funded key, it watches a contract with no user in the loop, and it decides on its own to
//! spend money. Nobody asks it to. That difference is the whole lesson.
//!
//! ## The pipeline the week teaches
//!
//! ```text
//!   (Event)   watcher.rs    a Deposit/Withdraw log, or a clock tick
//!      |
//!   (State)   state.rs      decide(): Idle | Act | Backoff | Refuse
//!      |
//!   (Action)  executor.rs   performUpkeep(payload) -- or nothing at all
//! ```
//!
//! Three questions separate those boxes, and they are the questions of the exercise:
//!
//! * **Event — what woke me up?** Anything. The trigger carries no opinion, only a nudge.
//! * **State — should I act?** Not "is the health factor below 1.0?", which the *keeper* would
//!   have to compute and could get wrong. The contract's own `checkUpkeep()` answers, and it
//!   answers with a **payload**: a decision record the keeper is obliged to forward unmodified.
//! * **Action — what do I send?** `performUpkeep(payload)`, and only ever after a second,
//!   free `eth_call` confirms it would not revert.
//!
//! ## The four modules, and the one number
//!
//! | Module | Job | Fails how |
//! |---|---|---|
//! | [`watcher`] | the log subscription **and** a poll timer | reconnects, or stops noticing |
//! | [`state`] | the pure decision function | cannot fail: no I/O |
//! | [`resolver`] | `checkUpkeep()` → a [`PoolAnswer`](state::PoolAnswer) | `Backoff` vs `Refuse` |
//! | [`executor`] | dry run, sign, broadcast, receipt | reverts, and we measure it |
//!
//! The number that matters is `keeper_gas_burned_on_reverts_wei`, exactly as
//! `relayer_gas_burned_on_reverts_wei` is the number that matters for the relayer. A keeper that
//! pays for a reverting `performUpkeep` is donating money to the pool it was protecting, and the
//! only way to know whether it is doing that is to count it. [`metrics`] exists so the number
//! cannot be a guess.
//!
//! ## Why the two-function interface is the design
//!
//! `checkUpkeep()` / `performUpkeep(bytes)` is not a convenience — it is what makes a keeper
//! *safe to run*. The contract owns the condition; the keeper owns only the timing. Read
//! [`abi`] for the argument in full, because the alternative ("the keeper reads `healthFactor`
//! and compares it to 1.0 itself") is what most student projects do, and it reintroduces exactly
//! the class of stale-state bug the relayer spent Day 4 learning about.
//!
//! ## What is deliberately missing
//!
//! A production keeper also needs: multiple pools, a gas budget per period, an HSM signer,
//! backfill on restart (scan N blocks for accounts already underwater), and profitability
//! accounting (the bonus must exceed the gas). Each is a `KEEPERS.md` gap entry rather than code
//! here, and each is named in the checklist. The lab builds the skeleton that is *correct*, so the
//! gaps are visible as gaps rather than hidden inside a heuristic.

pub mod abi;
pub mod config;
pub mod executor;
pub mod metrics;
pub mod mock;
pub mod resolver;
pub mod state;
pub mod watcher;

pub use config::KeeperConfig;
pub use metrics::KeeperMetrics;
pub use state::{PoolAnswer, Trigger, Upkeep, decide};
