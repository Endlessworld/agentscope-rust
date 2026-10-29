//! Memory tuning and diagnostics for the ACP host processes.
//!
//! One prompt turn allocates several megabyte-sized buffers -- the compiled model request, the
//! journal `state/checkpoint` payload, the base64 wrapper a backend adds around it -- and frees
//! them as soon as the turn is committed. On Windows the CRT hands those blocks straight back to
//! the OS (`VirtualFree` above 512 KiB), which is why mimalloc was **removed** there: it kept a
//! heap per thread and purged on a 1 s timer, so the same workload measured 54 MB peak RSS across
//! 32 threads instead of 4.6 MB. Linux/glibc keeps freed pages in the arena instead, so mimalloc
//! still pays for itself and stays installed on unix.
//!
//! Both the worker cap and the per-turn collect are on by default; the environment tunes them
//! per host. On Windows `collect()` is a no-op because the system allocator already returns the
//! pages, and the footprint is read from `GetProcessMemoryInfo`.
//!
//! | variable | default | effect |
//! |---|---|---|
//! | `AGENTSCOPE_ACP_WORKER_THREADS` | `min(ncpu, 8)` | worker threads of the agent runtime |
//! | `AGENTSCOPE_ACP_MEM_REPORT` | off | also log the serialized `AgentState` size per turn |
//! | `AGENTSCOPE_ACP_MEM_COLLECT` | on | `mi_collect(true)` after every turn; `=0` disables |

const WORKER_THREADS_ENV: &str = "AGENTSCOPE_ACP_WORKER_THREADS";
const MEM_REPORT_ENV: &str = "AGENTSCOPE_ACP_MEM_REPORT";
const MEM_COLLECT_ENV: &str = "AGENTSCOPE_ACP_MEM_COLLECT";

/// A turn is I/O bound (one model stream plus tool calls), so the default worker count is
/// capped: every extra worker costs a thread stack and, under mimalloc, another heap that keeps
/// its own free pages resident.
const DEFAULT_WORKER_THREADS_CAP: usize = 8;

/// Process footprint, in the same four numbers on every platform.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub rss: usize,
    pub peak_rss: usize,
    pub commit: usize,
    pub peak_commit: usize,
}

fn env_flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        ),
        Err(_) => false,
    }
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// An opt-out switch: reads `0`/`false`/`off`/`no` as "disabled".
fn env_off(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => false,
    }
}

/// Worker threads for the agent runtime.
pub fn worker_threads() -> usize {
    if let Some(threads) = env_usize(WORKER_THREADS_ENV) {
        return threads.max(1);
    }
    std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(4)
        .clamp(1, DEFAULT_WORKER_THREADS_CAP)
}

/// The tokio runtime both ACP binaries run on.
///
/// `#[tokio::main]` hard-codes `worker_threads = ncpu`, which on this class of machine means 32
/// threads for a single-conversation stdio agent.
pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads())
        .enable_all()
        .build()
        .expect("failed to build the tokio runtime")
}

/// Current footprint.
#[cfg(unix)]
pub fn usage() -> Usage {
    let (mut elapsed, mut user, mut system) = (0usize, 0usize, 0usize);
    let (mut rss, mut peak_rss, mut commit, mut peak_commit) = (0usize, 0usize, 0usize, 0usize);
    let mut page_faults = 0usize;
    // SAFETY: `mi_process_info` only writes through the pointers it is given, and all of them
    // point at live locals for the duration of the call.
    unsafe {
        libmimalloc_sys::mi_process_info(
            &mut elapsed,
            &mut user,
            &mut system,
            &mut rss,
            &mut peak_rss,
            &mut commit,
            &mut peak_commit,
            &mut page_faults,
        );
    }
    Usage {
        rss,
        peak_rss,
        commit,
        peak_commit,
    }
}

/// Current footprint, straight from the OS.
#[cfg(windows)]
pub fn usage() -> Usage {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: the counters struct is plain data, and `cb` is set to its own size as the API
    // requires before the call.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: `GetCurrentProcess` returns a pseudo handle (no ownership) and the counters
    // pointer is valid for the documented size.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        return Usage::default();
    }
    Usage {
        rss: counters.WorkingSetSize,
        peak_rss: counters.PeakWorkingSetSize,
        commit: counters.PagefileUsage,
        peak_commit: counters.PeakPagefileUsage,
    }
}

/// Returns freed pages to the OS.
#[cfg(unix)]
pub fn collect() {
    // SAFETY: `mi_collect` takes no ownership of Rust memory; `force` only skips mimalloc's own
    // purge timer.
    unsafe { libmimalloc_sys::mi_collect(true) }
}

/// Nothing to do: the system allocator frees large blocks back to the OS on `free`.
#[cfg(windows)]
pub fn collect() {}

