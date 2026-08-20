//! Enhanced agent runtime, mirroring `HarnessAgent`: wires a `Filesystem`, its tools and a
//! middleware chain around the core agent abstraction.
use std::path::PathBuf;
use std::sync::Arc;

use agentscope_core::agent::{ReActAgent, StreamOptions};
use agentscope_core::event::AgentEvent;
use agentscope_core::middleware::MiddlewareChain;
use agentscope_core::model::Model;
use agentscope_core::tool::Toolkit;

use crate::filesystem::{Filesystem, LocalFilesystem};

use super::distributed_store::DistributedStore;

/// A harness agent that operates on a filesystem with a configured toolkit and middleware chain.
pub struct HarnessAgent {
    name: String,
    model: Option<Arc<dyn Model>>,
    filesystem: Arc<dyn Filesystem>,
    toolkit: Toolkit,
    middleware: MiddlewareChain,
    cwd: PathBuf,
    distributed_store: Option<DistributedStore>,
    /// The embedded ReAct agent that drives the reasoning→acting loop.
    /// `None` when no model was configured (assembly-only mode).
    agent: Option<ReActAgent>,
}

impl HarnessAgent {
    pub fn builder() -> HarnessAgentBuilder {
        HarnessAgentBuilder::default()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn model(&self) -> Option<&Arc<dyn Model>> {
        self.model.as_ref()
    }

    pub fn filesystem(&self) -> &dyn Filesystem {
        self.filesystem.as_ref()
    }

    pub fn toolkit(&self) -> &Toolkit {
        &self.toolkit
    }

    pub fn middleware(&self) -> &MiddlewareChain {
        &self.middleware
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    pub fn distributed_store(&self) -> Option<&DistributedStore> {
        self.distributed_store.as_ref()
    }

    /// The embedded ReAct agent driving the reasoning→acting loop, if a model was configured.
    pub fn agent(&self) -> Option<&ReActAgent> {
        self.agent.as_ref()
    }

    /// Streams agent events for the given messages through the embedded ReAct agent.
    ///
    /// This is the primary driver entry: it runs the middleware-wrapped ReAct loop and
    /// yields filtered [`AgentEvent`]s for the given [`StreamOptions`]. Returns a stream
    /// that yields a single error when no model was configured (assembly-only mode).
    pub fn stream_events(
        &self,
        messages: Vec<agentscope_core::message::Msg>,
        options: StreamOptions,
    ) -> futures::stream::BoxStream<'static, agentscope_core::error::Result<AgentEvent>> {
        match &self.agent {
            Some(agent) => agent.stream_events(messages, options),
            None => Box::pin(futures::stream::once(async move {
                Err(agentscope_core::error::Error::Agent(
                    "HarnessAgent has no model configured; cannot run".into(),
                ))
            })),
        }
    }
}
/// Builder for `HarnessAgent`.
#[derive(Default)]
pub struct HarnessAgentBuilder {
    name: Option<String>,
    model: Option<Arc<dyn Model>>,
    filesystem: Option<Arc<dyn Filesystem>>,
    cwd: Option<PathBuf>,
    distributed_store: Option<DistributedStore>,
    with_filesystem_tools: bool,
    middleware: Option<MiddlewareChain>,
    sys_prompt: String,
    max_iters: i32,
}

impl HarnessAgentBuilder {
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn model(mut self, model: Arc<dyn Model>) -> Self {
        self.model = Some(model);
        self
    }

    pub fn filesystem(mut self, fs: Arc<dyn Filesystem>) -> Self {
        self.filesystem = Some(fs);
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn distributed_store(mut self, ds: DistributedStore) -> Self {
        self.distributed_store = Some(ds);
        self
    }

    /// Sets the system prompt prepended to every reasoning call of the embedded ReAct agent.
    pub fn sys_prompt(mut self, sys_prompt: impl Into<String>) -> Self {
        self.sys_prompt = sys_prompt.into();
        self
    }

    /// Sets the maximum reasoning→acting iterations of the embedded ReAct agent.
    pub fn max_iters(mut self, max_iters: i32) -> Self {
        self.max_iters = max_iters;
        self
    }

    /// Registers the standard filesystem tools (`read_file`, `write_file`, ...) into the toolkit.
    pub fn register_filesystem_tools(mut self, on: bool) -> Self {
        self.with_filesystem_tools = on;
        self
    }

    pub fn middleware(mut self, mw: MiddlewareChain) -> Self {
        self.middleware = Some(mw);
        self
    }

    pub fn build(self) -> HarnessAgent {
        let cwd = self
            .cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let filesystem = self
            .filesystem
            .unwrap_or_else(|| Arc::new(LocalFilesystem::new(cwd.clone())));
        let toolkit = Toolkit::new();
        if self.with_filesystem_tools {
            crate::tool::FilesystemTool::register(&toolkit, filesystem.clone());
        }
        let middleware = self.middleware.unwrap_or_default();

        // Build the embedded ReAct agent from the configured model/toolkit/middleware.
        // Only built when a model is provided (assembly-only mode leaves it `None`).
        let agent = self.model.as_ref().map(|model| {
            let mut agent_builder = ReActAgent::builder()
                .name(self.name.clone().unwrap_or_else(|| "agent".to_string()))
                .model(Arc::clone(model))
                .toolkit(toolkit.clone())
                .middleware_chain(middleware.clone());
            if !self.sys_prompt.is_empty() {
                agent_builder = agent_builder.sys_prompt(self.sys_prompt.clone());
            }
            if self.max_iters > 0 {
                agent_builder = agent_builder.max_iters(self.max_iters);
            }
            agent_builder.build().expect("build ReActAgent")
        });

        HarnessAgent {
            name: self.name.unwrap_or_else(|| "agent".to_string()),
            model: self.model,
            filesystem,
            toolkit,
            middleware,
            cwd,
            distributed_store: self.distributed_store,
            agent,
        }
    }
}
