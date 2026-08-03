//! ICG (初始化配置) 硬件配置段
//!
//! HC32F460 复位解除后, 硬件自动读取 flash `0x400~0x41F` 的 ICG 配置字
//! 并载入**运行期只读**的初始化配置寄存器 (ICG0~ICG7)。链接脚本将本段
//! 固定放置在 `0x400` (见 `link.ld` 的 `.icg`)。
//!
//! 配置项经 `.cargo/config.toml` 生成 (见 [`crate::config`]):
//! - **ICG1.HRCFREQSEL** (bit0): HRC 频率 16/20MHz (`CFG_HRC_FREQ`);
//! - **ICG1.HRCSTOP** (bit8): HRC 复位后停止/振荡 (`CFG_HRC_STOP`);
//!
//! 其余配置字/位保持全 1 (官方默认); `0x408~0x40F` (ICG2/3) 与
//! `0x418~0x41F` (ICG6/7) 为预留区, 手册要求写入全 1。

/// ICG 配置字 (8 × 32bit, flash 0x400~0x41F)
#[unsafe(link_section = ".icgs")]
#[unsafe(no_mangle)]
pub static ICGS: [u32; 8] = icg_words();

/// ICG1 (flash 0x404):
/// - bit0  = HRCFREQSEL: 0=20MHz, 1=16MHz;
/// - bit8  = HRCSTOP: 0=复位后振荡, 1=复位后停止。
///
/// 其余位保持 1 (预留/官方默认)。
const fn icg1() -> u32 {
    // 默认全 1 (HRCFREQSEL=1 → 16MHz, HRCSTOP=1 → 复位后停止)
    let word = 0xFFFF_FFFF;
    let word = if crate::config::HRC_FREQ_MHZ == 16 {
        word
    } else {
        word & !(1 << 0) // HRCFREQSEL=0 → 20MHz
    };
    if crate::config::HRC_STOP {
        word
    } else {
        word & !(1 << 8) // HRCSTOP=0 → 复位后持续振荡
    }
}

/// 生成全部 ICG 配置字 (const 求值, 编译期确定)
const fn icg_words() -> [u32; 8] {
    [
        0xFFFF_FFFF, // ICG0 (0x400, 保持官方默认)
        icg1(),      // ICG1 (0x404, HRC 频率选择)
        0xFFFF_FFFF, // ICG2 (0x408, 预留)
        0xFFFF_FFFF, // ICG3 (0x40C, 预留)
        0xFFFF_FFFF, // ICG4 (0x410, 保持官方默认)
        0xFFFF_FFFF, // ICG5 (0x414, 保持官方默认)
        0xFFFF_FFFF, // ICG6 (0x418, 预留)
        0xFFFF_FFFF, // ICG7 (0x41C, 预留)
    ]
}
