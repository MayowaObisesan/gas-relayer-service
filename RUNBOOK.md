# RUNBOOK — running the hardened relayer, step by step

This is the operational guide: how to start it, what you should see at each step, what will bite you,
and what changes when the same code faces a real network with real money.

Every output below was captured from this repository on anvil. If your output differs, the
**troubleshooting** section has the likely reason.

| Guide | For |
|---|---|
| `RUNBOOK.md` (this file) | Running it, and operating it |
| `WORKSHOP.md` | Teaching it (Day 4 run sheet, timings) |
| `KEEPERS_WORKSHOP.md` | Teaching it (Week 5, the keeper) — start here for Day 5 |
| `README.md` | What the code is and why |
| `SECURITY_AUDIT_CHECKLIST.md` | Grading it |

**Assumptions:** macOS or Linux, Rust toolchain, Foundry (`anvil`, `cast`) on `PATH`, and a checkout
of this repository. Everything is local: no RPC keys, no testnet ETH, no wallet needed.

---

## 0. The five-minute version

```bash
anvil                                            # T1: the chain
cargo run                                        # T2: the relayer
cargo test --lib                                 # 24 proofs of the security claims
cargo run --bin simulator -- honest 20           # T3: legitimate traffic
./scripts/break_my_code.sh --grief               # T3: the whole attack suite + the loss demo
curl -s 127.0.0.1:3000/metrics | grep -v '^#'    # T4: the scoreboard
```

What "good" looks like, in one line: **every benign intent relayed, every attack refused, and
`relayer_gas_burned_on_reverts_wei 0`.**

---

## 1. Prerequisites

```bash
cargo --version      # 1.93.0 was used for every output below
anvil --version      # 0.2.0
cast --version
```

Versions of the libraries are pinned in `Cargo.toml` (alloy 2.1.1, axum 0.8.9, tokio 1.53.1,
reqwest 0.13.5). Exact error wording may differ slightly on other versions.

**Do this before a class, not during one:** `cargo build --all-targets`. It pre-warms alloy, and
every later rebuild drops from minutes to ~5 seconds.

---

## 2. Start the chain (T1)

```bash
anvil
```

You should see ten funded accounts and `Listening on 127.0.0.1:8545`. Leave it running.

The chain starts at block `0` and **blocks only advance when a transaction lands**. That makes the
block number your most honest instrument all session: if it does not move, nothing was relayed, and
nobody paid.

Two things to know:

* **Restarting anvil wipes everything** — balances, nonces, any contract you deployed with
  `anvil_setCode`. If you restart it, restart the relayer too (see §4.5).
* Your `.env` key should be one of anvil's funded accounts for the demos to work end to end.

---

## 3. Build, then prove the claims (T2)

```bash
cargo build --all-targets
cargo test --lib
```

Expected: `test result: ok. 24 passed`. These are not decoration — they are the evidence for the
claims the rest of this runbook makes. A few worth reading before you trust anything:

| Test | The claim it proves |
|---|---|
| `a_signature_for_another_contract_is_rejected` | The domain separator binds the forwarder, so "sign once, relay anywhere" is dead |
| `a_high_s_signature_is_rejected_as_malleable` | One intent cannot have two valid signatures |
| `a_race_of_identical_intents_produces_exactly_one_winner` | Check-and-insert is atomic: 64 concurrent copies, one accepted |
| `the_same_nonce_under_a_different_digest_is_still_refused` | The `(user, nonce)` index catches the grief the digest check misses |
| `debug_never_prints_the_secret` | The key cannot leak through a stray `println!("{:?}")` |

---

## 4. Start the relayer (T2)

```bash
cargo run
```

### 4.1 Read the banner — it is a security briefing

```
┌─ hardened relayer ─────────────────────────────────────────────
│ relayer address   0xa0Ee7A142d267C1f36714E4a8F75612F20a79720
│ chain id          31337
│ simulate against  http://127.0.0.1:8545
│ broadcast to      http://127.0.0.1:8545   ← PUBLIC mempool: anyone can see and front-run this
│ trusted forwarder 0x0000000000000000000000000000000000000000
│   ⚠️  that address has NO CODE on chain 31337.
│       Relays will be mined, report success, and forward nothing.
│       Fine for the lab; set FORWARDER_ADDRESS for anything real.
│ EIP-712 domain    HardenedRelayer/1
│ queue capacity    100
│ replay registry   100000 entries max
│ DRY RUN           ON  (eth_call before every broadcast)
└────────────────────────────────────────────────────────────────
🚀 relayer listening on http://127.0.0.1:3000
```

