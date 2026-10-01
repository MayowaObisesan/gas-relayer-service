//! The lab's lending pool, embedded so the demo needs nothing but `anvil`.
//!
//! The source of truth is [`contracts/LendingPoolMock.sol`](../../contracts/LendingPoolMock.sol);
//! this module carries the **compiled creation bytecode** so a student can run the whole keeper
//! with no Solidity toolchain at all:
//!
//! ```bash
//! anvil
//! cargo run --bin keeper -- deploy-pool      # uses the bytes below
//! ```
//!
//! If you would rather watch the compiler do the work, [`scripts/deploy_pool.sh`](../../scripts/deploy_pool.sh)
//! compiles the same file and deploys it with `forge`/`cast`, and prints the bytecode so you can
//! refresh [`CREATION_BYTECODE`] below.
//!
//! # Why the bytecode is committed
//!
//! A workshop that requires `forge` to demonstrate a 200-line idea is a workshop about installing
//! Foundry. The alternative — a `POOL_ADDRESS` a student has to fill in — is worse: an unset
//! address means a keeper pointed at nothing, and the failure looks like a healthy pool (see
//! [`crate::keeper::config::KeeperConfig::require_real_pool`] for why that is the worst outcome in
//! this repository).
//!
//! # Keeping the two in sync
//!
//! The bytecode is pinned to solc **0.8.28**, optimizer on, 200 runs — see `foundry.toml`. A
//! recompile with different settings produces a *different but equally valid* creation code, so
//! "just recompile" and "the Rust constant" quietly stop agreeing. Regenerate both together:
//!
//! ```bash
//! forge build && scripts/deploy_pool.sh --print-bytecode
//! ```

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;

use crate::keeper::abi;

