#!/usr/bin/env bash
# Deploy the Week-5 lending pool with the Solidity toolchain, instead of the embedded bytecode.
#
# `cargo run --bin keeper -- deploy-pool` deploys the *same* contract from the creation bytecode
# committed in `src/keeper/mock.rs`, and needs nothing but anvil. This script is the other door:
# it compiles `contracts/LendingPoolMock.sol` in front of you, so you can change the contract, read
# what solc did, and deploy the result.
#
#   ./scripts/deploy_pool.sh                 compile and deploy, print the address
#   ./scripts/deploy_pool.sh --print-bytecode  also print the creation code, to refresh mock.rs
#   ./scripts/deploy_pool.sh --no-deploy       compile only
#
# Preconditions: anvil on 127.0.0.1:8545, `forge` on PATH, and (for deploying) a funded key in
# $PRIVATE_KEY or anvil's first account.
set -euo pipefail

RPC=${RPC_URL:-http://127.0.0.1:8545}
ARTIFACT=out/LendingPoolMock.sol/LendingPoolMock.json
PRINT_BYTECODE=0
DEPLOY=1

for arg in "$@"; do
  case $arg in
    --print-bytecode) PRINT_BYTECODE=1 ;;
    --no-deploy)      DEPLOY=0 ;;
    -h|--help)        sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

cd "$(dirname "$0")/.."

if ! command -v forge >/dev/null 2>&1; then
  echo "forge is not on PATH. Either install Foundry, or skip this script entirely:" >&2
  echo "    cargo run --bin keeper -- deploy-pool    # uses the embedded bytecode" >&2
  exit 1
fi

# The settings here MUST match foundry.toml and src/keeper/mock.rs, or the committed constant and
# a fresh compile will disagree. See the "Keeping the two in sync" note in src/keeper/mock.rs.
echo "▸ compiling contracts/LendingPoolMock.sol (solc 0.8.28, optimizer 200 runs)"
forge build --offline

if [[ ! -f $ARTIFACT ]]; then
  echo "expected artefact not found: $ARTIFACT" >&2
  exit 1
fi

# Fail loudly rather than printing an empty address: a silently-empty contract address is exactly
# the failure this whole lab is built to avoid.
BYTECODE=$(jq -r '.bytecode.object' "$ARTIFACT")
if [[ -z $BYTECODE || $BYTECODE == "0x" ]]; then
  echo "artefact has no creation bytecode -- did the compile actually run?" >&2
  exit 1
fi
echo "▸ creation bytecode: ${#BYTECODE} hex chars"

if [[ $PRINT_BYTECODE == 1 ]]; then
  echo
  echo "Paste this into CREATION_BYTECODE in src/keeper/mock.rs to refresh the embedded copy:"
  echo
  jq -r '.bytecode.object' "$ARTIFACT" | python3 -c "
import sys
b = sys.stdin.read().strip()
lines = [b[i:i+96] for i in range(0, len(b), 96)]
print('pub const CREATION_BYTECODE: &str = concat!(')
print('    \"' + lines[0] + '\",')
for line in lines[1:-1]:
    print('    \"' + line + '\",')
print('    \"' + lines[-1] + '\"')
print(');')
"
  echo
fi

if [[ $DEPLOY == 0 ]]; then
  echo "▸ --no-deploy given; stopping after the compile."
  exit 0
fi

# Anvil funds ten accounts; the first is the conventional default for this repo. Prefer the
# operator's own key so a deployed pool is owned by an address they control (the mock's `reset`
# is owner-only).
if [[ -n "${PRIVATE_KEY:-}" ]]; then
  KEY_ARGS=(--private-key "$PRIVATE_KEY")
  echo "▸ deploying with PRIVATE_KEY from the environment"
else
  KEY_ARGS=(--private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80)
  echo "▸ PRIVATE_KEY not set; deploying with anvil's first account"
fi

# `forge create` is used rather than `cast send --create` because the cast subcommands that take
# `--create` / `publish` do not accept a signing key in Foundry 0.2.0, and hand-rolling the key
# plumbing here would be more code than the step is worth. `forge create` compiles, signs, broadcasts
# and prints the address in one go.
#
# Note the address lands at 0x5FbDB...aa3, not the anvil-account-derived 0x700b...: the pool is
# deployed by *this* key's next nonce, and both are perfectly valid. Only the address matters.
DEPLOY_OUT=$(forge create --offline \
  --rpc-url "$RPC" \
  "${KEY_ARGS[@]}" \
  contracts/LendingPoolMock.sol:LendingPoolMock 2>&1) || {
    echo "forge create failed:" >&2
    echo "$DEPLOY_OUT" >&2
    exit 1
  }

# `forge create` prints "Deployed to: 0x...". Parse that rather than recomputing the CREATE address
# locally: it is the node's answer, and the whole point of checking is to trust the node.
ADDR=$(echo "$DEPLOY_OUT" | grep -i 'deployed to' | awk '{print $3}')

echo "✅ pool deployed at $ADDR"
echo
echo "next:"
echo "   POOL_ADDRESS=$ADDR cargo run --bin keeper"
echo "   POOL_ADDRESS=$ADDR cargo run --bin keeper -- status"