Five lines in that banner are load-bearing. Read them every single time you start it:

| Line | Why it matters |
|---|---|
| `relayer address` | The key was loaded. It prints the **address only** — if you ever see key material here, that is a bug worth reporting |
| `broadcast to` | `PUBLIC mempool` means front-runnable. `PRIVATE MEMPOOL` means it is not |
| `trusted forwarder` + the warning | A forwarder with no code means every relay succeeds and does nothing. This is the #1 silent production failure |
| `EIP-712 domain` | These values are inside the signatures. Change either one and every client must re-sign |
| `DRY RUN` | `OFF` means the primary defence is disabled and the relayer will pay for failures |

If the banner says `⚠️ PRIVATE_KEY is not set` you are running with a throwaway key. That is fine for
anvil and fatal for anything else — it just means no `.env` was found (see §5.2).

### 4.2 Check it is alive

```bash
curl -s 127.0.0.1:3000/health          # ok
curl -s 127.0.0.1:3000/domain
```

`/domain` returns the four parameters a client must sign against, plus the resulting separator:

```json
{"chain_id":"31337",
 "domain_separator":"0xfc11b38f3599328ba870b5b81b0575f39d5dc540d860a237ba35d73630ba9d13",
 "name":"HardenedRelayer",
 "primary_type":"Intent(address user,uint256 nonce,uint256 deadline,bytes32 dataHash)",
 "verifying_contract":"0x0000000000000000000000000000000000000000",
 "version":"1"}
```

None of that is secret. Publishing it is the point: it is how a client proves it means **this**
relayer and **this** forwarder, on **this** chain.

### 4.3 Send honest traffic (T3)

```bash
cargo run --bin simulator -- honest 20
```

```
mode              Honest (honest)
first nonce       1790246182000
expected accepts  20

━━━ 20 submissions (0 attack / 20 benign) ━━━
  202 Accepted                 20
  ✅ legitimate traffic unaffected (20 benign intents relayed).
```

In T2 you now see one `✅ relayed ... nonce ... -> 0x...` line per intent. Keep an eye on that:
it is the only place the two halves of the system meet.

### 4.4 Read the scoreboard (T4)

```bash
curl -s 127.0.0.1:3000/metrics | grep -v '^#'
```

Every number has a job:

| Metric | Meaning | Watch for |
|---|---|---|
| `relayer_dry_run_enabled` | Is the primary defence on? | `0` on a real network — page somebody |
| `relayer_intents_accepted_total` | Passed every check and queued | Should track your legitimate load |
| `relayer_intents_rejected_total` | Refused, any reason | A spikes is an attack *or* a broken client |
| `relayer_rejections_total{reason="replayed"}` | Same digest twice | Spikes = a retrying client or an attacker |
| `...{reason="nonce_taken"}` | The same nonce, different digest | Spikes = clients racing their own retries |
| `...{reason="would_revert"}` | **The griefing attack, refused for free** | This should be the *only* cost of an attack |
| `...{reason="rpc_unavailable"}` | Your node, not your user | Any non-zero value deserves a look |
| `relayer_transactions_total{status="dispatched"}` | Broadcasts attempted | Compare with `accepted` after the queue drains |
| `...{status="confirmed"}` | Mined successfully | Should equal `dispatched` minus `reverted` |
| `...{status="reverted"}` | **Money lost.** Mined and failed | Must be `0`. Every one is a dry-run miss |
| `relayer_gas_spent_wei` | Total cost of doing business | Alert on the *rate*, not the total |
| `relayer_gas_burned_on_reverts_wei` | **Wei paid for nothing** | Must be `0`. This is the number this class exists for |
| `relayer_replay_registry_entries` | Claims retained in memory | Pinned at `MAX_SEEN_ENTRIES` means you are evicting |
| `relayer_queue_len` | Intents waiting | Non-zero and growing = the worker is the bottleneck |