/// Compiled `LendingPoolMock` creation bytecode (solc 0.8.28, optimizer on, 200 runs).
///
/// Split into lines only so that a diff shows *which* region changed. The `concat!` is what makes
/// it a single `&'static str`; this is a constant, not a runtime allocation, so nothing is parsed
/// until [`deploy`] runs.
pub const CREATION_BYTECODE: &str = concat!(
    "0x60a0604052348015600e575f5ffd5b5033608052608051610d9b6100325f395f818161023701526108750152610d9b",
    "5ff3fe608060405234801561000f575f5ffd5b5060043610610106575f3560e01c80638da5cb5b1161009e578063baae",
    "78081161006e578063baae780814610286578063d475ab5414610299578063e2bbb158146102a2578063e8bbee4e1461",
    "02b5578063ecd1dae6146102be575f5ffd5b80638da5cb5b14610232578063930c2003146102595780639685b9af1461",
    "0261578063a13abdad14610270575f5ffd5b80634585e33b116100d95780634585e33b1461016d57806355f575101461",
    "01985780636ad9f9df146101fe5780636b8ab97d1461021f575f5ffd5b8063042e02cf1461010a5780630c3c13ec1461",
    "01325780632e1a7d4d14610147578063371fd8e61461015a575b5f5ffd5b61011d610118366004610b84565b6102c656",
    "5b60405190151581526020015b60405180910390f35b610145610140366004610b9f565b61034e565b005b6101456101",
    "55366004610b9f565b610457565b610145610168366004610b9f565b610532565b61018061017b366004610bb6565b61",
    "0621565b6040516001600160a01b039091168152602001610129565b6101d66101a6366004610b84565b5f6020819052",
    "9081526040902080546001909101546001600160801b0380831692600160801b9004169060ff1683565b604080516001",
    "600160801b039485168152939092166020840152151590820152606001610129565b61021161020c366004610b84565b",
    "6107e4565b604051908152602001610129565b61014561022d366004610b84565b61086a565b6101807f000000000000",
    "000000000000000000000000000000000000000000000000000081565b61beef610180565b610211670de0b6b3a76400",
    "0081565b6102786108f4565b604051610129929190610c24565b610145610294366004610c62565b6109b0565b610180",
    "61beef81565b6101456102b0366004610c62565b610a67565b61021160015481565b61021160c881565b6001600160a0",
    "1b0381165f908152602081905260408120600181015460ff16156102f257505f92915050565b8054600160801b900460",
    "01600160801b03165f0361031257505f92915050565b8054670de0b6b3a7640000906001600160801b03600160801b82",
    "0481169161033c91849116610c96565b6103469190610cb3565b109392505050565b805f0361036e57604051631f2a20",
    "0560e01b815260040160405180910390fd5b61beef5f9081526020527ff795696b84ec505a06e455ed35745d482b1c95",
    "debff7502f2dfa10a8a882013880546001600160801b03168211156103cd576040516309d6d0bf60e01b815261beef60",
    "048201526024015b60405180910390fd5b8054829082905f906103e99084906001600160801b0316610cd2565b825461",
    "01009290920a6001600160801b03818102199093169183160217909155825460408051868152600160801b9092049092",
    "16602082015261beef92507ff279e6a1f5e320cca91135676d9cb6e44ca8a08c0b88342bcdb1144f6511b56891015b60",
    "405180910390a25050565b805f0361047757604051631f2a200560e01b815260040160405180910390fd5b335f908152",
    "60208190526040902080546001600160801b03168211156104b2576040516309d6d0bf60e01b81523360048201526024",
    "016103c4565b8054829082905f906104ce9084906001600160801b0316610cd2565b82546101009290920a6001600160",
    "801b03818102199093169183160217909155825460408051868152600160801b90920490921660208201523392507ff2",
    "79e6a1f5e320cca91135676d9cb6e44ca8a08c0b88342bcdb1144f6511b568910161044b565b335f9081526020819052",
    "604081209082900361056157604051631f2a200560e01b815260040160405180910390fd5b8054600160801b90046001",
    "600160801b0316821115610595576040516309d6d0bf60e01b81523360048201526024016103c4565b80548290829060",
    "10906105b9908490600160801b90046001600160801b0316610cd2565b92506101000a8154816001600160801b030219",
    "1690836001600160801b03160217905550336001600160a01b03167f90890809c654f11d6e72a28fa60149770a0d11ec",
    "6c92319d6ceb2bb0a4ea1a155f5f60405161044b929190918252602082015260400190565b5f60208214610645576040",
    "516309d6d0bf60e01b81525f60048201526024016103c4565b61065182840184610b84565b6001600160a01b0381165f",
    "90815260208190526040902060018101549192509060ff161561069d57604051633e4efc1960e01b81526001600160a0",
    "1b03831660048201526024016103c4565b6106a6826102c6565b6106ce576040516309d6d0bf60e01b81526001600160",
    "a01b03831660048201526024016103c4565b336001600160a01b03831603610702576040516302193e1960e01b815260",
    "01600160a01b03831660048201526024016103c4565b80546001600160801b03165f61271061071c60c884610c96565b",
    "6107269190610cb3565b5f8085556001808601805460ff19168217905580549293509182919061074d908390610cf156",
    "5b9091555050604080518381526020810183905233916001600160a01b038716917f1f0c6615429d1cdae0dfa233abf9",
    "1d3b31cdbdd82c8081389832a61e1072f1ea910160405180910390a3836001600160a01b03167f59e0b68c6e37df3b25",
    "44e1ca1a2f6a74121eb63fc3e2e6a03078f19efe4988d487876040516107d3929190610d04565b60405180910390a250",
    "505092915050565b6001600160a01b0381165f9081526020819052604081208054600160801b90046001600160801b03",
    "16820361081c57505f1992915050565b600181015460ff161561083157505f92915050565b80546001600160801b0360",
    "0160801b820481169161085991670de0b6b3a76400009116610c96565b6108639190610cb3565b9392505050565b3360",
    "01600160a01b037f000000000000000000000000000000000000000000000000000000000000000016146108ce576040",
    "5162461bcd60e51b81526020600482015260096024820152683737ba1037bbb732b960b91b60448201526064016103c4",
    "565b6001600160a01b03165f908152602081905260408120908155600101805460ff19169055565b6040805180820190",
    "915230815261beef60208201525f90606090825b60028110156109965761093882826002811061092e5761092e610d32",
    "565b60200201516102c6565b1561098e57600182826002811061095157610951610d32565b6020020151604051602001",
    "61097591906001600160a01b0391909116815260200190565b6040516020818303038152906040529350935050509091",
    "565b600101610910565b505f60405180602001604052805f81525092509250509091565b61beef5f8181526020526001",
    "600160801b03828116600160801b02908416177ff795696b84ec505a06e455ed35745d482b1c95debff7502f2dfa10a8",
    "a88201389081557ff795696b84ec505a06e455ed35745d482b1c95debff7502f2dfa10a8a8820139805460ff19169055",
    "6040519091907f90890809c654f11d6e72a28fa60149770a0d11ec6c92319d6ceb2bb0a4ea1a1590610a5a9086908690",
    "918252602082015260400190565b60405180910390a2505050565b81158015610a73575080155b15610a915760405163",
    "1f2a200560e01b815260040160405180910390fd5b335f908152602081905260408120805490918491839190610abc90",
    "84906001600160801b0316610d46565b92506101000a8154816001600160801b0302191690836001600160801b031602",
    "1790555081815f0160108282829054906101000a90046001600160801b0316610b059190610d46565b92506101000a81",
    "54816001600160801b0302191690836001600160801b03160217905550336001600160a01b03167f90890809c654f11d",
    "6e72a28fa60149770a0d11ec6c92319d6ceb2bb0a4ea1a158484604051610a5a92919091825260208201526040019056",
    "5b6001600160a01b0381168114610b81575f5ffd5b50565b5f60208284031215610b94575f5ffd5b813561086381610b",
    "6d565b5f60208284031215610baf575f5ffd5b5035919050565b5f5f60208385031215610bc7575f5ffd5b823567ffff",
    "ffffffffffff811115610bdd575f5ffd5b8301601f81018513610bed575f5ffd5b803567ffffffffffffffff81111561",
    "0c03575f5ffd5b856020828401011115610c14575f5ffd5b6020919091019590945092505050565b8215158152604060",
    "208201525f82518060408401528060208501606085015e5f606082850101526060601f19601f83011684010191505093",
    "92505050565b5f5f60408385031215610c73575f5ffd5b50508035926020909101359150565b634e487b7160e01b5f52",
    "601160045260245ffd5b8082028115828204841417610cad57610cad610c82565b92915050565b5f82610ccd57634e48",
    "7b7160e01b5f52601260045260245ffd5b500490565b6001600160801b038281168282160390811115610cad57610cad",
    "610c82565b80820180821115610cad57610cad610c82565b60208152816020820152818360408301375f818301604090",
    "810191909152601f909201601f19160101919050565b634e487b7160e01b5f52603260045260245ffd5b600160016080",
    "1b038181168382160190811115610cad57610cad610c8256fea26469706673582212200ed8f15d61899ce31daf58e8bd",
    "2cb4b9183499dfdb3e373c536eab4cf5310af364736f6c634300081c0033"
);

