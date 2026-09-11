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
//! panic/fault 后的行为由编译期配置 [`crate::config::PANIC_STRATEGY`]
//! (`.cargo/config.toml` `CFG_PANIC_STRATEGY`) 决定:
//! - [`PanicStrategy::Halt`]: 屏蔽中断后 wfi 死循环 (调试期推荐);
//! - [`PanicStrategy::Reset`]: 软复位重启 (产品部署推荐)。
//!
//! 注意: 输出依赖 [`crate::console`] 绑定的 UART, 未初始化时静默丢弃
//! (不会死锁)。
//!
//! `Reset` 变体默认未启用 (配置为 `halt`), 故忽略死代码警告。
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
        crate::arch::wait_for_interrupt();
    }
}

/// panic/fault 后的行为策略 (编译期常量, 经 `.cargo/config.toml`
/// `CFG_PANIC_STRATEGY` 配置)
const STRATEGY: PanicStrategy = crate::config::PANIC_STRATEGY;

/// SCB 外设基址 (Cortex-M4)
const SCB_BASE: usize = 0xE000_ED00;

/// 栈顶 (与 link.ld 的 RAM 段末尾一致), 用于估算栈使用量
const STACK_TOP: usize = 0x2002_7000;

use crate::exception_frame as ef;
/// Cortex-M 异常帧布局与 EXC_RETURN 位 (布局索引与主机回归测试见
/// [`crate::exception_frame`])。
use crate::exception_frame::{BASIC_WORDS, EXTENDED_BYTES, FP_WORDS, try_decode};

const EXC_RETURN_USE_PSP: u32 = 1 << 2;
const EXC_RETURN_BASIC_FRAME: u32 = 1 << 4;

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
// 由汇编在编译器生成函数序言前捕获 IPSR、EXC_RETURN、现场帧指针，
// 并按 EXC_RETURN.bit2 选择硬件实际使用的 MSP/PSP。随后直接跳入
// fault_diagnose (b 跳转不经过调用约定, r0~r3 传参)。
core::arch::global_asm!(
    ".syntax unified",
    ".cpu cortex-m4",
    ".thumb",
    ".section .text.fault_handler, \"ax\"",
    ".global fault_handler",
    ".type fault_handler, %function",
    ".thumb_func",
    "fault_handler:",
    "    mrs r0, ipsr", // r0: 异常号
    "    tst lr, #4",   // EXC_RETURN.bit2: 0=MSP, 1=PSP
    "    ite eq",
    "    mrseq r1, msp", // r1: 异常压栈区的原始 SP
    "    mrsne r1, psp",
    "    mov r2, lr", // r2: EXC_RETURN (bit4 区分 basic/extended)
    "    mov r3, r7", // r3: fault 指令所在函数的帧指针
    "    b fault_diagnose",
    ".size fault_handler, . - fault_handler",
);

// 硬件 fault 汇编入口 (由上方 global_asm 定义), 向量表引用此符号
unsafe extern "C" {
    pub fn fault_handler();
}

/// fault 现场诊断。
///
/// 由 [`fault_handler`] 汇编跳入: r0=ipsr, r1=stacked_sp,
/// r2=EXC_RETURN, r3=现场帧指针。
#[unsafe(no_mangle)]
unsafe extern "C" fn fault_diagnose(ipsr: u32, stacked_sp: u32, exc_return: u32, fp: u32) -> ! {
    write_fmt(format_args!(
        "=== 硬件故障 ===\r\n  异常号: {} ({})\r\n",
        ipsr,
        exception_name(ipsr)
    ));
    let cfsr = report_fault_registers();
    report_exception_frame(stacked_sp, exc_return, cfsr);
    if cfsr & CFSR_UNRELIABLE_STACK != 0 {
        write_fmt(format_args!("  栈状态不可信, 跳过回溯\r\n"));
    } else {
        report_backtrace(fp as usize);
    }
    terminate()
}

