//! The Resolver: the "brain" — but a brain that only *asks questions*.
//!
//! One job: call `checkUpkeep()`, and turn whatever comes back into a [`PoolAnswer`]. No logging,
//! no metrics, no signing, no retry policy. Everything downstream can be reasoned about without
//! this file, and this file can be reasoned about without anything downstream.
//!
//! # The `from` field is load-bearing
//!
//! The call is sent with `from = keeper_address`. On any contract that branches on `msg.sender`
//! — a real `checkUpkeep` does not, but a real pool's *access control* often does — an unset
//! `from` defaults to the zero address, which may not be a permitted caller. It is the same reason
//! the relayer sets `from` before its dry run: `eth_call` runs against state, and state rules are
//! evaluated for a sender.
//!
//! # Triage: three outcomes, not two
//!
//! ```text
//!   node answered with our ABI  ->  NoAction | Action
//!   node answered with junk     ->  Malformed   (our config is wrong; will not heal)
//!   node did not answer         ->  Unavailable (the network; probably will heal)
//! ```
//!
//! The second and third branches can carry identical text and mean opposite things
//! operationally: one is fixed by editing `.env`, the other by waiting.
//! [`crate::dry_run`] makes the same split for the relayer, and it is the most valuable habit in
//! this repository: **never collapse "I could not find out" into "there is nothing to do."**

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;

use crate::keeper::abi;
use crate::keeper::state::PoolAnswer;

/// Largest payload we are willing to forward. A real pool's payload is a few hundred bytes;
/// anything past this is a bug, a hostile contract, or a broken proxy, and forwarding it would
/// mean broadcasting unbounded attacker-chosen data at our own expense.
///
/// The keeper's analogue of the relayer's `MAX_CALLDATA_BYTES`, and it earns its keep for the
/// same reason: an unbounded value on the path to a signed transaction is unbounded gas.
pub const MAX_PAYLOAD_BYTES: usize = 4 * 1024;

/// Ask the pool whether anything needs doing.
///
/// # Errors
/// Never returns `Err`. Every failure is a [`PoolAnswer`] variant, because the caller has to
/// *count* the failure and the type has to carry which kind it was.
pub async fn check_upkeep<P: Provider>(
    provider: &P,
    pool: Address,
    keeper: Address,
) -> PoolAnswer {
    let call = TransactionRequest::default()
        .with_from(keeper)
        .with_to(pool)
        .with_input(abi::check_upkeep_calldata());

    let returndata = match provider.call(call).await {
        Ok(bytes) => bytes,
        Err(err) => {
            // The same triage the relayer uses: a node that *answered* with a revert is telling
            // us the call is wrong (wrong address, or a pool that reverts on read), while a
            // transport failure says nothing about the call at all. Neither is `NoAction`.
            if let Some(payload) = err.as_error_resp() {
                return PoolAnswer::Unavailable(format!(
                    "node rejected the call (code {}): {}{}",
                    payload.code,
                    payload.message,
                    payload
                        .as_revert_data()
                        .map(|d| format!(" revert_data=0x{}", alloy::hex::encode(&d)))
                        .unwrap_or_default()
                ));
            }
            return PoolAnswer::Unavailable(err.to_string());
        }
    };

    // Past this point something answered, so every failure is a *permanent* one: our address, our
    // ABI, or the deployment. Reporting `Unavailable` here would tell the operator to wait, which
    // would be a lie about a misconfiguration.
    classify(returndata)
}

/// Turn a `checkUpkeep` reply into a verdict, with no I/O involved.
///
/// Split out from [`check_upkeep`] so the triage is a testable pure function, and so the transport
/// path stays about transport.
///
/// # Errors
/// Never returns `Err`; see [`check_upkeep`].
pub fn classify(returndata: Bytes) -> PoolAnswer {
    // An empty reply is the signature of a call to an address with no code: the EVM returns
    // success and zero bytes. Treating that as `NoAction` would be the worst bug in this file --
    // a keeper pointed at the wrong address, idling forever, reporting perfect health.
    if returndata.is_empty() {
        return PoolAnswer::Malformed(
            "pool returned no data at all -- most likely an address with no code (an empty \
             account returns success and 0 bytes)"
                .to_string(),
        );
    }

    match abi::decode_check_upkeep(&returndata) {
        Ok((false, _)) => PoolAnswer::NoAction,
        Ok((true, payload)) => {
            if !payload_is_forwardable(&payload) {
                return PoolAnswer::Malformed(format!(
                    "payload is {} bytes, outside the 1..={MAX_PAYLOAD_BYTES} range we forward; \
                     refusing",
                    payload.len()
                ));
            }
            PoolAnswer::Action(payload)
        }
        // A `false` with a non-empty payload is legal: "nothing to do, and here is context you did
        // not need". The contract's boolean is the answer, and we do not get to disagree with it
        // because the extra data looks interesting.
        Err(reason) => PoolAnswer::Malformed(reason),
    }
}

