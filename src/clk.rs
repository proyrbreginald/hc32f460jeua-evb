//! 时钟管理: 外部高速晶振 (XTAL) 支持与系统时钟源
//!
//! 参考 DDL `CLK_XtalInit` / `CLK_XtalCmd` / `CLK_SetSysClockSrc` 与
//! `BSP_CLK_Init`, 参考手册 CMU 章节 (XTAL 频率范围 4~25MHz)。
//!
//! # 时钟链 (配置后)
//!
//! XTAL (默认 8MHz, 修改 [`XTAL_HZ`]) → 系统时钟 (CKSWR.CKSW=3)
//! → HCLK / PCLK0~4 (SCFGR 无分频, 复位默认)
//!
//! # 使用流程
//!
//! 1. [`xtal_init`]: 配置 PH0/PH1 模拟功能 → 配置/启动晶振 → 等待稳定;
//! 2. [`switch_to_xtal`]: 按新频率配置 FLASH 等待周期 (表 7-1) 后切换时钟源。
//!
//! 切换后各外设通过 [`system_clock_hz`] / [`pclk1_hz`] 查询实际频率
//! (systick/uart 模块已接入)。
//!
//! 部分位掩码常量仅作寄存器位定义文档用途, 忽略死代码警告。
#![allow(dead_code)]

use core::sync::atomic::{AtomicU8, Ordering};

use crate::gpio::{Config, Drive, Level, Mode, Pin, PortH};

/// 外部高速晶振频率 (Hz)。合法范围 4~25MHz (参考手册)。
/// **按实际电路板晶振修改此常量!**
pub const XTAL_HZ: u32 = 8_000_000;
const _: () = assert!(XTAL_HZ >= 4_000_000 && XTAL_HZ <= 25_000_000, "XTAL 频率必须在 4~25MHz");

/// 内部中速 RC 频率 (Hz), 复位默认系统时钟源
pub const MRC_HZ: u32 = 8_000_000;
/// 内部低速 RC 频率 (Hz)
const LRC_HZ: u32 = 32_768;
/// 外部低速晶振频率 (Hz)
const XTAL32_HZ: u32 = 32_768;

/// CMU 外设基址
const CMU_BASE: usize = 0x4005_4000;
/// PWC 外设基址 (CLK 寄存器写保护控制)
const PWC_BASE: usize = 0x4004_8000;
/// PWC.FPRC 偏移: 写 0xA5 键值 + 解锁位 0/1 → 允许写 CMU 寄存器
const PWC_FPRC_OFF: usize = 0xC3FE;
/// FPRC 解锁值 (对齐 DDL PWC_UNLOCK_CODE0|CODE1)
const FPRC_UNLOCK: u16 = 0xA503;
/// FPRC 锁定值 (键值 0xA5, 无解锁位)
const FPRC_LOCK: u16 = 0xA500;

/// CMU 寄存器偏移 (SVD)
const CMU_CKSWR: usize = 0x26; // 系统时钟源选择 (8 位)
const CMU_XTALCR: usize = 0x32; // XTAL 控制 (8 位)
const CMU_OSCSTBSR: usize = 0x3C; // 振荡器稳定状态 (8 位)
const CMU_SCFGR: usize = 0x20; // 总线时钟分频 (32 位)
const CMU_XTALSTBCR: usize = 0xA2; // XTAL 稳定时间 (8 位)
const CMU_XTALCFGR: usize = 0x410; // XTAL 配置 (8 位)
const CMU_PLLCFGR: usize = 0x100; // MPLL 配置 (32 位)

/// 系统时钟源编码 (CKSW[2:0])
pub const CLK_SRC_HRC: u32 = 0;
pub const CLK_SRC_MRC: u32 = 1;
pub const CLK_SRC_LRC: u32 = 2;
pub const CLK_SRC_XTAL: u32 = 3;
pub const CLK_SRC_XTAL32: u32 = 4;
pub const CLK_SRC_PLL: u32 = 5;

/// OSCSTBSR 位
const OSCSTBSR_XTALSTBF: u32 = 1 << 3; // XTAL 稳定标志

/// XTALSTBCR 位
const XTALSTBCR_XTALSTB_MASK: u32 = 0x0F; // 稳定时间选择

