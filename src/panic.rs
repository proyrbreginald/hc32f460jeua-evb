//! panic 与硬件 fault 诊断处理
//!
//! 在 [`panic`] (Rust panic) 与 [`fault_handler`] (HardFault/BusFault/
//! UsageFault/MemManage) 两个入口输出诊断信息到控制台, 随后按
//! [`STRATEGY`] 停机或软复位。
//!
//! # 诊断内容
//!
//! - Rust panic: 消息 + 位置 (file:line:col);
//! - 硬件 fault: 异常号、栈指针、SCB fault 状态寄存器 (CFSR/HFSR/
//!   BFAR/MMFAR) 逐位解码;
//! - 通用: 当前异常上下文 (IPSR)、栈指针与栈使用量。
//!
//! # 策略
//!
//! 修改 [`STRATEGY`] 选择 panic/fault 后的行为:
//! - [`PanicStrategy::Halt`]: 屏蔽中断后 wfi 死循环 (调试期推荐);
//! - [`PanicStrategy::Reset`]: 软复位重启 (产品部署推荐)。
//!
//! 注意: 输出依赖 [`crate::console`] 绑定的 UART, 未初始化时静默丢弃
//! (不会死锁)。
//!
//! 策略枚举的 `Reset` 变体当前未启用 (默认 Halt), 切换 [`STRATEGY`] 时生效,
//! 故忽略死代码警告。
#![allow(dead_code)]

use crate::console::write_fmt_raw as write_fmt;

/// NMI 处理器 (向量表异常 2)
///
/// HC32F460 的 SRAM 奇偶/ECC 错误默认经 **NMI** 上报 (见 `sram` 模块,
/// CKCR.PYOAD/ECCOAD 可改为复位)。此处输出诊断后停机 —— 否则静默
/// 死循环无法定位。无锁输出 (write_fmt_raw), 中断上下文安全。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nmi_handler() {
    let status = crate::sram::status();
    let err = crate::sram::error();
    write_fmt(core::format_args!(
        "\r\n[NMI] SRAM 奇偶/ECC 错误? CKSR = {:#x}, 最高位错误 = {:?}\r\n",
        status,
        err
    ));
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

/// panic/fault 后的行为策略 (编译期常量, 修改此处即可切换)
const STRATEGY: PanicStrategy = PanicStrategy::Halt;

/// SCB 外设基址 (Cortex-M4)
const SCB_BASE: usize = 0xE000_ED00;

/// 栈顶 (与 link.ld 的 RAM 段末尾一致), 用于估算栈使用量
const STACK_TOP: usize = 0x2002_7000;

/// panic/fault 后的行为
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PanicStrategy {
    /// 停机: 屏蔽中断, wfi 死循环等待复位/调试器 (默认)
    Halt,
    /// 软复位: 写 AIRCR.SYSRESETREQ 重启系统 (产品部署场景)
    Reset,
}

/// Rust panic 处理器
///
/// `link_section` + 链接脚本 `KEEP` 保证符号始终保留 (无 panic 路径时
/// 链接器会 GC 掉未引用项), 便于 gdb `b panic` 定位。
#[panic_handler]
#[unsafe(link_section = ".text.panic_handler")]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // 入口处捕获帧指针 (函数序言刚完成, r7 即本帧 FP,
    // 其 [0] 指向调用者帧, 供栈回溯)
    let fp: usize;
    unsafe {
        core::arch::asm!("mov {}, r7", out(reg) fp);
    }

    match info.location() {
        Some(loc) => write_fmt(format_args!("程序异常于 {}: {}\r\n", loc, info.message())),
        None => write_fmt(format_args!("程序异常: {}\r\n", info.message())),
    }
    report_context();
    report_backtrace(fp);
    terminate()
}

// 硬件 fault 统一入口 (HardFault/MemManage/BusFault/UsageFault 向量指向):
// 由内联汇编定义符号, 入口处寄存器尚未被编译器破坏, 捕获
// IPSR/MSP/SP/帧指针 (r7) 后直接跳入 fault_diagnose
// (b 跳转不经过调用约定, r0~r3 传参)。
core::arch::global_asm!(
    ".section .text.fault_handler, \"ax\"",
    ".global fault_handler",
    ".thumb_func",
    "fault_handler:",
    "    mrs r0, ipsr", // 异常号
    "    mrs r1, msp",  // MSP (异常压栈帧基址)
    "    mov r2, sp",   // 当前 SP (handler 模式下即 MSP)
    "    mov r3, r7",   // 现场帧指针 (fault 指令所在函数的 FP)
    "    b fault_diagnose",
);

