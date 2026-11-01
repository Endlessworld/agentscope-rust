//! Memory tuning and diagnostics for the ACP host processes.
//!
//! One prompt turn allocates several megabyte-sized buffers -- the compiled model request, the
//! journal `state/checkpoint` payload, the base64 wrapper a backend adds around it -- and frees
//! them as soon as the turn is committed. Large blocks go back to the OS on `free` everywhere
//! (`VirtualFree` above 512 KiB on Windows), which is why mimalloc was **removed** on Windows: it
//! kept a heap per thread and purged on a 1 s timer, so the same workload measured 54 MB peak RSS
//! across 32 threads instead of 4.6 MB. Linux/glibc keeps freed pages in the arena instead, so
//! mimalloc still pays for itself and stays installed on unix.
//!
//! What does *not* come back on its own is what the heap kept from the many small allocations made
//! while the agent is built: measured on Windows, the process settles at ~26 MB RSS / 29 MB commit
//! within 70 ms of `main` and stays there whether or not a turn ever runs. Two knobs hand that
//! back -- [`collect`] (`HeapCompact` after every turn) and the idle trim, which drops the working
//! set once the conversation has been quiet for `AGENTSCOPE_ACP_MEM_TRIM_IDLE` seconds.
//!
//! | variable | default | effect |
//! |---|---|---|
//! | `AGENTSCOPE_ACP_WORKER_THREADS` | `min(ncpu, 8)` | worker threads of the agent runtime |
//! | `AGENTSCOPE_ACP_MEM_REPORT` | off | also log the serialized `AgentState` size per turn |
//! | `AGENTSCOPE_ACP_MEM_COLLECT` | on | `HeapCompact` / `mi_collect(true)` after every turn; `=0` disables |
//! | `AGENTSCOPE_ACP_MEM_TRIM_IDLE` | `3` | seconds of quiet before the working set is dropped; `=0` disables |
//!
//! Build with `--features alloc-stats` to install a counting allocator: the `[mem]` lines then also
//! report live heap bytes, which is what tells real data apart from pages the allocator kept after
//! freeing them.

use std::sync::atomic::{AtomicU64, Ordering};

const WORKER_THREADS_ENV: &str = "AGENTSCOPE_ACP_WORKER_THREADS";
const MEM_REPORT_ENV: &str = "AGENTSCOPE_ACP_MEM_REPORT";
const MEM_COLLECT_ENV: &str = "AGENTSCOPE_ACP_MEM_COLLECT";
const MEM_TRIM_IDLE_ENV: &str = "AGENTSCOPE_ACP_MEM_TRIM_IDLE";

/// Seconds of quiet before the working set is dropped. Short enough to trim between turns of an
/// interactive session, long enough that a burst of tool calls does not pay the page faults.
const DEFAULT_TRIM_IDLE_SECONDS: usize = 3;

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

/// Decommits the free pages the heap kept.
///
/// The system allocator already `VirtualFree`s large blocks on `free`, but the hundreds of small
/// allocations made while the agent is built leave their pages committed. `HeapCompact` walks the
/// heap and decommits the free runs, which is the only way to shrink that baseline.
#[cfg(windows)]
pub fn collect() {
    use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapCompact};

    // SAFETY: `GetProcessHeap` returns the process-heap pseudo handle (no ownership), and
    // `HeapCompact` only releases free memory inside that heap.
    unsafe {
        HeapCompact(GetProcessHeap(), 0);
    }
}

/// Drops the resident pages of the whole process until they are touched again.
///
/// This is the idle trim: nothing is freed, the OS is simply allowed to take the working set away,
/// so the next turn pays soft faults to bring the image and the heap back.
pub fn trim_working_set() {
    let before = usage();
    // Freed blocks stay committed until the heap coalesces and decommits them, which is what makes
    // a released model catalog (~15 MB) show up as committed memory even though nothing uses it.
    collect();
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::K32EmptyWorkingSet;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        // SAFETY: `GetCurrentProcess` returns a pseudo handle the caller does not own, and the call
        // only affects the working set of this process.
        unsafe {
            K32EmptyWorkingSet(GetCurrentProcess());
        }
    }
    let after = usage();
    tracing::info!(
        "[mem] idle trim | rss {:.1}->{:.1}MB commit {:.1}->{:.1}MB",
        mb(before.rss),
        mb(after.rss),
        mb(before.commit),
        mb(after.commit)
    );
}

/// Bumped by every turn so a pending idle trim knows the conversation is no longer quiet.
static ACTIVITY: AtomicU64 = AtomicU64::new(0);

/// Marks a turn as started, cancelling any idle trim that is already armed.
pub fn note_activity() {
    ACTIVITY.fetch_add(1, Ordering::Relaxed);
}

/// Seconds of quiet before the working set is dropped.
pub fn trim_idle_seconds() -> usize {
    let value = std::env::var(MEM_TRIM_IDLE_ENV).ok();
    parse_trim_idle(value.as_deref())
}

/// `AGENTSCOPE_ACP_MEM_TRIM_IDLE` parsing: unset or unparseable falls back to the default, `0`
/// disables the trim.
fn parse_trim_idle(value: Option<&str>) -> usize {
    value
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_TRIM_IDLE_SECONDS)
}

/// Arms the idle trim: after `AGENTSCOPE_ACP_MEM_TRIM_IDLE` seconds without a new turn, drop the
/// working set. A turn that starts in the meantime bumps [`ACTIVITY`] and cancels this one.
///
/// Must be called from inside a tokio runtime: it spawns the timer task.
pub fn schedule_idle_trim() {
    let seconds = trim_idle_seconds();
    if seconds == 0 {
        return;
    }
    let armed = ACTIVITY.load(Ordering::Relaxed);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(seconds as u64)).await;
        if ACTIVITY.load(Ordering::Relaxed) == armed {
            trim_working_set();
        }
    });
}