/// XTALCFGR 位
const XTALCFGR_XTALDRV_POS: u32 = 4; // [5:4] 驱动能力
const XTALCFGR_SUPDRV: u32 = 1 << 7; // 超强驱动

/// SCFGR 位
const SCFGR_PCLK1S_POS: u32 = 4; // [6:4] PCLK1 分频

/// 稳定时间: 约 2ms (对齐 DDL CLK_XTAL_STB_2MS)
const XTAL_STABLE_TIME: u32 = 0x05;
/// 驱动能力: 超低驱动 (对齐 DDL CLK_XTAL_DRV_ULOW), 典型 4~8MHz 晶振
const XTAL_DRV: u32 = 0x03 << XTALCFGR_XTALDRV_POS;
/// 振荡模式 (对齐 DDL CLK_XTAL_MD_OSC)
const XTAL_MODE_OSC: u32 = 0x00;

/// 时钟配置失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClkError {
    /// 晶振起振超时 (检查电路/引脚/稳定时间)
    XtalStableTimeout,
}

/// 外部晶振状态 (由 [`xtal_init`] / [`switch_to_xtal`] 更新, [`xtal_status`] 查询)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XtalStatus {
    /// 未尝试 (未调用 [`xtal_init`])
    NotAttempted,
    /// 晶振启动成功并已切换为系统时钟源
    Active,
    /// 启动失败 (起振超时), 系统继续使用默认时钟源 (MRC)
    Failed,
}

const STATUS_NOT_ATTEMPTED: u8 = 0;
const STATUS_ACTIVE: u8 = 1;
const STATUS_FAILED: u8 = 2;

/// 外部晶振状态记录 (中断无关, Relaxed 足够)
static XTAL_STATUS: AtomicU8 = AtomicU8::new(STATUS_NOT_ATTEMPTED);

/// 查询外部晶振当前状态
///
/// 供应用在输出通道就绪后 (如 UART 初始化完成) 报告时钟配置结果。
pub fn xtal_status() -> XtalStatus {
    match XTAL_STATUS.load(Ordering::Relaxed) {
        STATUS_ACTIVE => XtalStatus::Active,
        STATUS_FAILED => XtalStatus::Failed,
        _ => XtalStatus::NotAttempted,
    }
}

/// 启动外部高速晶振并等待稳定 (对齐 DDL CLK_XtalInit + CLK_XtalCmd)
///
/// 序列: PH0/PH1 模拟功能 (DDIS=1) → CMU 解锁 → 稳定时间/驱动/模式
/// → 启动 (XTALCR=0) → 等待 OSCSTBSR.XTALSTBF。
///
/// 晶振引脚: PH0 (XTAL_OUT) / PH1 (XTAL_IN), 见数据手册表 2-1。
pub fn xtal_init() -> Result<(), ClkError> {
    // 1. 配置晶振引脚为模拟功能 (对齐 BSP_CLK_Init 的 GPIO_AnalogCmd)
    Pin::<PortH, 0>::new().configure(Config {
        mode: Mode::Analog,
        pull_up: false,
        drive: Drive::Low,
        initial_level: Level::Low,
    });
    Pin::<PortH, 1>::new().configure(Config {
        mode: Mode::Analog,
        pull_up: false,
        drive: Drive::Low,
        initial_level: Level::Low,
    });

    // 2. 解锁 CMU 寄存器
    cmu_unlock();

    // 3. 稳定时间 (必须 ≥ 晶振厂商要求)
    write8(CMU_BASE + CMU_XTALSTBCR, XTAL_STABLE_TIME);
    // 4. 驱动能力/模式
    write8(CMU_BASE + CMU_XTALCFGR, XTAL_DRV | XTAL_MODE_OSC);
    // 5. 启动晶振 (XTALSTP=0)
    write8(CMU_BASE + CMU_XTALCR, 0);

    // 6. 等待稳定 (带超时)
    let mut timeout = 0u32;
    while read8(CMU_BASE + CMU_OSCSTBSR) & OSCSTBSR_XTALSTBF == 0 {
        timeout += 1;
        if timeout > 1_000_000 {
            cmu_lock();
            // 记录失败状态: 应用在输出通道就绪后报告
            XTAL_STATUS.store(STATUS_FAILED, Ordering::Relaxed);
            return Err(ClkError::XtalStableTimeout);
        }
    }

    // 7. 恢复写保护
    cmu_lock();
    Ok(())
}