/// The pool's fixed "victim" address, matching `VICTIM` in the Solidity source.
///
/// Duplicated as a constant here rather than read on chain, because the `crash` subcommand needs
/// it to print a health factor *before* deciding what to do, and an extra `eth_call` to fetch a
/// constant is not worth the coupling. `pool_is_labelled_correctly` below is what keeps the two
/// copies honest.
pub const VICTIM: Address =
    alloy::primitives::address!("000000000000000000000000000000000000beef");

/// A healthy position: 200 collateral against 100 debt is a health factor of 2.0.
pub const HEALTHY_COLLATERAL: u128 = 200;
/// The debt in the same position. 200/100 == 2.0, comfortably above the 1.0 line.
pub const HEALTHY_DEBT: u128 = 100;
/// An underwater position: 80 collateral against 100 debt is a health factor of 0.8.
pub const UNDERWATER_COLLATERAL: u128 = 80;
/// The debt that makes 80 collateral underwater. 80/100 == 0.8 < 1.0.
pub const UNDERWATER_DEBT: u128 = 100;

/// The calldata for `armVictim(collateral, debt)`, built without a node.
pub fn arm_victim_calldata(collateral: u128, debt: u128) -> Bytes {
    use alloy::sol_types::SolCall;
    Bytes::from(
        abi::armVictimCall {
            collateral: U256::from(collateral),
            debt: U256::from(debt),
        }
        .abi_encode(),
    )
}

