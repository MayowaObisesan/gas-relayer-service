//! The traffic generator -- now in two flavours: **honest** and **hacker**.
//!
//! ```text
//! cargo run --bin simulator                          # honest 50 (the Day 3 demo)
//! cargo run --bin simulator -- honest 200            # honest, 200 intents
//! cargo run --bin simulator -- replay 5              # one intent, submitted 5 times
//! cargo run --bin simulator -- nonce-reuse 4         # same nonce, different calldata
//! cargo run --bin simulator -- malleable 3           # high-s twins of valid signatures
//! cargo run --bin simulator -- badsig 3              # signed by somebody else
//! cargo run --bin simulator -- expired 3             # deadline in the past
//! cargo run --bin simulator -- far-deadline 3        # deadline in a year
//! cargo run --bin simulator -- empty-calldata 3      # 0x, a successful no-op on chain
//! cargo run --bin simulator -- garbage 3             # not even hex
//! cargo run --bin simulator -- mixed 30              # ALL of the above (the Part 4 game)
//! ```
//!
//! Every mode signs against the domain the relayer publishes on `GET /domain`, so there is no
//! shared secret and no copy-pasted chain id. If the relayer changes its forwarder or its
//! domain name, every honest intent here stops verifying -- which is exactly the point.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy::primitives::Address;
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::Eip712Domain;
use reqwest::Client;
use tokio::task;

use traffic_simulator::intent::{
    MetaTxRequest, build_intent, domain_named, malleable_twin, signing_hash, unix_now,
};

/// The Default intent count
const DEFAULT_COUNT: usize = 50;
/// How long an intent stays valid. Well inside the relayer's 300-second window.
const DEADLINE_SECS: u64 = 120;

/// One submission, tagged with whether it was *supposed* to be an attack.
struct Payload {
    body: MetaTxRequest,
    attack: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Correct, distinct, signed intents. The happy path.
    Honest,
    /// The same signed intent, over and over. Expect exactly one 202 and the rest 409.
    Replay,
    /// The same user *and nonce*, but different calldata each time: distinct digests, one spent
    /// nonce. Only the second index in the replay guard catches this.
    NonceReuse,
    /// Perfectly valid signatures, re-encoded in their non-canonical `high-s` form.
    Malleable,
    /// The `user` field says Alice; the signature is Bob's.
    BadSig,
    /// Correctly signed, already dead.
    Expired,
    /// Correctly signed, valid for a year: a liability if it leaks.
    FarDeadline,
    /// Correctly signed with no calldata. On chain this is a successful no-op -- gas for nothing.
    EmptyCalldata,
    /// Calldata that is not even hex.
    Garbage,
    /// Interleave honest intents and every attack. The self-grading run.
    Mixed,
    /// Sign one intent and print it as JSON instead of submitting. The building block for every
    /// hand-run `curl` experiment, including the "restart the relayer and replay" test.
    Dump,
}

impl Mode {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "honest" | "flood" => Self::Honest,
            "replay" => Self::Replay,
            "nonce-reuse" => Self::NonceReuse,
            "malleable" => Self::Malleable,
            "badsig" | "wrong-signer" => Self::BadSig,
            "expired" => Self::Expired,
            "far-deadline" => Self::FarDeadline,
            "empty-calldata" => Self::EmptyCalldata,
            "garbage" => Self::Garbage,
            "mixed" => Self::Mixed,
            "dump" | "intent" => Self::Dump,
            _ => return None,
        })
    }

    fn is_attack(self) -> bool {
        !matches!(self, Self::Honest | Self::Dump)
    }
}

// =======================================================================================
// Signing helpers -- these mirror the relayer's `intent` module exactly.
// =======================================================================================

/// Sign an intent the way a user's wallet would. Returns the request body ready to POST.
fn signed(
    user: &PrivateKeySigner,
    domain: &Eip712Domain,
    nonce: u64,
    deadline: u64,
    data: &str,
) -> MetaTxRequest {
    let calldata = alloy::hex::decode(data.trim_start_matches("0x")).unwrap_or_default();
    let intent = build_intent(user.address(), nonce, deadline, &calldata);
    let signature = user.sign_hash_sync(&signing_hash(&intent, domain)).unwrap();
    MetaTxRequest {
        user: user.address(),
        nonce,
        deadline,
        data: data.to_string(),
        signature: signature.to_string(),
    }
}

