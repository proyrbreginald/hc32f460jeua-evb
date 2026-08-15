//! 控制台输出: 将任意 UART 绑定到 `print!` / `println!`
//!
//! 绑定在**编译期**完成: 输出串口由 `.cargo/config.toml` 的 `CFG_UART_UNIT`
//! 决定 (经 [`crate::config::ConsoleUart`]), 零运行时开销 (`Uart<U>` 是
//! 零大小类型)。
//!
//! # 用法
//!
//! ```no_run
//! println!("Hello!");
//! println!("value = {}", 42);
//! print!("no newline");
//! ```
//!
//! 换行符为 CRLF (`\r\n`), 兼容大多数串口终端。
//!
//! # 并发设计
//!
//! 一次打印调用是**原子输出**的: 线程上下文的打印 (`write_fmt`, 即
//! `print!`/`println!`) 由带**优先级继承**的互斥量 ([`rtos::Mutex`])
//! 串行化, 任意时刻至多一个线程占用串口, 输出不会交错。
//! 锁以 RAII 守卫形式持有 ([`rtos::MutexGuard`]), 作用域结束自动
//! 释放, 不存在忘解锁/重复解锁路径。
//!
//! 优先级继承保证**不会出现高优先级线程无界等待低优先级线程**:
//! 低优先级线程持有打印锁时, 等待的高优先级线程会将其提升到自己的
//! 优先级, 打印完成立即释放, 等待时间有界 (仅一次打印的时长)。
//!
//! # 中断上下文
//!
//! 中断/定时器回调中的 `print!`/`println!` 自动退化为 [`write_fmt_raw`]
//! (无锁，输出可能与线程交错)，不会尝试阻塞互斥量。内核 panic/fault
//! 诊断也使用该通道。
//!
//! # 串口背压
//!
//! UART 为 115200 8N1 **无流控**。打印本身始终原子; 但当输出速率
//! 接近或超过 PC 端读取能力时, USB 转串口 (CH340) 缓冲可能溢出并
//! 丢弃字节, 表现为"行尾截断" (与打印交错无关)。需要可靠全量输出时
//! 应限制输出速率或改用带流控的接口。

use crate::rtos::{Mutex, Timeout};
use core::sync::atomic::{AtomicBool, Ordering};

/// 控制台输出串口 (编译期绑定: `.cargo/config.toml` 的 `CFG_UART_UNIT`)
pub type ConsoleUart = crate::config::ConsoleUart;

/// 打印互斥量 (优先级继承): 串行化线程上下文的打印输出
///
/// 保护数据为 `()`: 本模块仅需锁语义 (输出串行化), 经
/// [`MutexGuard`](crate::rtos::MutexGuard) 独占持有即可。
static PRINT_MUTEX: Mutex<()> = Mutex::new(());

/// 控制台是否就绪: UART 初始化前 (`mark_ready` 前) 的打印**静默丢弃**,
/// 防止在 UART 时钟未使能时访问 USART (TXE 读回 0 导致等待死循环)。
static READY: AtomicBool = AtomicBool::new(false);

/// 标记控制台就绪 (由应用在 UART 初始化完成后调用一次)
pub fn mark_ready() {
    // 发布 UART 初始化在先、其他执行上下文观察 READY 在后的关系。
    READY.store(true, Ordering::Release);
}

/// 向控制台输出格式化内容 (由 `print!` 宏调用)
///
/// 线程上下文使用; 持有打印锁期间输出为原子操作。
/// 注意: 仅内容原子, 若需"整行"原子输出请用 [`write_fmt_line`]。
///
/// 调度器启动前 (boot 阶段, 单执行流) 自动退化为无锁输出,
/// 避免在 `rtos::init()` 之前使用互斥量 (此时 `sched::current()`
/// 为空, 内核阻塞原语不可用)。
pub fn write_fmt(args: core::fmt::Arguments<'_>) {
    if !READY.load(Ordering::Acquire) {
        return; // UART 未就绪: 静默丢弃, 防止 TXE 等待死循环
    }
    if crate::rtos::scheduler_started() {
        if crate::critical_section::in_isr() {
            write_fmt_raw(args);
            return;
        }
        // 只有确实取得 RAII 守卫后才输出。递归格式化等锁协议错误会
        // 丢弃本次嵌套输出，而不是绕过锁破坏外层打印的原子性。
        let Ok(_guard) = PRINT_MUTEX.lock(Timeout::Forever) else {
            return;
        };
        write_fmt_raw(args);
    } else {
        write_fmt_raw(args);
    }
}

