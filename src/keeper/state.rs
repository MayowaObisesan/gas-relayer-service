//! The state machine: `(Event) -> (State Logic) -> (Action)`.
//!
//! This module is the answer to the week's exercise, written down as types instead of prose.
//! Everything else in `keeper/` is plumbing around the decisions encoded here.
//!
//! ```text
//!   Trigger          (Event)      watcher.rs    "a Deposit log arrived at block 12"
//!      |
//!      v
//!   decide()         (State)      state.rs      "Idle | Act | Backoff | Refuse"
//!      |
//!      v
//!   Upkeep::action() (Action)     executor.rs   "performUpkeep(payload) -- or nothing at all"
//! ```
//!
//! # The three-state answer, and why there are four
//!
//! The naive shape is a boolean: `checkUpkeep() == true ? liquidate() : nothing`. It has two
//! states, and it is wrong, because "the node did not answer" and "the answer was no" are
//! different facts. A keeper that conflates them either:
//!
//! * acts on nothing during an RPC blip — harmless, but it *silences* the outage, and a silent
//!   keeper is worse than a loud one; or
//! * treats an outage as "nothing to do" and backs off forever, so it stops watching while the
//!   pool bleeds.
//!
//! [`crate::dry_run`] already made this exact argument for the relayer, in
//! `Simulation::Unavailable` versus `Simulation::WouldRevert`. The keeper inherits the
//! distinction, and adds one the relayer never needed:
//!
//! | State | Meaning | Action |
//! |---|---|---|
//! | [`Upkeep::Idle`] | the contract said "no" | log a line, spend nothing |
//! | [`Upkeep::Act`] | the contract said "yes, and here is the payload" | forward the payload |
//! | [`Upkeep::Backoff`] | **we could not find out** | retry later, act on nothing |
//!
//! # State, not control flow
//!
//! The important structural claim is that [`decide`] is a **pure function**. It performs no I/O,
//! takes no `async`, and returns a value. The watcher, resolver and executor are plumbing that can
//! fail, reconnect and be rewritten; this function cannot. That is what makes the interesting half
//! of a keeper testable without a chain — which is why every claim this module makes is proven
//! below by a unit test rather than asserted in a comment.

use alloy::primitives::Bytes;

/// What woke the keeper up.
///
/// Deliberately *not* carrying the interesting data. A trigger says only "the world may have
/// changed"; the condition is re-derived on-chain every single time. A `Trigger` that carried a
/// decoded event payload would tempt the watcher into deciding from log data, and log data is a
/// historical record of a state that has since moved on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A `Deposit` log: a position was opened, or its debt was increased.
    Deposit,
    /// A `Withdraw` log: collateral left a position, shrinking its health factor. The call that
    /// most often pushes a user under water, and the one a balance check alone would never catch.
    Withdraw,
    /// A `Liquidated` log, from *any* liquidator including us.
    ///
    /// The keeper watches this even though it is not "its" event, because a competitor
    /// liquidating the account first is the single most common reason a keeper's next transaction
    /// would revert. Noticing it turns a wasted transaction into a log line. See `watcher.rs`.
    Liquidated,
    /// The polling fallback fired: no event, just the clock.
    ///
    /// This is the trigger that makes the keeper *correct* rather than merely reactive. A log
    /// subscription can be dropped, disconnected, or filtered by an RPC provider that
    /// reconfigures itself; the events that would have saved the pool never arrive. A keeper with
    /// no timer is a keeper whose silence is indistinguishable from health.
    Tick,
    /// A human (or a test) asked the keeper to look now: `POST /upkeep`.
    Manual,
}

impl Trigger {
    /// A short label for logs and metrics. Stable, because it appears as a metric label.
    pub fn label(self) -> &'static str {
        match self {
            Trigger::Deposit => "deposit",
            Trigger::Withdraw => "withdraw",
            Trigger::Liquidated => "liquidated",
            Trigger::Tick => "tick",
            Trigger::Manual => "manual",
        }
    }

    /// `true` for triggers produced by the poll timer rather than by a log.
    ///
    /// A keeper that only ever acts on timer triggers is a *cron* job with an RPC client, and the
    /// distinction is worth a metric: `keeper_triggers_total{kind="poll"}` climbing while
    /// `kind="event"` sits at zero is the exact signature of a subscription that stopped
    /// delivering, with the pool quietly unguarded.
    pub fn is_poll(self) -> bool {
        matches!(self, Trigger::Tick)
    }
}


