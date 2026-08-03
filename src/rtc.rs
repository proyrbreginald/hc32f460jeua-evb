//! RTC 实时时钟驱动 (对齐 DDL v3.3.0 `hc32_ll_rtc.c/h`)
//!
//! # 时钟源
//!
//! - **LRC** (内部低速 RC 32.768kHz, 无需外部器件) — 本工程默认
//!   (JEUA 48pin 无 XTAL32 引脚对);
//! - XTAL32 (外部晶振): 需先经 CLK 启动晶振并配置模拟引脚, 本模块
//!   提供源选择但不负责晶振启动。
//!
//! # 特性
//!
//! - 时间 (时/分/秒) 与日期 (年/月/日/周), 寄存器 **BCD** 编码,
//!   24/12 小时制可配;
//! - 读/写时间日期需进入 **RW 模式** (CR2.RWREQ/RWEN, 自动处理);
//! - 周期中断: 0.5s/1s/1min/1hour/1day/1month (CR1.PRDS);
//! - 闹钟: 时+分匹配且星期位掩码命中 (ALMWEEK, 0x7F=每天);
//! - 无 VBAT 备份域: VDD 供电, 掉电后寄存器丢失, 需重新初始化;
//! - **运行时长**: [`elapsed_dhms`] 从日历寄存器推导自 [`start`] 起的
//!   (天:时:分:秒), 供日志时间戳使用。
//!
//! # 使用
//!
//! ```no_run
//! rtc::init(rtc::Config::default());
//! rtc::set_date(rtc::Date { year: 26, month: 1, day: 1, weekday: 4 });
//! rtc::set_time(rtc::Time { hour: 0, minute: 0, second: 0 });
//! rtc::start();
//! // 周期中断 (1s, 需 intc 线) 与闹钟中断的事件源:
//! // intc::src::RTC_PRD (82) / intc::src::RTC_ALM (81)
//! ```
//!
//! 部分 API (闹钟/周期中断/12H 制) 供应用按需选用, 忽略未使用项的死代码警告。
#![allow(dead_code)]

/// RTC 基址
const RTC_BASE: usize = 0x4004_C000;

// ---- 寄存器偏移 (DDL CM_RTC_TypeDef 逐项核对) ----
//
// 注意: 每个寄存器后带 3 字节 RESERVED 填充 (32 位总线字对齐),
// 寄存器间距为 **4 字节**, 不是连续排布!
const CR0: usize = 0x00;
const CR1: usize = 0x04;
const CR2: usize = 0x08;
const CR3: usize = 0x0C;
const SEC: usize = 0x10;
const MIN: usize = 0x14;
const HOUR: usize = 0x18;
const WEEK: usize = 0x1C;
const DAY: usize = 0x20;
const MON: usize = 0x24;
const YEAR: usize = 0x28;
const ALMMIN: usize = 0x2C;
const ALMHOUR: usize = 0x30;
const ALMWEEK: usize = 0x34;
const ERRCRH: usize = 0x38;
const ERRCRL: usize = 0x3C;

// ---- 位定义 ----
const CR0_RESET: u8 = 1 << 0; // 软件复位 (写 1 复位, 需等待清零)
const CR1_PRDS: u8 = 0x07; // 周期中断节拍
const CR1_AMPM: u8 = 1 << 3; // 1 = 24 小时制
const CR1_ALMFCLR: u8 = 1 << 4; // 写 1 清闹钟标志
const CR1_START: u8 = 1 << 7; // 1 = 启动计数
const CR2_RWREQ: u8 = 1 << 0; // RW 模式请求
const CR2_RWEN: u8 = 1 << 1; // RW 模式使能 (硬件应答)
const CR2_ALMF: u8 = 1 << 3; // 闹钟标志
const CR2_PRDIE: u8 = 1 << 5; // 周期中断使能
const CR2_ALMIE: u8 = 1 << 6; // 闹钟中断使能
const CR2_ALME: u8 = 1 << 7; // 闹钟功能使能
const CR3_LRCEN: u8 = 1 << 4; // 使能内部 LRC 振荡器
const CR3_RCKSEL: u8 = 1 << 7; // 时钟源: 1=LRC, 0=XTAL32

