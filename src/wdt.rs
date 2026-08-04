//! 硬件看门狗 (WDT) 驱动
//!
//! 对齐 DDL `hc32_ll_wdt.c/h` (软件启动模式): 写 CR 配置计数时钟分频/
//! 溢出周期/刷新窗口/异常动作, **首次喂狗启动计数**。
//!
//! # 时钟与溢出时间
//!
//! WDT 计数时钟 = **PCLK3** (参考手册 WDT 章节)。本工程 PCLK3 = 50MHz
//! (CFG_DIV_PCLK3=4, 200MHz 系统时钟): 分频 2048 + 周期 65536 →
//! 溢出时间 ≈ 65536 × 2048 / 50MHz ≈ **2.68s**。
//!
//! 喂狗由 [`crate::rtos::idle`] 空闲线程执行 (每节拍 1ms, 余量充足):
//! - 任意线程死循环/死锁 (中断仍开启) → 空闲线程得不到运行 → 超时复位;
//! - 中断被屏蔽 (PRIMASK 常置) → WDT 独立于 CPU 计数 → 仍复位。
//!
//! # 启用
//!
//! `.cargo/config.toml` `CFG_WDT_ENABLE = "true"` (默认关闭: 调试器
//! 断点暂停时 WDT 会超时复位, 开发期建议关闭)。
//!
//! # 注意
//!
//! - 复位后 WDT 停止 (ICG0.WDTAUTS=1, 见 icg.rs), 首次喂狗才启动计数;
//! - 溢出动作固定为**复位** (WDT_EXP_TYPE_RST), 中断动作不适用于
//!   无人值守场景;
//! - 睡眠模式下继续计数 (SLPOFF=0): 空闲线程 wfi 期间 WDT 照常走,
//!   依赖 SysTick 节拍唤醒喂狗。

// 完整 API 供应用按需选用 (状态查询/诊断), 忽略未使用项的死代码警告
#![allow(dead_code)]

/// WDT 基址 (PCLK3 域)
const WDT_BASE: usize = 0x4004_9000;

// ---- 寄存器偏移 ----
const CR: usize = 0x00; // 控制
const SR: usize = 0x04; // 状态
const RR: usize = 0x08; // 刷新键

// ---- CR 位 (对齐 DDL WDT_CR_*) ----
const CR_PERI: u32 = 0x0000_0003; // [1:0] 计数周期 (3 = 65536)
const CR_CKS: u32 = 0x0000_00F0; // [7:4] 计数时钟分频
const CR_WDPT: u32 = 0x0000_0F00; // [11:8] 刷新窗口
const CR_SLPOFF: u32 = 0x0001_0000; // [16] 睡眠停止计数
const CR_ITS: u32 = 0x8000_0000; // [31] 异常动作: 1 = 复位
const CR_CLR_MASK: u32 = CR_PERI | CR_CKS | CR_WDPT | CR_SLPOFF | CR_ITS;

// ---- SR 位 ----
const SR_UDF: u32 = 1 << 16; // 计数下溢 (溢出)
const SR_REF: u32 = 1 << 17; // 刷新错误 (窗口外喂狗)

// ---- RR 键 (对齐 DDL WDT_REFRESH_KEY_*) ----
const RR_KEY_START: u32 = 0x0123;
const RR_KEY_END: u32 = 0x3210;

/// 看门狗配置 (编译期常量组合, 见 [`init`])
#[derive(Clone, Copy)]
pub struct Config {
    /// 计数时钟分频编码 (WDT_CR_CKS, 对齐 DDL WDT_CLK_DIV_*)
    pub cks: u32,
    /// 计数周期编码 (WDT_CR_PERI, 对齐 DDL WDT_CNT_PERIOD_*)
    pub peri: u32,
    /// 刷新窗口编码 (WDT_CR_WDPT, 对齐 DDL WDT_RANGE_*; 0x0F = 0~100%)
    pub wdpt: u32,
}

/// 常用配置: PCLK3=50MHz 下溢出 ≈ 2.68s
pub const DEFAULT: Config = Config {
    cks: 0x0B, // ÷2048
    peri: 0x03, // 65536
    wdpt: 0x0F, // 0~100%
};

/// 初始化 WDT (软件启动模式): 写 CR, 计数器未启动 (首次喂狗后启动)
///
/// 溢出动作固定为复位; 睡眠模式继续计数 (SLPOFF=0)。
/// 对齐 DDL `WDT_Init` (MODIFY_REG32(CR, CLR_MASK, ...))。
pub fn init(cfg: Config) {
    let cr = (cfg.peri & CR_PERI)
        | ((cfg.cks << 4) & CR_CKS)
        | ((cfg.wdpt << 8) & CR_WDPT)
        | CR_ITS; // 异常动作 = 复位
    let v = read(CR);
    write(CR, (v & !CR_CLR_MASK) | cr);
}

/// 喂狗 (首次调用同时启动计数; 对齐 DDL `WDT_FeedDog`)
///
/// 必须在溢出窗口内调用 (见 [`DEFAULT`]); 由空闲线程每节拍调用。
pub fn feed() {
    write(RR, RR_KEY_START);
    write(RR, RR_KEY_END);
}

/// 溢出标志 (SR.UDF, 复位后为 0 —— 复位动作已清除)
pub fn underflow_flag() -> bool {
    read(SR) & SR_UDF != 0
}

/// 刷新错误标志 (SR.REF: 窗口外喂狗)
pub fn refresh_error() -> bool {
    read(SR) & SR_REF != 0
}

fn read(offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((WDT_BASE + offset) as *const u32) }
}

fn write(offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile((WDT_BASE + offset) as *mut u32, value) };
}