A healthy run looks like this — note that `accepted`, `dispatched` and `confirmed` agree exactly:

```
relayer_intents_accepted_total 46
relayer_transactions_total{status="dispatched"} 46
relayer_transactions_total{status="confirmed"} 46
relayer_transactions_total{status="reverted"} 0
relayer_gas_burned_on_reverts_wei 0
```

### 4.5 Inspect and hand-run an intent

`dump` signs one intent and prints it — clean JSON on **stdout**, all hints on **stderr**, so it pipes:

```bash
cargo run --bin simulator -- dump > /tmp/intent.json
cat /tmp/intent.json
```

```json
{
  "user": "0xf358a87c7b9bddd73df3b8845c7a639ee799bd63",
  "nonce": 1790247990000,
  "deadline": 1790248110,
  "data": "0xdeadbeef",
  "signature": "0xe23ac94f..."
}
```

Now the interesting part. Send it twice:

```bash
curl -s -w ' [HTTP %{http_code}]\n' -X POST -H 'content-type: application/json' \
  -d @/tmp/intent.json 127.0.0.1:3000/submit
curl -s -w ' [HTTP %{http_code}]\n' -X POST -H 'content-type: application/json' \
  -d @/tmp/intent.json 127.0.0.1:3000/submit
```

```
{"accepted":true,"digest":"0x0c4e6dd6...","nonce":1790248006000,"status":"queued ..."} [HTTP 202]
{"accepted":false,"code":"replayed","reason":"intent 0x0c4e6dd6... has already been relayed"} [HTTP 409]
```

Identical bytes, refused the second time, by the relayer's own memory. Now do the thing that should
make you uncomfortable — **restart the relayer and send the same bytes a third time**:

```bash
kills=$(lsof -nP -iTCP:3000 -sTCP:LISTEN -t); kill $kills      # stop it
cargo run &                                                    # start it again
curl -s -w ' [HTTP %{http_code}]\n' -X POST -H 'content-type: application/json' \
  -d @/tmp/intent.json 127.0.0.1:3000/submit
```

```
{"accepted":true,"digest":"0x0c4e6dd6...","nonce":1790248006000,"status":"queued ..."} [HTTP 202]
```

**202 again. The relayer just paid gas twice for the same authorisation.** The registry is in memory,
and memory does not survive a restart. This is the single best argument for a shared store, and it is
§5.4 below with the fix.

> Why is the lab chain so unforgiving here? Because the forwarder at `0x0000...0000` has no code and
> therefore no nonce. Point `FORWARDER_ADDRESS` at a real forwarder that enforces nonces and the
> *dry run* catches the third submission instead: `eth_call` sees the spent nonce, returns the revert,
> and the relayer answers `422` having spent nothing. That is defence in depth doing its job — the
> in-memory cache is the fast path, the chain is the truth.

### 4.6 Run the attacks (T3)

```bash
cargo run --bin simulator -- replay 5
cargo run --bin simulator -- nonce-reuse 4
cargo run --bin simulator -- malleable 3
cargo run --bin simulator -- badsig 3
cargo run --bin simulator -- expired 3
cargo run --bin simulator -- far-deadline 3
cargo run --bin simulator -- empty-calldata 3
cargo run --bin simulator -- garbage 3
cargo run --bin simulator -- mixed 40            # everything, interleaved
```

Expected, and what each one teaches:

| Mode | Result | The lesson |
|---|---|---|
| `replay 5` | `1x202`, `4x409` | One authorisation, one spend |
| `nonce-reuse 4` | `1x202`, `3x409 nonce_taken` | Four *different* digests, one spent nonce. Only the second index catches this |
| `malleable 3` | `3x401` | The high-`s` twin is a legal signature of the same intent |
| `badsig 3` | `3x401` | `user` is a claim; recovery is the fact |
| `expired 3` | `3x400` | A leaked signature must rot |
| `far-deadline 3` | `3x400` | Ten years of validity is a liability, not a feature |
| `empty-calldata 3` | `3x400` | On an empty account `0x` succeeds: you would pay for a no-op |
| `garbage 3` | `3x400` | Malformed input dies at the edge, before curve maths |
| `mixed 40` | 34 accepted, 30 refused, **HACKER SCORE 0** | A defence that refuses everything scores no better than one that refuses nothing |

