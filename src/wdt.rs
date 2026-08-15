//! 硬件看门狗 (WDT) 驱动
//!
//! 对齐 DDL `hc32_ll_wdt.c/h` (软件启动模式): 写 CR 配置计数时钟分频/
//! 溢出周期/刷新窗口/异常动作, **首次喂狗启动计数**。
//!
//! # 时钟与溢出时间
//!
//! WDT 计数时钟 = **PCLK3** (参考手册 WDT 章节)。默认配置下 PCLK3 =
//! 50MHz (CFG_DIV_PCLK3=4, 200MHz 系统时钟): 分频 2048 + 周期 65536 →
//! 溢出时间 ≈ 65536 × 2048 / 50MHz ≈ **2.68s**。
//! BSP 启动时按硬件实际 PCLK3 重新计算超时并校验喂狗余量，因此系统
//! 时钟回退或修改总线分频不会沿用这个默认值。
//!
//! 喂狗由最高优先级 supervisor 线程周期执行：线程每次喂狗后主动
//! 休眠，约 500ms 后由 SysTick 唤醒。轮询 UART 等合法长任务即使令
//! idle 长时间得不到运行也不会误复位；若 SysTick、PendSV 或中断长期
//! 停止工作，supervisor 无法再次运行，WDT 仍会复位。
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
//! - 睡眠模式下继续计数 (SLPOFF=0)，依赖 SysTick 唤醒 supervisor。

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// 计数时钟分频编码 (WDT_CR_CKS, 对齐 DDL WDT_CLK_DIV_*)
    pub cks: u32,
    /// 计数周期编码 (WDT_CR_PERI, 对齐 DDL WDT_CNT_PERIOD_*)
    pub peri: u32,
    /// 刷新窗口编码 (WDT_CR_WDPT, 对齐 DDL WDT_RANGE_*; 0x0F = 0~100%)
    pub wdpt: u32,
}

impl Config {
    /// DDL CKS 编码对应的实际分频系数。
    const fn clock_divider(self) -> Option<u32> {
        match self.cks {
            0x02 => Some(4),
            0x06 => Some(64),
            0x07 => Some(128),
            0x08 => Some(256),
            0x09 => Some(512),
            0x0A => Some(1024),
            0x0B => Some(2048),
            0x0D => Some(8192),
            _ => None,
        }
    }

    /// DDL PERI 编码对应的计数周期。
    const fn count_cycles(self) -> Option<u32> {
        match self.peri {
            0x00 => Some(256),
            0x01 => Some(4096),
            0x02 => Some(16_384),
            0x03 => Some(65_536),
            _ => None,
        }
    }

    /// 配置编码是否合法。WDPT=0 未由 DDL 定义，其余 1~15 均有效。
    pub const fn is_valid(self) -> bool {
        self.clock_divider().is_some()
            && self.count_cycles().is_some()
            && self.wdpt >= 1
            && self.wdpt <= 0x0F
    }

    /// 按实际 PCLK3 计算溢出时间（微秒，向下取整）。
    ///
    /// 使用 `u64` 中间值，覆盖所有合法 CKS/PERI 组合而不溢出。配置
    /// 非法或 PCLK3 未运行时返回 `None`。
    pub fn timeout_us(self, pclk3_hz: u32) -> Option<u64> {
        if pclk3_hz == 0 || !self.is_valid() {
            return None;
        }
        let ticks = u64::from(self.clock_divider()?) * u64::from(self.count_cycles()?);
        Some(ticks * 1_000_000 / u64::from(pclk3_hz))
    }
}

/// 常用配置: PCLK3=50MHz 下溢出 ≈ 2.68s
pub const DEFAULT: Config = Config {
    cks: 0x0B,  // ÷2048
    peri: 0x03, // 65536
    wdpt: 0x0F, // 0~100%
};

/// 初始化 WDT (软件启动模式): 写 CR, 计数器未启动 (首次喂狗后启动)
///
/// 溢出动作固定为复位; 睡眠模式继续计数 (SLPOFF=0)。
/// 对齐 DDL `WDT_Init` (MODIFY_REG32(CR, CLR_MASK, ...))。
pub fn init(cfg: Config) {
    assert!(cfg.is_valid(), "WDT 配置编码非法");
    let cr =
        (cfg.peri & CR_PERI) | ((cfg.cks << 4) & CR_CKS) | ((cfg.wdpt << 8) & CR_WDPT) | CR_ITS; // 异常动作 = 复位
    let v = read(CR);
    write(CR, (v & !CR_CLR_MASK) | cr);
}

/// 喂狗 (首次调用同时启动计数; 对齐 DDL `WDT_FeedDog`)
///
/// 必须在溢出窗口内调用 (见 [`DEFAULT`]); 由 BSP 的高优先级 supervisor
/// 按配置周期调用。
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
    crate::mmio::Reg::new(WDT_BASE + offset).read()
}

fn write(offset: usize, value: u32) {
    crate::mmio::Reg::new(WDT_BASE + offset).write(value);
}
