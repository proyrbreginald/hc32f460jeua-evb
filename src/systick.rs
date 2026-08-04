//! SysTick 节拍驱动
//!
//! 对齐 DDL `hc32_ll_utility.c` 的 SysTick_Init (内部即 CMSIS
//! `SysTick_Config`: LOAD=ticks-1, VAL=0, CTRL=CLKSOURCE|TICKINT|ENABLE)。
//!
//! # 时钟源
//!
//! CLKSOURCE=1 → SysTick 时钟 = **处理器时钟 HCLK** (系统时钟 ÷
//! SCFGR.HCLKS 分频), 运行时经 [`crate::clk::hclk_hz`] 查询 ——
//! 支持切换外部晶振时钟源后自动适配 (无需修改本模块), 且 HCLK
//! 分频非 1 时节拍频率依然正确 (DDL 直接用 SystemCoreClock, 仅在
//! HCLK÷1 时成立)。
//!
//! # 时序
//!
//! 中断频率 = `HCLK / (reload + 1)`, 其中 `reload` 为 24 位。
//! 中断服务函数由应用层实现 (RTOS 节拍, 见 `main::sys_tick_handler`),
//! 由向量表 [`crate::vector_table::EXCEPTIONS`] 的 SysTick 槽位
//! (异常 15) 指向, ISR 末尾留意 Arm Errata 838869 (DSB)。
//!
//! 注意: 本模块**不维护独立节拍计数** —— RTOS 已内置节拍
//! ([`crate::rtos::tick`]), 避免双计数源; 节拍类 API 请使用 RTOS 的。
//!
//! HAL 提供完整 API, 但应用往往只使用其中一部分 (节拍/延时/暂停恢复等),
//! 因此忽略未使用项的死代码警告。
#![allow(dead_code)]

// 无原子状态: 本模块不维护独立节拍计数 (RTOS 内置)

/// SysTick 外设基地址 (Cortex-M4 内核外设)
const SYST_BASE: usize = 0xE000_E010;

/// 重装载值寄存器宽度 (24 位)
const RELOAD_MASK: u32 = 0x00FF_FFFF;

/// 内存映射寄存器 (绝对地址, 32 位)
struct Reg {
    addr: usize,
}

impl Reg {
    const fn new(offset: usize) -> Self {
        Self {
            addr: SYST_BASE + offset,
        }
    }

    fn read(&self) -> u32 {
        unsafe { core::ptr::read_volatile(self.addr as *mut u32) }
    }

    fn write(&self, value: u32) {
        unsafe { core::ptr::write_volatile(self.addr as *mut u32, value) }
    }

    /// 读-改-写寄存器
    fn modify(&self, f: impl FnOnce(u32) -> u32) {
        self.write(f(self.read()));
    }
}

/// 控制与状态寄存器 (CSR)
const CSR: Reg = Reg::new(0x00);
/// 重装载值寄存器 (RVR)
const RVR: Reg = Reg::new(0x04);
/// 当前值寄存器 (CVR)
const CVR: Reg = Reg::new(0x08);

/// CSR 位定义
const CSR_ENABLE: u32 = 1 << 0; // 计数器使能
const CSR_TICKINT: u32 = 1 << 1; // 中断使能
const CSR_CLKSOURCE: u32 = 1 << 2; // 时钟源: 1 = 处理器时钟 (HCLK)

/// SysTick 配置失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystickError {
    /// 频率为 0, 非法
    InvalidFrequency,
    /// 频率超出硬件范围: 过高 (freq > HCLK/2) 或过低 (freq < HCLK/2^24)
    FrequencyOutOfRange,
}

/// 配置 SysTick 中断频率并启动计数器
///
/// 中断频率 `freq_hz` = HCLK / (reload + 1), 合法范围约为
/// `HCLK / 2^24 ≈ 0.48 Hz` ~ `HCLK / 2 ≈ 4 MHz`。
///
/// 写入顺序: 先设重装载值, 再清零当前值 (同时清除 COUNTFLAG),
/// 最后使能 (时钟源 + 中断 + 计数器), 避免计数器运行时修改装载值。
pub fn init(freq_hz: u32) -> Result<(), SystickError> {
    if freq_hz == 0 {
        return Err(SystickError::InvalidFrequency);
    }

    // 每次中断间隔的 HCLK 周期数 (运行时查询, 支持切时钟源/总线分频)
    let hclk = crate::clk::hclk_hz();
    let ticks = hclk / freq_hz;
    if ticks == 0 || ticks > RELOAD_MASK + 1 {
        return Err(SystickError::FrequencyOutOfRange);
    }

    RVR.write(ticks - 1);
    CVR.write(0);
    CSR.write(CSR_CLKSOURCE | CSR_TICKINT | CSR_ENABLE);
    Ok(())
}
