//! Embedded JavaScript (rquickjs) script engine that exposes `Toolkit` tools as `tools.<name>`.
//!
//! QuickJS itself provides **no** filesystem or network APIs; host capabilities are supplied
//! by tools registered on the toolkit (see [`FsSandbox`] and [`HttpRequestTool`]).

#[cfg(feature = "js")]
pub mod engine;
pub mod fs;
pub mod http;
#[cfg(feature = "js")]
pub mod limits;
#[cfg(feature = "js")]
pub mod run_script;

#[cfg(feature = "js")]
pub use engine::ScriptEngine;
pub use fs::FsSandbox;
pub use http::{HttpConfig, HttpRequestTool};
#[cfg(feature = "js")]
pub use limits::ScriptLimits;
#[cfg(feature = "js")]
pub use run_script::RunScriptTool;