The verdict lines are the point:

```
  ✅ DEFENDER WINS: 34 accepted, of which 0 were attacks -- nothing reached the chain that should not have.
  ✅ legitimate traffic unaffected (34 benign intents relayed).
```

Or, for the whole suite at once:

```bash
./scripts/break_my_code.sh --grief
```

### 4.7 The griefing demo: watch the money leave, then stop it

This is the demonstration the whole day builds toward. **Do it in this order.**

```bash
# 1. Install a contract that reverts at the forwarder address
#    (60006000fd = PUSH1 0, PUSH1 0, REVERT)
cast rpc anvil_setCode 0x0000000000000000000000000000000000000000 0x60006000fd

# 2. The undefended relayer: no dry run, and a fixed gas limit so the node's own
#    gas estimation cannot save us (see the note below)
DRY_RUN=0 GAS_LIMIT=100000 cargo run
```

The banner now reads `DRY RUN OFF ⚠️ the relayer will pay for transactions that revert`. In T3:

```bash
before=$(cast block-number)
cargo run --bin simulator -- honest 5
sleep 3
echo "blocks: $before -> $(cast block-number)"
```

```
  202 Accepted                 5
blocks: 130 -> 135
```

Five correctly signed intents, five **new blocks**, all of them failures:

```bash
curl -s 127.0.0.1:3000/metrics | grep -E 'reverted|gas_burned'
```

```
relayer_transactions_total{status="reverted"} 5
relayer_gas_burned_on_reverts_wei 2991940
```

Nobody exploited a bug. The relayer agreed to buy five failing transactions.

```bash
# 3. Clean up: leave the broken forwarder in place, and turn the defence on
kill $(lsof -nP -iTCP:3000 -sTCP:LISTEN -t)
cargo run                                          # DRY RUN ON
cargo run --bin simulator -- honest 5              # 5 x 422 Unprocessable Entity
curl -s 127.0.0.1:3000/metrics | grep -E 'would_revert|gas_burned'
cast block-number                                  # unchanged
```

```
relayer_rejections_total{reason="would_revert"} 5
relayer_gas_burned_on_reverts_wei 0
```

Same five intents, same broken destination, zero blocks, zero gas. Then:

```bash
cast rpc anvil_setCode 0x0000000000000000000000000000000000000000 0x    # restore the no-op
```

> **Why `GAS_LIMIT` is in the recipe.** Alloy's default gas filler calls `eth_estimateGas` before
> sending, and estimation *also* reverts — so a reverting transaction would never be broadcast and
> you would see zero loss even with `DRY_RUN=0`. That is a free dry run hiding inside a dependency:
> useful to know, unwise to rely on (it is an extra round trip, it does not tell you why, and it runs
> after your queue slot is spent). `GAS_LIMIT` removes the crutch. Production relayers set an explicit
> cap anyway, to bound their worst case.

---

## 5. What to look out for

Nine ways this bites people. Each one has been reproduced.

### 5.1 `localhost` is not `127.0.0.1` (sometimes)

The listener binds **IPv4 only**. On some macOS configurations `localhost` resolves to `::1` first and
you get a silent connection failure that looks like a crashed relayer.

```bash
curl -s 127.0.0.1:3000/health     # always this
```

It happens to work on this machine (both resolve to 127.0.0.1), which is exactly why this is worth
knowing: it will fail on a colleague's laptop. Need it reachable from outside? `BIND_ADDR=0.0.0.0:3000
-- and then it is on the network, so put a reverse proxy and a rate limit in front of it.

### 5.2 `.env` is read from the *current directory*

The relayer loads `.env` itself (no `dotenvy` dependency: see `src/config.rs`, twenty lines). If you
launch it from somewhere else, it finds nothing and silently generates a throwaway key:

```
⚠️  PRIVATE_KEY is not set. Generating a THROWAWAY key for this run.
   Production relayers load this from a KMS/HSM and never from a file.
```

