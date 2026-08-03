//! 启动横幅 (banner) — 应用启动时的标志性输出
//!
//! 块字符大标题 + 信息面板: 实时查询时钟/堆/就绪线程数,
//! 构建日期与 rustc 版本由 [`build.rs`] 注入。
//!
//! 位于应用层 (非内核): 内核 [`crate::rtos`] 不依赖任何应用模块。
//! 须在创建全部线程后、[`crate::rtos::start`] 之前调用
//! (此时系统仍为单执行流, 打印安全, 且就绪统计包含所有线程)。

use crate::clk;
use crate::heap;
use crate::println; // #[macro_export] 宏需显式引入
use crate::rtos::{IDLE_PRIORITY, PRIORITY_MAX, TICKS_PER_SEC, sched};

/// 内核版本 (与 Cargo.toml 包版本一致)
pub const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");
/// 芯片型号 / 内核名 (板级信息, 来自 .cargo/config.toml `[env]`)
const CHIP_MODEL: &str = crate::config::CHIP_MODEL;
const CORE: &str = crate::config::CORE;

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
const SEP: &str = "── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ──";

/// 构建日期 (UTC, 由 build.rs 注入)
const BUILD_DATE: &str = env!("RTOS_BUILD_DATE");
/// rustc 版本 (由 build.rs 注入)
const RUSTC: &str = env!("RTOS_RUSTC");
/// 构建 profile
const PROFILE: &str = if cfg!(debug_assertions) {
    "debug"
} else {
    "release"
};

/// 输出启动横幅 (每行原子打印)
pub fn show() {
    println!();
    for line in TITLE {
        println!("{}", line);
    }
    println!();
    println!(
        "{} v{}  —  RT-Thread 架构的 Rust RTOS ({})",
        env!("CARGO_PKG_NAME"),
        KERNEL_VERSION,
        CHIP_MODEL
    );
    println!("{}", SEP);
    // XTAL 状态 (失败时已自动回退 MRC, 在此显式告警)
    let xtal = match clk::xtal_status() {
        clk::XtalStatus::Active => "",
        clk::XtalStatus::Failed => " (XTAL failed, fallback MRC)",
        clk::XtalStatus::NotAttempted => " (XTAL not attempted)",
    };
    println!(
        "处理器 : {} @ {} MHz{}",
        CORE,
        clk::system_clock_hz() / 1_000_000,
        xtal
    );
    println!("节拍 : {} ms ({} Hz)", 1000 / TICKS_PER_SEC, TICKS_PER_SEC);
    println!("优先级 : {} 级 (空闲 = {})", PRIORITY_MAX, IDLE_PRIORITY);
    println!("堆 : {} KB", heap::capacity() / 1024);
    println!("{}", SEP);
    println!("构建 : {} [{}] {}", BUILD_DATE, PROFILE, RUSTC);
    println!(
        "就绪 : {} 个线程 (位图 0x{:08x})",
        sched::ready_thread_count(),
        sched::ready_group_value()
    );
}