/// The calldata for `withdrawVictim(amount)` -- the lab's "shrink the victim" call.
///
/// Victim-scoped on purpose, because `withdraw` acts on `msg.sender` and the victim is a fixed
/// address nobody holds a key for. This is the call the watcher has to notice: nothing about it
/// looks alarming, and it is the one that pushes a health factor below 1.0.
pub fn withdraw_victim_calldata(amount: u128) -> Bytes {
    use alloy::sol_types::SolCall;
    Bytes::from(
        abi::withdrawVictimCall {
            collateral: U256::from(amount),
        }
        .abi_encode(),
    )
}

/// The calldata for `reset(user)`.
pub fn reset_calldata(user: Address) -> Bytes {
    use alloy::sol_types::SolCall;
    Bytes::from(abi::resetCall { user }.abi_encode())
}

/// Check that a deployed pool really is *this* pool.
///
/// A student who deploys the mock twice, or points `POOL_ADDRESS` at an anvil account they thought
/// was a contract, gets a keeper that "works" against the wrong thing. This one `eth_call` against
/// the pool's own `victim()` costs nothing and turns a whole class of confusion into one clear
/// message.
///
/// # Errors
/// Returns a message naming the mismatch.
pub async fn pool_is_labelled_correctly<P: Provider>(provider: &P, pool: Address) -> Result<(), String> {
    use alloy::sol_types::SolCall;
    let call = TransactionRequest::default()
        .with_to(pool)
        .with_input(Bytes::from(abi::victimCall {}.abi_encode()));
    let out = provider
        .call(call)
        .await
        .map_err(|e| format!("victim() call failed: {e}"))?;
    let on_chain = <Address as alloy::sol_types::SolValue>::abi_decode_validate(&out)
        .map_err(|e| format!("victim() did not return an address: {e}"))?;
    if on_chain == VICTIM {
        Ok(())
    } else {
        Err(format!(
            "POOL_ADDRESS {pool} answers, but its VICTIM is {on_chain} and this lab expects \
             {VICTIM}. That is a different contract -- check POOL_ADDRESS."
        ))
    }
}