A throwaway key on anvil is harmless. The same warning on a testnet means your funded key was not
loaded and every relay will fail for insufficient funds. **Check the `relayer address` line against
what you expect.** Real environment variables always win over `.env`, so `PRIVATE_KEY=... cargo run`
overrides the file.

### 5.3 Port already in use

```
Error: cannot bind 127.0.0.1:3000: Address already in use
   is another relayer already running? check with: lsof -nP -iTCP:3000 -sTCP:LISTEN
```

Two relayers, one port, one of them with a stale registry. Kill the old one:

```bash
kill $(lsof -nP -iTCP:3000 -sTCP:LISTEN -t)
```

### 5.4 The restart gap (the most important one)

Shown in full in §4.5. The 60-second version:

```
send intent -> 202
send again  -> 409   (in-memory registry)
restart the relayer
send again  -> 202   (memory is gone; the relayer pays twice)
```

On a chain whose forwarder enforces nonces, the dry run catches it and you see `422` instead. On a
chain without that, or when the nonce is not part of your signed struct, nothing catches it. The fix
is a shared registry keyed by digest (`Redis`, TTL ≈ your deadline window) **plus** the on-chain nonce
as the real source of truth. Until you have both, treat a restart as a replay window and drain the
queue before deploying.

### 5.5 Two processes, one key

The relayer's own nonces come from the node's *pending* count. Two instances signing with the same key
race each other for the same nonce: one wins, the other gets `nonce too low` or "replacement
transaction underpriced". One key, one signer, one process. If you need scale, give each instance its
own key and split the queue — do not share a hot key.

### 5.6 A forwarder with no code is a silent no-op

The default `FORWARDER_ADDRESS=0x0000...0000` has no code. Every relay will be **mined and reported as
success while forwarding nothing**: `confirmed` climbs, users get `202`, and no state changes. The
banner now warns about it, and production should hard-fail instead of warning.

The same trap exists in production with a *real* address if it is an EOA rather than a contract —
sending calldata to an EOA also "succeeds". Verify the address has code before trusting a green
dashboard.

### 5.7 `DRY_RUN=0` is a loaded gun

It exists to demonstrate the loss. It also disables your primary defence, and nothing else in the
codebase notices. That is on purpose — and it is why the switch is visible from outside:

```bash
curl -s 127.0.0.1:3000/metrics | grep relayer_dry_run_enabled   # must be 1
```

Alert on it. A relayer running without its primary defence should be a page, not a log line.

### 5.8 `GAS_LIMIT` too low burns real gas

Setting an explicit limit skips estimation, which is what makes the griefing demo honest — and also
means **the node will not warn you** that a transaction needs more gas than you gave it. Set it far
too low and every relay is mined as a failure, with `relayer_gas_burned_on_reverts_wei` climbing.
Start generous (200k-500k for a forwarder call), watch `reverted`, then tighten. Never set it below
the real cost of your forwarder's worst-case path.

### 5.9 `202 Accepted` does not mean "relayed"

The handler answers as soon as the intent is verified, simulated and queued. The broadcast happens
later, in one sequential worker. Consequences:

* restarting the relayer **with a non-empty queue loses whatever is queued** — nothing is durable;
* a script that changes on-chain state immediately after its last `202` can race the worker (this
  bit us while building the demo: installing a reverting forwarder while the queue drained made the
  last-chance re-simulation refuse the rest — correct behaviour, confusing output);
* `accepted` and `dispatched` legitimately differ for a moment. Wait for
  `relayer_queue_len 0` before drawing conclusions. `scripts/break_my_code.sh` does exactly that
  between phases.

---

## 6. What changes in production

Six differences matter more than the rest. For each, what the lab does, what production does, and what
actually breaks first.

### 6.1 There is a real forwarder, and it enforces nonces

The lab relays to an empty address, so *nothing* checks the user's nonce on chain. In production the
forwarder is a deployed contract that:

* verifies the same EIP-712 struct you verify (`domainSeparator`, `Intent`, `dataHash`);
* keeps `nonces[user]` and reverts on reuse;
* enforces the same `deadline`;
* is the address in your domain separator, so changing it invalidates every client signature.

**What breaks first:** the struct in `sol!` and the Solidity struct drift apart — field order is part of
the type hash, so reordering fields silently invalidates every signature. Keep one definition, test it
with a known vector both sides.

