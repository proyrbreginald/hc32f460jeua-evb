//! 时钟管理 (CMU 模块): 系统时钟源 (MRC/HRC/XTAL/PLL) 配置与切换
//!
//! 参考 DDL `CLK_XtalInit` / `CLK_PLLInit` / `CLK_SetSysClockSrc` /
//! `CLK_HrcCmd` / `CLK_MCOConfig` / `CLK_GetBusClockFreq` 与
//! `BSP_CLK_Init`, 参考手册 CMU/SRAMC/EFM 章节。
//! 所有寄存器偏移已与 DDL v3.3.0 头文件逐项核对一致。
//!
//! # 时钟链 (200MHz 方案)
//!
//! XTAL 8MHz → MPLL (÷1 ×50 ÷2) → 200MHz 系统时钟
//! ```text
//!                ┌─ HCLK  ÷1 → 200MHz (CPU / SysTick)
//! 200MHz SYSCLK ─┼─ PCLK0 ÷1 → 200MHz
//!                ├─ PCLK1 ÷2 → 100MHz (UART)
//!                ├─ PCLK2/3 ÷4 → 50MHz
//!                └─ PCLK4/EXCLK ÷2 → 100MHz
//! ```
//!
//! # 使用
//!
//! 一键完成全部时钟配置: [`init()`]—— 按 `.cargo/config.toml` 的
//! `CFG_CLK_SOURCE` 选择时钟源 (mrc/hrc/xtal/pll), 完成振荡器启动、
//! 总线分频、PLL 锁定、FLASH/SRAM/GPIO 等待周期、高性能电源、
//! 时钟源切换。任一步失败自动回退, 结果经 [`xtal_status`] 查询。
//! **PLL 源 (XTAL 或 HRC) 由 `CFG_PLL_SRC` 决定**, 无晶振的板子
//! 可配 HRC 源 (16/20MHz × 倍频)。
//!
//! 各外设通过 [`system_clock_hz`] / [`hclk_hz`] / [`pclk1_hz`] 等查询
//! 实际频率 (systick/uart 模块已接入); 频率测量可用 [`mco1_config`]
//! 把时钟输出到 PA8。
//!
//! # 限制
//!
//! 本模块仅应在启动阶段 (中断使能前) 调用, 未做中断安全保护。
//!
//! 部分位掩码常量仅作寄存器位定义文档用途, 忽略死代码警告。
#![allow(dead_code)]

use core::sync::atomic::{AtomicU8, Ordering};

use crate::gpio::{Config, Drive, Level, Mode, Pin, PortH};

/// 外部高速晶振频率 (Hz), 合法范围 4~25MHz (参考手册)。
/// **来自 .cargo/config.toml `CFG_XTAL_HZ`, 按实际电路板晶振修改!**
pub const XTAL_HZ: u32 = crate::config::XTAL_HZ;
const _: () = assert!(
    XTAL_HZ >= 4_000_000 && XTAL_HZ <= 25_000_000,
    "XTAL 频率必须在 4~25MHz"
);

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

/// CMU 寄存器偏移 (SVD/DDL v3.3.0 逐项核对一致)
const CMU_CKSWR: usize = 0x26; // 系统时钟源选择 (8 位)
const CMU_PLLCR: usize = 0x2A; // MPLL 控制 (8 位)
const CMU_XTALCR: usize = 0x32; // XTAL 控制 (8 位)
const CMU_HRCCR: usize = 0x36; // HRC 控制 (8 位, HRCSTP=0 开启)
const CMU_MRCCR: usize = 0x3A; // MRC 控制 (8 位, MRCSTP=0 开启)
const CMU_OSCSTBSR: usize = 0x3C; // 振荡器稳定状态 (8 位)
const CMU_MCO1CFGR: usize = 0x3D; // MCO1 配置 (MCOSEL/MCODIV/MCOEN)
const CMU_MCO2CFGR: usize = 0x3E; // MCO2 配置
const CMU_SCFGR: usize = 0x20; // 总线时钟分频 (32 位)
const CMU_XTALSTBCR: usize = 0xA2; // XTAL 稳定时间 (8 位)
const CMU_PLLCFGR: usize = 0x100; // MPLL 配置 (32 位)
const CMU_XTALCFGR: usize = 0x410; // XTAL 配置 (8 位)

