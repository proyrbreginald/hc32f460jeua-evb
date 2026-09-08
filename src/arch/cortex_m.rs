//! Cortex-M architecture primitives.
//!
//! 架构层不持有工程配置: 栈守卫尺寸 (CFG_MPU_STACK_GUARD) 等编译期
//! 常量由 `config` 模块统一提供, 消费方 (rtos/mpu) 直接引用。

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

/// DWT 周期计数器 (与 SysTick/RTOS 解耦的单调时钟)。
///
/// 未使能时惰性初始化 (与 [`crate::latency::init`] 的序列一致, 幂等):
/// DEMCR.TRCENA 开放调试组件 → 清零 CYCCNT → 使能计数。32 位回绕,
/// 差值必须用 `wrapping_sub`。调度器启动前 (单执行流) 即可使用,
/// 供校准短延时与裸驱动的单调超时 (避免依赖 RTOS 节拍)。
pub(crate) fn cycles_now() -> u32 {
    const DWT_CTRL: usize = 0xE000_1000;
    const DWT_CYCCNT: usize = 0xE000_1004;
    const DEMCR: usize = 0xE000_EDFC;
    const DEMCR_TRCENA: u32 = 1 << 24;
    const CYCCNTENA: u32 = 1 << 0;

    let ctrl = crate::mmio::Reg::new(DWT_CTRL);
    if ctrl.read() & CYCCNTENA == 0 {
        let demcr = crate::mmio::Reg::new(DEMCR);
        demcr.write(demcr.read() | DEMCR_TRCENA);
        ctrl.write(0);
        crate::mmio::Reg::new(DWT_CYCCNT).write(0);
        ctrl.write(CYCCNTENA);
    }
    crate::mmio::Reg::new(DWT_CYCCNT).read()
}

/// Request a full system reset through SCB.AIRCR.
pub(crate) fn system_reset() -> ! {
    const AIRCR_ADDR: usize = 0xE000_ED0C;
    const PRIGROUP_MASK: u32 = 0x7 << 8;
    const VECTKEY: u32 = 0x05FA << 16;
    const SYSRESETREQ: u32 = 1 << 2;

    disable_interrupts();
    data_sync_barrier();
    let aircr = crate::mmio::Reg::new(AIRCR_ADDR);
    let priority_group = aircr.read() & PRIGROUP_MASK;
    aircr.write(VECTKEY | priority_group | SYSRESETREQ);
    data_sync_barrier();
    loop {
        unsafe {
            core::arch::asm!("nop");
        }
    }
}