/// The pool's reply to `checkUpkeep()`, plus the ways that reply can fail to be usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolAnswer {
    /// `checkUpkeep()` returned `(false, _)`. The contract considered its own state: nothing to do.
    NoAction,
    /// `checkUpkeep()` returned `(true, payload)`. `payload` is the contract's decision record.
    Action(Bytes),
    /// The node could not be reached, or answered something that is not this interface at all.
    ///
    /// Distinct from [`PoolAnswer::Malformed`] even though both end in "cannot act": one is a
    /// transient transport problem that will heal on its own, the other is a fault that will sit
    /// there until somebody changes a config file.
    Unavailable(String),
    /// Something answered, but the answer cannot be acted on: a `true` with an empty payload, a
    /// reply that does not match the declared ABI, a pool address with no code.
    Malformed(String),
}

impl PoolAnswer {
    /// A human-readable one-liner for the log line. Never includes key material, and never the
    /// full payload — that is unbounded, and it is attacker-influenceable input.
    pub fn describe(&self) -> String {
        match self {
            PoolAnswer::NoAction => "checkUpkeep() = false".to_string(),
            PoolAnswer::Action(payload) => {
                format!("checkUpkeep() = true, payload = {} bytes", payload.len())
            }
            PoolAnswer::Unavailable(reason) => format!("pool unreachable: {reason}"),
            PoolAnswer::Malformed(reason) => format!("pool reply unusable: {reason}"),
        }
    }
}

/// What the keeper will do about a trigger. This *is* the state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upkeep {
    /// Do nothing. This is the overwhelmingly common case, and it is not an error.
    Idle {
        /// Why we believe there is nothing to do.
        trigger: Trigger,
    },
    /// Send `performUpkeep(payload)`.
    Act {
        trigger: Trigger,
        /// Forwarded **verbatim**. No code path in this module modifies it.
        payload: Bytes,
    },
    /// We could not find out. Retry on the next tick; act on nothing meanwhile.
    ///
    /// This is the state a two-state boolean cannot express, and the reason a boolean is wrong:
    /// `Backoff` must not be counted as "no action needed", because a run of them means the
    /// keeper is *blind*, not idle. Those are opposite operational states and a dashboard has to
    /// be able to tell them apart.
    Backoff {
        trigger: Trigger,
        /// Why the question could not be answered. Not a reason to stop watching.
        reason: String,
    },
    /// The answer exists but is unusable, and paying gas to find that out would be a mistake.
    Refuse {
        trigger: Trigger,
        reason: String,
    },
}

impl Upkeep {
    /// `true` when this state causes a transaction to be broadcast.
    ///
    /// Every other state spends nothing. Keeping this the *only* gate in front of the executor
    /// makes the no-spend property one `if`, visible in one place, rather than a property spread
    /// across four call sites that each have to remember it.
    pub fn should_broadcast(&self) -> bool {
        matches!(self, Upkeep::Act { .. })
    }

    /// The label used for metrics. Deliberately *not* [`Trigger::label`]: debugging a dashboard
    /// needs to know why the keeper stayed idle, and "idle" versus "blind" versus "refusing" are
    /// three different incidents with three different fixes.
    pub fn outcome(&self) -> &'static str {
        match self {
            Upkeep::Idle { .. } => "idle",
            Upkeep::Act { .. } => "act",
            Upkeep::Backoff { .. } => "backoff",
            Upkeep::Refuse { .. } => "refuse",
        }
    }

    /// The payload to forward, if this state has one. `None` for every other state.
    pub fn payload(&self) -> Option<&Bytes> {
        match self {
            Upkeep::Act { payload, .. } => Some(payload),
            _ => None,
        }
    }
}