/// 系统时钟源编码 (CKSW[2:0])
pub const CLK_SRC_HRC: u32 = 0;
pub const CLK_SRC_MRC: u32 = 1;
pub const CLK_SRC_LRC: u32 = 2;
pub const CLK_SRC_XTAL: u32 = 3;
pub const CLK_SRC_XTAL32: u32 = 4;
pub const CLK_SRC_PLL: u32 = 5;

/// OSCSTBSR 位
const OSCSTBSR_HRCSTBF: u32 = 1 << 0; // HRC 稳定标志
const OSCSTBSR_XTALSTBF: u32 = 1 << 3; // XTAL 稳定标志
const OSCSTBSR_MPLLSTBF: u32 = 1 << 5; // MPLL 稳定标志

/// PLLCFGR 位
const PLLCFGR_MPLLM_POS: u32 = 0; // [4:0] 输入分频 (÷(M+1))
const PLLCFGR_PLLSRC_POS: u32 = 7; // [7] 时钟源: 0=XTAL, 1=HRC
const PLLCFGR_MPLLN_POS: u32 = 8; // [16:8] 倍频 (×(N+1))
const PLLCFGR_MPLLR_POS: u32 = 20; // [23:20] R 输出分频 (÷(R+1))
const PLLCFGR_MPLLQ_POS: u32 = 24; // [27:24] Q 输出分频 (÷(Q+1))
const PLLCFGR_MPLLP_POS: u32 = 28; // [31:28] P 输出分频 (÷(P+1))

/// XTALSTBCR 位
const XTALSTBCR_XTALSTB_MASK: u32 = 0x0F; // 稳定时间选择

/// XTALCFGR 位
const XTALCFGR_XTALDRV_POS: u32 = 4; // [5:4] 驱动能力
const XTALCFGR_SUPDRV: u32 = 1 << 7; // 超强驱动

/// SCFGR 位
const SCFGR_PCLK0S_POS: u32 = 0; // [2:0] PCLK0 分频
const SCFGR_PCLK1S_POS: u32 = 4; // [6:4] PCLK1 分频
const SCFGR_PCLK2S_POS: u32 = 8; // [10:8] PCLK2 分频
const SCFGR_PCLK3S_POS: u32 = 12; // [14:12] PCLK3 分频
const SCFGR_PCLK4S_POS: u32 = 16; // [18:16] PCLK4 分频
const SCFGR_EXCKS_POS: u32 = 20; // [22:20] EXCLK 分频
const SCFGR_HCLKS_POS: u32 = 24; // [26:24] HCLK 分频

/// MCO1CFGR/MCO2CFGR 位 (共用定义)
const MCOCFGR_MCODIV_POS: u32 = 4; // [6:4] 输出分频
const MCOCFGR_MCOEN: u32 = 1 << 7; // [7] 输出使能

/// 总线分频编码 (SCFGR): 0=÷1, 1=÷2, 2=÷4, 3=÷8, 4=÷16
///
/// 分频系数 → 寄存器编码 (1/2/4/8/16; 合法性已在 config.rs 编译期校验)
fn div_code(div: u32) -> u32 {
    match div {
        1 => 0,
        2 => 1,
        4 => 2,
        8 => 3,
        16 => 4,
        _ => unreachable!("总线分频已在编译期校验"),
    }
}

/// 稳定时间: 编码 1~9 (≈133µs~32ms), 来自配置 CFG_XTAL_STABLE_TIME
/// (对齐 DDL CLK_XTAL_STB_*, 默认 0x05 = 2ms)
const XTAL_STABLE_TIME: u32 = crate::config::XTAL_STABLE_TIME;
/// 驱动能力编码 (0=HIGH, 1=MID, 2=LOW, 3=ULOW), 来自配置 CFG_XTAL_DRV
/// (对齐 DDL CLK_XTAL_DRV_*; ULOW 典型 4~8MHz 晶振)
const XTAL_DRV: u32 = crate::config::XTAL_DRV << XTALCFGR_XTALDRV_POS;
/// 振荡模式 (对齐 DDL CLK_XTAL_MD_OSC)
const XTAL_MODE_OSC: u32 = 0x00;