/// Emits the `[mem] <stage>` footprint line at `INFO`.
///
/// With `--features alloc-stats` the line also carries the live heap: that is what tells a real
/// baseline apart from pages the allocator kept after freeing them.
pub fn log_footprint(stage: &str) {
    let usage = usage();
    match live_heap() {
        Some((live, peak_live, blocks)) => tracing::info!(
            "[mem] {stage} | rss {:.1}MB commit {:.1}MB peak_rss {:.1}MB | live {:.1}MB \
             peak_live {:.1}MB blocks {blocks}",
            mb(usage.rss),
            mb(usage.commit),
            mb(usage.peak_rss),
            mb(live),
            mb(peak_live)
        ),
        None => tracing::info!(
            "[mem] {stage} | rss {:.1}MB commit {:.1}MB peak_rss {:.1}MB",
            mb(usage.rss),
            mb(usage.commit),
            mb(usage.peak_rss)
        ),
    }
}

/// Live heap bytes, peak live bytes and live block count when the counting allocator is installed.
#[cfg(feature = "alloc-stats")]
pub fn live_heap() -> Option<(usize, usize, usize)> {
    Some(counting::snapshot())
}

/// No live-heap numbers without `--features alloc-stats`.
#[cfg(not(feature = "alloc-stats"))]
pub fn live_heap() -> Option<(usize, usize, usize)> {
    None
}

/// Diagnostic allocator: wraps [`std::alloc::System`] and tracks live bytes.
///
/// Installed by the two binaries instead of mimalloc under `--features alloc-stats`. It is a
/// measurement tool, not a default: the atomics on the hot path cost throughput.
#[cfg(feature = "alloc-stats")]
pub mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    static LIVE: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);
    static BLOCKS: AtomicUsize = AtomicUsize::new(0);

    fn add(bytes: usize) {
        let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
        BLOCKS.fetch_add(1, Relaxed);
        if live > PEAK.load(Relaxed) {
            PEAK.store(live, Relaxed);
        }
    }

    fn sub(bytes: usize) {
        LIVE.fetch_sub(bytes, Relaxed);
        BLOCKS.fetch_sub(1, Relaxed);
    }

    /// `(live bytes, peak live bytes, live blocks)`.
    pub fn snapshot() -> (usize, usize, usize) {
        (LIVE.load(Relaxed), PEAK.load(Relaxed), BLOCKS.load(Relaxed))
    }

    /// The counting global allocator.
    pub struct CountingAlloc;

    // SAFETY: every method forwards to `System` unchanged; the counters only observe the layout
    // that was passed in, so they cannot disagree with the allocation they describe.
    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = System.alloc(layout);
            if !ptr.is_null() {
                add(layout.size());
            }
            ptr
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = System.alloc_zeroed(layout);
            if !ptr.is_null() {
                add(layout.size());
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            sub(layout.size());
            System.dealloc(ptr, layout)
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let out = System.realloc(ptr, layout, new_size);
            if !out.is_null() {
                sub(layout.size());
                add(new_size);
            }
            out
        }
    }
}

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

    /// The idle trim defaults to 3 s; `0` is the documented off switch and a garbage value must not
    /// turn the trim off silently.
    #[test]
    fn the_idle_trim_defaults_to_three_seconds() {
        assert_eq!(parse_trim_idle(None), 3);
        assert_eq!(parse_trim_idle(Some("0")), 0);
        assert_eq!(parse_trim_idle(Some("30")), 30);
        assert_eq!(parse_trim_idle(Some("  12 ")), 12);
        assert_eq!(parse_trim_idle(Some("off")), 3);
    }

    /// Dropping the working set must be safe to call on any thread and must not free anything.
    #[test]
    fn trimming_the_working_set_is_safe() {
        let buffer = vec![9u8; 8 * 1024 * 1024];
        std::hint::black_box(&buffer);
        trim_working_set();
        assert_eq!(buffer.len(), 8 * 1024 * 1024);
        assert_eq!(buffer[0], 9);
    }

    /// `collect()` is the per-turn hook: it must be callable repeatedly and must not disturb the
    /// process (a failure here would only show up as a crash under the heap lock).
    #[test]
    fn collect_survives_repeated_calls() {
        drop(vec![0u8; 4 * 1024 * 1024]);
        collect();
        collect();
        assert!(usage().peak_rss > 0);
    }

    /// A turn cancels a pending trim by bumping the activity generation.
    #[test]
    fn a_new_turn_cancels_the_armed_trim() {
        note_activity();
        let armed = ACTIVITY.load(Ordering::Relaxed);
        note_activity();
        assert_ne!(armed, ACTIVITY.load(Ordering::Relaxed));
    }

    /// The counting allocator is only installed under `--features alloc-stats`; without it the
    /// footprint line must simply omit the live-heap fields instead of reporting zeros.
    #[cfg(not(feature = "alloc-stats"))]
    #[test]
    fn live_heap_is_absent_without_the_feature() {
        assert_eq!(live_heap(), None);
    }

    #[cfg(feature = "alloc-stats")]
    #[test]
    fn live_heap_is_reported_when_the_feature_is_on() {
        // The counting allocator is installed by the two binaries -- a library cannot own the
        // process-wide `#[global_allocator]` -- so this binary only checks that the accessor
        // reports numbers instead of `None`.
        let (live, peak, _) = live_heap().expect("feature on");
        assert!(peak >= live, "peak {peak} < live {live}");
    }
}