### 6.2 The dry run stops being a nicety and becomes your margin

At classroom volume a dry run costs one round trip. In production it is the difference between a
healthy business and a drain:

* keep it, and keep the last-chance re-check before broadcast;
* **accept the TOCTOU gap out loud** — state moves between simulation and mining. Measured with
  `reverted` and `gas_burned_on_reverts_wei`, not eliminated with optimism;
* cap `GAS_LIMIT` so the worst case is bounded even when the simulation is fooled;
* alert on `reverted > 0`, not on a weekly report. A single revert is either a bug or a griefer
  probing you.

### 6.3 The registry must outlive the process; the key must not

| | Lab | Production |
|---|---|---|
| Replay registry | `HashSet` in memory | Redis/managed store keyed by digest, TTL ≈ deadline window, **and** the on-chain nonce as truth |
| Queue | bounded `mpsc`, lost on restart | durable outbox; a restart resumes, it does not forget |
| Private key | env var / `.env` | KMS or HSM: the process asks a boundary to sign a digest and never holds key bytes |
| Key rotation | n/a | planned for, with a migration window where both old and new relayer addresses are trusted |

The honest limit of `secure_key.rs` is written in its own doc comment: Rust cannot stop the environment
block, the `.env` file, the shell history and the OS page cache from having seen the bytes. A
`Zeroizing` struct makes the key *tidy*, not *safe*. Only removing the key from the process does that.

### 6.4 Public orderflow means predictable loss

In the lab both providers point at the same anvil and the banner says `PUBLIC mempool`. In production:

* a signed transaction in the public mempool is a public bet — someone can pay to take it;
* point `PRIVATE_RPC_URL` at a relay/builder and the WARNING becomes a design decision you can defend;
* remember it is a **trust** choice, not a cryptographic one: you are trusting the relay not to censor,
  delay or leak. Bundles (`eth_sendBundle`) are the next rung: atomic inclusion, or the whole bundle
  reverts.
* **the nonce caveat:** a relay will not answer `eth_getTransactionCount`. Production relayers fetch the
  nonce from the public node and set it explicitly. Otherwise the filler asks the relay, gets a stale
  answer, and you rebuild half a nonce manager during an incident.

### 6.5 What actually goes wrong in the first week

Not the cryptography. These:

1. **Balance runs out.** The relayer is a wallet that only spends. Alert on balance and on spend rate,
   not on per-transaction cost.
2. **The key is not what you think.** Loaded from the wrong file, a copy of a dev key, or a key that
   also signs something else. Log the *address*, check it against the allowlist at boot, and refuse to
   start on mismatch.
3. **A client hammers one nonce.** A buggy frontend retrying "user nonce 7" hits `409 nonce_taken`
   forever and looks like your bug. Return the reason (`reason` is in the JSON body) and document
   that a retry needs a *new* nonce and a fresh signature.
4. **Deadlines are too short for real wallets.** The window here is 300 seconds: a user who takes a
   coffee break between signing and submitting gets `400`. Size it to your UX, not to your paranoia.
5. **Two deployments, one key, one chain.** See §5.5. Nonce chaos, and it looks like a random flake.
6. **Restart during a deploy = replayed intents.** See §5.4. Drain the queue first; that is what the
   graceful shutdown path is for.

### 6.6 Numbers you should be able to produce on demand

If you cannot answer these with a metric, you are guessing:

| Question | Answer in this codebase |
|---|---|
| How much have we spent? | `relayer_gas_spent_wei` |
| How much did we waste? | `relayer_gas_burned_on_reverts_wei` (must be 0) |
| Are we under attack, and how? | `relayer_rejections_total` by `reason` |
| Is the defence on? | `relayer_dry_run_enabled` |
| Are we falling behind? | `relayer_queue_len` and its trend |
| Are we forgetting replays? | `relayer_replay_registry_entries` pinned at the cap |

---

## 7. Appendix