/// 打印异常压栈帧。
///
/// ARMv7-M 异常入口的压栈布局 (以异常入口时的 SP 为起点, 低地址在前):
///
/// ```text
/// +0x00 r0  +0x04 r1  +0x08 r2  +0x0C r3
/// +0x10 r12 +0x14 lr  +0x18 pc  +0x1C xpsr
/// +0x20 s0  ...                   +0x5C s15
/// +0x60 fpscr                     +0x64 reserved
/// ```
///
/// `EXC_RETURN.bit4 == 0` 表示扩展帧 (含 FPU 上下文, 共 0x68 字节),
/// **基本帧位于低地址、FP 上下文随后** (与 Cortex-M4F TRM / ARMv7-M ARM
/// B3.3.2 一致); bit4 == 1 时仅有基本帧 (0x20 字节)。原始 SP 始终指向
/// 基本帧的首字 r0。
fn report_exception_frame(stacked_sp: u32, exc_return: u32, cfsr: u32) {
    write_fmt(format_args!(
        "  EXC_RETURN=0x{:08x}, 原始栈指针=0x{:08x}\r\n",
        exc_return, stacked_sp
    ));

    if !valid_exc_return(exc_return) {
        write_fmt(format_args!("  EXC_RETURN 非法, 跳过异常帧\r\n"));
        return;
    }
    if cfsr & CFSR_UNRELIABLE_STACK != 0 {
        write_fmt(format_args!("  CFSR 栈状态不可信, 跳过异常帧\r\n"));
        return;
    }

    let extended = exc_return & EXC_RETURN_BASIC_FRAME == 0;
    let total_bytes = if extended {
        EXTENDED_BYTES
    } else {
        ef::BASIC_BYTES
    };
    let raw = stacked_sp as usize;
    if !stack_range_is_readable(raw, total_bytes) {
        write_fmt(format_args!("  异常帧范围无效/未对齐, 跳过\r\n"));
        return;
    }

    // 读原始压栈字 (首字 = 基本帧 r0; 扩展时拼接 FP 上下文), 交给
    // 纯解码契约 (布局索引经主机回归测试锁定)。
    let word_count = BASIC_WORDS + if extended { FP_WORDS } else { 0 };
    let mut words = [0u32; BASIC_WORDS + FP_WORDS];
    let frame = raw as *const u32;
    for (i, word) in words[..word_count].iter_mut().enumerate() {
        *word = unsafe { core::ptr::read_volatile(frame.add(i)) };
    }
    let Some(decoded) = try_decode(&words[..word_count], extended) else {
        write_fmt(format_args!("  异常帧解码失败, 跳过\r\n"));
        return;
    };
    let basic = decoded.basic;
    let stack_name = if exc_return & EXC_RETURN_USE_PSP != 0 {
        "PSP"
    } else {
        "MSP"
    };
    let frame_name = if extended { "extended FP" } else { "basic" };
    write_fmt(format_args!(
        "  异常帧 @0x{:08x} ({}，{}):\r\n    r0=0x{:08x} r1=0x{:08x} r2=0x{:08x} r3=0x{:08x}\r\n",
        raw, stack_name, frame_name, basic[0], basic[1], basic[2], basic[3]
    ));
    write_fmt(format_args!(
        "    r12=0x{:08x} lr=0x{:08x} pc=0x{:08x} xpsr=0x{:08x}\r\n",
        basic[4], basic[5], basic[6], basic[7]
    ));
    if let Some(fp_regs) = decoded.fp {
        write_fmt(format_args!(
            "    s0 =0x{:08x} s1 =0x{:08x} s2 =0x{:08x} s3 =0x{:08x}\r\n",
            fp_regs[0], fp_regs[1], fp_regs[2], fp_regs[3]
        ));
        write_fmt(format_args!(
            "    s4 =0x{:08x} s5 =0x{:08x} s6 =0x{:08x} s7 =0x{:08x}\r\n",
            fp_regs[4], fp_regs[5], fp_regs[6], fp_regs[7]
        ));
        write_fmt(format_args!(
            "    s8 =0x{:08x} s9 =0x{:08x} s10=0x{:08x} s11=0x{:08x}\r\n",
            fp_regs[8], fp_regs[9], fp_regs[10], fp_regs[11]
        ));
        write_fmt(format_args!(
            "    s12=0x{:08x} s13=0x{:08x} s14=0x{:08x} s15=0x{:08x}\r\n",
            fp_regs[12], fp_regs[13], fp_regs[14], fp_regs[15]
        ));
        write_fmt(format_args!(
            "    fpscr=0x{:08x} reserved=0x{:08x}\r\n",
            fp_regs[16], fp_regs[17]
        ));
    }
}