/// 切换系统时钟源到外部晶振
///
/// **切换前**按目标频率配置 FLASH/SRAM 等待周期 (表 7-1/8-1):
/// 高时钟下若等待周期不足, 切换瞬间取指/栈操作即出错, 顺序不可颠倒。
pub fn switch_to_xtal() {
    set_flash_wait_cycle(XTAL_HZ);
    set_sram_wait_cycle(XTAL_HZ);
    cmu_unlock();
    write8(CMU_BASE + CMU_CKSWR, CLK_SRC_XTAL);
    // 等待时钟源切换稳定 (对齐 DDL CLK_SYSCLK_SW_STB)
    delay_short();
    cmu_lock();
    // 记录使用中状态
    XTAL_STATUS.store(STATUS_ACTIVE, Ordering::Relaxed);
}

/// 当前系统时钟频率 (Hz), 依据 CKSWR 实时查询
pub fn system_clock_hz() -> u32 {
    match read8(CMU_BASE + CMU_CKSWR) & 0x7 {
        CLK_SRC_HRC => hrc_hz(),
        CLK_SRC_MRC => MRC_HZ,
        CLK_SRC_LRC => LRC_HZ,
        CLK_SRC_XTAL => XTAL_HZ,
        CLK_SRC_XTAL32 => XTAL32_HZ,
        CLK_SRC_PLL => pll_hz(),
        _ => 0,
    }
}

/// PCLK1 频率 (Hz), 由系统时钟与 SCFGR.PCLK1S 分频推导
pub fn pclk1_hz() -> u32 {
    let pclk1s = (read32(CMU_BASE + CMU_SCFGR) >> SCFGR_PCLK1S_POS) & 0x7;
    system_clock_hz() >> pclk1s
}

/// 表 8-1: SRAM 读写访问等待周期设定与 CPU 时钟频率的关系
///
/// - SRAMH (高速): 恒 0 等待, 0~200MHz 全支持;
/// - SRAM1/2/Ret: HCLK ≤ 100MHz → 0 等待; HCLK > 100MHz → 1 等待;
/// - SRAM3: **恒 1 等待** —— 表 8-1 脚注: "在使用 SRAM3 作为堆栈空间时,
///   须将 SRAM3 的等待时间设置为 1wait (2 个 CPU 周期以上访问)"。
///   本工程的栈顶位于 SRAM3 末尾 (0x2002_7000), 故 SRAM3 必须保持 1 等待。
///
/// 结果与 DDL 启动配置 (SRAM3=1) 及 BSP_CLK_Init (200MHz 时 SRAM12/3/R=1)
/// 完全一致。
pub const fn sram_wtcr_value(hclk_hz: u32) -> u32 {
    let w12r = if hclk_hz > 100_000_000 { 1u32 } else { 0 }; // SRAM1/2/Ret
    // WTCR 位: SRAM12_RWT[2:0] SRAM12_WWT[6:4] SRAM3_RWT[10:8] SRAM3_WWT[14:12]
    //          SRAMH_RWT[18:16] SRAMH_WWT[22:20] SRAMR_RWT[26:24] SRAMR_WWT[30:28]
    (w12r << 0)
        | (w12r << 4)
        | (1 << 8)
        | (1 << 12) // SRAM3 恒 1 等待 (栈空间, 脚注要求)
        | (0 << 16)
        | (0 << 20) // SRAMH 恒 0 等待
        | (w12r << 24)
        | (w12r << 28)
}

/// 配置 SRAM 读写等待周期 (表 8-1 + 脚注), 时钟源切换前调用
///
/// SRAMC 基址 0x4005_0800: WTCR(+0x0) WTPR(+0x4) CKCR(+0x8) CKPR(+0xC)
/// WTPR/CKPR 写保护键值: 0x77 解锁, 0x76 锁定。
///
/// 注意: 复位处理函数开头的 SRAMC 内联配置 (SRAM3=1 等待) 保持不动 ——
/// 栈顶位于 SRAM3 末尾, 配置完成前禁止任何函数调用; 本接口供时钟切换
/// 等**后续**场景使用, 结果与复位配置一致 (SRAM3 恒 1 等待)。
pub fn set_sram_wait_cycle(hclk_hz: u32) {
    const SRAMC: usize = 0x4005_0800;
    let wtcr = sram_wtcr_value(hclk_hz);

    unsafe {
        // 解锁 SRAMC 寄存器写保护
        core::ptr::write_volatile((SRAMC + 0x04) as *mut u32, 0x77);
        core::ptr::write_volatile((SRAMC + 0x0C) as *mut u32, 0x77);
        // 写入等待周期
        core::ptr::write_volatile((SRAMC + 0x00) as *mut u32, wtcr);
        // 恢复写保护
        core::ptr::write_volatile((SRAMC + 0x04) as *mut u32, 0x76);
        core::ptr::write_volatile((SRAMC + 0x0C) as *mut u32, 0x76);
    }
}

