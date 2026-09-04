//! 硬实时指标测量 (DWT CYCCNT + SysTick CVR, 按 HCLK 周期计数)
//!
//! # 两个核心指标
//!
//! 1. **最长关中断时间** ([`critical_begin`]/[`critical_end`]):
//!    `critical_section::with` 内部实测 PRIMASK 保持的时间 —— 所有
//!    线程/ISR 抢占不可用的窗口, 硬实时系统的首要预算对象;
//! 2. **SysTick ISR 到达延迟** ([`tick_entry`]): 节拍中断实际入口时刻
//!    相对硬件触发时刻的偏差。经 SysTick 当前值寄存器 (CVR) 测量 ——
//!    硬件在中断触发瞬间把计数器重装载并继续倒计时, `reload - VAL`
//!    即到达延迟。**无需入口时刻基线**, 晚处理追赶 (上一次处理耗时
//!    超过一个节拍) 不会污染下一拍测量 (旧版基线法在追赶场景把下一拍
//!    误判为 ~2^32 周期的"负延迟")。
//!
//! # Flash 擦写窗口 (bus hold)
//!
//! HC32F460 擦/写 Flash 时总线被占用, CPU 取指与中断响应被硬件 stall
//! (毫秒级) —— 与软件无关的硬件固有特性, 且**触发 store 的临界区
//! 随 stall 一起延长** (PRIMASK 恢复代码同样被 stall)。EFM 驱动在每次
//! 擦/写前后调用 [`flash_window_begin`]/[`flash_window_end`] 标注窗口;
//! 与窗口重叠的临界区样本与节拍分别计入独立的
//! [`max_critical_flash_cycles`]/[`max_tick_latency_flash_cycles`]
//! (信息性), 不污染判定指标。
//!
//! # 测量自身开销
//!
//! 每次临界区增加两次 DWT 读取 (~数周期), SysTick ISR 入口一次 CVR +
//! 一次 DWT 读取; 与 200MHz 下 1ms 节拍周期 (200k 周期) 相比可忽略,
//! 且关中断测量把该开销也计入自身 (结果只高不低, 偏保守)。
//!
//! # 初始化时机
//!
//! [`init`] 由板级初始化在时钟配置完成后调用; 之前的临界区不测量。
//! CYCCNT 32 位, 200MHz 下约 21.5s 回绕一次, 全部差值计算为回绕安全。

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::mmio::Reg;

/// DWT 控制 (Cortex-M4)
const DWT_CTRL: usize = 0xE000_1000;
/// DWT 周期计数器
const DWT_CYCCNT: usize = 0xE000_1004;
/// 调试异常与监视控制寄存器 (DEMCR.TRCENA 开放 DWT)
const DEMCR: usize = 0xE000_EDFC;

const DEMCR_TRCENA: u32 = 1 << 24;
const DWT_CTRL_CYCCNTENA: u32 = 1 << 0;

static ENABLED: AtomicBool = AtomicBool::new(false);
static HCLK_HZ: AtomicU32 = AtomicU32::new(0);
/// SysTick 节拍周期 (HCLK 周期数; CVR 重装载值 = 周期 - 1)
static PERIOD_CYCLES: AtomicU32 = AtomicU32::new(0);

/// 最长关中断时间 (cycles, 不含 Flash 擦写窗口)
static CRITICAL_MAX: AtomicU32 = AtomicU32::new(0);
/// Flash 擦写窗口内的最大关中断时间 (cycles, 信息性)
static CRITICAL_FLASH_MAX: AtomicU32 = AtomicU32::new(0);
/// 最长 SysTick 到达延迟 (cycles, 不含 Flash 擦写窗口)
static TICK_LATENCY_MAX: AtomicU32 = AtomicU32::new(0);
/// Flash 擦写窗口内的最大到达延迟 (cycles, 信息性)
static TICK_LATENCY_FLASH_MAX: AtomicU32 = AtomicU32::new(0);
/// Flash 擦写窗口起止时刻 (CYCCNT, 信息性; 分类判定以
/// [`FLASH_OP_ACTIVE`] 标志为准)
static FLASH_WIN_START: AtomicU32 = AtomicU32::new(0);
static FLASH_WIN_END: AtomicU32 = AtomicU32::new(0);
/// Flash 擦/写业务进行中 (总线 stall + 窗口内/后处理)。
///
/// 节拍在 Flash 操作期间排队 (含 bus hold), 恢复后进入
/// [`tick_entry`] 时据此把样本确定性地归入 Flash 桶 —— 窗口时序
/// (due 时刻) 在长排队场景下不可靠 (终点跨越多个周期时误判),
/// 原子标志则与"该次中断是否于操作期间排队"严格对应。
static FLASH_OP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 初始化 DWT 周期计数器并记录节拍周期。
///
/// 须在系统时钟配置完成后、调度器启动前调用 (板级初始化)。
pub fn init(hclk_hz: u32) {
    let demcr = Reg::new(DEMCR);
    demcr.write(demcr.read() | DEMCR_TRCENA);
    let ctrl = Reg::new(DWT_CTRL);
    ctrl.write(0);
    Reg::new(DWT_CYCCNT).write(0);
    ctrl.write(DWT_CTRL_CYCCNTENA);

    HCLK_HZ.store(hclk_hz, Ordering::Relaxed);
    PERIOD_CYCLES.store(hclk_hz / crate::config::SYSTICK_FREQ_HZ, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Release);
}

