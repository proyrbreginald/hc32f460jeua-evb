//! OTS 片内温度传感器驱动 (对齐 DDL v3.3.0 `hc32_ll_ots.c/h` 与例程 ots_base)
//!
//! # 原理
//!
//! OTS (On-chip Temperature Sensor) 测量芯片结温: 以 **LRC** (32.768kHz
//! 内部低速 RC) 为工作时序基准, 用 **XTAL 或 HRC** 计数, 采样完成后
//! 结果存在两个数据寄存器:
//!
//! ```text
//! T = K × (1/DR1 − 1/DR2) × ECR + M          (ECR 仅 HRC 源读寄存器)
//! ```
//!
//! K (斜率) 与 M (偏移) 由**定标实验**得到 (每颗芯片不同, 需在已知温度
//! 下测量 DR1/DR2 反推); 也可直接采用 DDL 例程提供的内置参数:
//!
//! | 时钟源 | K           | M     |
//! |--------|-------------|-------|
//! | XTAL   | 737272.73   | 27.55 |
//! | HRC    | 3002.59     | 27.92 |
//!
//! 工程默认值来自 `.cargo/config.toml` (`CFG_OTS_SLOPE_K` /
//! `CFG_OTS_OFFSET_M`), 见 [`crate::config`]。
//!
//! # 数值表示: 纯整数定点
//!
//! 本工程 **禁止浮点** (README 验证记录: core 浮点格式化在 no_std 下
//! 导致内存破坏)。定标参数以 **千分度整数** 存储 (×1000): K=3002.59 →
//! 3002590, M=27.92 → 27920; 温度计算用 i64/i128 定点:
//!
//! ```text
//! A = (1e12/DR1 − 1e12/DR2)          (i64, 12 位小数)
//! T_milli = (K1000 × ECR × A) / 1e12 + M1000   (i128 中间量, 毫度)
//! ```
//!
//! 对外输出一律为整数 (十分度 [`to_deci`] / 原始 `DR1/DR2/ECR`)。
//!
//! # 寄存器 (基址 0x4004_A400, 全部 16 位)
//!
//! - `CTL`: OTSST (bit0, 写 1 启动/读 1 采样中)、OTSCK (bit1, 0=XTAL/1=HRC)、
//!   OTSIE (bit2, 中断使能)、TSSTP (bit3, 采样完成后模拟传感器状态);
//! - `DR1`/`DR2`: 采样数据寄存器; `ECR`: HRC 源时的频率误差补偿值。
//!
//! > 注: DDL 头文件 `OTS_AUTO_OFF_*` 的两条注释与位语义相反 (写反),
//! > 以参考手册 17.2 节为准: **TSSTP=1 → 采样完成后关闭模拟传感器**,
//! > 本驱动已按实际语义命名; OTSST 采样完成恒自动清零。
//!
//! # 时钟依赖 (参考手册 17.2 节)
//!
//! - **LRC 必须使能** (OTS 工作时序基准, `clk::lrc_cmd`);
//! - **HRC 源**: 启动 HRC 外还需启动 **XTAL32** (PC14/PC15 外接
//!   32.768kHz 晶振, 本板 JEUA UU 板载 Y2)—— 消除 HRC 频率误差,
//!   计算温度时读取 ECR 补偿; 否则采样永不完成 (OTSST 不自动清零);
//! - **XTAL 源**: 系统未启用晶振时由本驱动经 `clk::xtal_init` 启动
//!   (含 PH0/PH1 模拟引脚配置), 计算时 ECR 按常量 1 处理;
//! - 外设时钟门控: FCG3.bit12 (清位 = 使能, 无写保护)。
//!
//! # 使用
//!
//! ```no_run
//! ots::init(ots::Config {
//!     clock_src: ots::ClockSource::Hrc,
//!     slope_k: 3_002_590, // 3002.59 × 1000
//!     offset_m: 27_920,   // 27.92 × 1000
//!     auto_off: ots::AutoOff::Enable,
//! });
//! let t = ots::polling_until(rtos::uptime_ms() + 100); // Result<i32 毫度, Error>
//! let (w, f) = ots::split_deci(ots::to_deci(t));
//! ```
//!
//! 中断模式 (事件源 `intc::src::OTS` = 435, 独立线 INT110): 仅提供
//! [`int_enable`] (CTL.OTSIE), 路由/回调由应用自行接入 (参考 DDL 例程
//! `OtsIrqConfig`), 本模块默认轮询。
#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// OTS 外设基址
const OTS_BASE: usize = 0x4004_A400;
/// PWC 外设基址 (FCG 时钟门控)
const PWC_BASE: usize = 0x4004_8000;

