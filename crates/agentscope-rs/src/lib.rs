//! One-dependency facade for the AgentScope Rust workspace.
//!
//! This crate holds no logic of its own. It re-exports the sibling crates under short module
//! names, so a downstream project can depend on a single crate and reach every part of the
//! framework through `agentscope_rs::<module>`, while Cargo features decide what actually gets
//! compiled. Each module section of the documentation on docs.rs is the re-exported crate's own
//! documentation.
//!
//! # Modules
//!
//! | Feature | Module | Backing crate |
//! | --- | --- | --- |
//! | `core` (default) | [`core`] | `agentscope-core` |
//! | `model` (default, via `harness`) | [`model`] | `agentscope-extensions-model` |
//! | `harness` (default) | [`harness`] | `agentscope-harness` |
//! | `protocol` | [`protocol`] | `agentscope-extensions-protocol` (umbrella over the four below) |
//! | `a2a-client` | [`a2a_client`] | `agentscope-extensions-a2a-client` |
//! | `a2a-server` | [`a2a_server`] | `agentscope-extensions-a2a-server` |
//! | `agent-protocol` | [`agent_protocol`] | `agentscope-extensions-agent-protocol` |
//! | `agui` | [`agui`] | `agentscope-extensions-agui` |
//! | `chat-completions-web` | [`chat_completions_web`] | `agentscope-extensions-chat-completions-web` |
//! | `acp` | [`acp`] | `agentscope-extensions-acp` |
//! | `rag` | [`rag`] | `agentscope-extensions-rag` |
//! | `sandbox` | [`sandbox`] | `agentscope-extensions-sandbox` |
//! | `channel` | [`channel`] | `agentscope-extensions-channel` |
//! | `mem` | [`mem`] | `agentscope-extensions-mem` |
//! | `controlplane` | [`controlplane`] | `agentscope-extensions-controlplane` |
//! | `studio` | [`studio`] | `agentscope-extensions-studio` |
//! | `skills` | [`skills`] | `agentscope-extensions-skills` |
//! | `scheduler` | [`scheduler`] | `agentscope-extensions-scheduler` |
//! | `training` | [`training`] | `agentscope-extensions-training` |
//! | `nacos` | [`nacos`] | `agentscope-extensions-nacos` |
//! | `higress` | [`higress`] | `agentscope-extensions-higress` |
//! | `judge` | [`judge`] | `agentscope-extensions-judge` |
//! | `jdbc` | [`jdbc`] | `agentscope-extensions-jdbc` |
//! | `mysql` | [`mysql`] | `agentscope-extensions-mysql` |
//! | `postgresql` | [`postgresql`] | `agentscope-extensions-postgresql` |
//! | `mongodb` | [`mongodb`] | `agentscope-extensions-mongodb` |
//! | `redis` | [`redis`] | `agentscope-extensions-redis` |
//! | `cos` | [`cos`] | `agentscope-extensions-cos` |
//! | `oss` | [`oss`] | `agentscope-extensions-oss` |
//! | `full` | all of the above | convenience superset |
//!
//! # Usage
//!
//! ```toml
//! [dependencies]
//! agentscope-rs = { version = "0.1.0", features = ["acp", "rag"] }
//! ```
//!
//! ```ignore
//! use agentscope_rs::core::agent::ReActAgent;
//! use agentscope_rs::harness::agent::HarnessAgent;
//! ```
//!
//! # Choosing what to compile
//!
//! The default features are `core` + `harness` (which pulls in `model`): no database driver and
//! no HTTP client stack. `full` enables every module and is meant for exploration or for
//! examples that exercise the whole workspace, not for production dependencies.
//!
//! Features on a backing crate that are not mirrored here (driver backends such as
//! `agentscope-extensions-mysql/mysql_async`, the bundled PDF/DOCX readers of `rag`, the
//! embedded `lite` build of `harness`/`acp`, ...) stay reachable by depending on that crate
//! directly: Cargo unifies features across the graph, so adding both dependencies is safe.

#[cfg(feature = "core")]
pub use agentscope_core as core;
#[cfg(feature = "model")]
pub use agentscope_extensions_model as model;
#[cfg(feature = "harness")]
pub use agentscope_harness as harness;

#[cfg(feature = "a2a-client")]
pub use agentscope_extensions_a2a_client as a2a_client;
#[cfg(feature = "a2a-server")]
pub use agentscope_extensions_a2a_server as a2a_server;
#[cfg(feature = "agent-protocol")]
pub use agentscope_extensions_agent_protocol as agent_protocol;
#[cfg(feature = "agui")]
pub use agentscope_extensions_agui as agui;
#[cfg(feature = "chat-completions-web")]
pub use agentscope_extensions_chat_completions_web as chat_completions_web;
#[cfg(feature = "protocol")]
pub use agentscope_extensions_protocol as protocol;

#[cfg(feature = "acp")]
pub use agentscope_extensions_acp as acp;
#[cfg(feature = "channel")]
pub use agentscope_extensions_channel as channel;
#[cfg(feature = "controlplane")]
pub use agentscope_extensions_controlplane as controlplane;
#[cfg(feature = "higress")]
pub use agentscope_extensions_higress as higress;
#[cfg(feature = "judge")]
pub use agentscope_extensions_judge as judge;
#[cfg(feature = "mem")]
pub use agentscope_extensions_mem as mem;
#[cfg(feature = "nacos")]
pub use agentscope_extensions_nacos as nacos;
#[cfg(feature = "rag")]
pub use agentscope_extensions_rag as rag;
#[cfg(feature = "sandbox")]
pub use agentscope_extensions_sandbox as sandbox;
#[cfg(feature = "scheduler")]
pub use agentscope_extensions_scheduler as scheduler;
#[cfg(feature = "skills")]
pub use agentscope_extensions_skills as skills;
#[cfg(feature = "studio")]
pub use agentscope_extensions_studio as studio;
#[cfg(feature = "training")]
pub use agentscope_extensions_training as training;

#[cfg(feature = "cos")]
pub use agentscope_extensions_cos as cos;
#[cfg(feature = "jdbc")]
pub use agentscope_extensions_jdbc as jdbc;
#[cfg(feature = "mongodb")]
pub use agentscope_extensions_mongodb as mongodb;
#[cfg(feature = "mysql")]
pub use agentscope_extensions_mysql as mysql;
#[cfg(feature = "oss")]
pub use agentscope_extensions_oss as oss;
#[cfg(feature = "postgresql")]
pub use agentscope_extensions_postgresql as postgresql;
#[cfg(feature = "redis")]
pub use agentscope_extensions_redis as redis;
