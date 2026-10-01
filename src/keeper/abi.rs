//! The contract surface a keeper is allowed to touch, in one file.
//!
//! # Why the interface is written down at all
//!
//! A keeper is not a general EOA. It is a piece of infrastructure holding a funded key, and the
//! only safe thing it can do is call a **fixed, small, named** set of functions. Every additional
//! function it can reach is a way for a bug — or an attacker who finds one — to make it spend
//! gas. So the surface is declared here, once, and the rest of the keeper is written against
//! these names rather than against calldata somebody handed it.
//!
//! This is the mirror image of the relayer's `FORWARDER_ADDRESS`. The relayer will relay *any*
//! calldata to *one* whitelisted address, because there the flexibility is the feature. A keeper
//! is the opposite: it has no users to serve, so it gets no flexibility at all.
//!
//! # The two-function contract
//!
//! Chainlink's `AutomationCompatibleInterface` is `checkUpkeep()` + `performUpkeep(bytes)`, and
//! it is the reason keepers are tractable:
//!
//! | Function | Kind | Who decides | Keeper's obligation |
//! |---|---|---|---|
//! | `checkUpkeep()` | `view` | the **contract** | Call it. Read `(bool, bytes)`. |
//! | `performUpkeep(bytes)` | state-changing | the **contract** | Send back the `bytes` **verbatim**. |
//!
//! The keeper's entire job is `false -> log a line` and `true -> forward the payload`. It never
//! derives the condition itself, and it never *rewrites* the payload. That is not a shortcut, it
//! is the security property: a payload is a *decision record* produced by the contract, and any
//! keeper-side modification of it is the keeper overruling the only party that has the state.
//!
//! The tempting alternative — keeper calls `healthFactor(user)`, compares against 1.0 itself,
//! then calls `liquidate(user)` — is what this module exists to argue against. That design gives
//! the keeper its own copy of the truth, and a copy of the truth is a thing that can be stale.
//! The cost is one extra `eth_call`; the benefit is that the TOCTOU window shrinks from "the
//! whole off-chain read" to "one block".
//!
//! # What is *not* here
//!
//! No `transfer`, no `approve`, no arbitrary-call fallback, no owner sweep. Every one of those
//! would turn a gas-budgeted watchdog into a wallet with a trigger attached.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent, SolValue};

sol! {
    /// The upkeep pair. These two signatures are the keeper's entire authority on this chain.
    ///
    /// `checkUpkeep` is `view`, so the keeper can call it as often as it likes for free — which
    /// is exactly what the watcher does on every trigger, and what the polling fallback does on
    /// a timer.
    function checkUpkeep() external view returns (bool upkeepNeeded, bytes memory payload);

    /// Consumes a payload that `checkUpkeep` produced. Reverts if the payload is stale, which is
    /// the contract's own last line of defence (see the TOCTOU note in `keeper::executor`).
    function performUpkeep(bytes calldata payload) external returns (address user);

    /// `collateral * 1e18 / debt`; `type(uint256).max` when there is no debt, `0` once
    /// liquidated. Used for **logging only** — never to decide.
    function healthFactor(address user) external view returns (uint256);

    /// The condition, on-chain, for a single user. Also for logging and tests only.
    function isLiquidatable(address user) external view returns (bool);

    /// Monotonic count of liquidations. A keeper that has "performed" 3 upkeeps against a pool
    /// whose `liquidationCount` moved by 1 has the double-spend bug, and this is how you see it.
    function liquidationCount() external view returns (uint256);

    /// Solidity's auto-generated getter for `mapping(address => Position)`. Note it returns the
    /// struct's *members* as separate values, not a struct — a real trip hazard when decoding.
    function positions(address user)
        external
        view
        returns (uint128 collateral, uint128 debt, bool liquidated);

    // --- Lab-only helpers (contracts/LendingPoolMock.sol). Not part of any standard. ---

    function deposit(uint256 collateral, uint256 debt) external;
    function withdraw(uint256 collateral) external;
    /// Withdraws from the *fixed victim* rather than from `msg.sender`. See
    /// `LendingPoolMock.withdrawVictim` for why the lab needs both.
    function withdrawVictim(uint256 collateral) external;
    function repay(uint256 debt) external;
    function armVictim(uint256 collateral, uint256 debt) external;
    function reset(address user) external;
    function victim() external view returns (address);

    /// Position-changing events: the watcher's trigger set, and nothing else.
    ///
    /// `Liquidated` is included on purpose. The keeper must notice when *someone else* wins the
    /// race, or it will keep trying to liquidate an account that is already gone: a keeper that
    /// only watches its own inputs eventually pays for the privilege. See `watcher.rs`.
    event Deposit(address indexed user, uint256 collateral, uint256 debt);
    event Withdraw(address indexed user, uint256 collateral, uint256 debt);
    event Liquidated(
        address indexed user,
        address indexed liquidator,
        uint256 collateral,
        uint256 bonus
    );

    /// The pool's own confirmation, emitted by `performUpkeep`. Seeing this is how you know the
    /// *contract* agrees the liquidation happened, independently of the keeper's own receipt.
    event UpkeepPerformed(address indexed user, bytes payload);
}