/// 系统时钟方案 (由 [`init`] 按配置统一编排)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClockSource {
    /// 内部中速 RC 8MHz (复位默认, 无需配置)
    Mrc,
    /// 内部高速 RC 16/20MHz (频率由 ICG1.HRCFREQSEL 决定, 见 [`hrc_hz`])
    Hrc,
    /// 外部晶振直通 (频率 [`XTAL_HZ`])
    Xtal,
    /// MPLL 倍频 (PLL 源 XTAL 或 HRC, 目标频率由 `CFG_PLL_*` 决定)
    Pll,
}

impl ClockSource {
    /// 名称 (与 `CFG_CLK_SOURCE` 取值一致, shell 显示用)
    pub const fn name(self) -> &'static str {
        match self {
            ClockSource::Mrc => "mrc",
            ClockSource::Hrc => "hrc",
            ClockSource::Xtal => "xtal",
            ClockSource::Pll => "pll",
        }
    }
}

/// 时钟初始化: **按 .cargo/config.toml 配置**启动时钟源并切换系统时钟。
///
/// 编排 (任一步失败自动回退, 结果经 [`xtal_status`] 查询):
/// - mrc: 无操作 (复位默认);
/// - hrc: HRC 启动 → FLASH/SRAM 等待 → 切换;
/// - xtal: 晶振启动 → FLASH/SRAM 等待 → 切换;
/// - pll: **按 `CFG_PLL_SRC` 启动对应源** (0=XTAL/1=HRC) → 总线分频 →
///   PLL 锁定 → FLASH/SRAM/GPIO 等待 + 高性能电源 → 切换; 失败降级为
///   PLL 源直通 (无晶振的板子可配 HRC 源)。
pub fn init() -> Result<(), ClkError> {
    match crate::config::CLOCK_SOURCE {
        ClockSource::Mrc => Ok(()),
        ClockSource::Hrc => {
            hrc_cmd(true)?;
            switch_to_hrc();
            Ok(())
        }
        ClockSource::Xtal => {
            xtal_init()?;
            switch_to_xtal();
            Ok(())
        }
        ClockSource::Pll => {
            let pll = pll_config();
            // 按 PLL 源启动对应振荡器 (PLLCFGR.PLLSRC: 0=XTAL, 1=HRC)
            match pll.src {
                0 => xtal_init()?,
                _ => hrc_cmd(true)?,
            }
            set_bus_clock_div();
            if pll_init(pll).is_ok() {
                switch_to_pll();
            } else {
                // PLL 锁定失败: 降级为 PLL 源直通 (总线分频在低频下无害)
                match pll.src {
                    0 => switch_to_xtal(),
                    _ => switch_to_hrc(),
                }
            }
            Ok(())
        }
    }
}