/// `true` when a payload is the size this keeper is willing to forward.
///
/// Exposed as a function so the *executor* can re-check the same condition before signing. A limit
/// enforced only at read time is one refactor away from not being enforced at all; enforced in
pub fn payload_is_forwardable(payload: &Bytes) -> bool {
    !payload.is_empty() && payload.len() <= MAX_PAYLOAD_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use abi::{checkUpkeepCall, checkUpkeepReturn};
    use alloy::sol_types::SolCall;

    /// A well-formed `false`: `abi.encode(false, "")`.
    ///
    /// Encoded with alloy's own `sol!` types rather than hand-assembled words, so the test cannot
    /// disagree with the deployed contract's ABI. Hand-written dynamic-offset encoding is easy to
    /// get subtly wrong, and a test that encodes the wrong thing tests nothing.
    fn reply_false() -> Bytes {
        reply(false, &[])
    }

    /// A well-formed reply with an explicit payload: `abi.encode(needed, payload)`.
    fn reply(needed: bool, payload: &[u8]) -> Bytes {
        let ret = checkUpkeepReturn {
            upkeepNeeded: needed,
            payload: Bytes::from(payload.to_vec()),
        };
        Bytes::from(<checkUpkeepCall as SolCall>::abi_encode_returns(&ret))
    }

    /// A 32-byte payload: this pool ABI-encodes a single `address` as one word.
    fn address_payload(last_byte: u8) -> Vec<u8> {
        let mut word = [0u8; 32];
        word[31] = last_byte;
        word.to_vec()
    }

    /// A well-formed `true` with a 32-byte payload, plus the payload for comparison.
    fn reply_true(last_byte: u8) -> (Bytes, Bytes) {
        let payload = address_payload(last_byte);
        let bytes = Bytes::from(payload.clone());
        (reply(true, &payload), bytes)
    }

    /// The bug this file exists to prevent. Calling an address with no code returns success and
    /// zero bytes. A resolver that read that as `NoAction` would leave a keeper pointed at the
    /// wrong address sitting at "healthy, nothing to do" forever: no error, no alert, no
    /// liquidation, and a dashboard full of reassuring zeros.
    #[test]
    fn an_empty_reply_is_never_read_as_nothing_to_do() {
        match classify(Bytes::new()) {
            PoolAnswer::Malformed(reason) => {
                assert!(
                    reason.contains("no code"),
                    "the reason must point at the likely cause: {reason}"
                );
            }
            other => panic!("empty reply must be Malformed, got {other:?}"),
        }
    }

    /// Both well-formed verdicts, classified by the contract's own boolean, with the payload
    /// surviving intact on the `true` path.
    #[test]
    fn well_formed_replies_are_classified_by_the_contract() {
        assert_eq!(classify(reply_false()), PoolAnswer::NoAction);

        let (encoded, expected_payload) = reply_true(0xbe);
        assert_eq!(classify(encoded), PoolAnswer::Action(expected_payload));
    }

    /// A `false` that carries a payload is still `NoAction`. The contract's boolean is the
    /// answer; the keeper does not get to disagree because the extra data looks interesting.
    #[test]
    fn a_false_verdict_with_a_payload_is_still_no_action() {
        assert_eq!(
            classify(reply(false, &[0x11u8; 32])),
            PoolAnswer::NoAction
        );
    }

    /// A `true` with an empty payload is refused **here**, at the resolver, and the shared
    /// predicate says why.
    ///
    /// The doc comment on this test originally claimed the resolver returns `Action` and
    /// `state::decide` refuses it one layer up. That is not what the code does, and the code is
    /// right: `payload_is_forwardable` is the single definition of "a payload we will send", and
    /// checking it in exactly one place means there is no path that can construct an `Action` the
    /// executor would then have to re-check. `decide` still refuses empty payloads — that is
    /// defence in depth against a *future* variant added without consulting the predicate — but
    /// the first line of defence is the cheaper one: never classify it as an action at all.
    #[test]
    fn a_true_with_an_empty_payload_is_refused_by_the_forwardable_predicate() {
        match classify(reply(true, &[])) {
            PoolAnswer::Malformed(reason) => {
                assert!(reason.contains("0 bytes"), "reason must name the size: {reason}");
            }
            other => panic!("an empty payload must not become an Action, got {other:?}"),
        }
        // And the predicate itself agrees.
        assert!(!payload_is_forwardable(&Bytes::new()));
    }

    /// An oversized payload is refused at the boundary, before it can reach a signed transaction.
    /// The limit is the keeper's bound on attacker-influenceable data on the path to gas.
    #[test]
    fn an_oversized_payload_is_malformed_not_a_very_large_action() {
        let big = Bytes::from(vec![0u8; MAX_PAYLOAD_BYTES + 1]);
        assert!(!payload_is_forwardable(&big));

        // And at the ABI level: a `true` whose payload is one byte over the limit.
        match classify(reply(true, &vec![0u8; MAX_PAYLOAD_BYTES + 1])) {
            PoolAnswer::Malformed(reason) => {
                assert!(reason.contains("outside"), "reason must name the limit: {reason}");
            }
            other => panic!("oversized payload must be Malformed, got {other:?}"),
        }
    }

    /// Exactly at the limit is allowed. A limit that is off by one in the strict direction rejects
    /// legitimate payloads; the test pins the boundary from both sides.
    #[test]
    fn the_payload_limit_is_inclusive() {
        assert!(payload_is_forwardable(&Bytes::from(vec![0u8; MAX_PAYLOAD_BYTES])));
        assert!(!payload_is_forwardable(&Bytes::from(vec![0u8; MAX_PAYLOAD_BYTES + 1])));
        assert!(!payload_is_forwardable(&Bytes::new()));
    }

    /// Garbage that is not our ABI is `Malformed` (a configuration problem), never `NoAction`.
    #[test]
    fn junk_replies_are_malformed_not_silently_idle() {
        // Truncated, too short to be a bool.
        assert!(matches!(
            classify(Bytes::from(vec![0xff; 10])),
            PoolAnswer::Malformed(_)
        ));
        // A full 32-byte word of junk is a valid `bool`-shaped word only if it is 0 or 1; this is
        // neither, and the strict decoder must say so.
        assert!(matches!(
            classify(Bytes::from(vec![0xff; 96])),
            PoolAnswer::Malformed(_)
        ));
    }
}

