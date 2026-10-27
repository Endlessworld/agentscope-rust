//! Global graceful shutdown lifecycle and active request tracking, mirroring
//! `io.agentscope.core.shutdown`.
//!
//! 与 Java 的对应关系：
//!
//! | Java | Rust |
//! |---|---|
//! | `GracefulShutdownManager` | [`GracefulShutdownManager`] |
//! | `GracefulShutdownConfig` | [`GracefulShutdownConfig`] |
//! | `ActiveRequestContext` | [`ActiveRequestInfo`]（登记信息） |
//! | `ShutdownStateSaver` | [`ShutdownStateSaver`] |
//! | `AgentScopeJvmShutdownHook` | 无 JVM 运行时，由宿主调用 [`GracefulShutdownManager::perform_graceful_shutdown`] |
//!
//! Rust 侧没有 JVM shutdown hook，也没有引入 tokio 的 `signal` feature：宿主（例如
//! `agentscope-extensions-acp` 的 `spawn_shutdown_handler`）在收到 Ctrl-C / SIGTERM 后
//! 主动调用 [`GracefulShutdownManager::perform_graceful_shutdown`] +
//! [`GracefulShutdownManager::await_termination`] 即可。
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::error::Result;
use crate::state::AgentState;

/// Global framework shutdown state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownState {
    Running,
    ShuttingDown,
    Terminated,
}

/// Policy for handling incomplete reasoning content on shutdown interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialReasoningPolicy {
    Save,
    Discard,
}

/// Configuration for graceful shutdown behavior.
#[derive(Debug, Clone)]
pub struct GracefulShutdownConfig {
    /// Maximum wait for ongoing operations; `None` waits indefinitely.
    pub shutdown_timeout: Option<std::time::Duration>,
    pub partial_reasoning_policy: PartialReasoningPolicy,
}

impl Default for GracefulShutdownConfig {
    fn default() -> Self {
        Self {
            shutdown_timeout: None,
            partial_reasoning_policy: PartialReasoningPolicy::Save,
        }
    }
}

impl GracefulShutdownConfig {
    pub fn new(
        shutdown_timeout: Option<std::time::Duration>,
        partial_reasoning_policy: PartialReasoningPolicy,
    ) -> Result<Self> {
        if let Some(t) = shutdown_timeout {
            if t.is_zero() {
                return Err(crate::Error::Other(
                    "shutdownTimeout must be > 0 or None".into(),
                ));
            }
        }
        Ok(Self {
            shutdown_timeout,
            partial_reasoning_policy,
        })
    }
}

const STATE_RUNNING: u8 = 0;
const STATE_SHUTTING_DOWN: u8 = 1;
const STATE_TERMINATED: u8 = 2;

/// 超时强制中断监控循环的轮询间隔（配置了超时时的精度）。
const MONITOR_TICK: std::time::Duration = std::time::Duration::from_millis(50);

/// 没有配置超时时的轮询间隔（此时只感知状态变化，代价很低）。
const MONITOR_IDLE_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// 一次活跃请求的登记信息（Java `ActiveRequestContext` 的等价物）。
///
/// Java 的 `ActiveRequestContext` 还持有 `RunControl`（用于中断请求）与绑定的
/// `AgentState`（用于保存状态）。Rust 侧把"中断"交给持有 `RunControl` 的调用方，
/// 这里只保留登记信息：请求 id 与登记时刻。
#[derive(Debug, Clone)]
pub struct ActiveRequestInfo {
    /// 请求的唯一标识（调用方提供）。
    pub request_id: String,
    /// 登记时刻（用于快照与超时诊断）。
    pub started_at: std::time::Instant,
}

/// shutdown 收尾时的状态持久化策略（Java `ShutdownStateSaver`）。
///
/// 实现通过 [`GracefulShutdownManager::bind_state_saver`] 注册；保存失败只应记录
/// 日志，不得掩盖 shutdown 流程本身的错误。
#[async_trait]
pub trait ShutdownStateSaver: Send + Sync {
    /// 持久化给定的 agent 状态快照。
    ///
    /// 传入的状态已经带上 `shutdown_interrupted = true` 标记。
    async fn save(&self, state: &AgentState) -> Result<()>;
}