// ---- 寄存器偏移 (CM_OTS_TypeDef: 16 位寄存器, 2 字节步进) ----
const CTL: usize = 0x00; // 控制
const DR1: usize = 0x02; // 数据寄存器 1
const DR2: usize = 0x04; // 数据寄存器 2
const ECR: usize = 0x06; // 误差校准寄存器 (仅 HRC 源有效)

// ---- CTL 位 ----
const CTL_OTSST: u16 = 1 << 0; // 写 1 启动采样; 读 1 = 采样中, 0 = 完成
const CTL_OTSCK: u16 = 1 << 1; // 时钟源: 0=XTAL, 1=HRC
const CTL_OTSIE: u16 = 1 << 2; // 采样完成中断使能
const CTL_TSSTP: u16 = 1 << 3; // 采样完成后模拟传感器自动关闭

// ---- PWC.FC3 时钟门控 (清位 = 使能) ----
const FCG3: usize = 0x0C;
const FCG3_OTS: u32 = 1 << 12; // OTS 时钟门控位

/// 定点缩放: 定标参数 ×1000 (千分度)
pub const SCALE: i64 = 1000;
/// 定点缩放: A 参数 ×1e12 (12 位小数)
const A_SCALE: i64 = 1_000_000_000_000;

/// OTS 已初始化标志
static INITIALIZED: AtomicBool = AtomicBool::new(false);
/// 定标参数 (×1000), 由 [`init`] 写入, [`calculate_temp`] 使用
static SLOPE_K: AtomicI32 = AtomicI32::new(0);
static OFFSET_M: AtomicI32 = AtomicI32::new(0);
/// 时钟源 (HRC=1/XTAL=0, 决定 ECR 处理)
static CLOCK_HRC: AtomicBool = AtomicBool::new(false);

/// OTS 时钟源 (对齐 DDL `OTS_CLK_XTAL` / `OTS_CLK_HRC`)
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClockSource {
    /// 外部晶振 (需已启动; 计算时 ECR 按 1 处理)
    Xtal,
    /// 内部高速 RC (16/20MHz, 无外部器件; 计算时读取 ECR 寄存器)
    Hrc,
}

impl ClockSource {
    /// 配置取值 (`.cargo/config.toml` `CFG_OTS_CLK_SOURCE`)
    pub const fn name(self) -> &'static str {
        match self {
            ClockSource::Xtal => "xtal",
            ClockSource::Hrc => "hrc",
        }
    }
}

/// 采样完成后的模拟传感器状态 (CTL.TSSTP, 参考手册 17.2 节)
///
/// OTSST 采样完成**恒自动清零** (与 TSSTP 无关), TSSTP 仅控制采样
/// 完成后模拟温度传感器是否关闭。注: DDL 头文件 `OTS_AUTO_OFF_*` 的
/// 两条注释与位语义相反 (写反), 本驱动按实际位语义命名。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AutoOff {
    /// 采样完成关闭模拟传感器 (TSSTP=1, 默认): 省电, 下次采样
    /// 需经过传感器稳定时间
    Enable,
    /// 采样完成保持传感器开启 (TSSTP=0): 下次采样跳过稳定时间,
    /// 持续耗电 (适合连续测温场景)
    Disable,
}

/// OTS 配置 (对齐 DDL `stc_ots_init_t`; 定标参数为 **×1000 整数**)
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// 时钟源 (XTAL/HRC)
    pub clock_src: ClockSource,
    /// 斜率 K ×1000 (定标实验获得; 例: 3002.59 → 3002590)
    pub slope_k: i32,
    /// 偏移 M ×1000 (例: 27.92 → 27920)
    pub offset_m: i32,
    /// 采样完成自动关断
    pub auto_off: AutoOff,
}

impl Default for Config {
    /// 对齐 DDL `OTS_StructInit`: HRC 源 + 自动关断; K/M 由配置注入
    fn default() -> Self {
        Self {
            clock_src: ClockSource::Hrc,
            slope_k: 0,
            offset_m: 0,
            auto_off: AutoOff::Enable,
        }
    }
}

/// OTS 错误
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// 采样超时 (未在超时时间内完成, 检查 LRC/时钟源是否使能)
    Timeout,
}

// ============================== 底层访问 ==============================

fn read16(offset: usize) -> u16 {
    unsafe { core::ptr::read_volatile((OTS_BASE + offset) as *const u16) }
}

fn write16(offset: usize, value: u16) {
    unsafe { core::ptr::write_volatile((OTS_BASE + offset) as *mut u16, value) };
}

fn read32(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

fn write32(addr: usize, value: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, value) };
}

// ============================== 初始化 ==============================

