//! # Day 4 — "The Hardened Relayer: Adversarial Design"
//!
//! Days 2 and 3 were about *functionality*. This module tree is about *survival*.
//!
//! A relayer is a honey-pot: it holds a funded key and signs transactions on behalf of
//! strangers. Every module below exists to make one class of attack unprofitable.
//!
//! | Module | Attack it defends against |
//! |---|---|
//! | [`intent`] | Forged, malleable, expired, unbounded, or calldata-swapped signatures (EIP-712) |
//! | [`replay`] | The same signed intent being relayed twice (replay) — *including* nonce-reuse with a fresh digest |
//! | [`dry_run`] | "Griefing": a perfectly signed intent whose on-chain call reverts, so the **relayer** pays for the failure |
//! | [`secure_key`] | Key exfiltration: the "God Key" never lives in a global, is never `Debug`-printed, and is wiped on drop |
//! | [`metrics`] | Blindness: you cannot tell whether you are being drained. Every rejection is counted |
//! | [`config`] | Misparenthesis: pointing the broadcaster at a public mempool by accident |
//!
//! The rule students should leave with: *the compiler checks types, not intent.*

pub mod config;
pub mod dry_run;
pub mod intent;
pub mod metrics;
pub mod replay;
pub mod secure_key;

pub use intent::{
    Intent, MetaTxRequest, Reject, VerifiedIntent, domain, domain_named, verify_intent,
};
pub mod keeper;