// 硬件 fault 汇编入口 (由上方 global_asm 定义), 向量表引用此符号
unsafe extern "C" {
    pub fn fault_handler();
}

/// fault 现场诊断 (由 [`fault_handler`] 汇编跳入: r0=ipsr, r1=msp, r2=sp, r3=fp)
#[unsafe(no_mangle)]
unsafe extern "C" fn fault_diagnose(ipsr: u32, msp: u32, _sp: u32, fp: u32) -> ! {
    write_fmt(format_args!(
        "=== 硬件故障 ===\r\n  异常号: {} ({})\r\n",
        ipsr,
        exception_name(ipsr)
    ));
    report_fault_registers();
    report_exception_frame(msp);
    report_backtrace(fp as usize);
    terminate()
}

/// 打印异常压栈帧: 硬件在 fault 入口自动压入
/// `[r0, r1, r2, r3, r12, lr, pc, xpsr]` (msp 指向帧底)
fn report_exception_frame(msp: u32) {
    if msp as usize + 32 <= STACK_TOP {
        unsafe {
            let f = msp as *const u32;
            let rd = |i: usize| core::ptr::read_volatile(f.add(i));
            write_fmt(format_args!(
                "  异常帧 @0x{:08x}:\r\n    r0=0x{:08x} r1=0x{:08x} r2=0x{:08x} r3=0x{:08x}\r\n",
                msp,
                rd(0),
                rd(1),
                rd(2),
                rd(3)
            ));
            write_fmt(format_args!(
                "    r12=0x{:08x} lr=0x{:08x} pc=0x{:08x} xpsr=0x{:08x}\r\n",
                rd(4),
                rd(5),
                rd(6),
                rd(7)
            ));
        }
    }
}

/// 栈回溯: 沿帧指针链收集返回地址
///
/// AAPCS 帧布局 (force-frame-pointers): `[fp+0]` = 前一帧 FP,
/// `[fp+4]` = 返回地址 (LR)。合法性检查防止越界读与循环链:
/// - 帧指针必须在栈范围内且单调递减;
/// - 返回地址必须指向 flash 代码区 (thumb 位 + 512K 范围)。
///
/// 注意: core 库函数 (无帧指针) 可能中断链, 故不保证完整覆盖;
/// fault 场景的现场 r7 精确, 本 crate 内部调用链完整。
fn stack_backtrace(mut fp: usize, frames: &mut [usize]) -> usize {
    let mut n = 0;
    while n < frames.len() {
        // 帧必须在栈范围内 (fp 与 fp+8 均合法)
        if fp < STACK_BOTTOM || fp + 8 > STACK_TOP {
            break;
        }
        let prev = unsafe { core::ptr::read_volatile(fp as *const usize) };
        let pc = unsafe { core::ptr::read_volatile((fp + 4) as *const usize) };
        // 返回地址必须指向 flash 代码区 (thumb 位为 1)
        if pc & 1 == 0 || pc & !1 >= FLASH_SIZE {
            break;
        }
        frames[n] = pc;
        n += 1;
        // 栈向下增长: 前帧必须低于当前帧, 否则视为无效链 (防循环)
        if prev >= fp || prev == 0 {
            break;
        }
        fp = prev;
    }
    n
}

/// 打印回溯帧 (地址需用 addr2line 解析符号)
fn report_backtrace(fp: usize) {
    let mut frames = [0usize; MAX_BACKTRACE_FRAMES];
    let n = stack_backtrace(fp, &mut frames);
    write_fmt(format_args!("  栈回溯 ({} 帧):\r\n", n));
    for (i, pc) in frames[..n].iter().enumerate() {
        write_fmt(format_args!("    #{} 0x{:08x}\r\n", i, pc));
    }
}

/// 回溯帧数上限
const MAX_BACKTRACE_FRAMES: usize = 16;

/// flash 容量 (512K), 用于返回地址合法性检查
const FLASH_SIZE: usize = 0x8_0000;

/// 栈范围下界 (与 link.ld 的 RAM 段一致)
const STACK_BOTTOM: usize = 0x1FFF_8000;