/// 时钟配置失败原因
#[allow(clippy::enum_variant_names)] // 各振荡器同名超时, 语义清晰
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClkError {
    /// 晶振起振超时 (检查电路/引脚/稳定时间)
    XtalStableTimeout,
    /// HRC 起振超时
    HrcStableTimeout,
    /// MPLL 锁定超时 (检查倍频/分频参数与 VCO 范围)
    PllStableTimeout,
    /// LRC 是当前系统时钟源, 不可失能 (对齐 DDL `CLK_LrcCmd` 的 BUSY 返回)
    LrcBusy,
    /// XTAL32 是当前系统时钟源, 不可失能 (对齐 DDL `CLK_Xtal32Cmd` 的 BUSY 返回)
    Xtal32Busy,
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

/// MPLL 配置 (对齐 DDL stc_clock_pll_init_t 的 PLLCFGR 位域)
#[derive(Clone, Copy, Debug)]
pub struct PllConfig {
    /// 时钟源: 0=XTAL, 1=HRC (PLLSRC)
    pub src: u32,
    /// 输入分频 ÷(M+1)
    pub m: u32,
    /// 倍频 ×(N+1), 合法范围 20~480 (参考手册)
    pub n: u32,
    /// P 输出分频 ÷(P+1), 合法范围 2~16
    pub p: u32,
    /// Q 输出分频 ÷(Q+1)
    pub q: u32,
    /// R 输出分频 ÷(R+1)
    pub r: u32,
}

/// MPLL 配置: 来自 .cargo/config.toml 的 `CFG_PLL_*` 项
///
/// 默认 (200MHz 方案): XTAL 8MHz ÷1 ×50 ÷2 = 200MHz
/// VCO = 8M×50 = 400MHz ∈ [240, 480]MHz, 倍频 50 ∈ [20, 480],
/// 分频 2 ∈ [2, 16] —— 全部合法 (参考手册), 对齐 DDL BSP_CLK_Init。
fn pll_config() -> PllConfig {
    PllConfig {
        src: crate::config::PLL_SRC,
        m: crate::config::PLL_M,
        n: crate::config::PLL_N,
        p: crate::config::PLL_P,
        q: crate::config::PLL_Q,
        r: crate::config::PLL_R,
    }
}

/// PLL 源是否为 XTAL (0=XTAL, 1=HRC)
fn pll_src_is_xtal() -> bool {
    crate::config::PLL_SRC == 0
}

/// 启动 MPLL 并等待锁定 (对齐 DDL CLK_PLLInit + CLK_PLLCmd)
///
/// 前置条件: 时钟源 ([`PllConfig::src`] 选择的 XTAL/HRC) 必须已启动稳定。
/// 序列: CMU 解锁 → PLLCFGR 写入 → 确认源稳定 → PLLCR 开启
/// → 等待 OSCSTBSR.MPLLSTBF。
pub fn pll_init(cfg: PllConfig) -> Result<(), ClkError> {
    cmu_unlock();

    // PLLCFGR: MPLLM[4:0] | PLLSRC[7] | MPLLN[16:8] | MPLLR[23:20] | MPLLQ[27:24] | MPLLP[31:28]
    let pllcfgr = (cfg.m & 0x1F)
        | ((cfg.src & 0x1) << PLLCFGR_PLLSRC_POS)
        | ((cfg.n & 0x1FF) << PLLCFGR_MPLLN_POS)
        | ((cfg.r & 0xF) << PLLCFGR_MPLLR_POS)
        | ((cfg.q & 0xF) << PLLCFGR_MPLLQ_POS)
        | ((cfg.p & 0xF) << PLLCFGR_MPLLP_POS);
    write32(CMU_BASE + CMU_PLLCFGR, pllcfgr);

    // 开启前确认时钟源稳定 (对齐 CLK_PLLCmd 的 WaitStable)
    let src_flag = if cfg.src == 0 {
        OSCSTBSR_XTALSTBF
    } else {
        OSCSTBSR_HRCSTBF
    };
    if !wait_stable(src_flag) {
        cmu_lock();
        return Err(ClkError::XtalStableTimeout);
    }

    // 开启 MPLL (MPLLOFF=0) 并等待锁定
    write8(CMU_BASE + CMU_PLLCR, 0);
    if !wait_stable(OSCSTBSR_MPLLSTBF) {
        cmu_lock();
        return Err(ClkError::PllStableTimeout);
    }

    cmu_lock();
    Ok(())
}

/// 切换系统时钟源到 MPLL (200MHz)
///
/// **切换前**按目标频率配置 FLASH/SRAM 等待周期 (表 7-1/8-1)、
/// GPIO 读等待 (PCCR.RDWT) 并切换到高性能电源模式 (200MHz 必需),
/// 顺序不可颠倒 (高时钟下取指/栈/IO 采样必须先行满足时序)。
pub fn switch_to_pll() {
    // 目标频率 = 已配置 PLLCFGR 的实际输出 (运行时计算)
    let target = pll_hz();

    crate::efm::set_wait_cycle(target);
    crate::sram::set_wait_cycles(target);
    // 126~200MHz 输入采样需 3 个读等待周期 (对齐 BSP_CLK_Init)
    set_gpio_read_wait(GPIO_RD_WAIT_200MHZ);
    // 高性能电源模式 (200MHz 必需)
    pwc_high_performance();

    cmu_unlock();
    write8(CMU_BASE + CMU_CKSWR, CLK_SRC_PLL);
    delay_short();
    cmu_lock();
    // 仅当 PLL 源为 XTAL 时报告晶振激活 (HRC 源时 XTAL 未启动)
    if pll_src_is_xtal() {
        XTAL_STATUS.store(STATUS_ACTIVE, Ordering::Relaxed);
    }
}

/// 总线时钟分频配置 (SCFGR), 分频系数来自 .cargo/config.toml `CFG_DIV_*`:
///
/// | 总线 | 分频 | 频率 |
/// |---|---|---|
/// | HCLK | ÷1 | 200MHz |
/// | PCLK0 | ÷1 | 200MHz |
/// | PCLK1 | ÷2 | 100MHz |
/// | PCLK2 | ÷4 | 50MHz |
/// | PCLK3 | ÷4 | 50MHz |
/// | PCLK4 | ÷2 | 100MHz |
/// | EXCLK | ÷2 | 100MHz |
///
/// **200MHz 下外设总线必须分频**: PCLK1 等最大允许 100MHz, 不分频时
/// UART/定时器等外设全部失效。
///
/// 调用时机: PLL 启动后、CKSWR 切换前 (此时非 PLL 时钟源, 无需 FCG
/// 备份, 对齐 DDL SetSysClockDiv 的 PLL 分支条件)。
pub fn set_bus_clock_div() {
    let scfgr = (div_code(crate::config::DIV_PCLK0) << SCFGR_PCLK0S_POS) // PCLK0
        | (div_code(crate::config::DIV_PCLK1) << SCFGR_PCLK1S_POS) // PCLK1
        | (div_code(crate::config::DIV_PCLK2) << SCFGR_PCLK2S_POS) // PCLK2
        | (div_code(crate::config::DIV_PCLK3) << SCFGR_PCLK3S_POS) // PCLK3
        | (div_code(crate::config::DIV_PCLK4) << SCFGR_PCLK4S_POS) // PCLK4
        | (div_code(crate::config::DIV_EXCLK) << SCFGR_EXCKS_POS) // EXCLK
        | (div_code(crate::config::DIV_HCLK) << SCFGR_HCLKS_POS); // HCLK

    cmu_unlock();
    write32(CMU_BASE + CMU_SCFGR, scfgr);
    delay_short(); // 对齐 DDL CLK_SYSCLK_SW_STB
    cmu_lock();
}

/// GPIO 读等待周期: 126~200MHz 输入采样需要 3 个等待周期
/// (对齐 BSP_CLK_Init 的 GPIO_RD_WAIT3, 参考手册 GPIO 章节)
const GPIO_RD_WAIT_200MHZ: u32 = 3;

/// GPIO 外设基址与关键寄存器偏移 (SVD)
const GPIO_BASE_ADDR: usize = 0x4005_3800;
const GPIO_PWPR: usize = GPIO_BASE_ADDR + 0x3FC; // 写保护
const GPIO_PCCR: usize = GPIO_BASE_ADDR + 0x3F8; // 读等待等
const PCCR_RDWT_MASK: u16 = 0x3 << 14; // RDWT[15:14]

/// 配置 GPIO 读等待周期 (PCCR.RDWT[15:14])
///
/// 高系统时钟下单周期无法正确采样输入电平 (参考手册 GPIO 章节),
/// 需插入读等待。PCCR 受 PWPR 写保护 (0xA501 解锁 / 0xA500 锁定)。
fn set_gpio_read_wait(wait: u32) {
    unsafe {
        core::ptr::write_volatile(GPIO_PWPR as *mut u16, 0xA501);
        let pccr = core::ptr::read_volatile(GPIO_PCCR as *const u16);
        core::ptr::write_volatile(
            GPIO_PCCR as *mut u16,
            (pccr & !PCCR_RDWT_MASK) | ((wait as u16 & 0x3) << 14),
        );
        core::ptr::write_volatile(GPIO_PWPR as *mut u16, 0xA500);
    }
}

/// PWC 寄存器偏移 (SVD)
const PWC_PWRC2: usize = 0x4004_8000 + 0xC402; // 电源/驱动控制 (8 位)
const PWC_MDSWCR: usize = 0x4004_8000 + 0xC40F; // 模式切换命令 (8 位)

/// PWRC2 位
const PWRC2_DDAS: u8 = 0x0F; // [3:0] 数字驱动能力全速
const PWRC2_DVS: u8 = 0x30; // [6:4] 驱动电压选择

/// 模式切换命令 (对齐 DDL PWC_MD_SWITCH_CMD)
const MD_SWITCH_CMD: u8 = 0x10;

/// 切换到高性能电源模式 (对齐 DDL PWC_HighSpeedToHighPerformance)
///
/// PWRC2: DDAS=0xF (全速), DVS=0; MDSWCR=0x10 (模式切换命令),
/// 随后延时 ~30us 等待切换完成。PWC 寄存器由 FPRC CODE1 解锁
/// (cmu_unlock 已含 CODE0|CODE1)。
fn pwc_high_performance() {
    unsafe {
        let pwrc2 = core::ptr::read_volatile(PWC_PWRC2 as *const u8);
        core::ptr::write_volatile(
            PWC_PWRC2 as *mut u8,
            (pwrc2 & !(PWRC2_DDAS | PWRC2_DVS)) | PWRC2_DDAS,
        );
        core::ptr::write_volatile(PWC_MDSWCR as *mut u8, MD_SWITCH_CMD);
    }
    // 等待模式切换完成 (~30us @ 8MHz)
    delay_short();
    delay_short();
}

/// 等待振荡器稳定标志置位 (带超时)
fn wait_stable(flag: u32) -> bool {
    let mut timeout = 0u32;
    while read8(CMU_BASE + CMU_OSCSTBSR) & flag == 0 {
        timeout += 1;
        if timeout > 1_000_000 {
            return false;
        }
    }
    true
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
        invert: false,
    });
    Pin::<PortH, 1>::new().configure(Config {
        mode: Mode::Analog,
        pull_up: false,
        drive: Drive::Low,
        initial_level: Level::Low,
        invert: false,
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
    if !wait_stable(OSCSTBSR_XTALSTBF) {
        cmu_lock();
        // 记录失败状态: 应用在输出通道就绪后报告
        XTAL_STATUS.store(STATUS_FAILED, Ordering::Relaxed);
        return Err(ClkError::XtalStableTimeout);
    }

    // 7. 恢复写保护
    cmu_lock();
    Ok(())
}

/// HRC 使能/失能 (CMU_HRCCR.HRCSTP=0 开启, 对齐 DDL `CLK_HrcCmd`)
///
/// 使能后等待 OSCSTBSR.HRCSTBF 稳定。HRC 频率 16/20MHz 由硬件决定
/// (ICG1.HRCFREQSEL, 见 [`hrc_hz`]), 可作为系统时钟或 MPLL 源
/// (PLLCFGR.PLLSRC=1, 见 [`pll_init`])。
pub fn hrc_cmd(enable: bool) -> Result<(), ClkError> {
    cmu_unlock();
    write8(CMU_BASE + CMU_HRCCR, if enable { 0 } else { 1 });
    if enable && !wait_stable(OSCSTBSR_HRCSTBF) {
        cmu_lock();
        return Err(ClkError::HrcStableTimeout);
    }
    cmu_lock();
    Ok(())
}

/// XTAL 使能/失能 (CMU_XTALCR.XTALSTP=0 开启, 对齐 DDL `CLK_XtalCmd`)
///
/// 使能后等待稳定。仅用于晶振已由 [`xtal_init`] 配置过的场景
/// (参数/驱动/稳定时间已写入), 失能前请确保 XTAL 不再是系统时钟
/// 或 PLL 源, 否则系统时钟丢失。
pub fn xtal_cmd(enable: bool) -> Result<(), ClkError> {
    cmu_unlock();
    write8(CMU_BASE + CMU_XTALCR, if enable { 0 } else { 1 });
    if enable && !wait_stable(OSCSTBSR_XTALSTBF) {
        cmu_lock();
        XTAL_STATUS.store(STATUS_FAILED, Ordering::Relaxed);
        return Err(ClkError::XtalStableTimeout);
    }
    cmu_lock();
    Ok(())
}

/// 内部低速 RC (LRC) 使能/失能 (CMU_LRCCR.LRCSTP=0 开启, 对齐 DDL
/// `CLK_LrcCmd`)
///
/// LRC 为 32.768kHz 内部振荡器, 是 OTS/RTC/WDT 等工作的基础时钟
/// (OTS 每次采样依赖 LRC 产生工作时序, 见 `ots` 模块)。
/// 失能时若 LRC 仍是系统时钟源 (CKSWR) 则返回 Err (对齐 DDL 行为);
/// 使能后延时 ~160µs (对齐 DDL CLK_LRC_TIMEOUT) 等待起振。
pub fn lrc_cmd(enable: bool) -> Result<(), ClkError> {
    const CMU_LRCCR: usize = 0x427; // LRC 控制 (8 位, LRCSTP=0 开启)
    if !enable && system_clock_hz() == LRC_HZ {
        return Err(ClkError::LrcBusy);
    }
    cmu_unlock();
    write8(CMU_BASE + CMU_LRCCR, if enable { 0 } else { 1 });
    cmu_lock();
    // 等待起振 (对齐 DDL CLK_LRC_TIMEOUT = 160µs)
    delay_us(160);
    Ok(())
}

/// 外部低速晶振 (XTAL32) 使能/失能 (CMU_XTAL32CR.XTAL32STP=0 开启,
/// 对齐 DDL `CLK_Xtal32Cmd`)
///
/// 32.768kHz 晶振接 PC14 (IN) / PC15 (OUT)。OTS 选择 HRC 为测温时钟时
/// **必须先启动 XTAL32** (消除 HRC 频率误差, 计算时用 ECR 补偿,
/// 参考手册 17.2 节), 见 `ots` 模块。失能时若 XTAL32 仍是系统时钟源
/// 则返回 Err (对齐 DDL 行为)。
pub fn xtal32_cmd(enable: bool) -> Result<(), ClkError> {
    const CMU_XTAL32CR: usize = 0x420; // XTAL32 控制 (8 位, XTAL32STP=0 开启)
    if !enable && system_clock_hz() == XTAL32_HZ {
        return Err(ClkError::Xtal32Busy);
    }
    cmu_unlock();
    write8(CMU_BASE + CMU_XTAL32CR, if enable { 0 } else { 1 });
    cmu_lock();
    // 等待起振 (对齐 DDL CLK_XTAL32_TIMEOUT ≈ 5 × XTAL32 周期)
    delay_us(160);
    Ok(())
}

/// 切换系统时钟源到内部高速 RC (HRC 16/20MHz)
///
/// **切换前**按目标频率配置 FLASH/SRAM 等待周期 (表 7-1/8-1),
/// 顺序不可颠倒 (高时钟下取指/栈操作必须先满足时序)。
pub fn switch_to_hrc() {
    crate::efm::set_wait_cycle(hrc_hz());
    crate::sram::set_wait_cycles(hrc_hz());
    cmu_unlock();
    write8(CMU_BASE + CMU_CKSWR, CLK_SRC_HRC);
    delay_short();
    cmu_lock();
}

/// 切换系统时钟源到外部晶振
///
/// **切换前**按目标频率配置 FLASH/SRAM 等待周期 (表 7-1/8-1):
/// 高时钟下若等待周期不足, 切换瞬间取指/栈操作即出错, 顺序不可颠倒。
pub fn switch_to_xtal() {
    crate::efm::set_wait_cycle(XTAL_HZ);
    crate::sram::set_wait_cycles(XTAL_HZ);
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

/// SCFGR 中某总线的分频编码值 (0=÷1 ... 4=÷16)
fn bus_div(pos: u32) -> u32 {
    (read32(CMU_BASE + CMU_SCFGR) >> pos) & 0x7
}

/// HCLK 频率 (Hz): 系统时钟 ÷ SCFGR.HCLKS (CPU/SysTick/FLASH 总线)
pub fn hclk_hz() -> u32 {
    system_clock_hz() >> bus_div(SCFGR_HCLKS_POS)
}

/// PCLK0 频率 (Hz) (GPIO/ADC 等 AHB1 高速外设)
pub fn pclk0_hz() -> u32 {
    system_clock_hz() >> bus_div(SCFGR_PCLK0S_POS)
}

/// PCLK1 频率 (Hz) (UART/TIM 等 APB 外设, 见 uart 模块)
pub fn pclk1_hz() -> u32 {
    system_clock_hz() >> bus_div(SCFGR_PCLK1S_POS)
}

/// PCLK2 频率 (Hz)
pub fn pclk2_hz() -> u32 {
    system_clock_hz() >> bus_div(SCFGR_PCLK2S_POS)
}

/// PCLK3 频率 (Hz)
pub fn pclk3_hz() -> u32 {
    system_clock_hz() >> bus_div(SCFGR_PCLK3S_POS)
}

/// PCLK4 频率 (Hz)
pub fn pclk4_hz() -> u32 {
    system_clock_hz() >> bus_div(SCFGR_PCLK4S_POS)
}

/// EXCLK 频率 (Hz)
pub fn exclk_hz() -> u32 {
    system_clock_hz() >> bus_div(SCFGR_EXCKS_POS)
}

// ============================== MCO 时钟输出 (对齐 DDL CLK_MCOConfig/Cmd) ==============================

/// MCO 时钟输出源 (MCO1CFGR.MCOSEL, 与 DDL `CLK_MCO_SRC_*` 一致)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum McoSource {
    /// 内部高速 RC (16/20MHz)
    Hrc = 0x0,
    /// 内部中速 RC (8MHz)
    Mrc = 0x1,
    /// 内部低速 RC (32.768kHz)
    Lrc = 0x2,
    /// 外部晶振 (XTAL_HZ)
    Xtal = 0x3,
    /// 外部低速晶振 (32.768kHz)
    Xtal32 = 0x4,
    /// MPLL 的 P 输出
    PllP = 0x6,
    /// MPLL 的 Q 输出
    PllQ = 0x8,
    /// HCLK
    Hclk = 0xB,
}

/// MCO 输出分频 (MCO1CFGR.MCODIV, 与 DDL `CLK_MCO_DIV*` 一致)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum McoDiv {
    Div1 = 0,
    Div2 = 1,
    Div4 = 2,
    Div8 = 3,
    Div16 = 4,
    Div32 = 5,
    Div64 = 6,
    Div128 = 7,
}

