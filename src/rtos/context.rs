//! Cortex-M4 上下文切换汇编 — RT-Thread `libcpu/arm/cortex-m4/context_gcc.S` 的移植
//!
//! # 切换机制 (与 RT-Thread 一致)
//!
//! 调度器把切换请求编码为 PendSV 待处理位 (ICSR.PENDSVSET),
//! 中断返回时由最低优先级的 PendSV 异常执行实际切换:
//!
//! 1. 保存当前线程: `r4-r11` 与逐线程 `EXC_RETURN` 压入当前 PSP;
//! 2. 若 `EXC_RETURN.bit4 == 0`, 额外保存该线程的 `d8-d15`;
//! 3. 旧上下文保存完成后切换目标线程的 MPU 栈守卫;
//! 4. 按目标线程保存的 `EXC_RETURN` 对称恢复软件/硬件异常帧。
//!
//! # FPU
//!
//! `EXC_RETURN.bit4` 描述当前线程的硬件异常帧类型: 1 为基本帧,
//! 0 为包含 `s0-s15/FPSCR` 的浮点扩展帧。PendSV 将 EXC_RETURN 与
//! `r4-r11` 一起逐线程保存, 并只为扩展帧保存 `d8-d15`; `vstmdb`
//! 会在需要时促使硬件完成惰性压栈。这样未使用 FPU 的线程不承担
//! 64 字节保存开销, 使用 FPU 的线程又能完整保留各自浮点上下文。
//! 启动入口在进入任何 Rust 代码前显式设置 `FPCCR.ASPEN/LSPEN`，这是
//! 本模块按 `EXC_RETURN.bit4` 区分基本帧与扩展帧的硬件契约。

// 内核模块: unsafe 契约由临界区与模块文档统一说明, 函数体内不再逐段包裹
#![allow(unsafe_op_in_unsafe_fn)]

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// 切换请求标志 (PendSV 汇编读取并清零)
#[unsafe(no_mangle)]
pub(crate) static SWITCH_FLAG: AtomicU32 = AtomicU32::new(0);
/// 当前线程 `sp` 字段地址 (PendSV 汇编写入保存后的 PSP)
#[unsafe(no_mangle)]
pub(crate) static FROM_SP_ADDR: AtomicUsize = AtomicUsize::new(0);
/// 目标线程 `sp` 字段地址 (PendSV 汇编读取目标 PSP)
#[unsafe(no_mangle)]
pub(crate) static TO_SP_ADDR: AtomicUsize = AtomicUsize::new(0);

core::arch::global_asm!(
    r#"
    .syntax unified
    .fpu fpv4-sp-d16
    .thumb
    .global pendsv_handler
    .type pendsv_handler, %function
    .thumb_func
pendsv_handler:
    mrs     r2, primask
    cpsid   i                       @ 切换期间关中断 (防中断抢占破坏保存/恢复)
    ldr     r1, =SWITCH_FLAG
    ldr     r3, [r1]
    cbz     r3, 1f                  @ 无切换请求, 直接返回
    mov     r3, #0
    str     r3, [r1]                @ 清除请求标志
    ldr     r1, =FROM_SP_ADDR
    ldr     r3, [r1]
    cbz     r3, 2f                  @ from=0: 首次切换, 跳过保存
    mrs     r0, psp                 @ 当前线程 PSP (硬件异常帧之下)
    tst     lr, #0x10               @ EXC_RETURN.bit4=0: 浮点扩展帧
    it      eq
    vstmdbeq r0!, {{d8-d15}}        @ 仅保存使用过 FPU 的线程
    stmdb   r0!, {{r4-r11, lr}}     @ 保存通用寄存器及逐线程 EXC_RETURN
    str     r0, [r3]                @ from->sp = 保存后的 PSP
2:
    @ 旧 PSP 已完整保存, 此时才允许把 MPU 守卫切到最终目标线程。
    @ PendSV 使用 MSP; 调用期间不访问旧/新线程栈。保存 r2 中的
    @ PRIMASK 和异常入口 LR, 以遵守 AAPCS caller-saved 规则。
    push    {{r2, lr}}
    bl      pendsv_run_context_switch_hook
    pop     {{r2, lr}}
    ldr     r1, =TO_SP_ADDR
    ldr     r1, [r1]
    ldr     r0, [r1]                @ r0 = 目标线程 PSP
    ldmia   r0!, {{r4-r11, lr}}     @ 恢复目标线程及其 EXC_RETURN
    tst     lr, #0x10
    it      eq
    vldmiaeq r0!, {{d8-d15}}        @ 仅扩展帧带有高位浮点上下文
    msr     psp, r0                 @ PSP 指向目标线程异常帧
    isb
    msr     primask, r2             @ 恢复中断状态
    bx      lr                      @ 异常返回, 硬件弹出异常帧进入目标线程
1:
    msr     primask, r2
    bx      lr
    .pool
    .size pendsv_handler, .-pendsv_handler
    "#
);