/// The `POST /domain` response, parsed into the exact parameters we must sign against.
struct RemoteDomain {
    domain: Eip712Domain,
    /// Printed in the banner, so students can see the domain separator binding the contract.
    separator: String,
    forwarder: Address,
    chain_id: u64,
}

/// A clock-derived nonce base, so that running the same demo twice in a row does not collide with
/// the relayer's used-signature registry (which is exactly what a replay looks like).
fn nonce_base() -> u64 {
    (unix_now() * 1_000) % (u64::MAX / 2)
}

async fn fetch_domain(client: &Client, base: &str) -> Result<RemoteDomain, String> {
    let value: serde_json::Value = client
        .get(format!("{base}/domain"))
        .send()
        .await
        .map_err(|e| format!("cannot reach the relayer at {base}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("unexpected /domain response: {e}"))?;

    let name = value["name"]
        .as_str()
        .unwrap_or("HardenedRelayer")
        .to_string();
    let version = value["version"].as_str().unwrap_or("1").to_string();
    let chain_id = match &value["chain_id"] {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
    .ok_or_else(|| "domain response is missing a usable chain_id".to_string())?;
    let forwarder = value["verifying_contract"]
        .as_str()
        .ok_or_else(|| "domain response is missing verifying_contract".to_string())?
        .parse::<Address>()
        .map_err(|e| format!("verifying_contract is not an address: {e}"))?;

    Ok(RemoteDomain {
        domain: domain_named(name, version, chain_id, forwarder),
        separator: value["domain_separator"]
            .as_str()
            .unwrap_or("?")
            .to_string(),
        forwarder,
        chain_id,
    })
}

// =======================================================================================
// Payload construction
// =======================================================================================

/// Build the list of submissions for a mode. `base_nonce` is derived from the clock so that
/// running the same demo twice in a row does not collide with the relayer's replay registry.
fn build(mode: Mode, count: usize, domain: &Eip712Domain, base_nonce: u64) -> Vec<Payload> {
    let now = unix_now();
    let user = PrivateKeySigner::random();
    let attacker = PrivateKeySigner::random();
    let honest = |nonce: u64| Payload {
        body: signed(&user, domain, nonce, now + DEADLINE_SECS, "0xdeadbeef"),
        attack: false,
    };

    match mode {
        Mode::Honest => (0..count as u64).map(|i| honest(base_nonce + i)).collect(),

        Mode::Replay => {
            // One intent, one signature, submitted `count` times. If the relayer relays it more
            // than once, gas is gone. The first submission is indistinguishable from legitimate
            // traffic -- it is the *duplicates* that are the attack, and `expected` below encodes
            // exactly that.
            let one = honest(base_nonce);
            let body = one.body.clone();
            (0..count)
                .map(|i| Payload {
                    body: body.clone(),
                    attack: i != 0,
                })
                .collect()
        }

        Mode::NonceReuse => (0..count as u64)
            .map(|i| Payload {
                // Same nonce, different calldata => different digest, same spent nonce.
                body: signed(
                    &user,
                    domain,
                    base_nonce,
                    now + DEADLINE_SECS,
                    &format!("0xdeadbeef{i:02x}"),
                ),
                attack: i != 0,
            })
            .collect(),

        Mode::Malleable => (0..count as u64)
            .map(|i| {
                let mut body = signed(
                    &user,
                    domain,
                    base_nonce + i,
                    now + DEADLINE_SECS,
                    "0xdeadbeef",
                );
                let sig: alloy::primitives::Signature = body.signature.parse().unwrap();
                body.signature = malleable_twin(&sig).to_string();
                Payload { body, attack: true }
            })
            .collect(),

        Mode::BadSig => (0..count as u64)
            .map(|i| {
                let request = signed(
                    &user,
                    domain,
                    base_nonce + i,
                    now + DEADLINE_SECS,
                    "0xdeadbeef",
                );
                // The signature is perfectly valid -- for somebody else. The `user` field is a
                // claim, not a fact.
                let forged = build_intent(
                    user.address(),
                    base_nonce + i,
                    now + DEADLINE_SECS,
                    &alloy::hex::decode("deadbeef").unwrap(),
                );
                let mut body = request;
                body.signature = attacker
                    .sign_hash_sync(&signing_hash(&forged, domain))
                    .unwrap()
                    .to_string();
                Payload { body, attack: true }
            })
            .collect(),

        Mode::Expired => (0..count as u64)
            .map(|i| Payload {
                body: signed(&user, domain, base_nonce + i, now - 60, "0xdeadbeef"),
                attack: true,
            })
            .collect(),

        Mode::FarDeadline => (0..count as u64)
            .map(|i| Payload {
                body: signed(
                    &user,
                    domain,
                    base_nonce + i,
                    now + 31_536_000,
                    "0xdeadbeef",
                ),
                attack: true,
            })
            .collect(),

        Mode::EmptyCalldata => (0..count as u64)
            .map(|i| Payload {
                body: signed(&user, domain, base_nonce + i, now + DEADLINE_SECS, "0x"),
                attack: true,
            })
            .collect(),

        Mode::Garbage => (0..count as u64)
            .map(|i| {
                let mut body = signed(
                    &user,
                    domain,
                    base_nonce + i,
                    now + DEADLINE_SECS,
                    "0xdeadbeef",
                );
                body.data = "0xnot-hex".to_string();
                Payload { body, attack: true }
            })
            .collect(),

        Mode::Dump => vec![honest(base_nonce)],

        Mode::Mixed => {
            // Every attack, interleaved with legitimate traffic, so a defence that rejects
            // *everything* scores no better than one that rejects nothing.
            let attacks = [
                Mode::Replay,
                Mode::NonceReuse,
                Mode::Malleable,
                Mode::BadSig,
                Mode::Expired,
                Mode::FarDeadline,
                Mode::EmptyCalldata,
                Mode::Garbage,
            ];
            let each = (count / (attacks.len() + 1)).max(1);
            let mut out = Vec::new();
            for (round, attack) in attacks.iter().enumerate() {
                out.extend(build(
                    *attack,
                    each,
                    domain,
                    base_nonce + (round as u64 + 1) * 1_000,
                ));
                out.extend(
                    (0..each)
                        .map(|i| honest(base_nonce + (round as u64 + 1) * 1_000 + 500 + i as u64)),
                );
            }
            out
        }
    }
}

// =======================================================================================
// Submission + scoreboard
// =======================================================================================

/// Tallies of what the relayer answered, so the exercise grades itself.
#[derive(Default)]
struct Tally {
    by_status: std::sync::Mutex<BTreeMap<u16, u64>>,
    /// Submissions that never got an HTTP answer at all (relayer down, wrong port, ...).
    transport_failures: AtomicU64,
    /// Every 2xx the relayer returned.
    accepted: AtomicU64,
}

impl Tally {
    fn record(&self, status: Option<u16>) {
        match status {
            None => {
                self.transport_failures.fetch_add(1, Ordering::Relaxed);
            }
            Some(code) => {
                *self.by_status.lock().unwrap().entry(code).or_insert(0) += 1;
                if (200..300).contains(&code) {
                    self.accepted.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// `expected` is how many *legitimate* intents this run contains. Anything accepted beyond
    /// that is a defence failure, and the arithmetic is deliberately dumb: a count, not a
    /// per-payload flag, because the relayer decides the race, not the client.
    fn report(&self, mode: Mode, sent: usize, attacks: usize, honest: usize, expected: usize) {
        let by_status = self.by_status.lock().unwrap();
        println!(
            "\n━━━ {} submissions ({} attack / {} honest) ━━━",
            sent, attacks, honest
        );
        for (code, count) in by_status.iter() {
            println!("  {code} {:<24} {count}", reason_phrase(*code));
        }
        let failures = self.transport_failures.load(Ordering::Relaxed);
        if failures > 0 {
            println!("  --  {:<24} {failures}", "no HTTP response");
        }

        if mode.is_attack() {
            let accepted = self.accepted.load(Ordering::Relaxed) as usize;
            let leaked = accepted.saturating_sub(expected);
            if leaked == 0 {
                println!(
                    "  ✅ DEFENDER WINS: {accepted} accepted, of which 0 were attacks -- nothing reached the chain that should not have."
                );
            } else {
                println!(
                    "  🔥 HACKER WINS: {leaked} attack intents were accepted (at most {expected} were legitimate)."
                );
            }
        }
        if honest > 0 {
            let accepted = self.accepted.load(Ordering::Relaxed) as usize;
            if accepted < expected {
                println!(
                    "  ℹ️  {} intents were refused as well -- read the reason above (and in /metrics). That is correct behaviour when the destination would revert.",
                    expected - accepted
                );
            } else {
                println!("  ✅ legitimate traffic unaffected ({accepted} benign intents relayed).");
            }
        }
        println!(
            "\n  scoreboard: curl -s 127.0.0.1:3000/metrics | grep -E 'rejected|reverted|gas_burned'"
        );
        println!(
            "  chain check: cast block-number   # compare with `relayer_transactions_total{{status=\"dispatched\"}}`"
        );
    }
}

fn reason_phrase(code: u16) -> &'static str {
    match code {
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "",
    }
}

#[tokio::main]
async fn main() {
    // Usage: [honest|hacker] <mode> [count]. A leading `hacker` is accepted because
    // `cargo run --bin simulator -- hacker mixed 30` reads better on a slide.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|a| a.as_str()) == Some("hacker") {
        args.remove(0);
    }
    let mode = args
        .first()
        .and_then(|raw| Mode::parse(raw))
        .unwrap_or(Mode::Honest);
    let count = args
        .get(1)
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(DEFAULT_COUNT);

    let base = traffic_simulator::config::env_or("RELAYER_URL", "http://127.0.0.1:3000");
    let client = Arc::new(Client::new());

    let remote = match fetch_domain(&client, &base).await {
        Ok(remote) => remote,
        Err(err) => {
            eprintln!("❌ {err}");
            eprintln!("   is the relayer running?  cargo run   (and: curl {base}/health)");
            std::process::exit(1);
        }
    };

    // `dump` is a *build tool*, not traffic. It writes exactly one correctly signed body to stdout
    // — so `> /tmp/intent.json` needs no cleanup — and every human-readable hint to stderr. This is
    // the building block for every hand-run `curl` experiment in the runbook.
    if mode == Mode::Dump {
        let base_nonce = nonce_base();
        let payloads = build(Mode::Dump, 1, &remote.domain, base_nonce);
        println!(
            "{}",
            serde_json::to_string_pretty(&payloads[0].body).expect("intent is serializable")
        );
        eprintln!("# signed for {base}/submit");
        eprintln!("#   forwarder        {}", remote.forwarder);
        eprintln!("#   chain id         {}", remote.chain_id);
        eprintln!("#   domain separator {}", remote.separator);
        eprintln!("#   nonce            {base_nonce}");
        eprintln!("#");
        eprintln!("# send it twice, then restart the relayer and send it a third time:");
        eprintln!("#   cargo run --bin simulator -- dump > /tmp/intent.json");
        eprintln!(
            "#   curl -s -X POST -H 'content-type: application/json' -d @/tmp/intent.json {base}/submit   # 202"
        );
        eprintln!(
            "#   curl -s -X POST -H 'content-type: application/json' -d @/tmp/intent.json {base}/submit   # 409 replayed"
        );
        eprintln!("#   <restart the relayer>");
        eprintln!(
            "#   curl -s -X POST -H 'content-type: application/json' -d @/tmp/intent.json {base}/submit   # 202 again <- the gap"
        );
        return;
    }

    println!("target            {base}/submit");
    println!("forwarder         {}", remote.forwarder);
    println!("domain separator  {}", remote.separator);

    // A clock-derived nonce base: run the same demo twice and the second run does not look like
    // a replay attack.
    let base_nonce = nonce_base();
    let payloads = build(mode, count, &remote.domain, base_nonce);
    let attacks = payloads.iter().filter(|p| p.attack).count();
    let honest = payloads.len() - attacks;

    // How many *successful* 2xx responses constitute a win for the defender. `Replay` and
    // `NonceReuse` send one legitimate intent plus copies, so exactly one acceptance is correct.
    let expected = match mode {
        Mode::Honest => count,
        Mode::Replay | Mode::NonceReuse => 1,
        Mode::Mixed => honest,
        _ => 0,
    };

    println!(
        "mode              {mode:?} ({})",
        if mode.is_attack() {
            "adversarial"
        } else {
            "honest"
        }
    );
    println!("first nonce       {base_nonce}");
    println!("expected accepts  {expected}\n");

    let tally = Arc::new(Tally::default());
    let url = format!("{base}/submit");
    let mut handles = Vec::with_capacity(payloads.len());

    for payload in payloads {
        let client = Arc::clone(&client);
        let tally = Arc::clone(&tally);
        let url = url.clone();
        handles.push(task::spawn(async move {
            let response = client.post(url).json(&payload.body).send().await;
            let status = match response {
                Ok(response) => Some(response.status().as_u16()),
                Err(_) => None,
            };
            tally.record(status);
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }

    tally.report(mode, attacks + honest, attacks, honest, expected);
}