/// 读取当前周期计数 (未初始化时为 0)
#[inline]
fn now_cycles() -> u32 {
    Reg::new(DWT_CYCCNT).read()
}

// ============================== 关中断测量 (critical_section 内) ==============================

/// 临界区进入时刻 (在 PRIMASK 置位后调用; 未初始化返回 0)
#[inline]
pub(crate) fn critical_begin() -> u32 {
    if ENABLED.load(Ordering::Relaxed) {
        now_cycles()
    } else {
        0
    }
}

/// 临界区退出时刻 (在 PRIMASK 恢复前调用; `begin == 0` 表示未测量)。
///
/// Flash 擦/写期间 bus hold 会把触发 store 所在临界区一起 stall (毫秒级),
/// 属于硬件固有特性: 该临界区运行于 [`FLASH_OP_ACTIVE`] 置位期间,
/// 据此把样本计入独立的 flash 峰值, 不污染 [`max_critical_cycles`]
/// 判定指标 (区间重叠判定在跨周期场景不可靠, 以原子标志为准)。
///
/// 注意: 若临界区恰好在 CYCCNT==0 时开始, 该次采样被跳过 (计数器
/// 初值时刻才可能发生, 影响可忽略)。
#[inline]
pub(crate) fn critical_end(begin: u32) {
    if begin == 0 {
        return;
    }
    let cs_len = now_cycles().wrapping_sub(begin);
    let counter = if FLASH_OP_ACTIVE.load(Ordering::Acquire) {
        &CRITICAL_FLASH_MAX
    } else {
        &CRITICAL_MAX
    };
    counter.fetch_max(cs_len, Ordering::Relaxed);
}

// ============================== Flash 擦写窗口 (efm 标注) ==============================

/// 标注 Flash 擦/写窗口开始 (bus hold 起点, 由 efm 驱动在触发前调用)
///
/// 同时置位 [`FLASH_OP_ACTIVE`]: 窗口覆盖整个擦/写业务 (含 bus hold
/// 与后续收尾), 期间排队的中断由 [`tick_entry`] 据此归类。
pub(crate) fn flash_window_begin() {
    if ENABLED.load(Ordering::Relaxed) {
        FLASH_OP_ACTIVE.store(true, Ordering::Release);
        FLASH_WIN_START.store(now_cycles(), Ordering::Release);
    }
}

/// 标注 Flash 擦/写窗口结束 (bus hold 终点, 由 efm 驱动在等待结束后调用)
pub(crate) fn flash_window_end() {
    if ENABLED.load(Ordering::Relaxed) {
        FLASH_OP_ACTIVE.store(false, Ordering::Release);
        FLASH_WIN_END.store(now_cycles(), Ordering::Release);
    }
}

/// 上次节拍入口的 CYCCNT (基线; 0 = 下一拍仅记录基线)
static LAST_TICK_CYCCNT: AtomicU32 = AtomicU32::new(0);

// ============================== SysTick 到达延迟 (sys_tick_handler 入口) ==============================