/// 表 7-1: CPU 时钟频率 → FLASH 读等待周期 (普通读模式)
pub const fn flash_wait_cycle(hclk_hz: u32) -> u32 {
    if hclk_hz <= 33_000_000 {
        0
    } else if hclk_hz <= 66_000_000 {
        1
    } else if hclk_hz <= 99_000_000 {
        2
    } else if hclk_hz <= 132_000_000 {
        3
    } else if hclk_hz <= 168_000_000 {
        4
    } else {
        5
    }
}

/// 配置 FLASH 读等待周期 (对齐 DDL EFM_SetWaitCycle)
///
/// EFM 基址 0x4001_0400: FAPRT(+0x0, 写保护) FRMC(+0x8, FLWT[7:4])
/// 解锁键值 0x0123/0x3210, 锁定 0x0000。
pub fn set_flash_wait_cycle(hclk_hz: u32) {
    const EFM_BASE: usize = 0x4001_0400;
    const FRMC_FLWT_MASK: u32 = 0x0000_00F0;
    let cycles = flash_wait_cycle(hclk_hz);

    unsafe {
        // 解锁 EFM 寄存器写保护
        core::ptr::write_volatile(EFM_BASE as *mut u32, 0x0123);
        core::ptr::write_volatile(EFM_BASE as *mut u32, 0x3210);
        // FRMC: 读-改-写 FLWT
        let frmc = core::ptr::read_volatile((EFM_BASE + 0x08) as *const u32);
        core::ptr::write_volatile(
            (EFM_BASE + 0x08) as *mut u32,
            (frmc & !FRMC_FLWT_MASK) | (cycles << 4),
        );
        // 回读确认配置生效
        while core::ptr::read_volatile((EFM_BASE + 0x08) as *const u32) & FRMC_FLWT_MASK
            != cycles << 4
        {
            // 等待
        }
        // 恢复写保护
        core::ptr::write_volatile(EFM_BASE as *mut u32, 0x0000);
    }
}

/// 解锁 CMU 寄存器写保护 (PWC.FPRC)
fn cmu_unlock() {
    unsafe {
        core::ptr::write_volatile((PWC_BASE + PWC_FPRC_OFF) as *mut u16, FPRC_UNLOCK);
    }
}

/// 恢复 CMU 寄存器写保护 (PWC.FPRC)
fn cmu_lock() {
    unsafe {
        core::ptr::write_volatile((PWC_BASE + PWC_FPRC_OFF) as *mut u16, FPRC_LOCK);
    }
}

/// HRC 频率: 由 ICG1.HRCFREQSEL (0x4001_0684 bit0) 决定, 1→16MHz, 0→20MHz
fn hrc_hz() -> u32 {
    unsafe {
        if core::ptr::read_volatile(0x4001_0684 as *const u32) & 1 != 0 {
            16_000_000
        } else {
            20_000_000
        }
    }
}

/// MPLL 输出频率: PLLCLK = src/(M+1)·(N+1)/(P+1)
fn pll_hz() -> u32 {
    let r = read32(CMU_BASE + CMU_PLLCFGR);
    let m = (r >> 0) & 0x1F;
    let n = (r >> 8) & 0x1FF;
    let p = (r >> 28) & 0xF;
    let src = if r & (1 << 7) != 0 { hrc_hz() } else { XTAL_HZ };
    src / (m + 1) * (n + 1) / (p + 1)
}

fn read8(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u8) as u32 }
}

fn write8(addr: usize, value: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u8, value as u8) }
}

fn read32(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// 短延时 (时钟源切换稳定等待)
fn delay_short() {
    for _ in 0..200 {
        unsafe {
            core::arch::asm!("nop");
        }
    }
}