/// shutdown 收尾时使用的保存器与其状态来源。
#[derive(Clone)]
struct StateSaverBinding {
    saver: Arc<dyn ShutdownStateSaver>,
    state: Arc<Mutex<AgentState>>,
}

/// Global manager for graceful shutdown lifecycle and active request tracking.
pub struct GracefulShutdownManager {
    state: Arc<AtomicU8>,
    config: Arc<RwLock<GracefulShutdownConfig>>,
    /// 活跃请求登记表（id -> 登记信息）。
    ///
    /// 活跃计数以本表长度为准，因此"计数"与"id 集合"永远一致：注销一个 id 的
    /// 同时计数减一，注销一个不存在的 id 不会影响计数。
    active: Arc<Mutex<HashMap<String, ActiveRequestInfo>>>,
    /// 匿名登记（[`GracefulShutdownManager::register_request`]）的 id 队列，
    /// 按 FIFO 与 [`GracefulShutdownManager::unregister_request`] 配对。
    anonymous: Arc<Mutex<VecDeque<String>>>,
    /// 匿名登记的自增序号。
    anonymous_seq: AtomicUsize,
    shutdown_started: Arc<tokio::sync::Mutex<Option<std::time::Instant>>>,
    terminated: Arc<Notify>,
    /// shutdown 收尾时调用的状态保存器。
    state_saver: Arc<RwLock<Option<StateSaverBinding>>>,
    /// 超时强制中断的监控任务句柄（正常收尾时会被 abort，避免 task 泄漏）。
    monitor: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// 监控任务是否已启动（幂等保护）。
    monitor_started: AtomicBool,
    /// 收尾流程（保存状态 → 终止）是否已在进行。
    finalizing: AtomicBool,
}

/// 进程级单例。
static INSTANCE: OnceLock<Arc<GracefulShutdownManager>> = OnceLock::new();