/// 配置并启动 MCO1 时钟输出 (MCOSEL/MCODIV/MCOEN 一次写入)
///
/// 输出引脚: **PA8** (MCO_1, Func_Grp1, `gpio::Pin::<PortA, 8>::set_func(1)`),
/// 用于示波器/频率计测量内部时钟。MCO2 (CMU_MCO2CFGR) 布局相同, 未封装。
pub fn mco1_config(source: McoSource, div: McoDiv) {
    cmu_unlock();
    write8(
        CMU_BASE + CMU_MCO1CFGR,
        (source as u32) | ((div as u32) << MCOCFGR_MCODIV_POS) | MCOCFGR_MCOEN,
    );
    cmu_lock();
}

/// 使能/失能 MCO1 输出 (MCO1CFGR.MCOEN, 对齐 DDL `CLK_MCOCmd`)
pub fn mco1_cmd(enable: bool) {
    cmu_unlock();
    let mut v = read8(CMU_BASE + CMU_MCO1CFGR);
    if enable {
        v |= MCOCFGR_MCOEN;
    } else {
        v &= !MCOCFGR_MCOEN;
    }
    write8(CMU_BASE + CMU_MCO1CFGR, v);
    cmu_lock();
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

/// HRC 频率: 由运行期只读的 ICG1.HRCFREQSEL (bit0) 决定: 1→16MHz, 0→20MHz
///
/// 该位由复位时从 flash 0x404 ICG1 配置字载入 (见 [`crate::icg`] 模块),
/// 即由配置 `CFG_HRC_FREQ` 决定 —— 两者恒一致 (flash 字由配置生成)。
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
    let m = r & 0x1F;
    let n = (r >> 8) & 0x1FF;
    let p = (r >> 28) & 0xF;
    let src = if r & (1 << PLLCFGR_PLLSRC_POS) != 0 {
        hrc_hz()
    } else {
        XTAL_HZ
    };
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

fn write32(addr: usize, value: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, value) }
}

/// 短延时 (时钟源切换稳定等待)
fn delay_short() {
    for _ in 0..200 {
        unsafe {
            core::arch::asm!("nop");
        }
    }
}

/// 忙等延时 (微秒, 按 HCLK 折算; 仅启动阶段使用, 时钟已稳定)
fn delay_us(us: u32) {
    let cycles = (us as u64 * hclk_hz() as u64 / 1_000_000) as u32;
    for _ in 0..cycles {
        unsafe {
            core::arch::asm!("nop");
        }
    }
}
