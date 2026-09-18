//! `agentscope-extensions-nacos` — Nacos-backed configuration / registry-center
//! extensions, mirroring `agentscope-extensions-nacos` (sub-modules
//! `nacos-a2a`, `nacos-prompt`, `nacos-skill`).
//!
//! Unlike the storage extensions (JDBC / Redis / MongoDB / …), this crate does
//! **not** implement `DistributedStore`. It provides:
//! * [`a2a`] — publish A2A AgentCards + endpoints to Nacos and resolve AgentCards
//!   from a Nacos registry.
//! * [`prompt`] — subscribe to Nacos prompts (with local caching + templating).
//! * [`skill`] — a read-only [`agentscope_harness::agent::skill::SkillRepository`]
//!   backed by Nacos skill ZIP packages.
//!
//! All three sub-modules talk to Nacos through the [`service::NacosAiService`]
//! trait. There is no official Rust Nacos AI SDK, so the trait is the injection
//! seam (mirroring Java's `com.alibaba.nacos.api.ai.AiService`): embedders supply
//! a real HTTP client, tests use [`service::InMemoryNacosAiService`].
//!
//! See `docs/extensions/NACOS__GAP_TRACKER.md` for the migration mapping.

pub mod a2a;
pub mod prompt;
pub mod service;
pub mod skill;

pub use service::{AiServiceError, AiServiceResult, InMemoryNacosAiService, NacosAiService};

