//! 复位启动流程
//!
//! 上电/复位后的执行序列:
//! 1. SRAMC 初始化 (SRAM3 等待周期等, 任何 RAM 使用之前);
//! 2. FPU 访问与自动/惰性上下文保存使能;
//! 3. `.data` / `.bss` 段初始化;
//! 4. 进入应用入口 [`crate::main`]。
//!
//! 对应的复位向量在 [`crate::vector_table::RESET_VECTOR`]。

// 复位时 MSP 已指向 SRAM3，而 SRAM3 的复位等待周期配置不足以安全访问。
// 因此第一阶段必须是不会生成函数序言的汇编入口；普通 Rust 函数即使只
// 包含 volatile MMIO，也可能在第一条指令压栈。
core::arch::global_asm!(
    r#"
    .syntax unified
    .cpu cortex-m4
    .thumb

    .section .text.reset_handler, "ax", %progbits
    .p2align 2
    .global reset_handler
    .type reset_handler, %function
    .thumb_func
reset_handler:
    movw r0, #0x0800
    movt r0, #0x4005

    // 清除 SRAM 校验错误标志 (CKSR: 1ERR/2ERR/PYERR)。
    movs r1, #0x1f
    str  r1, [r0, #0x10]

    // 解锁等待周期与校验控制寄存器。
    movs r1, #0x77
    str  r1, [r0, #0x04]
    str  r1, [r0, #0x0c]

    // SRAM3 读、写各等待 1 周期 (WTCR = 0x1100)。
    movw r1, #0x1100
    str  r1, [r0]

    // 恢复写保护，并确保配置在任何 SRAM3 访问前完成。
    movs r1, #0x76
    str  r1, [r0, #0x04]
    str  r1, [r0, #0x0c]
    dsb  sy

    // 只核对 SRAM3 的 RWT/WWT 字段。失败时尚无可用栈和诊断输出，
    // 因此保持关中断并 fail-stop，绝不以不安全等待周期进入 Rust。
    ldr  r2, [r0]
    movw r3, #0x7700
    and  r2, r2, r3
    movw r3, #0x1100
    cmp  r2, r3
    bne  .Lreset_sram_wait_failed
    isb  sy

    // 开放 CP10/CP11 完全访问权限。CPACR 是执行上下文控制寄存器，
    // 写入后必须完成 DSB/ISB，随后 Rust hard-float 代码才可安全执行。
    movw r0, #0xed88
    movt r0, #0xe000
    ldr  r1, [r0]
    orr  r1, r1, #0x00f00000
    str  r1, [r0]
    dsb  sy
    isb  sy

    // PendSV 依赖硬件自动保存 s0-s15/FPSCR，并在需要时惰性压栈。
    // 显式置位 FPCCR.ASPEN/LSPEN，不依赖实现或复位默认值。
    movw r0, #0xef34
    movt r0, #0xe000
    ldr  r1, [r0]
    orr  r1, r1, #0xc0000000
    str  r1, [r0]
    dsb  sy
    isb  sy

    b    reset_handler_rust

.Lreset_sram_wait_failed:
    cpsid i
    b    .Lreset_sram_wait_failed
    .size reset_handler, . - reset_handler
"#,
);

unsafe extern "C" {
    /// 第一阶段无栈复位入口，由上方汇编定义。
    pub fn reset_handler() -> !;
}

// 声明链接脚本中定义的段边界符号
unsafe extern "C" {
    unsafe static _data_load_addr: u32;
    unsafe static mut _data_ram_start: u32;
    unsafe static mut _data_ram_end: u32;
    unsafe static mut _bss_ram_start: u32;
    unsafe static mut _bss_ram_end: u32;
    /// 堆上界 (link.ld: RAM 顶 - 栈预留 - 主栈 canary)
    static _heap_end: u8;
}

/// 第二阶段 Rust 复位处理函数。
///
/// 仅由无栈的 [`reset_handler`] 在配置 SRAM3 等待周期后跳转进入。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reset_handler_rust() -> ! {
    // ---- EFM 初始化: FLASH 读等待周期 (参考手册表 7-1) ----
    //
    // 复位后系统时钟为 MRC 8MHz → FLWT=0 (无等待)。
    // 逻辑见 `efm::set_wait_cycle_early` (表 7-1 全频段映射);
    // 切换外部晶振/更高时钟时由 `clk::switch_to_xtal` 在切换前重新配置。
    crate::efm::set_wait_cycle_early(crate::clk::MRC_HZ);

    // 初始化 .data 段
    unsafe {
        let mut src = core::ptr::addr_of!(_data_load_addr);
        let mut dest = core::ptr::addr_of_mut!(_data_ram_start);
        let end = core::ptr::addr_of_mut!(_data_ram_end);

        while dest < end {
            core::ptr::write_volatile(dest, core::ptr::read_volatile(src));
            src = src.add(1);
            dest = dest.add(1);
        }
    }

    // 初始化 .bss 段
    unsafe {
        let mut dest = core::ptr::addr_of_mut!(_bss_ram_start);
        let end = core::ptr::addr_of_mut!(_bss_ram_end);

        while dest < end {
            core::ptr::write_volatile(dest, 0);
            dest = dest.add(1);
        }
    }

    // 主栈 (MSP: 启动/中断栈) canary: 位于堆/主栈边界 (link.ld 预留),
    // 由空闲线程巡检 —— 主栈向下溢出首先破坏该字 (见 rtos/thread.rs)
    unsafe {
        core::ptr::write_volatile(
            core::ptr::addr_of!(_heap_end) as *mut u32,
            crate::rtos::thread::STACK_PATTERN,
        );
    }

    // 进入应用入口
    crate::main();
}