### 7.1 Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `PRIVATE_KEY` | *(none)* | The relayer's key. Unset = throwaway key + warning |
| `RPC_URL` | `http://127.0.0.1:8545` | Full node: `eth_call`, chain id, receipts |
| `PRIVATE_RPC_URL`, `FLASHBOTS_RPC_URL` | *(none)* | Broadcast endpoint. Set = private orderflow |
| `FORWARDER_ADDRESS` | `0x0000...0000` | Relayed-to contract. Bound into the domain separator |
| `BIND_ADDR` | `127.0.0.1:3000` | API bind address |
| `EIP712_NAME`, `EIP712_VERSION` | `HardenedRelayer`, `1` | Part of the domain separator |
| `MAX_SEEN_ENTRIES` | `100000` | Replay registry bound |
| `QUEUE_CAPACITY` | `100` | Back-pressure threshold |
| `AWAIT_RECEIPTS` | `1` | `0` = fire and forget (no gas metrics) |
| `DRY_RUN` | `1` | **Lab switch.** `0` = buy failing transactions |
| `GAS_LIMIT` | *(none)* | Explicit gas cap; also skips `eth_estimateGas` |
| `RELAYER_URL` | `http://127.0.0.1:3000` | Simulator's target only |

### 7.1b Keeper environment variables (Week 5)

The keeper reuses `PRIVATE_KEY`, `RPC_URL` and `PRIVATE_RPC_URL` from the table above, and adds:

| Variable | Default | Purpose |
|---|---|---|
| `POOL_ADDRESS` | `0x0000...0000` | The pool to watch. **Must have code** — the keeper refuses to start otherwise |
| `WS_RPC_URL` | scheme-rewritten `RPC_URL` | Log subscription. The port is *not* guessed; anvil/geth use 8546 for WS |
| `KEEPER_POLL_SECS` | `30` | Timer fallback. **This is the keeper's real reaction time** if the socket dies |
| `KEEPER_MAX_TX_PER_HOUR` | `12` | Broadcast cap. `0` = unlimited, and the banner says so |
| `KEEPER_PRIORITY_FEE_GWEI` | `2` | EIP-1559 priority fee. The fee cap is derived as 2× this |
| `KEEPER_QUEUE_CAPACITY` | `100` | Trigger-queue depth. A full queue drops a nudge, which the timer recovers |
| `KEEPER_GAS_LIMIT` | *(none)* | Explicit gas cap; also skips `eth_estimateGas` |
| `KEEPER_AWAIT_RECEIPTS` | `1` | `0` = fire and forget, and the gas metrics become meaningless |
| `KEEPER_DRY_RUN` | `1` | **Lab switch.** `0` = buy reverting liquidations |
| `KEEPER_BIND_ADDR` | `127.0.0.1:3001` | `/metrics`, `/health`, `POST /upkeep` |

### 7.2 HTTP API

| Route | Response |
|---|---|
| `POST /submit` | `202` queued · `400` malformed/out of range · `401` signature does not prove the claim · `409` valid but already spent · `422` authentic but would revert · `503` ours, retry |
| `GET /domain` | EIP-712 parameters + domain separator |
| `GET /metrics` | Prometheus text |
| `GET /health` | `ok` |

Refusals are machine-readable: `{"accepted":false,"code":"nonce_taken","reason":"nonce 7 for 0x... was already spent on a different intent"}`.

### 7.3 Simulator reference

```
cargo run --bin simulator -- [hacker] <mode> [count]
```

`honest` (alias `flood`) · `replay` · `nonce-reuse` · `malleable` · `badsig` · `expired` ·
`far-deadline` · `empty-calldata` · `garbage` · `mixed` · `dump`

A leading `hacker` is ignored, because `cargo run --bin simulator -- hacker mixed 30` reads better on
a slide. Exit status is 0 either way — the verdict is in the output, and the real scoreboard is
`/metrics`.

### 7.4 Reset everything

```bash
kill $(lsof -nP -iTCP:3000 -sTCP:LISTEN -t)        # stop the relayer
pkill anvil                                         # stop the chain (wipes all state)
anvil &                                             # fresh chain
cargo run &                                         # fresh relayer: empty registry, empty metrics
cast rpc anvil_setCode 0x0000000000000000000000000000000000000000 0x    # undo lab cheats
```

Restarting anvil resets balances, nonces and any `anvil_setCode`, but **not** the relayer's in-memory
registry — restart both together and the state is coherent.