/// The pure core: turn an answer into a decision.
///
/// No I/O, no `async`, no logging, no side effects. Everything the week asks to be taught about
/// state transitions is decided here and proven by the tests below.
///
/// Two rules are encoded rather than documented:
///
/// 1. **A payload is only ever forwarded.** There is no branch that builds, edits, or defaults
///    one. If a future contributor needs to "just also send the user address", the compiler makes
///    them go find [`PoolAnswer`] and add a variant — a decision, in the open, rather than a
///    shortcut inside a match arm.
/// 2. **Only `true` with a non-empty payload can become [`Upkeep::Act`].** A `true` with nothing
///    to forward would revert in `performUpkeep`, and the keeper would pay to discover that.
pub fn decide(trigger: Trigger, answer: PoolAnswer) -> Upkeep {
    match answer {
        // The contract itself said there is nothing to do. This is the answer we trust most and
        // it should be the cheapest: zero gas, one log line, done.
        PoolAnswer::NoAction => Upkeep::Idle { trigger },

        // The contract asked for something and gave us the means. Forward it untouched.
        PoolAnswer::Action(payload) if !payload.is_empty() => Upkeep::Act { trigger, payload },

        // A `true` with no payload is a contract that wants an action but declined to specify
        // one. There is nothing to send, so sending nothing is correct — and saying so is
        // cheaper than a revert.
        PoolAnswer::Action(payload) => Upkeep::Refuse {
            trigger,
            reason: format!(
                "pool returned upkeepNeeded=true with an empty payload ({} bytes); \
                 performUpkeep would revert",
                payload.len()
            ),
        },

        // Our fault or the network's, but either way not the pool's verdict. Keep watching.
        PoolAnswer::Unavailable(reason) => Upkeep::Backoff { trigger, reason },

        // A fault that will not fix itself: a wrong address, an ABI mismatch, a proxy. Loud, and
        // counted, because it is a configuration bug masquerading as a healthy keeper.
        PoolAnswer::Malformed(reason) => Upkeep::Refuse { trigger, reason },
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic payload: this pool ABI-encodes a single `address` as one 32-byte word.
    fn payload_of(user: u8) -> Bytes {
        let mut word = [0u8; 32];
        word[31] = user;
        Bytes::from(word.to_vec())
    }

    const EVERY_TRIGGER: [Trigger; 5] = [
        Trigger::Deposit,
        Trigger::Withdraw,
        Trigger::Liquidated,
        Trigger::Tick,
        Trigger::Manual,
    ];

    /// The headline claim: the contract says yes, so the keeper forwards, and the payload comes
    /// out the other side **unchanged**. If this ever fails, the keeper has started editing a
    /// decision it was given rather than executing it.
    #[test]
    fn a_true_verdict_forwards_the_contract_payload_verbatim() {
        let payload = payload_of(0xbe);
        let decision = decide(Trigger::Deposit, PoolAnswer::Action(payload.clone()));

        assert!(decision.should_broadcast());
        assert_eq!(decision.outcome(), "act");
        assert_eq!(decision.payload(), Some(&payload));
    }

    /// The exercise's condition, end to end through the pure function: the pool reported
    /// "health factor < 1.0", and the action is a transaction. Nothing else maps to one.
    #[test]
    fn the_underwater_position_is_the_only_broadcasting_outcome() {
        for trigger in EVERY_TRIGGER {
            assert!(
                decide(trigger, PoolAnswer::Action(payload_of(1))).should_broadcast(),
                "{trigger:?} with a payload must act"
            );
            assert!(!decide(trigger, PoolAnswer::NoAction).should_broadcast());
            assert!(!decide(trigger, PoolAnswer::Unavailable("x".into())).should_broadcast());
            assert!(!decide(trigger, PoolAnswer::Malformed("x".into())).should_broadcast());
            assert!(
                !decide(trigger, PoolAnswer::Action(Bytes::new())).should_broadcast(),
                "{trigger:?} with an empty payload must not act"
            );
        }
    }

    /// The distinction the whole module exists for. An outage and a healthy pool both end in "no
    /// transaction", and a two-state boolean reports them as the same event. A run of `Backoff`
    /// is a keeper that is *blind*; a run of `Idle` is a keeper that is *watching*. An operator
    /// must be able to tell them apart from a dashboard, and an alert must be able to fire on the
    /// first without firing on the second.
    #[test]
    fn an_unreachable_pool_is_not_the_same_as_a_healthy_pool() {
        let blind = decide(Trigger::Tick, PoolAnswer::Unavailable("connection refused".into()));
        let healthy = decide(Trigger::Tick, PoolAnswer::NoAction);

        assert_eq!(blind.outcome(), "backoff");
        assert_eq!(healthy.outcome(), "idle");
        assert_ne!(blind, healthy);
        assert!(!blind.should_broadcast());
        assert!(!healthy.should_broadcast());
    }

    /// `Backoff` must stay retry-able: a keeper that gave up after one failed `eth_call` would
    /// stop guarding the pool the first time anvil hiccupped. The state carries its reason and no
    /// terminal marker, and the only thing that consumes it is the next tick.
    #[test]
    fn backoff_is_retryable_and_carries_no_terminal_state() {
        let first = decide(Trigger::Deposit, PoolAnswer::Unavailable("timeout".into()));
        // The next trigger runs the identical pure function: there is no remembered failure to
        // consult, so a recovered node immediately yields a normal answer again.
        let second = decide(Trigger::Deposit, PoolAnswer::Action(payload_of(2)));

        assert_eq!(first.outcome(), "backoff");
        assert!(second.should_broadcast(), "recovery must not require a restart");
    }

    /// A `true` with an empty payload is the case students get wrong. Forwarding it would revert
    /// in `performUpkeep` and cost gas; ignoring it silently would hide a pool that is asking for
    /// an action we cannot express. `Refuse` does neither -- it declines, and says why.
    #[test]
    fn a_true_verdict_with_no_payload_is_refused_rather_than_forwarded() {
        let decision = decide(Trigger::Withdraw, PoolAnswer::Action(Bytes::new()));

        assert!(!decision.should_broadcast());
        assert_eq!(decision.outcome(), "refuse");
        assert_eq!(decision.payload(), None);
        match decision {
            Upkeep::Refuse { reason, .. } => {
                assert!(reason.contains("empty payload"), "reason must name the fault: {reason}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    /// The watcher must not be able to launder an opinion into the state machine. Every trigger is
    /// just a nudge; the answer alone decides, and it decides identically for all of them. A
    /// keeper that treated `Withdraw` as "definitely act" would liquidate healthy accounts.
    #[test]
    fn the_trigger_never_changes_the_decision() {
        for trigger in EVERY_TRIGGER {
            assert_eq!(
                decide(trigger, PoolAnswer::NoAction).outcome(),
                "idle",
                "{trigger:?} must not upgrade a 'no' into an action"
            );
            assert_eq!(decide(trigger, PoolAnswer::Action(payload_of(9))).outcome(), "act");
        }
    }

    /// `Malformed` and `Unavailable` both stop the keeper, and collapsing them would lose the one
    /// fact an operator needs: whether to fix a config file or wait for the network. They must
    /// land in different states even when the text is identical.
    #[test]
    fn transient_and_permanent_faults_land_in_different_states() {
        let transient = decide(Trigger::Tick, PoolAnswer::Unavailable("same text".into()));
        let permanent = decide(Trigger::Tick, PoolAnswer::Malformed("same text".into()));

        assert_ne!(transient.outcome(), permanent.outcome());
        assert_eq!(transient.outcome(), "backoff");
        assert_eq!(permanent.outcome(), "refuse");
    }

    /// `describe()` is what the operator reads and what goes to a log file. The `true` branch must
    /// report the *size* of the payload rather than the payload: the payload is unbounded and
    /// attacker-influenceable data, and a log line is the wrong place for it.
    #[test]
    fn describe_is_safe_to_log() {
        assert_eq!(PoolAnswer::NoAction.describe(), "checkUpkeep() = false");
        assert_eq!(
            PoolAnswer::Action(payload_of(7)).describe(),
            "checkUpkeep() = true, payload = 32 bytes"
        );
        // A huge payload is summarised, not echoed.
        assert_eq!(
            PoolAnswer::Action(Bytes::from(vec![0u8; 10_000])).describe(),
            "checkUpkeep() = true, payload = 10000 bytes"
        );
        assert!(PoolAnswer::Unavailable("dns failure".into())
            .describe()
            .contains("dns failure"));
        assert!(PoolAnswer::Malformed("bad selector".into())
            .describe()
            .contains("bad selector"));
    }

    /// The trigger is still carried into the state, because the *operator* needs it: a keeper that
    /// only ever acts on `manual` triggers has a wired-up subscription that delivers nothing.
    #[test]
    fn the_trigger_is_preserved_for_observability() {
        match decide(Trigger::Withdraw, PoolAnswer::NoAction) {
            Upkeep::Idle { trigger } => assert_eq!(trigger, Trigger::Withdraw),
            other => panic!("expected Idle, got {other:?}"),
        }
        assert_eq!(Trigger::Tick.label(), "tick");
        assert!(Trigger::Tick.is_poll());
        assert!(!Trigger::Deposit.is_poll());
    }
}