core::arch::global_asm!(
    r#"
    .syntax unified
    .fpu fpv4-sp-d16
    .thumb
    .global switch_to_first
    .type switch_to_first, %function
    .thumb_func
switch_to_first:
    @ r0 = 线程 sp 字段地址: 首次切换经 PendSV 完成 (from=0 跳过保存),
    @ 与常规切换共用同一恢复路径, 保证栈帧布局一致。
    ldr     r1, =TO_SP_ADDR
    str     r0, [r1]
    ldr     r1, =FROM_SP_ADDR
    mov     r0, #0
    str     r0, [r1]                @ from = 0
    ldr     r1, =SWITCH_FLAG
    mov     r0, #1
    str     r0, [r1]
    ldr     r0, =0xE000ED04         @ ICSR
    ldr     r1, =0x10000000         @ PENDSVSET
    str     r1, [r0]
    bx      lr
    .pool
    .size switch_to_first, .-switch_to_first
    "#
);

unsafe extern "C" {
    /// 启动调度器: 编码首个切换请求 (PendSV 即将执行), 返回后调用方须循环
    pub(crate) fn switch_to_first(sp_addr: usize);
    /// PendSV 异常入口 (global_asm 定义, 由向量表指向)
    pub(crate) fn pendsv_handler();
}

/// 请求一次上下文切换 (须在临界区内调用)
///
/// 仅写入共享标志并置位 PendSV; 实际切换在中断返回后由 PendSV
/// 汇编执行。`from_sp`/`to_sp` 为线程 TCB 中 `sp` 字段的地址。
///
/// 与 RT-Thread 的 `rt_hw_context_switch` 一致: **若已有切换请求
/// 挂起, 不再更新 from** (首个请求的 from 即实际运行线程, 覆盖会
/// 造成上下文错位), 只更新目标线程 (最后一次请求生效)。
#[inline]
pub(crate) unsafe fn request_switch(from_sp: *mut usize, to_sp: *mut usize) {
    if SWITCH_FLAG.load(Ordering::Relaxed) == 0 {
        FROM_SP_ADDR.store(from_sp as usize, Ordering::Relaxed);
    }
    TO_SP_ADDR.store(to_sp as usize, Ordering::Relaxed);
    SWITCH_FLAG.store(1, Ordering::Relaxed);
    // NVIC_ICSR.PENDSVSET (Cortex-M 内核外设 0xE000ED04)
    unsafe { core::ptr::write_volatile(0xE000_ED04 as *mut u32, 1 << 28) };
}

/// 设置 PendSV 为最低优先级、SysTick 次低 (SCB.SHPR3)
///
/// PendSV 必须低于所有中断, 确保切换发生在所有中断返回之后。
pub(crate) fn scb_priority_init() {
    // SHPR3: [23:16] = PendSV (优先级 15), [31:24] = SysTick (优先级 14)
    unsafe { core::ptr::write_volatile(0xE000_ED20 as *mut u32, 0xE0F0_0000) };
}

#[inline]
unsafe fn write_u32(addr: usize, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
}

/// 初始化线程初始栈帧 (RT-Thread `rt_hw_stack_init` 移植)
///
/// 新线程尚未使用 FPU, 因而初始栈顶向下依次为: 基本硬件异常帧
/// (32B) → EXC_RETURN (4B) + r4-r11 (32B), 共 68B。保存的
/// EXC_RETURN 为 `0xffff_fffd` (线程模式、PSP、基本帧)。
///
/// 首次切换到该线程时, PendSV 恢复 `r4-r11/EXC_RETURN` 并跳过
/// `d8-d15`, 然后异常返回弹出基本帧: `r0` = 参数,
/// `lr` = 线程退出函数, `pc` = 线程入口, `xpsr` = Thumb 模式。
pub(crate) unsafe fn init_stack(
    stack: *mut u8,
    stack_size: usize,
    entry: usize,
    param: usize,
    exit: usize,
) -> usize {
    let top = (stack as usize + stack_size) & !7; // 8 字节对齐

    // 硬件异常帧 (异常返回时由硬件自动弹出)
    let mut p = top - 32;
    write_u32(p, param as u32); // r0
    write_u32(p + 4, 0); // r1
    write_u32(p + 8, 0); // r2
    write_u32(p + 12, 0); // r3
    write_u32(p + 16, 0); // r12
    write_u32(p + 20, exit as u32); // lr → 线程退出
    write_u32(p + 24, entry as u32); // pc → 线程入口
    write_u32(p + 28, 0x0100_0000); // xpsr: Thumb 模式

    // PendSV 软件帧: r4-r11 后紧跟逐线程 EXC_RETURN。新线程使用
    // 基本硬件帧 (bit4=1), 因而初始帧没有 d8-d15 区域。
    p -= 36;
    for i in 0..8 {
        write_u32(p + i * 4, 0);
    }
    write_u32(p + 32, 0xffff_fffd);

    p // 初始 PSP
}
