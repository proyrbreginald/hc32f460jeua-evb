//! UART 的 RTOS 阻塞接收适配层。
//!
//! [`crate::uart`] 只负责寄存器、中断、SPSC 接收环和非阻塞 API。本模块
//! 把底层的 OS 无关接收通知转换为 RTOS 信号量, 避免 UART HAL 反向依赖
//! 内核。

use crate::rtos::{Semaphore, Timeout};
use crate::uart::Uart;

/// 每个 USART 的数据就绪事件。
///
/// 容量为 1, 语义是“接收环可能非空”而不是逐字节计数。底层环形缓冲才是
/// 数据真值; 合并重复通知既不会溢出计数, 也不会改变已缓冲数据的数量。
static RX_READY: [Semaphore; 4] = [
    Semaphore::new(0, 1),
    Semaphore::new(0, 1),
    Semaphore::new(0, 1),
    Semaphore::new(0, 1),
];

/// ISR 通知入口: `Semaphore::release` 不等待资源, 只置位事件或唤醒等待者。
fn rx_notify<const U: u8>() {
    RX_READY[U as usize - 1].release();
}

/// UART 的 RTOS 阻塞读取扩展。
pub(crate) trait UartRtosExt {
    /// 阻塞等待并读取一个已由 UART ISR 缓冲的字节。
    ///
    /// 仅可在线程上下文调用, 且同一 USART 接收环只允许一个消费者。
    fn read_rx_blocking(&self) -> u8;

    /// 在限定时间内读取一个字节；超时返回 `None`。
    fn read_rx_timeout_ms(&self, timeout_ms: u32) -> Option<u8>;
}

impl<const U: u8> UartRtosExt for Uart<U> {
    fn read_rx_blocking(&self) -> u8 {
        // 先安装通知, 再检查接收环。若字节在安装前已经到达, 首次检查
        // 会直接读出; 若在检查与 take 之间到达, 信号量会记住该事件。
        self.set_rx_notify(rx_notify::<U>);

        loop {
            if let Some(byte) = self.read_rx() {
                return byte;
            }
            let _ = RX_READY[U as usize - 1].take(Timeout::Forever);
        }
    }

    fn read_rx_timeout_ms(&self, timeout_ms: u32) -> Option<u8> {
        self.set_rx_notify(rx_notify::<U>);
        let timeout = crate::rtos::ticks_from_ms(timeout_ms);
        let started = crate::rtos::tick();

        loop {
            if let Some(byte) = self.read_rx() {
                return Some(byte);
            }
            let elapsed = crate::rtos::tick().wrapping_sub(started);
            if elapsed >= timeout {
                return None;
            }
            if RX_READY[U as usize - 1]
                .take(Timeout::Ticks(timeout - elapsed))
                .is_err()
            {
                return self.read_rx();
            }
        }
    }
}