/// 12 小时制 PM 位 (HOUR 寄存器 bit5, 对齐 DDL RTC_HOUR_12H_PM)
const HOUR_12H_PM: u8 = 1 << 5;

/// RTC 时钟源
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClockSource {
    /// 内部低速 RC (32.768kHz, 无外部器件, 默认)
    Lrc,
    /// 外部晶振 XTAL32 (需先启动晶振并配模拟引脚)
    Xtal32,
}

/// 小时制
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HourFormat {
    /// 24 小时制 (0~23)
    H24,
    /// 12 小时制 (1~12 + AM/PM)
    H12,
}

/// 周期中断节拍 (CR1.PRDS)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntPeriod {
    /// 无效 (默认)
    None = 0,
    /// 0.5 秒
    HalfSec = 1,
    /// 1 秒
    Sec = 2,
    /// 1 分钟
    Min = 3,
    /// 1 小时
    Hour = 4,
    /// 1 天
    Day = 5,
    /// 1 个月
    Month = 6,
}

/// RTC 配置 (对齐 DDL `stc_rtc_init_t`)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// 时钟源
    pub clock_src: ClockSource,
    /// 小时制
    pub hour_format: HourFormat,
    /// 周期中断节拍
    pub int_period: IntPeriod,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clock_src: ClockSource::Lrc,
            hour_format: HourFormat::H24,
            int_period: IntPeriod::None,
        }
    }
}

/// 时间 (十进制)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Time {
    /// 时 (24H: 0~23; 12H: 1~12)
    pub hour: u8,
    /// 分 (0~59)
    pub minute: u8,
    /// 秒 (0~59)
    pub second: u8,
    /// 12H 制 PM 标志 (24H 制忽略)
    pub pm: bool,
}

/// 日期 (十进制)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Date {
    /// 年 (0~99, 表示 2000~2099)
    pub year: u8,
    /// 月 (1~12)
    pub month: u8,
    /// 日 (1~31)
    pub day: u8,
    /// 星期 (0=周日 ~ 6=周六)
    pub weekday: u8,
}

/// 闹钟 (对齐 DDL `stc_rtc_alarm_t`; 时+分匹配 + 星期位掩码)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Alarm {
    /// 时 (12H 制含 PM 标志)
    pub hour: u8,
    /// 分
    pub minute: u8,
    /// 星期位掩码 (bit0=周日 ~ bit6=周六; 0x7F = 每天)
    pub weekday_mask: u8,
    /// 12H 制 PM 标志
    pub pm: bool,
}

// ============================== 底层访问 ==============================

fn read8(offset: usize) -> u8 {
    unsafe { core::ptr::read_volatile((RTC_BASE + offset) as *const u8) }
}

fn write8(offset: usize, value: u8) {
    unsafe { core::ptr::write_volatile((RTC_BASE + offset) as *mut u8, value) };
}

fn modify8(offset: usize, f: impl FnOnce(u8) -> u8) {
    write8(offset, f(read8(offset)));
}

/// 十进制 → BCD
const fn dec2bcd(x: u8) -> u8 {
    ((x / 10) << 4) | (x % 10)
}

/// BCD → 十进制
const fn bcd2dec(x: u8) -> u8 {
    ((x >> 4) * 10) + (x & 0x0F)
}

// ============================== 初始化 ==============================

/// 初始化 RTC (对齐 DDL `RTC_Init`): 时钟源 → 小时制/周期 → 启动 LRC。
/// 不启动计数 (需 [`start`]), 不写时间日期。
pub fn init(cfg: Config) {
    deinit();
    // CR3: 时钟源 (LRC 时同时使能内部 LRC 振荡器)
    let cr3 = match cfg.clock_src {
        ClockSource::Lrc => CR3_LRCEN | CR3_RCKSEL,
        ClockSource::Xtal32 => 0,
    };
    write8(CR3, cr3);
    // CR1: 小时制 + 周期节拍 (START 保持 0)
    let amp = match cfg.hour_format {
        HourFormat::H24 => CR1_AMPM,
        HourFormat::H12 => 0,
    };
    write8(CR1, amp | (cfg.int_period as u8));
}