/// Topic hashes (topic0 = `keccak256("Event(type,...)")`) for the three events the watcher
/// subscribes to.
///
/// Handing these to the node's log filter is what makes the **node** do the filtering instead of
/// the keeper: on a busy chain that is the difference between a few kilobytes a minute and a
/// firehose that never lets the keeper's queue drain.
///
/// Filtering on topic0 alone is not enough. topic0 hashes the signature but *not* the emitting
/// address, so another contract's identically-shaped `Deposit` would match. The address is the
/// other half of the filter, and `keeper::watcher::watcher_filter` supplies it.
pub const DEPOSIT_TOPIC: B256 = <Deposit as SolEvent>::SIGNATURE_HASH;
pub const WITHDRAW_TOPIC: B256 = <Withdraw as SolEvent>::SIGNATURE_HASH;
pub const LIQUIDATED_TOPIC: B256 = <Liquidated as SolEvent>::SIGNATURE_HASH;

/// Build the calldata for `checkUpkeep()`.
///
/// Wrapped rather than inlined so that **one** function owns "how do we address the pool and what
/// does it return". Every path that wants the condition goes through here, and every path that
/// wants to act goes through [`perform_upkeep_calldata`]. A second way to build either call is a
/// second way to be subtly wrong.
pub fn check_upkeep_calldata() -> Bytes {
    Bytes::from(checkUpkeepCall {}.abi_encode())
}

/// Decode what `checkUpkeep()` returned.
///
/// # Errors
/// Returns a human-readable message when the node's reply is not a well-formed
/// `(bool, bytes)`. A decode failure here is *not* a revert: something answered, and what it
/// answered is not the interface we asked for — a wrong address, a proxy, or an ABI that does not
/// match. The keeper must treat that as "do not act", because the alternative is guessing at a
/// payload it was supposed to be handed.
pub fn decode_check_upkeep(returndata: &[u8]) -> Result<(bool, Bytes), String> {
    // `abi_decode_returns_validate` validates the *values* it decodes but does not require that it
    // consumed the whole buffer, so junk appended after a valid reply is silently ignored. Found
    // by `trailing_garbage_after_a_valid_reply_is_rejected` below, not by reading the docs. The
    // cost of insisting is one comparison; the cost of not insisting is a decoder that accepts
    // anything as long as its first 96 bytes look right.
    //
    // `abi_encode_returns` for `(bool, bytes)` is 32 (bool) + 32 (offset) + 32 (length) + the
    // payload **padded up to a whole number of 32-byte words**. The padding is easy to forget: a
    // 4097-byte payload occupies 4224 bytes on the wire, not 4193, so computing the expected
    // length without it rejects every payload that is not word-aligned. Found by the
    // `an_oversized_payload_is_malformed...` test in `resolver.rs`.
    let decoded = <checkUpkeepCall as SolCall>::abi_decode_returns_validate(returndata)
        .map_err(|e| format!("checkUpkeep() return data does not match the declared ABI: {e}"))?;
    // `sol!` gives the return a named struct, not a bare tuple, so destructure by field name: a
    // change to the ABI becomes a compile error rather than a silent swap of two same-typed
    // values, which a tuple `.0`/`.1` would not catch.
    let (needed, payload) = (decoded.upkeepNeeded, decoded.payload);

    let padded_len = payload.len().div_ceil(32) * 32;
    let expected_len = 96usize.saturating_add(padded_len);
    if returndata.len() != expected_len {
        return Err(format!(
            "checkUpkeep() returned {} bytes but the encoding needs exactly {expected_len} \
             (trailing or missing data)",
            returndata.len()
        ));
    }

    Ok((needed, payload))
}