/// 无锁输出格式化内容 (仅限中断上下文/panic 诊断使用)
///
/// 不获取打印锁, 不阻塞; 输出可能与其他上下文交错。
pub fn write_fmt_raw(args: core::fmt::Arguments<'_>) {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    let mut uart = ConsoleUart::take();
    let _ = core::fmt::write(&mut uart, args);
}

/// 无锁输出原始字节 (调用方负责持锁/不交错约束)
fn write_bytes_raw(bytes: &[u8]) {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    let uart = ConsoleUart::take();
    uart.write(bytes);
}

/// 打印锁内执行 `f`, `f` 收到原始字节写出器 (输出不会与其他线程交错)。
///
/// 锁协议与 [`write_fmt_line`] 相同: 调度器启动前/中断上下文自动无锁
/// 直写 (行为一致, 仅不保证互斥)。返回 `None` 表示 UART 未就绪或取锁
/// 失败, 此时 `f` 未执行。
pub fn with_print_lock<R>(f: impl FnOnce(&mut dyn FnMut(&[u8])) -> R) -> Option<R> {
    if !READY.load(Ordering::Acquire) {
        return None;
    }
    if crate::rtos::scheduler_started() {
        if crate::critical_section::in_isr() {
            return Some(f(&mut |bytes| write_bytes_raw(bytes)));
        }
        let Ok(_guard) = PRINT_MUTEX.lock(Timeout::Forever) else {
            return None;
        };
        debug_assert_eq!(
            PRINT_MUTEX.owner(),
            crate::rtos::sched::current(),
            "print lock lost during output"
        );
        Some(f(&mut |bytes| write_bytes_raw(bytes)))
    } else {
        Some(f(&mut |bytes| write_bytes_raw(bytes)))
    }
}

/// 格式化片段同时写 UART 与回调 `side` 的扇出 sink
struct FanoutSink<'a> {
    write_raw: &'a mut dyn FnMut(&[u8]),
    side: &'a mut dyn FnMut(&[u8]),
}

impl core::fmt::Write for FanoutSink<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        (self.write_raw)(s.as_bytes());
        (self.side)(s.as_bytes());
        Ok(())
    }
}

/// 原子输出一整行 (内容 + CRLF), 内容 = `prefix` + 格式化结果 + `suffix`。
///
/// 格式化片段在打印锁内同时回调 `side` (如日志落盘缓冲收集无颜色内容)。
pub fn write_fmt_line_fanout(
    prefix: &[u8],
    suffix: &[u8],
    args: core::fmt::Arguments<'_>,
    side: &mut dyn FnMut(&[u8]),
) {
    let _ = with_print_lock(|write_raw| {
        write_raw(prefix);
        let mut sink = FanoutSink { write_raw, side };
        let _ = core::fmt::write(&mut sink, args);
        write_raw(suffix);
        write_raw(b"\r\n");
    });
}

/// 原子输出一整行 (内容 + CRLF, 由 `println!` 宏调用)
///
/// 内容与换行在同一把锁内完成, 任意时刻至多一个线程占用串口,
/// 行与行之间不会交错。
pub fn write_fmt_line(args: core::fmt::Arguments<'_>) {
    write_fmt_line_fanout(b"", b"", args, &mut |_| {});
}

// panic/fault 诊断处理见 `panic` 模块 (输出经由 write_fmt_raw)

/// 输出格式化内容, 不换行
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::write_fmt(core::format_args!($($arg)*))
    };
}

/// 原子输出一整行并换行 (CRLF)
#[macro_export]
macro_rules! println {
    () => {
        $crate::console::write_fmt_line(core::format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::console::write_fmt_line(core::format_args!($($arg)*))
    };
}
