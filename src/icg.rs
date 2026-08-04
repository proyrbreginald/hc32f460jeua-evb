//! ICG (初始化配置) 硬件配置段
//!
//! HC32F460 复位解除后, 硬件自动读取 flash `0x400~0x41F` 的 ICG 配置字
//! 并载入**运行期只读**的初始化配置寄存器 (ICG0~ICG7)。链接脚本将本段
//! 固定放置在 `0x400` (见 `link.ld` 的 `.icg`)。
//!
//! # 配置值 (对齐 DDL hc32_ll_icg.h 官方默认)
//!
//! - **ICG0** (0x400): 取 DDL `ICG_REG_CFG0_CONST` (WDT/SWDT 复位停止、
//!   异常动作=复位、计数周期 65536、分频 2048/8192、刷新窗口 0~100%、
//!   睡眠停止 + 位掩码 0xE000E000), 值 `0xFFDFFFBF`;
//! - **ICG1** (0x404): 取 DDL `ICG_REG_CFG1_CONST` (HRC 16M + 复位后
//!   振荡、BOR 阈值 2.3V + 复位后关闭、NMI 引脚滤波/上升沿/中断使能),
//!   值 `0xFFFFFEFF`; 其中 HRC 两项经配置调整:
//!   - `CFG_HRC_FREQ` (bit0 HRCFREQSEL): 16MHz 保持 1, 20MHz 清 0;
//!   - `CFG_HRC_STOP` (bit8 HRCSTOP): 复位后振荡保持 0, 停止置 1;
//! - **ICG2~ICG7** (0x408~0x41F): `ICG_REG_RESV_CONST` = 全 1 (官方预留)。

/// ICG 配置字 (8 × 32bit, flash 0x400~0x41F)
#[unsafe(link_section = ".icgs")]
#[unsafe(no_mangle)]
pub static ICGS: [u32; 8] = icg_words();

/// ICG0 配置字: DDL `ICG_REG_CFG0_CONST` (WDT/SWDT 预载配置 + 0xE000E000)
const ICG0_DEFAULT: u32 = 0xFFDF_FFBF;

/// ICG1 配置字: DDL `ICG_REG_CFG1_CONST` (NMI/BOR/HRC 预载配置 + 0x03F8FEFE)
///
/// 基值已含 HRCFREQSEL=1 (16MHz) 与 HRCSTOP=0 (复位后振荡),
/// 与 DDL 的 ICG_RB_HRC_* 一致; 配置项仅调整这两位。
const ICG1_DEFAULT: u32 = 0xFFFF_FEFF;

/// 由配置生成 ICG1 值:
/// - bit0 = HRCFREQSEL: 0=20MHz, 1=16MHz (对齐 ICG_ICG1_HRCFREQSEL);
/// - bit8 = HRCSTOP: 0=复位后振荡, 1=复位后停止 (对齐 ICG_ICG1_HRCSTOP)。
const fn icg1() -> u32 {
    let word = ICG1_DEFAULT;
    let word = if crate::config::HRC_FREQ_MHZ == 16 {
        word
    } else {
        word & !(1 << 0) // HRCFREQSEL=0 → 20MHz
    };
    if crate::config::HRC_STOP {
        word | (1 << 8) // HRCSTOP=1 → 复位后停止
    } else {
        word
    }
}

/// 生成全部 ICG 配置字 (const 求值, 编译期确定)
const fn icg_words() -> [u32; 8] {
    [
        ICG0_DEFAULT,  // ICG0 (0x400, DDL ICG_REG_CFG0_CONST)
        icg1(),        // ICG1 (0x404, HRC 频率/停止由配置调整)
        0xFFFF_FFFF,   // ICG2 (0x408, 预留)
        0xFFFF_FFFF,   // ICG3 (0x40C, 预留)
        0xFFFF_FFFF,   // ICG4 (0x410, 预留)
        0xFFFF_FFFF,   // ICG5 (0x414, 预留)
        0xFFFF_FFFF,   // ICG6 (0x418, 预留)
        0xFFFF_FFFF,   // ICG7 (0x41C, 预留)
    ]
}