/// Build the calldata for `performUpkeep(payload)`.
///
/// The payload is forwarded **byte for byte**. There is no reinterpretation, no re-encoding
/// through a local struct, no `Address` round-trip: every one of those would be a place where the
/// keeper substitutes its own idea of the payload for the contract's.
pub fn perform_upkeep_calldata(payload: &Bytes) -> Bytes {
    Bytes::from(performUpkeepCall {
        payload: payload.clone(),
    }
    .abi_encode())
}

/// Build the calldata for a read-only `healthFactor(user)` call. Logging and demos only.
pub fn health_factor_calldata(user: Address) -> Bytes {
    Bytes::from(healthFactorCall { user }.abi_encode())
}

/// The health-factor sentinel: the pool's way of saying "no debt, infinitely healthy".
///
/// Printing `type(uint256).max` as a health factor would be a small lie on the banner, and the
/// banner is the thing a student reads to decide whether the lab is working.
pub const NO_DEBT: &str = "∞ (no debt)";

/// Render a raw `healthFactor` return value for humans.
///
/// `1e18` is health factor 1.0, so this is a fixed-point rendering. It is written by hand with
/// integer arithmetic rather than `format!("{:.4}", x / 1e14)` because the obvious one-liner is
/// *silently wrong*: `U256` division truncates, so `1e18 / 1e14` is `10000`, not `1.0000` — a
/// health factor of exactly 1.0 would print as `10000` and every threshold comparison a student
/// made by eye would be nonsense. Integer part and fraction are computed separately here.
pub fn format_health_factor(raw: U256) -> String {
    if raw == U256::MAX {
        return NO_DEBT.to_string();
    }
    let one = U256::from(10u64).pow(U256::from(18));
    let whole = raw / one;
    // Truncating here is what we want: this is a display value, four decimals, no rounding.
    let fraction = (raw % one) / U256::from(10u64).pow(U256::from(14));
    format!("{whole}.{fraction:04}")
}

/// Read and format a user's health factor. For the startup banner, the `crash` subcommand, and
/// the post-upkeep log line. Never used to decide anything — see the module docs.
///
/// # Errors
/// Propagates transport and decode failures. Callers that must distinguish "unhealthy" from
/// "cannot tell" have to keep the error rather than collapse it to a default.
pub async fn read_health_factor<P: Provider>(
    provider: &P,
    pool: Address,
    user: Address,
) -> Result<String, String> {
    let call = TransactionRequest::default()
        .with_to(pool)
        .with_input(health_factor_calldata(user));
    let out = provider
        .call(call)
        .await
        .map_err(|e| format!("healthFactor({user}) failed: {e}"))?;
    let raw = U256::abi_decode_validate(&out)
        .map_err(|e| format!("healthFactor({user}) reply was not a uint256: {e}"))?;
    Ok(format_health_factor(raw))
}