/// Cortex-M4/M4F 可生成的 EXC_RETURN 编码。
fn valid_exc_return(value: u32) -> bool {
    matches!(
        value,
        0xFFFF_FFF1 | 0xFFFF_FFF9 | 0xFFFF_FFFD | 0xFFFF_FFE1 | 0xFFFF_FFE9 | 0xFFFF_FFED
    )
}

/// 地址区间是否完整位于主 SRAM 且满足硬件字对齐。
fn stack_range_is_readable(addr: usize, bytes: usize) -> bool {
    addr & (core::mem::align_of::<u32>() - 1) == 0
        && addr >= STACK_BOTTOM
        && addr.checked_add(bytes).is_some_and(|end| end <= STACK_TOP)
}

/// 栈回溯: 沿帧指针链收集返回地址
///
/// AAPCS 帧布局 (force-frame-pointers): `[fp+0]` = 前一帧 FP,
/// `[fp+4]` = 返回地址 (LR)。合法性检查防止越界读与循环链:
/// - 帧指针必须在栈范围内、按字对齐且向调用者方向单调递增;
/// - 返回地址必须指向 flash 代码区 (thumb 位 + 512K 范围)。
///
/// 注意: core 库函数 (无帧指针) 可能中断链, 故不保证完整覆盖;
/// fault 场景的现场 r7 精确, 本 crate 内部调用链完整。
fn stack_backtrace(mut fp: usize, frames: &mut [usize]) -> usize {
    let mut n = 0;
    while n < frames.len() {
        // 帧必须在栈范围内 (fp 与 fp+8 均合法)，checked_add 防回绕。
        if !stack_range_is_readable(fp, 2 * core::mem::size_of::<usize>()) {
            break;
        }
        let prev = unsafe { core::ptr::read_volatile(fp as *const usize) };
        let Some(pc_addr) = fp.checked_add(core::mem::size_of::<usize>()) else {
            break;
        };
        let pc = unsafe { core::ptr::read_volatile(pc_addr as *const usize) };
        // 返回地址必须指向 flash 代码区 (thumb 位为 1)
        if pc & 1 == 0 || pc & !1 >= FLASH_SIZE {
            break;
        }
        frames[n] = pc;
        n += 1;
        // 栈向下增长: 调用者帧位于更高地址，且下一帧自身也必须可读。
        if prev <= fp || !stack_range_is_readable(prev, 2 * core::mem::size_of::<usize>()) {
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
    let uses_psp = ipsr == 0 && mrs_control() & (1 << 1) != 0;
    if uses_psp {
        write_fmt(format_args!(
            "  上下文: {} (ipsr={}), PSP=0x{:08x}\r\n",
            exception_name(ipsr),
            ipsr,
            mrs_psp()
        ));
    } else {
        let sp = mrs_msp();
        let stack_used = STACK_TOP.saturating_sub(sp as usize);
        write_fmt(format_args!(
            "  上下文: {} (ipsr={}), MSP=0x{:08x}, 主栈使用=0x{:x} B\r\n",
            exception_name(ipsr),
            ipsr,
            sp,
            stack_used
        ));
    }
}

/// 读取并解码 SCB fault 状态寄存器 (CFSR/HFSR/BFAR/MMFAR)
///
/// 逐位原因名 (CFSR_BITS/HFSR_BITS) 由 `CFG_PANIC_VERBOSE` 控制:
/// 关闭时仅输出原始寄存器值 (对照参考手册 CFSR/HFSR 位定义解码)。
fn report_fault_registers() -> u32 {
    let cfsr = crate::mmio::Reg::new(SCB_BASE + 0x28).read();
    let hfsr = crate::mmio::Reg::new(SCB_BASE + 0x2C).read();
    let mmfar = crate::mmio::Reg::new(SCB_BASE + 0x34).read();
    let bfar = crate::mmio::Reg::new(SCB_BASE + 0x38).read();

    write_fmt(format_args!("  CFSR 0x{:08x}:", cfsr));
    #[cfg(panic_verbose)]
    for (mask, name) in CFSR_BITS {
        if cfsr & mask != 0 {
            write_fmt(format_args!(" {}", name));
        }
    }
    write_fmt(format_args!("\r\n  HFSR 0x{:08x}:", hfsr));
    #[cfg(panic_verbose)]
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
    cfsr
}

/// 收尾: 按 [`STRATEGY`] 停机或软复位 (屏蔽中断, 防止被打断)
fn terminate() -> ! {
    crate::arch::disable_interrupts();
    // 故障指示: 点亮板载 ERROR LED (不依赖 BoardResources, 可在异常上下文调用)。
    // reset 策略下软复位会重新初始化 GPIO, 该指示只在 halt 策略下保持。
    crate::board::indicate_fault_early();
    match STRATEGY {
        PanicStrategy::Halt => loop {
            crate::arch::wait_for_interrupt();
        },
        PanicStrategy::Reset => crate::arch::system_reset(),
    }
}

/// CFSR 原因位表: (掩码, 名称)
///
/// MMFSR[7:0] + BFSR[15:8] + UFSR[24:16]。仅 `panic_verbose` 构建
/// 携带 (省 ~1.5KiB); 非详细模式输出原始寄存器值。
#[cfg(panic_verbose)]
const CFSR_BITS: [(u32, &str); 14] = [
    (1 << 0, "IACCVIOL"),
    (1 << 1, "DACCVIOL"),
    (1 << 3, "MUNSTKERR"),
    (1 << 4, "MSTKERR"),
    (1 << 5, "MLSPERR"),
    (1 << 8, "IBUSERR"),
    (1 << 9, "PRECISERR"),
    (1 << 10, "IMPRECISERR"),
    (1 << 11, "UNSTKERR"),
    (1 << 12, "STKERR"),
    (1 << 13, "LSPERR"),
    (1 << 16, "UNDEFINSTR"),
    (1 << 17, "INVSTATE"),
    (1 << 18, "INVPC"),
];
const CFSR_MMARVALID: u32 = 1 << 7;
const CFSR_BFARVALID: u32 = 1 << 15;
/// 异常帧可能没有完整生成或栈状态已不可信。
const CFSR_UNRELIABLE_STACK: u32 =
    (1 << 3) | (1 << 4) | (1 << 5) | (1 << 11) | (1 << 12) | (1 << 13);

/// HFSR 原因位表
#[cfg(panic_verbose)]
const HFSR_BITS: [(u32, &str); 3] = [
    (1 << 1, "VECTTBL"),
    (1 << 30, "FORCED"),
    (1 << 31, "DEBUGEVT"),
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

/// 读取 PSP。
fn mrs_psp() -> u32 {
    let value: u32;
    unsafe {
        core::arch::asm!("mrs {}, psp", out(reg) value);
    }
    value
}

/// 读取 CONTROL (bit1=线程模式使用 PSP)。
fn mrs_control() -> u32 {
    let value: u32;
    unsafe {
        core::arch::asm!("mrs {}, control", out(reg) value);
    }
    value
}