/// 初始化 OTS (对齐 DDL `OTS_Init` + 例程时钟配置)
///
/// 序列: 使能 OTS 外设时钟 (FCG3) → 按源启动振荡器 (HRC 源还需
/// XTAL32, XTAL 源经 `clk::xtal_init` 完整配置引脚并启动) → 使能
/// LRC → 停止采样 → 写 CTL (时钟源 + 自动关断) → 保存 K/M。
/// 任一步失败返回 Err (HRC 无 XTAL32 晶振的板子采样将永不完成,
/// 必须用 XTAL 源)。
pub fn init(cfg: Config) -> Result<(), crate::clk::ClkError> {
    // 1. 外设时钟 (FCG3.bit12 清位 = 使能)
    let fcg3 = read32(PWC_BASE + FCG3);
    write32(PWC_BASE + FCG3, fcg3 & !FCG3_OTS);

    // 2. 按源启动振荡器 (失败返回, 不破坏已配置状态)
    match cfg.clock_src {
        // HRC 源: 必须同时启动 XTAL32 消除 HRC 频率误差 (参考手册 17.2)
        ClockSource::Hrc => {
            crate::clk::hrc_cmd(true)?;
            crate::clk::xtal32_cmd(true)?;
        }
        // XTAL 源: 系统可能从未配置晶振 (PLL 源 HRC 时), 完整初始化
        ClockSource::Xtal => crate::clk::xtal_init()?,
    }

    // 3. LRC: OTS 工作时序基准 (必须使能)
    crate::clk::lrc_cmd(true)?;

    // 4. 停止采样并配置 CTL (时钟源 + 自动关断)
    stop();
    let auto_off = if cfg.auto_off == AutoOff::Enable {
        CTL_TSSTP
    } else {
        0
    };
    let otssck = match cfg.clock_src {
        ClockSource::Hrc => CTL_OTSCK,
        ClockSource::Xtal => 0,
    };
    write16(CTL, otssck | auto_off);

    // 5. 保存定标参数
    SLOPE_K.store(cfg.slope_k, Ordering::Relaxed);
    OFFSET_M.store(cfg.offset_m, Ordering::Relaxed);
    CLOCK_HRC.store(cfg.clock_src == ClockSource::Hrc, Ordering::Relaxed);
    INITIALIZED.store(true, Ordering::Relaxed);
    Ok(())
}

/// 去初始化 (对齐 DDL `OTS_DeInit`): 停止采样 + 清零全部寄存器
///
/// 不关闭振荡器 (HRC/LRC 可能被其他模块使用), 时钟门控保持使能。
pub fn deinit() {
    stop();
    write16(CTL, 0);
    write16(DR1, 0);
    write16(DR2, 0);
    write16(ECR, 0);
    INITIALIZED.store(false, Ordering::Relaxed);
}

/// 是否已初始化 (供 shell 判断, 未初始化时采样无意义)
pub fn enabled() -> bool {
    INITIALIZED.load(Ordering::Relaxed)
}

/// 启动采样 (CTL.OTSST=1, 对齐 DDL `OTS_Start`)
pub fn start() {
    write16(CTL, read16(CTL) | CTL_OTSST);
}

/// 停止采样 (CTL.OTSST=0, 对齐 DDL `OTS_Stop`)
pub fn stop() {
    write16(CTL, read16(CTL) & !CTL_OTSST);
}

/// 采样是否进行中 (CTL.OTSST 读值)
pub fn sampling() -> bool {
    read16(CTL) & CTL_OTSST != 0
}

// ============================== 温度测量 ==============================

/// 轮询测量温度 (对齐 DDL `OTS_Polling`)
///
/// 启动采样后等待 OTSST 清零 (采样完成**恒自动清零**, 参考手册
/// 17.2 节, 与 TSSTP 无关), 超时按 `u32Timeout` 次轮询计。
/// 返回 [`Error::Timeout`] 或温度 (**毫度** i32)。
///
/// 注意: DDL 例程的 10000 次超时在 200MHz 下仅约数百 µs, 冷启动
/// (传感器刚上电) 的首个采样可能超时 —— 应用建议用 [`polling_until`]
/// 按时间预算等待。
pub fn polling(timeout: u32) -> Result<i32, Error> {
    start();
    let mut count = timeout;
    let done = loop {
        if !sampling() {
            break true;
        }
        if count == 0 {
            break false;
        }
        count -= 1;
    };
    stop();
    if done {
        Ok(calculate_temp())
    } else {
        Err(Error::Timeout)
    }
}

/// 按绝对时间预算轮询测量 (shell 命令用)
///
/// `deadline_ms` 为 [`crate::rtos::uptime_ms`] 的绝对截止时刻, 例如
/// `polling_until(rtos::uptime_ms() + 100)`。相比 [`polling`] 的迭代
/// 计数, 时间预算与 CPU 频率/代码优化无关, 冷采样 (传感器稳定) 也
/// 不会误超时。
pub fn polling_until(deadline_ms: u32) -> Result<i32, Error> {
    start();
    loop {
        if !sampling() {
            stop();
            return Ok(calculate_temp());
        }
        if crate::rtos::uptime_ms() >= deadline_ms {
            stop();
            return Err(Error::Timeout);
        }
    }
}

