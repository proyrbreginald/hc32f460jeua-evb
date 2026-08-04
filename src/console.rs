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
//! 中断/定时器回调内**不可**调用加锁打印 (会挂起被打断的线程);
//! 如需在中断中输出, 使用 [`write_fmt_raw`] (无锁, 输出可能与其他
//! 上下文交错, 仅用于诊断)。内核 panic/fault 诊断即走该通道。
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
    READY.store(true, Ordering::Relaxed);
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
    if !READY.load(Ordering::Relaxed) {
        return; // UART 未就绪: 静默丢弃, 防止 TXE 等待死循环
    }
    if crate::rtos::scheduler_started() {
        // RAII 守卫: 离开本函数作用域时自动释放打印锁,
        // 忘解锁/重复解锁在类型层面被排除 (见 rtos::ipc::MutexGuard)
        let _guard = PRINT_MUTEX.lock(Timeout::Forever).ok();
        write_fmt_raw(args);
    } else {
        write_fmt_raw(args);
    }
}

/// 原子输出一整行 (内容 + CRLF, 由 `println!` 宏调用)
///
/// 内容与换行在同一把锁内完成, 任意时刻至多一个线程占用串口,
/// 行与行之间不会交错。
pub fn write_fmt_line(args: core::fmt::Arguments<'_>) {
    if !READY.load(Ordering::Relaxed) {
        return; // UART 未就绪: 静默丢弃, 防止 TXE 等待死循环
    }
    if crate::rtos::scheduler_started() {
        // 内容与换行在同一守卫内完成 (RAII), 行与行之间不会交错;
        // 守卫析构前校验锁仍由当前线程持有 (正常情况下必然成立,
        // 作为打印路径回归的哨兵检查)
        let guard = PRINT_MUTEX.lock(Timeout::Forever).ok();
        write_fmt_raw(args);
        debug_assert_eq!(
            PRINT_MUTEX.owner(),
            crate::rtos::sched::current(),
            "print lock lost during output"
        );
        write_fmt_raw(core::format_args!("\r\n"));
        drop(guard); // 显式释放: CRLF 输出完成后立即解锁
    } else {
        write_fmt_raw(args);
        write_fmt_raw(core::format_args!("\r\n"));
    }
}

/// 无锁输出格式化内容 (仅限中断上下文/panic 诊断使用)
///
/// 不获取打印锁, 不阻塞; 输出可能与其他上下文交错。
pub fn write_fmt_raw(args: core::fmt::Arguments<'_>) {
    let mut uart = ConsoleUart::take();
    let _ = core::fmt::write(&mut uart, args);
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