pub fn report_enabled() -> bool {
    env_flag(MEM_REPORT_ENV)
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Emits the per-turn `[mem]` line at `INFO`: what the turn needs resident once its buffers are
/// collectable, against what was resident before the collect.
///
/// Always logged -- the footprint is what tells a report from a leak -- except when
/// `AGENTSCOPE_ACP_MEM_COLLECT=0` disabled the collect, in which case the before/after pair would
/// be meaningless. `state_bytes` is only computed by the caller when `AGENTSCOPE_ACP_MEM_REPORT`
/// is set, because it costs a full `AgentState` serialization.
///
/// Both state-derived fields are optional: the caller must not block on the agent state, so when
/// the lock is contended it reports no `messages`/`state` at all rather than stalling the turn.
pub fn note_turn(stop: &str, messages: Option<usize>, state_bytes: Option<usize>) {
    if env_off(MEM_COLLECT_ENV) {
        return;
    }
    let before = usage();
    collect();
    let after = usage();
    tracing::info!(
        "{}",
        format_turn(stop, messages, state_bytes, before, after)
    );
}

fn format_turn(
    stop: &str,
    messages: Option<usize>,
    state_bytes: Option<usize>,
    before: Usage,
    after: Usage,
) -> String {
    let messages = match messages {
        Some(count) => format!(" messages={count}"),
        None => String::new(),
    };
    let state = match state_bytes {
        Some(bytes) => format!(" state={:.1}MB", mb(bytes)),
        None => String::new(),
    };
    format!(
        "[mem] turn stop={stop}{messages}{state} \
         | rss {:.1}->{:.1}MB commit {:.1}->{:.1}MB peak_rss {:.1}MB peak_commit {:.1}MB",
        mb(before.rss),
        mb(after.rss),
        mb(before.commit),
        mb(after.commit),
        mb(after.peak_rss),
        mb(after.peak_commit),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FFI declarations must resolve and report a live process footprint; a bad signature
    /// (wrong argument count, wrong `bool` ABI, wrong struct size) would show up as a zero here.
    #[test]
    fn usage_reports_the_process_footprint() {
        // Allocate something so rss cannot be zero for a trivial reason.
        let buffer = vec![7u8; 4 * 1024 * 1024];
        std::hint::black_box(&buffer);
        let usage = usage();
        assert!(usage.rss > 0, "rss must be reported: {usage:?}");
        assert!(
            usage.peak_rss >= usage.rss,
            "peak rss lags the current rss: {usage:?}"
        );
        assert!(
            usage.peak_commit >= usage.commit,
            "peak commit lags the current commit: {usage:?}"
        );
    }

    /// Collecting must be safe to call on any thread and must not disturb the footprint above
    /// the current high-water mark.
    #[test]
    fn collect_is_idempotent() {
        drop(vec![0u8; 2 * 1024 * 1024]);
        collect();
        collect();
        assert!(usage().peak_rss > 0);
    }

    /// The line hosts grep for is stable: one `[mem] turn` prefix, then before/after pairs.
    #[test]
    fn the_turn_line_renders_before_and_after() {
        let before = Usage {
            rss: 480 * 1024 * 1024,
            peak_rss: 600 * 1024 * 1024,
            commit: 950 * 1024 * 1024,
            peak_commit: 1100 * 1024 * 1024,
        };
        let after = Usage {
            rss: 96 * 1024 * 1024,
            ..before
        };
        let line = format_turn(
            "StopReason::EndTurn",
            Some(118),
            Some(3 * 1024 * 1024),
            before,
            after,
        );
        assert_eq!(
            line,
            "[mem] turn stop=StopReason::EndTurn messages=118 state=3.0MB | rss 480.0->96.0MB \
             commit 950.0->950.0MB peak_rss 600.0MB peak_commit 1100.0MB"
        );
    }

    /// Without the report flag the expensive `state=` field is absent; the footprint stays.
    #[test]
    fn the_turn_line_omits_state_without_the_report_flag() {
        let before = Usage {
            rss: 143 * 1024 * 1024,
            peak_rss: 143 * 1024 * 1024,
            commit: 228 * 1024 * 1024,
            peak_commit: 228 * 1024 * 1024,
        };
        let line = format_turn("StopReason::EndTurn", Some(118), None, before, before);
        assert_eq!(
            line,
            "[mem] turn stop=StopReason::EndTurn messages=118 | rss 143.0->143.0MB \
             commit 228.0->228.0MB peak_rss 143.0MB peak_commit 228.0MB"
        );
    }

    /// A contended agent-state lock must cost the turn nothing: the footprint is still reported,
    /// the state-derived fields are simply absent.
    #[test]
    fn the_turn_line_survives_a_contended_state_lock() {
        let usage = Usage {
            rss: 143 * 1024 * 1024,
            peak_rss: 143 * 1024 * 1024,
            commit: 228 * 1024 * 1024,
            peak_commit: 228 * 1024 * 1024,
        };
        let line = format_turn("StopReason::EndTurn", None, None, usage, usage);
        assert_eq!(
            line,
            "[mem] turn stop=StopReason::EndTurn | rss 143.0->143.0MB \
             commit 228.0->228.0MB peak_rss 143.0MB peak_commit 228.0MB"
        );
    }

    /// The line must actually reach a subscriber at the default `fmt` level (INFO), not just be
    /// formatted: a wrong level would make the per-turn footprint invisible.
    #[test]
    fn the_turn_line_is_emitted_at_info() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<u8>>>);

        impl Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
            type Writer = Capture;
            fn make_writer(&'a self) -> Capture {
                self.clone()
            }
        }

        let sink = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            note_turn("StopReason::EndTurn", Some(7), Some(1024 * 1024));
        });
        let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("[mem] turn stop=StopReason::EndTurn messages=7 state=1.0MB"),
            "captured: {out}"
        );
    }
}