/// 计算温度 (对齐 DDL `OTS_CalculateTemp`, 纯整数定点)
///
/// `T_milli = (K1000 × ECR × A) / 1e12 + M1000`, 其中
/// `A = 1e12/DR1 − 1e12/DR2` (12 位小数); XTAL 源时 ECR 按 1 处理,
/// HRC 源时读 ECR 寄存器。任一分母为 0 时返回 -300000 毫度 (对齐
/// DDL 的 -300.0°C)。
pub fn calculate_temp() -> i32 {
    let dr1 = read16(DR1);
    let dr2 = read16(DR2);
    let ecr = read16(ECR);
    let k = SLOPE_K.load(Ordering::Relaxed) as i64;
    let m = OFFSET_M.load(Ordering::Relaxed) as i64;
    let ecr = if CLOCK_HRC.load(Ordering::Relaxed) {
        ecr as i64
    } else {
        1
    };
    if dr1 != 0 && dr2 != 0 && ecr != 0 {
        let a = A_SCALE / dr1 as i64 - A_SCALE / dr2 as i64;
        let t = (k * ecr) as i128 * a as i128 / A_SCALE as i128;
        (t as i64 + m) as i32
    } else {
        -300_000
    }
}

/// 读取原始采样数据 (DR1/DR2/ECR, 供定标实验与诊断)
pub fn read_raw() -> (u16, u16, u16) {
    (read16(DR1), read16(DR2), read16(ECR))
}

/// 定标实验采样 (对齐 DDL `OTS_ScalingExperiment`)
///
/// 在**已知温度**环境下调用, 获得参数 A (×1e12):
/// `A = (1e12/DR1 − 1e12/DR2) × ECR` (HRC 源读 ECR, XTAL 源按 1)。
/// 不同温度点测量两次可得 K = ΔT/ΔA、M = T − K×A。
#[derive(Clone, Copy)]
pub struct ScalingResult {
    /// DR1
    pub dr1: u16,
    /// DR2
    pub dr2: u16,
    /// ECR (XTAL 源时为 1)
    pub ecr: u16,
    /// 参数 A ×1e12 (1/DR1 − 1/DR2) × ECR
    pub a: i64,
}

/// 执行一次定标实验采样 (对齐 DDL `OTS_ScalingExperiment`)
pub fn scaling_experiment(timeout: u32) -> Result<ScalingResult, Error> {
    let (dr1, dr2, ecr) = if polling(timeout).is_ok() {
        read_raw()
    } else {
        return Err(Error::Timeout);
    };
    let ecr = if CLOCK_HRC.load(Ordering::Relaxed) {
        ecr
    } else {
        1
    };
    let a = if dr1 != 0 && dr2 != 0 && ecr != 0 {
        (A_SCALE / dr1 as i64 - A_SCALE / dr2 as i64) * ecr as i64
    } else {
        0
    };
    Ok(ScalingResult { dr1, dr2, ecr, a })
}

/// 采样完成中断使能 (CTL.OTSIE, 对齐 DDL `OTS_IntCmd`)
///
/// 中断事件源 `intc::src::OTS` (435), 独立线 INT110; 路由/回调/清挂起
/// 由应用自行接入 (参考 DDL 例程 `OtsIrqConfig`), 完成后调用
/// [`calculate_temp`] 读取结果。
pub fn int_enable(enable: bool) {
    let v = read16(CTL);
    write16(CTL, if enable { v | CTL_OTSIE } else { v & !CTL_OTSIE });
}

// ============================== 显示辅助 ==============================

/// 毫度 → 十分度整数 (显示用, 规避浮点)
///
/// 例: 27920 毫度 → 279 (显示 "27.9°C"); -3140 → -31。
pub fn to_deci(temp_milli: i32) -> i32 {
    if temp_milli >= 0 {
        (temp_milli + 50) / 100
    } else {
        (temp_milli - 50) / 100
    }
}

/// 十分度拆分为 (整数部分, 小数部分), 供手工排版 `%d.%d`
pub fn split_deci(deci: i32) -> (i32, u32) {
    let sign = if deci < 0 { -1 } else { 1 };
    let abs = deci.unsigned_abs();
    (sign * (abs / 10) as i32, abs % 10)
}

/// 千分度拆分为 (整数, 十分, 百分, 千分), 供手工排版 `%d.%d%d%d`
pub fn split_milli(milli: i32) -> (i64, u32, u32, u32) {
    let sign = if milli < 0 { -1 } else { 1 };
    let abs = if milli < 0 { -(milli as i64) } else { milli as i64 } as u64;
    (
        sign * (abs / 1000) as i64,
        ((abs / 100) % 10) as u32,
        ((abs / 10) % 10) as u32,
        (abs % 10) as u32,
    )
}
