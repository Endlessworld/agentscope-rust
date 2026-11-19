//! Optional heap-attribution probes.
//!
//! This module is inert in production: every entry point returns after a single relaxed atomic
//! load while [`enable`] has not been called. Diagnostics flip it on, register the allocator's
//! live-byte counter via [`register_live`], and then bracket phases with [`mark`].
//!
//! Attribution is **per task**: a phase window belongs to the task that marked it, so an
//! allocation charges only to the work that made it. Windows are stored per thread, but a thread
//! that switches tasks closes the open window first: without that, a phase absorbed every
//! allocation made by unrelated work that merely ran on the same thread while it was suspended,
//! which on an async runtime is most of them. Each window reports two numbers: the bytes the task
//! allocated while in it (``observe_alloc``), and the process-wide live peak reached during it
//! (``observe``). The first answers "who allocated the megabytes", the second answers "when was
//! the heap at its highest" -- keeping them apart is what makes a window whose own code allocates
//! nothing distinguishable from one that is simply running while another task allocates.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

static ENABLED: AtomicBool = AtomicBool::new(false);
static LIVE_FN: OnceLock<fn() -> usize> = OnceLock::new();

thread_local! {
    /// Phase the current task is in.
    static LABEL: Cell<&'static str> = const { Cell::new("") };
    /// Process-wide live peak seen since the open window started.
    static PEAK: Cell<usize> = const { Cell::new(0) };
    /// Bytes the task allocated since the open window started.
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
    /// Task the open window belongs to (`None` outside a task).
    static OWNER: Cell<Option<tokio::task::Id>> = const { Cell::new(None) };
    /// Windows this thread closed, waiting to be merged into the shared list.
    ///
    /// A window closes from inside the allocator when the thread switches tasks, and that path
    /// must not take the shared lock: an allocation made while the lock is held would come back
    /// through here and deadlock.
    static CLOSED: RefCell<Vec<(&'static str, usize, usize)>> = const { RefCell::new(Vec::new()) };
    /// Re-entrancy guard, so the bookkeeping never charges its own allocations recursively.
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

struct DiagState {
    /// `(phase label, live peak during it, bytes this thread allocated in it)`.
    events: Vec<(&'static str, usize, usize)>,
}

fn state() -> &'static Mutex<DiagState> {
    static STATE: OnceLock<Mutex<DiagState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(DiagState { events: Vec::new() }))
}

/// Installs the allocator's live-byte reader (called once by a diagnostic harness).
pub fn register_live(f: fn() -> usize) {
    let _ = LIVE_FN.set(f);
}

/// Turns the probes on. Cheap to call repeatedly.
pub fn enable() {
    ENABLED.store(true, Relaxed);
}

/// Turns the probes off; marks become no-ops again.
pub fn disable() {
    ENABLED.store(false, Relaxed);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// Current live heap bytes, `0` when disabled or no reader was registered.
pub fn live() -> usize {
    if !is_enabled() {
        return 0;
    }
    LIVE_FN.get().map(|f| f()).unwrap_or(0)
}

/// Records the process-wide live bytes seen by this thread's allocations.
///
/// Called from the allocator on every allocation while enabled. A diagnostic-only cost: the
/// production allocator never calls it.
pub fn observe(live: usize) {
    if !is_enabled() {
        return;
    }
    PEAK.with(|cell| {
        if live > cell.get() {
            cell.set(live);
        }
    });
}

/// Identity of the task running on this thread, `None` outside one.
fn current_task() -> Option<tokio::task::Id> {
    tokio::task::try_id()
}

/// Runs `f` with the re-entrancy guard held; `None` when the guard is already taken.
fn guarded<T>(f: impl FnOnce() -> T) -> Option<T> {
    if BUSY.with(|cell| cell.replace(true)) {
        return None;
    }
    let value = f();
    BUSY.with(|cell| cell.set(false));
    Some(value)
}

/// Closes the open window when this thread now runs a different task.
///
/// A suspended task never allocates, so every allocation a thread sees between two marks
/// belongs to whichever task it is running *now*: switching tasks closes the previous window
/// instead of letting unrelated work be charged to it.
fn join_task(owner: Option<tokio::task::Id>) {
    if OWNER.with(|cell| cell.get()) == owner {
        return;
    }
    OWNER.with(|cell| cell.set(owner));
    let label = LABEL.with(|cell| cell.replace(""));
    let peak = PEAK.with(|cell| cell.replace(live()));
    let allocated = ALLOCATED.with(|cell| cell.replace(0));
    if label.is_empty() {
        return;
    }
    CLOSED.with(|closed| closed.borrow_mut().push((label, peak, allocated)));
}

/// Merges this thread's closed windows into the shared list.
fn drain_closed() {
    let closed = CLOSED.with(|closed| std::mem::take(&mut *closed.borrow_mut()));
    if closed.is_empty() {
        return;
    }
    if let Ok(mut st) = state().lock() {
        st.events.extend(closed);
    }
}

/// Records that the current task allocated `bytes`, charging it to the phase it is in.
///
/// This is the number that says which code path asks the heap for the megabytes; [`observe`]
/// alone cannot separate "allocated here" from "was running while another task allocated".
pub fn observe_alloc(bytes: usize) {
    if !is_enabled() {
        return;
    }
    // Closing a window pushes onto a list, so the bookkeeping itself allocates: the guard keeps
    // that allocation out of the counters instead of recursing into them.
    let _ = guarded(|| {
        join_task(current_task());
        ALLOCATED.with(|cell| cell.set(cell.get() + bytes));
    });
}

/// Ends the current task's phase and starts `label`, closing the phase's window.
pub fn mark(label: &'static str) {
    if !is_enabled() {
        return;
    }
    let rotate = || {
        join_task(current_task());
        let now = live();
        let previous = LABEL.with(|cell| cell.replace(label));
        let peak = PEAK.with(|cell| cell.replace(now));
        let allocated = ALLOCATED.with(|cell| cell.replace(0));
        if !previous.is_empty() {
            CLOSED.with(|closed| closed.borrow_mut().push((previous, peak, allocated)));
        }
    };
    if guarded(rotate).is_none() {
        // Called from inside an allocation: rotate the label without touching the lists.
        LABEL.with(|cell| cell.set(label));
        return;
    }
    drain_closed();
}

/// Clears collected phases and this thread's open window.
pub fn reset() {
    if !is_enabled() {
        return;
    }
    let now = live();
    LABEL.with(|cell| cell.set(""));
    OWNER.with(|cell| cell.set(current_task()));
    PEAK.with(|cell| cell.set(now));
    ALLOCATED.with(|cell| cell.set(0));
    CLOSED.with(|closed| closed.borrow_mut().clear());
    if let Ok(mut st) = state().lock() {
        st.events.clear();
    }
}

/// `(phase label, live peak during it, bytes its task allocated)` in entry order.
pub fn events() -> Vec<(&'static str, usize, usize)> {
    drain_closed();
    state()
        .lock()
        .map(|st| st.events.clone())
        .unwrap_or_default()
}

/// Collapses [`events`] to `(phase label, max live peak across its occurrences)`, sorted by peak.
pub fn peak_by_label() -> Vec<(&'static str, usize)> {
    use std::collections::BTreeMap;
    let mut peaks: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (label, peak, _) in events() {
        let slot = peaks.entry(label).or_insert(0);
        if peak > *slot {
            *slot = peak;
        }
    }
    let mut out: Vec<(&'static str, usize)> = peaks.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

/// Collapses [`events`] to `(phase label, bytes its thread allocated)`, sorted by allocation.
///
/// This is the attribution that survives concurrent work: only the code path that actually asked
/// the heap for memory is charged, no matter what other threads were doing at the time.
pub fn allocated_by_label() -> Vec<(&'static str, usize)> {
    use std::collections::BTreeMap;
    let mut allocated: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (label, _, bytes) in events() {
        *allocated.entry(label).or_insert(0) += bytes;
    }
    let mut out: Vec<(&'static str, usize)> = allocated.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}