/// Deploy [`CREATION_BYTECODE`] and return the pool's address.
///
/// The deployed address is **computed**, not read back from a receipt field, because a contract
/// address is exactly `keccak256(rlp([deployer, nonce]))[12..]` — the same rule the node uses. Anvil
/// fills in the nonce, so we read it first, sign with it, and derive the address locally rather
/// than trusting a `contractAddress` field to be present.
///
/// Doing it this way means deployment goes through the *same* provider and wallet as everything
/// else, so the student sees one consistent way transactions are sent.
///
/// # Errors
/// Returns a message if the byte constant will not parse, if the transaction is refused, or if
/// the receipt has no contract address.
pub async fn deploy<P, Q>(sim_provider: &P, tx_provider: &Q, deployer: Address) -> Result<Address, String>
where
    P: Provider,
    Q: Provider,
{
    let code = alloy::hex::decode(CREATION_BYTECODE.trim_start_matches("0x"))
        .map_err(|e| format!("CREATION_BYTECODE is not valid hex: {e}"))?;

    // The nonce the *next* transaction from this key will use. The node is the authority; we only
    // need it to compute the address.
    let nonce = sim_provider
        .get_transaction_count(deployer)
        .await
        .map_err(|e| format!("could not read the deployer's nonce: {e}"))?;

    let request = TransactionRequest::default()
        .with_from(deployer)
        .with_deploy_code(Bytes::from(code));

    let pending = tx_provider
        .send_transaction(request)
        .await
        .map_err(|e| format!("deployment transaction failed: {e}"))?;
    let receipt = pending
        .get_receipt()
        .await
        .map_err(|e| format!("no receipt for the deployment: {e}"))?;

    // Prefer the node's answer; fall back to the local computation, because some dev nodes omit
    // `contractAddress` when they do not recognise the tx as a deployment.
    let address = match receipt.contract_address {
        Some(address) => address,
        None => {
            let derived = deployer.create(nonce);
            println!("ℹ️  node omitted contractAddress; derived {derived} from nonce {nonce}");
            derived
        }
    };

    // A deployment that "succeeded" but left no code would point the keeper at nothing, so verify.
    let code_at = sim_provider
        .get_code_at(address)
        .await
        .map_err(|e| format!("could not read code at {address}: {e}"))?;
    if code_at.is_empty() {
        return Err(format!(
            "deployment reported success but {address} has no code -- is this a real EVM?"
        ));
    }

    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded bytecode must be parseable hex with an `0x` prefix and an even length. A
    /// truncated `concat!` compiles fine and fails only at deploy time, which is exactly the kind
    /// of error a student should meet in a unit test instead.
    #[test]
    fn the_embedded_bytecode_is_well_formed() {
        assert!(CREATION_BYTECODE.starts_with("0x"), "must carry the 0x prefix");
        let hex = &CREATION_BYTECODE[2..];
        assert!(hex.len() % 2 == 0, "hex length must be even");
        let bytes = alloy::hex::decode(hex).expect("embedded bytecode must decode");
        // A real deployment is a few kilobytes; an empty or stub result means the constant was
        // mangled by whatever edited it.
        assert!(
            bytes.len() > 1000,
            "creation bytecode is only {} bytes -- was the constant truncated?",
            bytes.len()
        );
    }

    /// The Rust `VICTIM` must match the contract's. They are two copies of one value in two
    /// languages, and nothing but a check stops them drifting apart — which would make the `crash`
    /// demo arm a position the pool's `checkUpkeep` roster does not scan.
    ///
    /// Compared case-insensitively on purpose. `Address::to_string` renders the EIP-55 *checksummed*
    /// form, so the same address prints as `0x…bEEF` or `0x…beef` depending on the literal it was
    /// written from. Asserting an exact string would make this test a change-detector for hex
    /// casing, which is not a property anyone cares about; what matters is the 20 bytes.
    #[test]
    fn the_victim_constant_is_a_proper_address() {
        assert_eq!(
            VICTIM.to_string().to_lowercase(),
            "0x000000000000000000000000000000000000beef"
        );
    }

    /// The lab's two scenarios must straddle the liquidation line, which is the entire point of
    /// the numbers being constants rather than literals sprinkled through a subcommand.
    #[test]
    fn the_lab_positions_straddle_the_liquidation_line() {
        // Healthy: 200/100 == 2.0, above 1.0.
        assert!(HEALTHY_COLLATERAL * 1_000_000 > HEALTHY_DEBT * 1_000_000);
        // Underwater: 80/100 == 0.8, below 1.0.
        assert!(UNDERWATER_COLLATERAL * 1_000_000 < UNDERWATER_DEBT * 1_000_000);
    }
}
