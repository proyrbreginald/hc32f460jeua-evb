//! SysTick 节拍驱动
//!
//! 对齐 DDL `hc32_ll_utility.c` 的 SysTick 驱动:
//! [`init`](SysTick_Init) / [`on_tick`](SysTick_IncTick) /
//! [`get_tick_ms`](SysTick_GetTick) / [`delay_ms`](SysTick_Delay) /
//! [`suspend`](SysTick_Suspend) / [`resume`](SysTick_Resume)。
//!
//! # 时钟源
//!
//! SysTick 时钟 = HCLK = 当前系统时钟, 运行时通过
//! [`crate::clk::system_clock_hz`] 查询 —— 支持切换外部晶振时钟源后
//! 自动适配 (无需修改本模块)。
//!
//! # 时序
//!
//! 中断频率 = `HCLK / (reload + 1)`, 其中 `reload` 为 24 位。
//! 节拍计数以毫秒为单位: 每次中断累加 `1000 / freq`
//! (与 DDL `SysTick_Init` 的 `m_u32TickStep` 一致), 因此 **仅当
//! `freq ≤ 1000 Hz` 时 tick 计数与 [`delay_ms`] 有效**; 更高频率下
//! SysTick 仍按配置触发中断, 但节拍功能不可用。
//!
//! # 中断安全
//!
//! 节拍计数使用原子类型, [`on_tick`] 在中断上下文自增,
//! 主循环可随时查询, 无竞争。
//!
//! 中断服务函数由应用层实现 (翻转 LED 等业务), 由向量表
//! [`crate::vector_table::EXCEPTIONS`] 的 SysTick 槽位 (异常 15) 指向,
//! 函数体内应首先调用 [`on_tick`] 并留意 Arm Errata 838869 (ISR 末尾 DSB)。
//!
//! HAL 提供完整 API, 但应用往往只使用其中一部分 (节拍/延时/暂停恢复等),
//! 因此忽略未使用项的死代码警告。
#![allow(dead_code)]

use core::sync::atomic::{AtomicU32, Ordering};

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

/// 节拍计数 (毫秒), 由 [`on_tick`] 在中断中累加
static TICK_MS: AtomicU32 = AtomicU32::new(0);

/// 每次中断累加的毫秒数 (1000 / freq)
static TICK_STEP_MS: AtomicU32 = AtomicU32::new(0);

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

    // 每次中断间隔的 HCLK 周期数 (系统时钟运行时查询, 支持切时钟源)
    let hclk = crate::clk::system_clock_hz();
    let ticks = hclk / freq_hz;
    if ticks == 0 || ticks > RELOAD_MASK + 1 {
        return Err(SystickError::FrequencyOutOfRange);
    }

    RVR.write(ticks - 1);
    CVR.write(0);
    CSR.write(CSR_CLKSOURCE | CSR_TICKINT | CSR_ENABLE);

    // 毫秒步进: freq ≤ 1000 时为 1~1000; 更高频率下为 0, 节拍功能不可用
    TICK_STEP_MS.store(1000 / freq_hz, Ordering::Relaxed);
    Ok(())
}

/// SysTick 中断服务函数内调用: 累加节拍计数 (对齐 DDL `SysTick_IncTick`)
pub fn on_tick() {
    TICK_MS.fetch_add(TICK_STEP_MS.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// 查询节拍计数 (毫秒, 对齐 DDL `SysTick_GetTick`)
///
/// 无符号回绕安全: 约 49.7 天 (2^32 ms) 回绕, 差值计算不受影响。
pub fn get_tick_ms() -> u32 {
    TICK_MS.load(Ordering::Relaxed)
}

/// 基于节拍计数的忙等待 (对齐 DDL `SysTick_Delay`)
///
/// 仅在 `freq ≤ 1000 Hz` 时有效 (见模块文档)。
/// 注意: 若中断被 [`suspend`] 暂停或 SysTick 未配置, 本函数可能无法返回。
pub fn delay_ms(ms: u32) {
    let start = get_tick_ms();
    while get_tick_ms().wrapping_sub(start) < ms {
        // 空转等待
    }
}

/// 暂停 SysTick 中断 (计数器继续运行, 对齐 DDL `SysTick_Suspend`)
pub fn suspend() {
    CSR.modify(|v| v & !CSR_TICKINT);
}

/// 恢复 SysTick 中断 (对齐 DDL `SysTick_Resume`)
pub fn resume() {
    CSR.modify(|v| v | CSR_TICKINT);
}