impl Default for GracefulShutdownManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GracefulShutdownManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(STATE_RUNNING)),
            config: Arc::new(RwLock::new(GracefulShutdownConfig::default())),
            active: Arc::new(Mutex::new(HashMap::new())),
            anonymous: Arc::new(Mutex::new(VecDeque::new())),
            anonymous_seq: AtomicUsize::new(0),
            shutdown_started: Arc::new(tokio::sync::Mutex::new(None)),
            terminated: Arc::new(Notify::new()),
            state_saver: Arc::new(RwLock::new(None)),
            monitor: Arc::new(Mutex::new(None)),
            monitor_started: AtomicBool::new(false),
            finalizing: AtomicBool::new(false),
        }
    }

    /// The process-wide singleton instance.
    pub fn instance() -> &'static GracefulShutdownManager {
        INSTANCE
            .get_or_init(|| Arc::new(GracefulShutdownManager::new()))
            .as_ref()
    }

    /// The process-wide singleton as a shareable handle.
    ///
    /// 中间件 / 控制器需要 `Arc` 才能被安全地共享给多个任务。
    pub fn instance_arc() -> Arc<GracefulShutdownManager> {
        INSTANCE
            .get_or_init(|| Arc::new(GracefulShutdownManager::new()))
            .clone()
    }

    pub fn get_state(&self) -> ShutdownState {
        match self.state.load(Ordering::SeqCst) {
            STATE_SHUTTING_DOWN => ShutdownState::ShuttingDown,
            STATE_TERMINATED => ShutdownState::Terminated,
            _ => ShutdownState::Running,
        }
    }

    /// 同步读取当前配置（供 `build()` 这类同步装配路径使用）。
    pub fn config_snapshot(&self) -> GracefulShutdownConfig {
        self.config.read().unwrap().clone()
    }

    pub async fn get_config(&self) -> GracefulShutdownConfig {
        self.config_snapshot()
    }

    pub async fn set_config(&self, config: GracefulShutdownConfig) {
        self.set_config_now(config);
    }

    /// 同步写配置。
    pub fn set_config_now(&self, config: GracefulShutdownConfig) {
        *self.config.write().unwrap() = config;
    }

    pub fn is_accepting_requests(&self) -> bool {
        self.get_state() == ShutdownState::Running
    }

    /// 当前活跃请求数（= 登记表长度）。
    pub fn get_active_request_count(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    pub fn ensure_accepting_requests(&self) -> Result<()> {
        if !self.is_accepting_requests() {
            return Err(crate::Error::ShuttingDown("agent is shutting down".into()));
        }
        Ok(())
    }

    /// 匿名登记一次请求。
    ///
    /// 登记会生成一个内部 id（`anon-<n>`），注销时按 FIFO 配对移除，因此活跃
    /// 计数与 id 集合始终保持一致。
    pub fn register_request(&self) {
        let id = format!(
            "anon-{}",
            self.anonymous_seq.fetch_add(1, Ordering::SeqCst)
        );
        self.anonymous.lock().unwrap().push_back(id.clone());
        self.register_request_with_id(id);
    }

    /// 注销一次匿名登记的请求。
    pub fn unregister_request(&self) {
        let id = { self.anonymous.lock().unwrap().pop_front() };
        if let Some(id) = id {
            self.unregister_request_with_id(&id);
        }
    }

    /// 按 id 登记一次请求（per-request 粒度）。
    ///
    /// 同一个 id 重复登记是幂等的（不会重复计数）。
    pub fn register_request_with_id(&self, request_id: impl Into<String>) -> String {
        let request_id = request_id.into();
        let mut active = self.active.lock().unwrap();
        active
            .entry(request_id.clone())
            .or_insert_with(|| ActiveRequestInfo {
                request_id: request_id.clone(),
                started_at: std::time::Instant::now(),
            });
        request_id
    }

    /// 按 id 注销一次请求。返回是否真的注销掉了一个已登记的请求。
    ///
    /// 注销一个不存在的 id 不会影响计数（也不会触发收尾）。
    pub fn unregister_request_with_id(&self, request_id: &str) -> bool {
        let removed = { self.active.lock().unwrap().remove(request_id).is_some() };
        {
            let mut anonymous = self.anonymous.lock().unwrap();
            anonymous.retain(|id| id != request_id);
        }
        if removed {
            self.maybe_finalize();
        }
        removed
    }

    /// 当前活跃请求的 id 列表（按登记顺序）。
    pub fn active_request_ids(&self) -> Vec<String> {
        self.active_requests_snapshot()
            .into_iter()
            .map(|info| info.request_id)
            .collect()
    }

    /// 当前活跃请求的快照（id + 起始时间，按登记顺序）。
    pub fn active_requests_snapshot(&self) -> Vec<ActiveRequestInfo> {
        let active = self.active.lock().unwrap();
        let mut items: Vec<ActiveRequestInfo> = active.values().cloned().collect();
        items.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.request_id.cmp(&b.request_id))
        });
        items
    }

    /// 指定 id 的请求是否仍在活跃。
    pub fn is_request_active(&self, request_id: &str) -> bool {
        self.active.lock().unwrap().contains_key(request_id)
    }

    /// 绑定 shutdown 收尾时调用的状态保存器。
    ///
    /// `state` 是收尾时要保存的状态来源，通常是 agent 的共享状态句柄
    /// （`ReActAgent::state_handle()`）。保存失败只记录日志，不会掩盖 shutdown
    /// 流程本身的错误。
    pub fn bind_state_saver(
        &self,
        saver: Arc<dyn ShutdownStateSaver>,
        state: Arc<Mutex<AgentState>>,
    ) {
        *self.state_saver.write().unwrap() = Some(StateSaverBinding { saver, state });
    }

    /// 解除绑定的状态保存器。
    pub fn unbind_state_saver(&self) {
        *self.state_saver.write().unwrap() = None;
    }

    /// 立即触发一次状态保存。返回保存过程中的错误（若有）。
    ///
    /// 保存前会把 `shutdown_interrupted` 标记置为 `true`（Java
    /// `ActiveRequestContext.saveState` 的语义）。
    pub async fn save_state_now(&self) -> Option<crate::Error> {
        let binding = { self.state_saver.read().unwrap().clone() };
        let Some(binding) = binding else {
            return None;
        };
        let snapshot = Self::snapshot_for_save(&binding.state);
        match binding.saver.save(&snapshot).await {
            Ok(()) => None,
            Err(err) => {
                tracing::warn!("shutdown 状态保存失败: {err}");
                Some(err)
            }
        }
    }

    /// 生成带 `shutdown_interrupted` 标记的状态快照。
    fn snapshot_for_save(state: &Arc<Mutex<AgentState>>) -> AgentState {
        let mut snapshot = { state.lock().unwrap().clone() };
        snapshot.shutdown_interrupted = true;
        snapshot
    }

    /// Initiate graceful shutdown. Returns `true` if the transition happened.
    pub async fn perform_graceful_shutdown(&self) -> bool {
        loop {
            let cur = self.state.load(Ordering::SeqCst);
            if cur == STATE_TERMINATED {
                return false;
            }
            if self
                .state
                .compare_exchange(cur, STATE_SHUTTING_DOWN, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                if cur == STATE_RUNNING {
                    *self.shutdown_started.lock().await = Some(std::time::Instant::now());
                    tracing::info!(
                        active_requests = self.get_active_request_count(),
                        timeout = ?self.config_snapshot().shutdown_timeout,
                        "优雅关闭已启动"
                    );
                }
                // 超时强制中断的监控循环（幂等）。
                self.start_monitor();
                // 没有活跃请求时立即收尾。
                self.maybe_finalize();
                return true;
            }
        }
    }

    /// 收尾流程：所有请求结束后先保存状态，再置为 `Terminated`。
    ///
    /// 状态保存是异步的，因此这里把收尾放进一个后台任务；等待者通过
    /// [`GracefulShutdownManager::await_termination`] 感知终止。
    fn maybe_finalize(&self) {
        if self.get_state() != ShutdownState::ShuttingDown {
            return;
        }
        if self.get_active_request_count() != 0 {
            return;
        }
        if self.finalizing.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = Arc::clone(&self.state);
        let terminated = Arc::clone(&self.terminated);
        let saver = Arc::clone(&self.state_saver);
        let monitor = Arc::clone(&self.monitor);
        // 没有 tokio 运行时时无法异步保存状态，直接终止——这条兜底路径需要独立的
        // Arc（`state`/`terminated` 已被 `finalize` 移入）。
        let fallback_state = Arc::clone(&state);
        let fallback_terminated = Arc::clone(&terminated);
        let finalize = async move {
            // 1) 请求全部结束：先保存状态（失败只记录日志，不掩盖主流程）。
            //    注意：读锁必须在 await 之前释放，因此这里先取出绑定再进入分支。
            let binding = { saver.read().unwrap().clone() };
            if let Some(binding) = binding {
                let snapshot = Self::snapshot_for_save(&binding.state);
                if let Err(err) = binding.saver.save(&snapshot).await {
                    tracing::warn!("shutdown 状态保存失败: {err}");
                }
            }
            // 2) 再终止。
            state.store(STATE_TERMINATED, Ordering::SeqCst);
            terminated.notify_waiters();
            // 3) 停掉超时监控任务，避免 task 泄漏。
            let monitor_handle = { monitor.lock().unwrap().take() };
            if let Some(handle) = monitor_handle {
                handle.abort();
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(finalize);
            }
            Err(_) => {
                // 没有 tokio 运行时（例如同步上下文）：无法异步保存状态，直接终止。
                fallback_state.store(STATE_TERMINATED, Ordering::SeqCst);
                fallback_terminated.notify_waiters();
            }
        }
    }

    /// 启动超时强制中断的监控循环（幂等）。
    ///
    /// shutdown 开始后按 `shutdown_timeout` 计时；到点仍有活跃请求时强制把状态
    /// 推到 `Terminated`、通知等待者，并记录"强制中断"日志。正常收尾时收尾流程
    /// 会 abort 该任务，不会泄漏。
    fn start_monitor(&self) {
        if self.monitor_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // 没有 tokio 运行时：无法启动监控任务，允许后续重试。
            self.monitor_started.store(false, Ordering::SeqCst);
            return;
        };
        let state = Arc::clone(&self.state);
        let config = Arc::clone(&self.config);
        let active = Arc::clone(&self.active);
        let terminated = Arc::clone(&self.terminated);
        let saver = Arc::clone(&self.state_saver);
        let started = Arc::clone(&self.shutdown_started);
        let task = handle.spawn(async move {
            loop {
                // 配置了超时才需要精细计时，否则用低频轮询即可。
                let configured = { config.read().unwrap().shutdown_timeout };
                let tick = if configured.is_some() {
                    MONITOR_TICK
                } else {
                    MONITOR_IDLE_TICK
                };
                tokio::time::sleep(tick).await;
                if state.load(Ordering::SeqCst) != STATE_SHUTTING_DOWN {
                    // 已经终止：监控结束。
                    break;
                }
                let timeout = { config.read().unwrap().shutdown_timeout };
                let Some(timeout) = timeout else {
                    // 没有配置超时：永不强制中断。
                    continue;
                };
                let begin = { *started.lock().await };
                let Some(begin) = begin else {
                    continue;
                };
                let elapsed = begin.elapsed();
                if elapsed < timeout {
                    continue;
                }
                let count = active.lock().unwrap().len();
                if count == 0 {
                    // 没有活跃请求：由正常收尾路径负责终止。
                    continue;
                }
                tracing::warn!(
                    active_requests = count,
                    elapsed_ms = elapsed.as_millis() as u64,
                    timeout_ms = timeout.as_millis() as u64,
                    "shutdown 超时，强制中断活跃请求并终止"
                );
                // 强制中断前尽力保存一次状态。
                let binding = { saver.read().unwrap().clone() };
                if let Some(binding) = binding {
                    let snapshot = Self::snapshot_for_save(&binding.state);
                    if let Err(err) = binding.saver.save(&snapshot).await {
                        tracing::warn!("shutdown 强制中断时保存状态失败: {err}");
                    }
                }
                state.store(STATE_TERMINATED, Ordering::SeqCst);
                terminated.notify_waiters();
                break;
            }
        });
        *self.monitor.lock().unwrap() = Some(task);
    }

    /// Wait until TERMINATED or the given timeout elapses.
    pub async fn await_termination(&self, timeout: Option<std::time::Duration>) -> bool {
        let wait = async {
            loop {
                // 先注册等待者、再复查状态：既不会漏掉"注册之前已经发生"的终止，
                // 也不会漏掉"复查之后才发生"的终止。
                let notified = self.terminated.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.get_state() == ShutdownState::Terminated {
                    return true;
                }
                notified.await;
            }
        };
        if let Some(t) = timeout {
            tokio::time::timeout(t, wait).await.is_ok()
        } else {
            wait.await;
            true
        }
    }

    /// Reset manager state. Intended for testing and demo purposes only.
    ///
    /// 对齐 Java `GracefulShutdownManager.resetForTesting`：状态复位为 `Running`，
    /// 活跃计数与 id 集合清空，并停掉超时监控任务。
    pub fn reset_for_testing(&self) {
        if let Some(handle) = { self.monitor.lock().unwrap().take() } {
            handle.abort();
        }
        self.monitor_started.store(false, Ordering::SeqCst);
        self.finalizing.store(false, Ordering::SeqCst);
        self.state.store(STATE_RUNNING, Ordering::SeqCst);
        self.active.lock().unwrap().clear();
        self.anonymous.lock().unwrap().clear();
        self.unbind_state_saver();
        if let Ok(mut started) = self.shutdown_started.try_lock() {
            *started = None;
        }
    }
}