/// 打印当前异常上下文与栈信息
fn report_context() {
    let ipsr = mrs_ipsr();
    let sp = mrs_msp();
    let stack_used = STACK_TOP.wrapping_sub(sp as usize);
    write_fmt(format_args!(
        "  上下文: {} (ipsr={}), sp=0x{:08x}, 栈使用=0x{:x} B\r\n",
        exception_name(ipsr),
        ipsr,
        sp,
        stack_used
    ));
}

/// 读取并解码 SCB fault 状态寄存器 (CFSR/HFSR/BFAR/MMFAR)
fn report_fault_registers() {
    unsafe {
        let cfsr = core::ptr::read_volatile((SCB_BASE + 0x28) as *const u32);
        let hfsr = core::ptr::read_volatile((SCB_BASE + 0x2C) as *const u32);
        let mmfar = core::ptr::read_volatile((SCB_BASE + 0x34) as *const u32);
        let bfar = core::ptr::read_volatile((SCB_BASE + 0x38) as *const u32);

        write_fmt(format_args!("  CFSR 0x{:08x}:", cfsr));
        for (mask, name) in CFSR_BITS {
            if cfsr & mask != 0 {
                write_fmt(format_args!(" {}", name));
            }
        }
        write_fmt(format_args!("\r\n  HFSR 0x{:08x}:", hfsr));
        for (mask, name) in HFSR_BITS {
            if hfsr & mask != 0 {
                write_fmt(format_args!(" {}", name));
            }
        }
        write_fmt(format_args!("\r\n"));

        // 仅当对应 VALID 位置位时地址有效
        if cfsr & CFSR_MMARVALID != 0 {
            write_fmt(format_args!("  MMFAR 0x{:08x}\r\n", mmfar));
        }
        if cfsr & CFSR_BFARVALID != 0 {
            write_fmt(format_args!("  BFAR 0x{:08x}\r\n", bfar));
        }
    }
}

/// 收尾: 按 [`STRATEGY`] 停机或软复位 (屏蔽中断, 防止被打断)
fn terminate() -> ! {
    unsafe {
        core::arch::asm!("cpsid i");
    }
    match STRATEGY {
        PanicStrategy::Halt => loop {
            unsafe {
                core::arch::asm!("wfi");
            }
        },
        PanicStrategy::Reset => unsafe {
            // AIRCR: VECTKEY=0x05FA, SYSRESETREQ=1 → 软复位
            core::ptr::write_volatile((SCB_BASE + 0x0C) as *mut u32, 0x05FA_0004);
            loop {
                core::arch::asm!("nop");
            }
        },
    }
}

/// CFSR 原因位表: (掩码, 名称)
///
/// MMFSR[7:0] + BFSR[15:8] + UFSR[24:16]
const CFSR_BITS: [(u32, &str); 12] = [
    (1 << 0, "IACCVIOL"),
    (1 << 1, "DACCVIOL"),
    (1 << 3, "MUNSTKERR"),
    (1 << 4, "MSTKERR"),
    (1 << 8, "IBUSERR"),
    (1 << 9, "PRECISERR"),
    (1 << 10, "IMPRECISERR"),
    (1 << 11, "UNSTKERR"),
    (1 << 12, "STKERR"),
    (1 << 16, "UNDEFINSTR"),
    (1 << 17, "INVSTATE"),
    (1 << 18, "INVPC"),
];
const CFSR_MMARVALID: u32 = 1 << 7;
const CFSR_BFARVALID: u32 = 1 << 14;

/// HFSR 原因位表
const HFSR_BITS: [(u32, &str); 3] = [
    (1 << 1, "VECTBL"),
    (1 << 30, "FORCED"),
    (1 << 31, "VECTTBL"),
];

/// 异常号 → 名称 (IPSR 值)
fn exception_name(n: u32) -> &'static str {
    match n {
        0 => "线程模式",
        2 => "NMI",
        3 => "HardFault",
        4 => "MemManage",
        5 => "BusFault",
        6 => "UsageFault",
        11 => "SVCall",
        12 => "DebugMonitor",
        14 => "PendSV",
        15 => "SysTick",
        16..=159 => "外部中断",
        _ => "未知",
    }
}

/// 读取 IPSR (当前异常号, 0 = 线程模式)
fn mrs_ipsr() -> u32 {
    let value: u32;
    unsafe {
        core::arch::asm!("mrs {}, ipsr", out(reg) value);
    }
    value
}

/// 读取 MSP
fn mrs_msp() -> u32 {
    let value: u32;
    unsafe {
        core::arch::asm!("mrs {}, msp", out(reg) value);
    }
    value
}
