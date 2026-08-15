//! 应用日志模块 — 与内核打印分离的**可开关**诊断输出
//!
//! # 设计 (第一性原理)
//!
//! - **分层**: 内核打印 ([`crate::console`]) 是平台底座 — 启动横幅、panic
//!   诊断、shell 提示符等**无论如何都输出**, 不经过本模块; 本模块是
//!   应用层可选项, 输出与否由 (全局开关 × 级别阈值) 共同决定;
//! - **编译期默认 + 运行时可调**: 默认开关/级别阈值来自 `.cargo/config.toml`
//!   (`CFG_LOG_ENABLE` / `CFG_LOG_LEVEL`), 运行时可经 shell 的 `log` 命令
//!   切换 (重启后恢复配置默认值);
//! - **原子整行输出**: 每条日志 (颜色标签 + 消息) 在一次
//!   [`crate::console::write_fmt_line`] 内输出, 多线程不交错;
//! - **双通道**: 控制台原样输出 (含 ANSI 颜色); 同时以**无颜色**格式
//!   追加到 RAM 日志缓冲 ([`crate::logring::LogRing`]), 由日志落盘线程
//!   (见 [`crate::logfile`]) 自动保存到 `/log/` — 控制台路径零改动,
//!   缓冲追加是一次独立的轻量格式化 (无额外拷贝到堆)。
//!
//! # 用法
//!
//! ```no_run
//! log_info!("系统启动");
//! log_debug!("value = {}", 42);
//! ```
//!
//! # 约束
//!
//! 与 `print!`/`println!` 相同: 仅在**线程上下文**可调用 (输出会取打印锁),
//! 中断上下文不可输出日志。日志缓冲在 RAM 中, 掉电或复位会丢失尚未
//! 落盘的内容 (落盘间隔见 `CFG_LOG_FLUSH_MS`)。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::critical_section;
use crate::logring::{EntryMark, LogRing};

/// 临界区互斥的日志缓冲 (追加方: 任意线程; 排空方: 日志落盘线程)。
/// 安全契约: 所有访问都发生在 [`critical_section::with`] 内, 单核上
/// 互斥成立; 缓冲内容无需跨线程共享可见性 (整块拷贝)。
struct CsLogRing<const CAP: usize> {
    inner: UnsafeCell<LogRing<CAP>>,
}

unsafe impl<const CAP: usize> Sync for CsLogRing<CAP> {}

impl<const CAP: usize> CsLogRing<CAP> {
    const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(LogRing::new()),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut LogRing<CAP>) -> R) -> R {
        critical_section::with(|_| {
            // SAFETY: 临界区内唯一可变访问
            let ring = unsafe { &mut *self.inner.get() };
            f(ring)
        })
    }
}

/// 日志缓冲容量 (字节, `CFG_LOG_RING`)
const RING_CAP: usize = crate::config::LOG_RING;

/// 待落盘的日志缓冲 (追加丢弃最旧)
static RING: CsLogRing<RING_CAP> = CsLogRing::new();

/// 落盘开关: 编译期默认 `CFG_LOG_FILE_ENABLE`, 运行时可经
/// shell `log file on|off` 切换
static FILE_ENABLED: AtomicBool = AtomicBool::new(crate::config::LOG_FILE_ENABLE);

/// 日志落盘是否启用 (仅影响 [`drain`] 是否被消费, 不影响缓冲追加)
pub fn file_enabled() -> bool {
    FILE_ENABLED.load(Ordering::Relaxed)
}

/// 切换日志落盘开关 (shell `log file on|off` 调用)
pub fn set_file_enabled(on: bool) {
    FILE_ENABLED.store(on, Ordering::Relaxed);
}

/// 把缓冲中的所有日志条目拷入 `out` (每条末尾补 `\n`) 并清空缓冲。
///
/// 由日志落盘线程周期性调用; 返回拷贝的字节数 (0 = 无待落盘日志)。
pub fn drain_into(out: &mut alloc::vec::Vec<u8>) -> usize {
    RING.with(|ring| ring.drain_into(out))
}

/// 待落盘的日志字节数 (含条目头, 不含换行符)
pub fn pending_bytes() -> usize {
    RING.with(|ring| ring.bytes_pending())
}

/// 日志级别 (数值越小越严重, 阈值比较用 `<=`)
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    /// 错误: 功能不可用/数据损坏
    Error = 0,
    /// 警告: 异常但可继续运行
    Warn = 1,
    /// 信息: 关键流程节点 (默认阈值)
    Info = 2,
    /// 调试: 详细状态 (默认不输出)
    Debug = 3,
    /// 追踪: 最详细 (逐条事件)
    Trace = 4,
}

impl Level {
    /// 数值编码 (原子阈值存储用)
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 数值编码 → 级别
    pub fn from_u8(v: u8) -> Level {
        match v {
            0 => Level::Error,
            1 => Level::Warn,
            2 => Level::Info,
            3 => Level::Debug,
            _ => Level::Trace,
        }
    }

    /// 级别名称 (shell `log level` 显示/解析用)
    pub const fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }

    /// 名称 → 级别
    pub fn from_name(name: &str) -> Option<Level> {
        match name {
            "error" => Some(Level::Error),
            "warn" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }

    /// 输出标签
    fn tag(self) -> &'static str {
        match self {
            Level::Error => "[ERR]",
            Level::Warn => "[WRN]",
            Level::Info => "[INF]",
            Level::Debug => "[DBG]",
            Level::Trace => "[TRC]",
        }
    }

    /// ANSI 前景色 (整行按级别着色; 落盘文件为无颜色纯文本)
    fn color(self) -> &'static str {
        match self {
            Level::Error => "\x1b[31m", // 红
            Level::Warn => "\x1b[33m",  // 黄
            Level::Info => "\x1b[32m",  // 绿
            Level::Debug => "\x1b[36m", // 青
            Level::Trace => "\x1b[37m", // 白
        }
    }
}

