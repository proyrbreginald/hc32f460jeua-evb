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
//! - **零依赖**: 直接复用 console 的打印锁与 `format_args!`, 不引入格式化
//!   缓冲区 (每条日志只有一次整行格式化, 无额外拷贝)。
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
//! 中断上下文不可输出日志。

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

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

    /// ANSI 前景色 (标签着色)
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
    crate::console::write_fmt_line(core::format_args!(
        "{}{}{}\x1b[0m {}",
        stamp,
        level.color(),
        level.tag(),
        args
    ));
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