#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::b256;

    /// A well-formed `checkUpkeep` reply, built with **alloy's own encoder** rather than
    /// hand-assembled 32-byte words.
    ///
    /// This matters more than it looks. Hand-written ABI is precisely the thing these tests would
    /// otherwise be checking, so a subtly wrong test is worse than no test: the deployed
    /// contract's encoding is the one that matters, and asking `sol!` to produce it guarantees the
    /// two agree.
    ///
    /// `SolCall::abi_encode_returns` (not `SolValue::abi_encode` on the struct) is the entry
    /// point: the generated return type implements the *call's* return layout, not the plain
    /// `SolValue` one, and using the wrong one silently produces a different byte string.
    fn reply(needed: bool, payload: &[u8]) -> Bytes {
        let ret = checkUpkeepReturn {
            upkeepNeeded: needed,
            payload: Bytes::from(payload.to_vec()),
        };
        Bytes::from(<checkUpkeepCall as SolCall>::abi_encode_returns(&ret))
    }

    /// A realistic payload: this pool ABI-encodes a single `address` as one 32-byte word.
    fn address_payload(last_byte: u8) -> Vec<u8> {
        let mut word = [0u8; 32];
        word[31] = last_byte;
        word.to_vec()
    }

    /// The topic0 values are constants of the interface, and every deployed pool, every block
    /// explorer and every competing keeper computes the same ones. If any of them changed, the
    /// watcher would silently stop seeing events — and it would look *healthy*, because
    /// `checkUpkeep` would legitimately keep returning `false` forever. Pin all three to the
    /// keccak of the canonical signature.
    #[test]
    fn event_topics_are_the_canonical_keccaks() {
        assert_eq!(
            DEPOSIT_TOPIC,
            b256!("90890809c654f11d6e72a28fa60149770a0d11ec6c92319d6ceb2bb0a4ea1a15")
        );
        assert_eq!(
            WITHDRAW_TOPIC,
            b256!("f279e6a1f5e320cca91135676d9cb6e44ca8a08c0b88342bcdb1144f6511b568")
        );
        assert_eq!(
            LIQUIDATED_TOPIC,
            b256!("1f0c6615429d1cdae0dfa233abf91d3b31cdbdd82c8081389832a61e1072f1ea")
        );
    }

    /// Two events with the same topic0 would make one log filter match the wrong event, and the
    /// keeper would react to a `Withdraw` as if it were a `Liquidated`.
    #[test]
    fn the_three_topics_are_distinct() {
        assert_ne!(DEPOSIT_TOPIC, WITHDRAW_TOPIC);
        assert_ne!(DEPOSIT_TOPIC, LIQUIDATED_TOPIC);
        assert_ne!(WITHDRAW_TOPIC, LIQUIDATED_TOPIC);
    }

    /// `checkUpkeep()` takes no arguments, so its calldata is *exactly* the 4-byte selector
    /// (`cast sig 'checkUpkeep()'` == 0xa13abdad). Anything longer means arguments crept into a
    /// function that has none — the classic sign that the interface and the deployment disagree.
    #[test]
    fn check_upkeep_calldata_is_exactly_the_selector() {
        let call = check_upkeep_calldata();
        assert_eq!(call.len(), 4);
        assert_eq!(&call[..], &[0xa1, 0x3a, 0xbd, 0xad]);
    }

    /// The assertion that stops someone "helpfully" re-encoding the payload later: decode the
    /// call we just built and require the payload to come back byte-identical.
    #[test]
    fn perform_upkeep_forwards_the_payload_byte_for_byte() {
        let payload = Bytes::from(vec![0xabu8; 32]);
        let call = perform_upkeep_calldata(&payload);

        // `abi_decode` consumes the *whole* calldata including the selector, so pass `&call`.
        let decoded = <performUpkeepCall as SolCall>::abi_decode(&call)
            .expect("calldata we just encoded decodes");
        assert_eq!(decoded.payload, payload);
    }

    /// An empty payload is not this pool's business — `performUpkeep` requires exactly 32 bytes —
    /// but the keeper must not *assume* that. `checkUpkeep` may legitimately answer `true` with
    /// an empty payload if a pool chooses a different encoding, and the right response is to
    /// refuse locally (free) rather than to broadcast and pay.
    #[test]
    fn an_actionable_verdict_with_an_empty_payload_still_decodes() {
        let (needed, payload) =
            decode_check_upkeep(&reply(true, &[])).expect("well-formed reply");
        assert!(needed);
        assert!(payload.is_empty());
    }

    /// The `false` reply is the common case, and it is exactly what a *broken* filter also looks
    /// like. It must decode cleanly and it must be cheap — but see `resolver.rs` for why "clean
    /// `false`" still cannot be trusted as evidence of a healthy pool.
    #[test]
    fn a_false_verdict_round_trips() {
        let (needed, payload) =
            decode_check_upkeep(&reply(false, &[])).expect("well-formed reply");
        assert!(!needed);
        assert!(payload.is_empty());
    }

    /// The full happy path: a `true` with a real 32-byte payload survives encode → decode
    /// unchanged, which is the whole basis for forwarding it verbatim.
    #[test]
    fn a_true_verdict_with_a_payload_round_trips() {
        let payload = address_payload(0xbe);
        let (needed, decoded) =
            decode_check_upkeep(&reply(true, &payload)).expect("well-formed reply");
        assert!(needed);
        assert_eq!(decoded, Bytes::from(payload));
    }

    /// Malformed replies must be errors, never a silent `false`. If they degraded to `false`, a
    /// proxy returning junk would make the keeper report a healthy pool forever.
    #[test]
    fn malformed_return_data_is_an_error_not_a_silent_false() {
        assert!(decode_check_upkeep(&[]).is_err());
        assert!(decode_check_upkeep(&[0u8; 3]).is_err());
        assert!(decode_check_upkeep(&[0u8; 31]).is_err());
    }

    /// Trailing garbage is rejected explicitly, by hand.
    ///
    /// This test exists because the *obvious* implementation of the same property fails.
    /// `abi_decode_returns_validate` validates the values it decodes, but it does **not** insist
    /// that it consumed the whole buffer -- so appending junk to a perfectly good reply still
    /// decodes. That is a real property of the library, discovered by writing the test rather than
    /// by reading the docs, and it is why the length check below is explicit.
    ///
    /// The practical risk is small (a node returning junk *after* a valid reply is not a thing
    /// that happens) but the cost of the check is one comparison, and "we decoded the first N
    /// bytes and ignored the rest" is not a sentence anyone wants to defend in a review.
    #[test]
    fn trailing_garbage_after_a_valid_reply_is_rejected() {
        let good = reply(true, &address_payload(1));

        // Sanity: the clean reply does decode.
        assert!(decode_check_upkeep(&good).is_ok());

        // With 32 bytes of junk appended it must not.
        let mut with_garbage = good.to_vec();
        with_garbage.extend_from_slice(&[0xffu8; 32]);
        assert!(
            decode_trailing_garbage_is_rejected(&with_garbage),
            "trailing garbage must not decode as a valid verdict"
        );
    }

    /// The check `decode_check_upkeep` relies on, expressed as a predicate so the test can state
    /// the property rather than re-deriving it.
    fn decode_trailing_garbage_is_rejected(data: &[u8]) -> bool {
        decode_check_upkeep(data).is_err()
    }

    /// The `no debt` sentinel is a UI question, but a wrong answer here is a student debugging a
    /// phantom bug: 2^256-1 read as a health factor looks like a catastrophically underwater
    /// position when it actually means "perfectly safe".
    #[test]
    fn the_health_sentinel_is_not_printed_as_a_number() {
        assert_eq!(format_health_factor(U256::MAX), NO_DEBT);
        // 1.0 == 1e18 -> exactly at the liquidation line, four decimals.
        assert_eq!(format_health_factor(U256::from(10u64).pow(U256::from(18))), "1.0000");
        // 0.85 -> "0.8500", which is how the under-water case reads on screen.
        assert_eq!(
            format_health_factor(U256::from(85u64) * U256::from(10u64).pow(U256::from(16))),
            "0.8500"
        );
        // A liquidated position reports 0 and must not look like a rounding artefact.
        assert_eq!(format_health_factor(U256::ZERO), "0.0000");
    }
}