/// 全局开关: 编译期默认 `CFG_LOG_ENABLE`, 运行时经 shell `log on|off` 切换
/// (Relaxed 足够: 开关是幂等布尔量, 最坏情况切换后下一条日志才生效)
static ENABLED: AtomicBool = AtomicBool::new(crate::config::LOG_ENABLE);

/// 级别阈值: 输出 `≤ 阈值的级别`。编译期默认 `CFG_LOG_LEVEL`,
/// 运行时经 shell `log level <级别>` 调整
static THRESHOLD: AtomicU8 = AtomicU8::new(crate::config::LOG_LEVEL.as_u8());

/// 日志是否启用
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 当前级别阈值
pub fn level() -> Level {
    Level::from_u8(THRESHOLD.load(Ordering::Relaxed))
}

/// 切换日志开关 (shell `log on|off` 调用; 重启后恢复配置默认值)
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// 设置级别阈值 (shell `log level <级别>` 调用)
pub fn set_level(level: Level) {
    THRESHOLD.store(level.as_u8(), Ordering::Relaxed);
}

/// 该级别当前是否会被输出 (开关 × 级别阈值, 两条件缺一不可)
pub fn should_log(l: Level) -> bool {
    enabled() && l <= level()
}

/// 输出一条日志: 时间戳 + 彩色标签 + 消息, 整行原子输出 (仅线程上下文)
///
/// 时间戳 `[天:时:分:秒]` 来自 RTC 运行时长 (见 [`crate::rtc::elapsed_dhms`]);
/// RTC 未初始化/未启动时省略前缀 (boot 阶段)。
///
/// # 单次格式化扇出
///
/// 控制台与落盘缓冲共用**一次** `core::fmt::write`。整条日志 (环条目
/// begin → 格式化 → commit) 在**同一把打印锁**内完成: 控制台接收
/// 颜色前缀的整行, 落盘环接收无颜色的纯文本; 环满时按"丢弃最旧"
/// 策略腾空间, 仍放不下则截断本条 (控制台输出始终完整)。
pub fn log(level: Level, args: core::fmt::Arguments<'_>) {
    if !should_log(level) {
        return;
    }
    // 时间戳 (运行时长): RTC 未运行时为空串
    let (d, h, m, s, has_stamp) = match crate::rtc::elapsed_dhms() {
        Some((d, h, m, s)) => (d, h, m, s, true),
        None => (0, 0, 0, 0, false),
    };
    let stamp = if has_stamp {
        core::format_args!("[{}:{:02}:{:02}:{:02}] ", d, h, m, s)
    } else {
        core::format_args!("")
    };
    // 整条日志在打印锁内完成 (begin → 格式化 → commit 不与其他线程交错)
    let _ = crate::console::with_print_lock(|write_raw| {
        // 颜色前缀可经 CFG_LOG_COLOR 关闭 (纯文本终端; 落盘环恒为无色)
        if crate::config::LOG_COLOR {
            write_raw(level.color().as_bytes());
        }
        let mut entry: Option<EntryMark> = RING.with(|ring| ring.begin_entry());
        {
            let mut sink = FanoutSink {
                write_raw,
                entry: &mut entry,
            };
            let _ = core::fmt::write(
                &mut sink,
                core::format_args!("{}{} {}", stamp, level.tag(), args),
            );
        }
        if let Some(mark) = entry {
            RING.with(|ring| ring.commit_entry(&mark));
        }
        if crate::config::LOG_COLOR {
            write_raw(b"\x1b[0m");
        }
        write_raw(b"\r\n");
    });
}

/// 扇出 sink: 每片段写控制台 (持锁) 并追加到落盘环条目
struct FanoutSink<'a> {
    write_raw: &'a mut dyn FnMut(&[u8]),
    entry: &'a mut Option<EntryMark>,
}

impl core::fmt::Write for FanoutSink<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        (self.write_raw)(s.as_bytes());
        if let Some(mark) = &mut *self.entry {
            RING.with(|ring| {
                ring.append_to_entry(mark, s.as_bytes());
            });
        }
        Ok(())
    }
}

/// 输出 Error 级日志 (红色 `[ERR]` 标签)
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::log($crate::log::Level::Error, core::format_args!($($arg)*))
    };
}

/// 输出 Warn 级日志 (黄色 `[WRN]` 标签)
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::log($crate::log::Level::Warn, core::format_args!($($arg)*))
    };
}

/// 输出 Info 级日志 (绿色 `[INF]` 标签)
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::log($crate::log::Level::Info, core::format_args!($($arg)*))
    };
}

/// 输出 Debug 级日志 (青色 `[DBG]` 标签, 默认阈值不输出)
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::log::log($crate::log::Level::Debug, core::format_args!($($arg)*))
    };
}

/// 输出 Trace 级日志 (白色 `[TRC]` 标签, 默认阈值不输出)
#[macro_export]
macro_rules! log_trace {
    ($($arg:tt)*) => {
        $crate::log::log($crate::log::Level::Trace, core::format_args!($($arg)*))
    };
}
