//! Harness 侧的优雅关闭装配：状态保存器实现 + 宿主可调用的关闭控制器。
//!
//! - [`GracefulShutdownController`] 是 Java `AgentScopeJvmShutdownHook` 的 Rust
//!   等价物。Rust 没有 JVM，也没有引入 tokio 的 `signal` feature（离线环境不允许
//!   改依赖），因此**信号处理由宿主负责**：宿主（例如
//!   `agentscope-extensions-acp` 的 `spawn_shutdown_handler`）在收到 Ctrl-C /
//!   SIGTERM 后调用 [`GracefulShutdownController::shutdown`] 即可。
//! - [`JsonFileShutdownStateSaver`] / [`StateStoreShutdownStateSaver`] 是两种可直接
//!   使用的状态落点：前者把状态写成 JSON 文件，后者写进已有的
//!   [`AgentStateStore`]。
//!
//! 关闭流程（与 core 的 `GracefulShutdownManager` 一致）：
//!
//! 1. 状态切到 `ShuttingDown`：新请求被拒绝（`ensure_accepting_requests`）。
//! 2. 等待所有活跃请求结束；超时（`shutdown_timeout`）后由监控循环强制中断。
//! 3. 请求全部结束后**先保存状态**，再切到 `Terminated`。
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::error::Result;
use agentscope_core::shutdown::{GracefulShutdownManager, ShutdownStateSaver};
use agentscope_core::state::{AgentState, AgentStateStore};

/// 关闭时额外等待的宽限时间。
///
/// 对齐 Java `AgentScopeJvmShutdownHook.INTERRUPT_GRACE_PERIOD`：等待上限要比
/// `shutdown_timeout` 更长一些，否则监控循环的强制中断可能来不及被观察到。
pub const INTERRUPT_GRACE_PERIOD: Duration = Duration::from_secs(5);

/// 宿主可调用的优雅关闭入口。
///
/// 用法（宿主侧）：
///
/// ```no_run
/// # use agentscope_harness::agent::shutdown::GracefulShutdownController;
/// # async fn demo() {
/// let controller = GracefulShutdownController::global();
/// // 收到 Ctrl-C / SIGTERM 后：
/// let terminated = controller.shutdown().await;
/// # let _ = terminated;
/// # }
/// ```
pub struct GracefulShutdownController {
    manager: Arc<GracefulShutdownManager>,
}

impl GracefulShutdownController {
    /// 绑定到指定的关闭管理器。
    pub fn new(manager: Arc<GracefulShutdownManager>) -> Self {
        Self { manager }
    }

    /// 绑定到进程级单例。
    pub fn global() -> Self {
        Self {
            manager: GracefulShutdownManager::instance_arc(),
        }
    }

    /// 绑定的关闭管理器。
    pub fn manager(&self) -> &Arc<GracefulShutdownManager> {
        &self.manager
    }

    /// 只触发关闭（拒绝新请求），不等待活跃请求结束。
    pub async fn begin_shutdown(&self) -> bool {
        self.manager.perform_graceful_shutdown().await
    }

    /// 触发关闭并等待结束。
    ///
    /// 返回 `true` 表示已经到达 `Terminated`（正常收尾或超时强制中断）。
    /// 等待上限取自配置的 `shutdown_timeout` 再加一个宽限期；`None` 表示无限等待
    /// （此时只有全部请求结束才会终止）。
    pub async fn shutdown(&self) -> bool {
        self.manager.perform_graceful_shutdown().await;
        let timeout = self
            .manager
            .config_snapshot()
            .shutdown_timeout
            .map(|t| t + INTERRUPT_GRACE_PERIOD);
        self.manager.await_termination(timeout).await
    }
}

/// 把 shutdown 时的 agent 状态写成 JSON 文件。
///
/// 落点示例：`{workspace}/.agentscope/shutdown-state.json`。
pub struct JsonFileShutdownStateSaver {
    path: PathBuf,
}

impl JsonFileShutdownStateSaver {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 状态文件的落盘路径。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait::async_trait]
impl ShutdownStateSaver for JsonFileShutdownStateSaver {
    async fn save(&self, state: &AgentState) -> Result<()> {
        let content = serde_json::to_string_pretty(state)?;
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        tokio::fs::write(&self.path, content).await?;
        Ok(())
    }
}

/// 把 shutdown 时的 agent 状态写进已有的 [`AgentStateStore`]。
///
/// 键默认为 `agent_state`（与 checkpoint 中间件一致），会话维度取自状态自身的
/// `(user_id, session_id)`。
pub struct StateStoreShutdownStateSaver {
    store: Arc<dyn AgentStateStore>,
    key: String,
}

impl StateStoreShutdownStateSaver {
    pub fn new(store: Arc<dyn AgentStateStore>) -> Self {
        Self {
            store,
            key: crate::agent::middleware::AGENT_STATE_KEY.to_string(),
        }
    }

    /// 覆盖写入的键。
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }
}

#[async_trait::async_trait]
impl ShutdownStateSaver for StateStoreShutdownStateSaver {
    async fn save(&self, state: &AgentState) -> Result<()> {
        let value = serde_json::to_value(state)?;
        let session_id = state.session_id().to_string();
        let user_id = state.user_id().map(str::to_string);
        self.store
            .save(user_id.as_deref(), &session_id, &self.key, value)
            .await;
        Ok(())
    }
}

/// 便捷构造：把状态保存到 `{dir}/shutdown-state.json`。
pub fn json_file_saver_in(dir: impl AsRef<Path>) -> JsonFileShutdownStateSaver {
    JsonFileShutdownStateSaver::new(dir.as_ref().join("shutdown-state.json"))
}