/// 节拍中断入口时刻 (由 [`crate::sys_tick_handler`] 第一条调用)。
///
/// 基线法测量: 期望 = 上次入口 CYCCNT + 节拍周期 (硬件按固定周期触发);
/// 延迟 = max(0, now − expected)。**与 CVR 单周期截断不同**, 延迟超过
/// 一个周期 (Flash 擦写 stall 等) 也能得到完整值; 并发排队抢跑造成的
/// "早于期望" 由 max(0, ·) 截断为 0, 不会产生 ~2^32 伪负数 (旧版缺陷)。
/// 队列内多拍追赶时后续入口的期望以"上次实际入口"为基准, 追赶拍测得
/// 0 延迟属正常, 不再污染峰值。到达时刻 = now − 延迟, 落在 Flash 擦写
/// 窗口内的样本单独归类 (信息性)。
pub fn tick_entry() {
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    let period = PERIOD_CYCLES.load(Ordering::Relaxed);
    if period == 0 {
        return;
    }
    let now = now_cycles();
    let last = LAST_TICK_CYCCNT.load(Ordering::Relaxed);
    if last == 0 {
        // 首拍: 仅建立基线
        LAST_TICK_CYCCNT.store(now, Ordering::Relaxed);
        return;
    }
    let expected = last.wrapping_add(period);
    let latency = now.wrapping_sub(expected);
    // 早到 (并发排队追赶) 截断为 0 —— 挂起中断只在硬件失序时产生,
    // max(0, ·) 下放样测不到伪负值
    let latency = if latency > u32::MAX / 2 { 0 } else { latency };
    LAST_TICK_CYCCNT.store(now, Ordering::Relaxed);
    // 确定性分类 (二者取或):
    // - 本次中断排队于 Flash 操作期 (FLASH_OP_ACTIVE 标志, 覆盖
    //   bus hold 与业务收尾); 或
    // - 到达时刻 (now − latency, 基线法全值) 落在擦写窗口内 ——
    //   覆盖"排队于擦除期、但 handler 进入时 Flag 已清除"的样本。
    // 两者命中即归入 Flash 桶, 不污染判定指标。
    let due = now.wrapping_sub(latency);
    let in_window = {
        let win_start = FLASH_WIN_START.load(Ordering::Acquire);
        let win_end = FLASH_WIN_END.load(Ordering::Acquire);
        due.wrapping_sub(win_start) < win_end.wrapping_sub(win_start)
    };
    if FLASH_OP_ACTIVE.load(Ordering::Acquire) || in_window {
        TICK_LATENCY_FLASH_MAX.fetch_max(latency, Ordering::Relaxed);
    } else {
        TICK_LATENCY_MAX.fetch_max(latency, Ordering::Relaxed);
    }
}

// ============================== 查询 (仅 soak 消费) ==============================

/// 清零全部峰值 (soak 每次运行开始时建立新基线)
#[cfg(shell_soak)]
pub fn reset_peaks() {
    CRITICAL_MAX.store(0, Ordering::Relaxed);
    CRITICAL_FLASH_MAX.store(0, Ordering::Relaxed);
    TICK_LATENCY_MAX.store(0, Ordering::Relaxed);
    TICK_LATENCY_FLASH_MAX.store(0, Ordering::Relaxed);
    LAST_TICK_CYCCNT.store(0, Ordering::Relaxed);
}

/// 最长关中断时间 (cycles, 不含 Flash 擦写窗口; 未初始化/无采样为 0)
#[cfg(shell_soak)]
pub fn max_critical_cycles() -> u32 {
    CRITICAL_MAX.load(Ordering::Relaxed)
}

/// Flash 擦写窗口内的最大关中断时间 (cycles, 信息性)
#[cfg(shell_soak)]
pub fn max_critical_flash_cycles() -> u32 {
    CRITICAL_FLASH_MAX.load(Ordering::Relaxed)
}

/// 最长 SysTick 到达延迟 (cycles, 不含 Flash 擦写窗口)
#[cfg(shell_soak)]
pub fn max_tick_latency_cycles() -> u32 {
    TICK_LATENCY_MAX.load(Ordering::Relaxed)
}

/// Flash 擦写窗口内的最大到达延迟 (cycles, 信息性)
#[cfg(shell_soak)]
pub fn max_tick_latency_flash_cycles() -> u32 {
    TICK_LATENCY_FLASH_MAX.load(Ordering::Relaxed)
}

/// cycles → 微秒 (按初始化时的 HCLK)
#[cfg(shell_soak)]
pub fn cycles_to_us(cycles: u32) -> u32 {
    let hclk = HCLK_HZ.load(Ordering::Relaxed);
    if hclk == 0 {
        return 0;
    }
    ((cycles as u64) * 1_000_000 / hclk as u64) as u32
}