/// 软件复位 (对齐 DDL `RTC_DeInit`)
///
/// **两步**: 先退出复位态 (RESET=0, 幂等), 再触发复位 (RESET=1),
/// 每步等待完成 —— 复位未完成时对 CR1/CR3 等寄存器的写入会被硬件
/// 忽略, 导致 RTC 无法启动。等待超时按 HCLK 折算 (~100ms, 对齐 DDL
/// `RTC_SW_RST_TIMEOUT × HCLK/20000`)。
pub fn deinit() {
    write8(CR0, 0);
    wait_reset_clear();
    write8(CR0, CR0_RESET);
    wait_reset_clear();
}

/// 等待 CR0.RESET 清零 (超时按 HCLK 折算, 对齐 DDL)
fn wait_reset_clear() {
    let timeout = 100 * (crate::clk::hclk_hz() / 20_000);
    for _ in 0..timeout {
        if read8(CR0) & CR0_RESET == 0 {
            return;
        }
    }
}

/// 启动计数 (CR1.START=1, 对齐 DDL `RTC_Cmd(ENABLE)`)
///
/// 记录基准日期/时间 (由 [`elapsed_dhms`] 计算运行时长)。
pub fn start() {
    write8(CR1, read8(CR1) | CR1_START);
    record_base();
}

/// 停止计数
pub fn stop() {
    write8(CR1, read8(CR1) & !CR1_START);
}

/// RTC 是否在计数 (CR1.START)
pub fn running() -> bool {
    read8(CR1) & CR1_START != 0
}

// ============================== RW 模式 ==============================

/// 进入读写模式 (CR2.RWREQ → 等 RWEN; 对齐 DDL `RTC_EnterRwMode`)
fn enter_rw() {
    if read8(CR1) & CR1_START != 0 && read8(CR2) & CR2_RWEN == 0 {
        write8(CR2, CR2_RWREQ | CR2_ALMF);
        wait_rw_en();
    }
}

/// 退出读写模式 (清 RWREQ, 等 RWEN 清 0; 对齐 DDL `RTC_ExitRwMode`)
fn exit_rw() {
    modify8(CR2, |v| v & !CR2_RWREQ);
    for _ in 0..wait_timeout() {
        if read8(CR2) & CR2_RWEN == 0 {
            break;
        }
    }
}

/// RW 模式切换等待次数 (按 HCLK 折算, 对齐 DDL `RTC_MD_SWITCH_TIMEOUT`)
fn wait_timeout() -> u32 {
    100 * (crate::clk::hclk_hz() / 20_000)
}

/// 等 RWEN 置位 (对齐 DDL EnterRwMode 的等待循环)
fn wait_rw_en() {
    for _ in 0..wait_timeout() {
        if read8(CR2) & CR2_RWEN != 0 {
            break;
        }
    }
}

// ============================== 时间/日期 ==============================

/// 设置时间 (对齐 DDL `RTC_SetTime`; 内部自动进出 RW 模式)
pub fn set_time(t: Time) {
    let (hour, pm) = match hour_format() {
        HourFormat::H24 => (t.hour, false),
        HourFormat::H12 => (t.hour, t.pm),
    };
    enter_rw();
    write8(HOUR, dec2bcd(hour) | if pm { HOUR_12H_PM } else { 0 });
    write8(MIN, dec2bcd(t.minute));
    write8(SEC, dec2bcd(t.second));
    exit_rw();
}

/// 读取时间 (对齐 DDL `RTC_GetTime`)
pub fn get_time() -> Time {
    enter_rw();
    let hour = bcd2dec(read8(HOUR) & 0x3F);
    let pm = read8(HOUR) & HOUR_12H_PM != 0;
    let minute = bcd2dec(read8(MIN));
    let second = bcd2dec(read8(SEC));
    exit_rw();
    Time {
        hour,
        minute,
        second,
        pm,
    }
}

/// 设置日期 (对齐 DDL `RTC_SetDate`)
pub fn set_date(d: Date) {
    enter_rw();
    write8(YEAR, dec2bcd(d.year));
    write8(MON, dec2bcd(d.month));
    write8(DAY, dec2bcd(d.day));
    write8(WEEK, d.weekday & 0x07);
    exit_rw();
}

/// 读取日期 (对齐 DDL `RTC_GetDate`)
pub fn get_date() -> Date {
    enter_rw();
    let d = Date {
        year: bcd2dec(read8(YEAR)),
        month: bcd2dec(read8(MON)),
        day: bcd2dec(read8(DAY)),
        weekday: read8(WEEK) & 0x07,
    };
    exit_rw();
    d
}

