//! Optional, allocation-free RTOS integration hooks.
//!
//! Hooks are stored as typed function pointers in kernel cells. Registration
//! and lookup use the RTOS critical section, avoiding both allocation and
//! function/data-pointer conversion. A missing hook is a no-op.

use crate::critical_section;
use crate::critical_section::CriticalSection;
use crate::rtos::klist::KCell;
use crate::rtos::thread::ThreadInner;

/// Function invoked once per idle-loop iteration.
pub type IdleHook = fn();

/// Metadata for the thread that is about to become current.
#[derive(Clone, Copy)]
pub struct ContextSwitchInfo {
    /// Thread name supplied at creation.
    pub name: &'static str,
    /// First usable byte of the thread stack.
    pub stack_base: usize,
    /// Usable stack size in bytes.
    pub stack_size: usize,
    /// Base of the reserved stack guard region below `stack_base`.
    pub guard_base: usize,
}

/// Function invoked before the first thread and every later context switch.
pub type ContextSwitchHook = fn(ContextSwitchInfo);

static IDLE_HOOK: KCell<Option<IdleHook>> = KCell::new(None);
static CONTEXT_SWITCH_HOOK: KCell<Option<ContextSwitchHook>> = KCell::new(None);

/// Install or remove the idle hook.
///
/// Registration must be completed before [`crate::rtos::start`]. Passing
/// `None` restores the no-op behavior. The hook runs in thread context with
/// interrupts enabled, but must remain bounded and non-blocking so defunct
/// cleanup and low-power entry are not delayed.
pub fn set_idle_hook(hook: Option<IdleHook>) {
    debug_assert!(
        !crate::rtos::scheduler_started(),
        "idle hook must be registered before the scheduler starts"
    );
    critical_section::with(|cs| unsafe {
        *IDLE_HOOK.get(cs) = hook;
    });
}

/// Install or remove the context-switch hook.
///
/// Registration must be completed before [`crate::rtos::start`]. The hook
/// executes from PendSV on the main exception stack, after the old thread's
/// PSP has been saved and before the new thread's PSP is restored. Interrupts
/// are masked, so it must be bounded, non-blocking, and must not call blocking
/// RTOS APIs.
pub fn set_context_switch_hook(hook: Option<ContextSwitchHook>) {
    debug_assert!(
        !crate::rtos::scheduler_started(),
        "context-switch hook must be registered before the scheduler starts"
    );
    critical_section::with(|cs| unsafe {
        *CONTEXT_SWITCH_HOOK.get(cs) = hook;
    });
}

pub(crate) fn run_idle_hook() {
    let hook = critical_section::with(|cs| unsafe { *IDLE_HOOK.get(cs) });
    if let Some(hook) = hook {
        hook();
    }
}

/// Invoke the registered switch hook for `next` while PRIMASK is held.
///
/// # Safety
///
/// `next` must point to a live TCB selected by the scheduler, and `cs` must be
/// the token for the scheduler's active critical section.
pub(crate) unsafe fn run_context_switch_hook(next: *mut ThreadInner, cs: CriticalSection<'_>) {
    let hook = unsafe { *CONTEXT_SWITCH_HOOK.get(cs) };
    if let Some(hook) = hook {
        let info = unsafe {
            ContextSwitchInfo {
                name: (*next).name,
                stack_base: (*next).stack_addr,
                stack_size: (*next).stack_size,
                guard_base: (*next).guard_addr,
            }
        };
        hook(info);
    }
}

/// PendSV bridge for the optional context-switch hook.
///
/// The scheduler updates `CURRENT` and the pending target PSP atomically under
/// PRIMASK. PendSV also runs with PRIMASK set, so `CURRENT` identifies the
/// final target even when several scheduling requests were coalesced before
/// PendSV ran.
#[unsafe(no_mangle)]
unsafe extern "C" fn pendsv_run_context_switch_hook() {
    critical_section::with(|cs| unsafe {
        let next = crate::rtos::sched::current();
        debug_assert!(!next.is_null(), "PendSV target thread must be set");
        if !next.is_null() {
            run_context_switch_hook(next, cs);
        }
    });
}
