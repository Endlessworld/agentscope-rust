//! The execution guard registers on a **process-wide** manager, so this assertion needs a test
//! binary of its own.
//!
//! In the library test binary hundreds of `ReActAgent` calls acquire guards in parallel, so any
//! exact count assertion races with them: the observed value is `before + 1 + others`. Keeping
//! the check in its own binary makes the observation deterministic instead of weakening the
//! assertion.

use agentscope_core::agent::ExecutionGuard;
use agentscope_core::shutdown::GracefulShutdownManager;

#[test]
fn execution_guard_registers_and_releases() {
    let mgr = GracefulShutdownManager::instance();
    let before = mgr.get_active_request_count();
    {
        let guard = ExecutionGuard::acquire().expect("the framework accepts requests");
        assert_eq!(mgr.get_active_request_count(), before + 1);
        drop(guard);
    }
    assert_eq!(mgr.get_active_request_count(), before);
}
