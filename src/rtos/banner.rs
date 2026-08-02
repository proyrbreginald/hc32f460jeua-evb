//! 启动横幅 (banner) — 内核启动时的标志性输出
//!
//! 块字符大标题 + 信息面板: 实时查询时钟/堆/就绪线程数,
//! 构建日期与 rustc 版本由 [`build.rs`] 注入。
//!
//! 须在创建全部线程后、[`crate::rtos::start`] 之前调用
//! (此时系统仍为单执行流, 打印安全, 且就绪统计包含所有线程)。

use crate::clk;
use crate::heap;
use crate::println; // #[macro_export] 宏需显式引入
use crate::rtos::{sched, PRIORITY_MAX, TICKS_PER_SEC};

/// 内核名称
pub const KERNEL_NAME: &str = "RT-Rust";
/// 内核版本 (与 Cargo.toml 包版本一致)
pub const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 块字符大标题 "RT-RUST" (5 行, 5x5 字模, 每字符 5 列 + 2 列间距)
const TITLE: [&str; 7] = [
    "ooooooooo.   ooooooooooooo         ooooooooo.                            .   ",
    "`888   `Y88. 8'   888   `8         `888   `Y88.                        .o8   ",
    " 888   .d88'      888               888   .d88' oooo  oooo   .oooo.o .o888oo ",
    " 888ooo88P'       888               888ooo88P'  `888  `888  d88(  \"8   888   ",
    " 888`88b.         888      8888888  888`88b.     888   888  `\"Y88b.    888   ",
    " 888  `88b.       888               888  `88b.   888   888  o.  )88b   888 . ",
    "o888o  o888o     o888o             o888o  o888o  `V88V\"V8P' 8\"\"888P\'   \"888\" ",
];

/// 信息面板分隔线
const SEP: &str = "── ── ── ── ── ── ── ── ── ── ── ── ── ──";

/// 构建日期 (UTC, 由 build.rs 注入)
const BUILD_DATE: &str = env!("RTOS_BUILD_DATE");
/// rustc 版本 (由 build.rs 注入)
const RUSTC: &str = env!("RTOS_RUSTC");
/// 构建 profile
const PROFILE: &str = if cfg!(debug_assertions) { "debug" } else { "release" };

/// 输出启动横幅 (每行原子打印)
pub fn show() {
    for line in TITLE {
        println!("{}", line);
    }
    println!();
    println!(
        "  {} v{}  —  RT-Thread 架构的 Rust RTOS (HC32F460JEUA)",
        KERNEL_NAME, KERNEL_VERSION
    );
    println!("  {}", SEP);
    // XTAL 状态 (失败时已自动回退 MRC, 在此显式告警)
    let xtal = match clk::xtal_status() {
        clk::XtalStatus::Active => "",
        clk::XtalStatus::Failed => " (XTAL failed, fallback MRC)",
        clk::XtalStatus::NotAttempted => " (XTAL not attempted)",
    };
    println!(
        "  处理器    : Cortex-M4F @ {} MHz{}",
        clk::system_clock_hz() / 1_000_000,
        xtal
    );
    println!("  节拍      : {} ms ({} Hz)", 1000 / TICKS_PER_SEC, TICKS_PER_SEC);
    println!("  堆        : {} KB", heap::capacity() / 1024);
    println!("  调度器    : 位图 {} 级优先级 + 时间片轮转", PRIORITY_MAX);
    println!("  进程通信  : 信号量 / 互斥量(优先级继承) / 事件 / 邮箱 / 消息队列");
    println!("  定时器    : 硬定时器 (有序链表, 节拍回绕安全)");
    println!("  控制台    : 原子整行打印 (优先级继承锁)");
    println!("  {}", SEP);
    println!("  构建      : {}  [{}]  {}", BUILD_DATE, PROFILE, RUSTC);
    println!(
        "  就绪      : {} 个线程 (位图 0x{:08x})",
        sched::ready_thread_count(),
        sched::ready_group_value()
    );
}
