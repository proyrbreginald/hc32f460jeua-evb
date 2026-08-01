//! 控制台输出: 将任意 UART 绑定到 `print!` / `println!`
//!
//! 绑定在**编译期**完成: 修改 [`ConsoleUart`] 类型别名即可切换输出串口,
//! 零运行时开销 (`Uart<U>` 是零大小类型)。
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
//! # 并发注意
//!
//! 发送为轮询忙等且**不加锁**: 若中断与主循环同时调用打印宏,
//! 字节可能交错。请保证任意时刻只有一个上下文调用打印宏。
//!
//! # 前置条件
//!
//! 绑定的 UART 必须已通过 [`crate::uart::Uart::init`] 初始化,
//! 否则输出被硬件忽略 (不会死锁: SR.TXE 复位即为 1)。

use crate::uart::Uart1;

/// 控制台输出串口 (编译期绑定, 切换此处即可换串口)
pub type ConsoleUart = Uart1;

/// 向控制台输出格式化内容 (由 `print!`/`println!` 宏调用)
///
/// 忽略格式化错误: UART 发送为轮询忙等, 不会返回错误。
pub fn write_fmt(args: core::fmt::Arguments<'_>) {
    let mut uart = ConsoleUart::take();
    let _ = core::fmt::write(&mut uart, args);
}

/// 输出格式化内容, 不换行
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::write_fmt(core::format_args!($($arg)*))
    };
}

/// 输出格式化内容并换行 (CRLF)
#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\r\n")
    };
    ($($arg:tt)*) => {
        $crate::print!($($arg)*);
        $crate::print!("\r\n")
    };
}
