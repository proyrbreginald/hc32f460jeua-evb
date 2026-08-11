//! Cortex-M architecture primitives.

/// Cortex-M MPU 的最小保护区域与基址对齐要求。
pub(crate) const STACK_GUARD_SIZE: usize = 32;
pub(crate) const STACK_GUARD_ALIGN: usize = 32;

/// 进入架构临界区前捕获的不透明中断状态。
pub(crate) struct InterruptState(u32);

/// Return the current PRIMASK value.
#[inline]
fn interrupt_mask() -> u32 {
    let value: u32;
    unsafe {
        core::arch::asm!("mrs {}, primask", out(reg) value);
    }
    value
}

/// Disable maskable interrupts.
#[inline]
pub(crate) fn disable_interrupts() {
    unsafe {
        core::arch::asm!("cpsid i");
    }
}

/// 关闭可屏蔽中断并返回进入前的完整状态。
#[inline]
pub(crate) fn acquire_interrupt_lock() -> InterruptState {
    let state = InterruptState(interrupt_mask());
    if state.0 & 1 == 0 {
        disable_interrupts();
    }
    state
}

/// 恢复 [`acquire_interrupt_lock`] 捕获的中断状态。
#[inline]
pub(crate) fn restore_interrupt_lock(state: InterruptState) {
    unsafe {
        core::arch::asm!("msr primask, {}", in(reg) state.0);
    }
}

/// Return whether execution is currently inside an exception handler.
#[inline]
pub(crate) fn in_exception() -> bool {
    let value: u32;
    unsafe {
        core::arch::asm!("mrs {}, ipsr", out(reg) value);
    }
    value != 0
}

/// Wait until an interrupt or event wakes the processor.
#[inline]
pub(crate) fn wait_for_interrupt() {
    unsafe {
        core::arch::asm!("wfi");
    }
}

/// Complete outstanding memory transactions before leaving an ISR.
#[inline]
pub(crate) fn data_sync_barrier() {
    unsafe {
        core::arch::asm!("dsb sy");
    }
}

/// Request a full system reset through SCB.AIRCR.
pub(crate) fn system_reset() -> ! {
    const AIRCR: *mut u32 = 0xE000_ED0C as *mut u32;
    const PRIGROUP_MASK: u32 = 0x7 << 8;
    const VECTKEY: u32 = 0x05FA << 16;
    const SYSRESETREQ: u32 = 1 << 2;

    disable_interrupts();
    data_sync_barrier();
    unsafe {
        let priority_group = core::ptr::read_volatile(AIRCR) & PRIGROUP_MASK;
        core::ptr::write_volatile(AIRCR, VECTKEY | priority_group | SYSRESETREQ);
    }
    data_sync_barrier();
    loop {
        unsafe {
            core::arch::asm!("nop");
        }
    }
}