/// 当前小时制 (CR1.AMPM)
pub fn hour_format() -> HourFormat {
    if read8(CR1) & CR1_AMPM != 0 {
        HourFormat::H24
    } else {
        HourFormat::H12
    }
}

// ============================== 闹钟 ==============================

/// 设置闹钟 (对齐 DDL `RTC_SetAlarm`; 无需 RW 模式)
pub fn set_alarm(a: Alarm) {
    let (hour, pm) = match hour_format() {
        HourFormat::H24 => (a.hour, false),
        HourFormat::H12 => (a.hour, a.pm),
    };
    write8(ALMHOUR, dec2bcd(hour) | if pm { HOUR_12H_PM } else { 0 });
    write8(ALMMIN, dec2bcd(a.minute));
    write8(ALMWEEK, a.weekday_mask & 0x7F);
}

/// 读取闹钟
pub fn get_alarm() -> Alarm {
    Alarm {
        hour: bcd2dec(read8(ALMHOUR) & 0x3F),
        minute: bcd2dec(read8(ALMMIN)),
        weekday_mask: read8(ALMWEEK) & 0x7F,
        pm: read8(ALMHOUR) & HOUR_12H_PM != 0,
    }
}

/// 使能/失能闹钟功能 (对齐 DDL `RTC_AlarmCmd`; 同时清闹钟标志)
pub fn alarm_enable(enable: bool) {
    modify8(CR2, |v| {
        let v = if enable { v | CR2_ALME } else { v & !CR2_ALME };
        v | CR2_ALMF // 写 1 清标志
    });
}

/// 周期/闹钟中断使能 (CR2.PRDIE/ALMIE, 对齐 DDL `RTC_IntCmd`)
pub fn int_enable(period: bool, alarm: bool) {
    modify8(CR2, |v| {
        let v = if period { v | CR2_PRDIE } else { v & !CR2_PRDIE };
        if alarm { v | CR2_ALMIE } else { v & !CR2_ALMIE }
    });
}

/// 闹钟标志 (CR2.ALMF)
pub fn alarm_flag() -> bool {
    read8(CR2) & CR2_ALMF != 0
}

/// 清除闹钟标志 (CR1.ALMFCLR)
pub fn clear_alarm_flag() {
    write8(CR1, read8(CR1) | CR1_ALMFCLR);
}

// ============================== 运行时长 (日志时间戳) ==============================

/// 基准 (启动时): 公历天数 + 秒数
static BASE_DAYS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static BASE_SECS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// 记录基准 (由 [`start`] 调用; 未设置日期时按 2000-01-01)
fn record_base() {
    let d = get_date();
    let t = get_time();
    BASE_DAYS.store(
        days_from_civil(2000 + d.year as i64, d.month as u32, d.day as u32) as u32,
        core::sync::atomic::Ordering::Relaxed,
    );
    BASE_SECS.store(secs_of_day(t), core::sync::atomic::Ordering::Relaxed);
}

/// 当日秒数
fn secs_of_day(t: Time) -> u32 {
    (t.hour as u32) * 3600 + (t.minute as u32) * 60 + t.second as u32
}

/// 自 [`start`] 起的运行时长 (天, 时, 分, 秒)
///
/// 由日历寄存器推导 (跨月/跨年正确, Howard Hinnant 公历算法);
/// RTC 未运行 (未初始化/未启动) 时返回 None —— 日志时间戳据此省略。
pub fn elapsed_dhms() -> Option<(u32, u32, u32, u32)> {
    if !running() {
        return None;
    }
    let d = get_date();
    let t = get_time();
    let now_days = days_from_civil(2000 + d.year as i64, d.month as u32, d.day as u32) as u32;
    let now_secs = secs_of_day(t);
    let base_days = BASE_DAYS.load(core::sync::atomic::Ordering::Relaxed);
    let base_secs = BASE_SECS.load(core::sync::atomic::Ordering::Relaxed);

    let total = (now_days - base_days) * 86_400 + now_secs - base_secs;
    Some((total / 86_400, (total / 3600) % 24, (total / 60) % 60, total % 60))
}

/// 公历天数 (自 1970-01-01, Howard Hinnant 算法, const)
const fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
