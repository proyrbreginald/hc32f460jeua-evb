//! 原子回调槽: ISR → 应用层无锁通知原语
//!
//! 各外设驱动 (uart 接收通知 / dma 传输完成·错误通知) 共用同一个
//! "原子安装一个无参回调、ISR 侧调用"的槽位模型, 本模块是其唯一实现。
//!
//! - **无锁 SPSC**: 安装 (store) 与调用 (call) 只靠原子序 (Release/Acquire),
//!   无临界区 —— ISR 中调用回调不会与线程安装互锁;
//! - **一次一槽**: 后安装者原子替换前一个回调 (如更换接收通知目标);
//! - 回调类型为无参 `fn()` (静态函数指针): 参数经闭包捕获或
//!   const 泛型编码在类型中 (如 [`crate::uart::rx_irq_handler::<U>`]),
//!   无需堆分配、无生命周期问题。
//!
//! 主机可测: 纯 `core` 原子操作, 不依赖硬件, 已加入 `lib.rs` 供 `cargo test`。
#![allow(dead_code)] // 部分 API (clear/is_installed) 供未来驱动选用

use core::sync::atomic::{AtomicUsize, Ordering};

/// 无参回调 (中断上下文执行, 必须有界、无阻塞且不得打印)
pub type Callback = fn();

// ABI 契约: 本槽位把函数指针按 `usize` 存储。Cortex-M (ARMv7-M) 使用
// 统一地址空间, 函数指针与数据指针同宽, `fn → usize` 为保留位模式
// 的显式转换; 反向经 `transmute` 仅在本断言保证同宽时才成立。若未来
// 移植到指针分宽的目标 (如哈佛结构的函数/数据指针不同宽), 此断言将
// 在编译期失败, 提示改用工整的标签表方案。
const _: () = assert!(core::mem::size_of::<Callback>() == core::mem::size_of::<usize>());

/// 原子回调槽 (0 = 未安装; 原子类型不可 Copy, 静态数组用
/// [`NotifySlot::new`] 逐项构造)
pub struct NotifySlot(AtomicUsize);

impl NotifySlot {
    /// 空槽 (未安装回调)
    pub const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    /// 安装回调 (原子替换; 传 [`None`] 语义的 0 由 [`Self::clear`] 提供)
    pub fn store(&self, cb: Callback) {
        self.0.store(cb as usize, Ordering::Release);
    }

    /// 卸载回调 (槽位恢复空)
    pub fn clear(&self) {
        self.0.store(0, Ordering::Release);
    }

    /// 调用已安装的回调 (未安装则为无操作)。
    ///
    /// 供 ISR 侧调用; 槽位只接受 [`Callback`], 存储值 0 恒为未安装。
    /// 宽度契约见模块级 const 断言。
    pub fn call(&self) {
        let value = self.0.load(Ordering::Acquire);
        if value != 0 {
            let cb = unsafe { core::mem::transmute::<usize, Callback>(value) };
            cb();
        }
    }

    /// 是否已安装回调
    pub fn is_installed(&self) -> bool {
        self.0.load(Ordering::Acquire) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static CALLED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

    fn probe() {
        CALLED.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn empty_slot_call_is_noop() {
        CALLED.store(0, Ordering::Relaxed);
        let slot = NotifySlot::new();
        slot.call();
        assert_eq!(CALLED.load(Ordering::Relaxed), 0);
        assert!(!slot.is_installed());
    }

    #[test]
    fn store_then_call_invokes() {
        CALLED.store(0, Ordering::Relaxed);
        let slot = NotifySlot::new();
        slot.store(probe);
        assert!(slot.is_installed());
        slot.call();
        assert_eq!(CALLED.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reinstall_replaces_previous() {
        CALLED.store(0, Ordering::Relaxed);
        fn first() {
            CALLED.store(10, Ordering::Relaxed);
        }
        let slot = NotifySlot::new();
        slot.store(first);
        slot.store(probe);
        slot.call();
        assert_eq!(CALLED.load(Ordering::Relaxed), 1); // 只有新回调生效
    }

    #[test]
    fn clear_removes_callback() {
        CALLED.store(0, Ordering::Relaxed);
        let slot = NotifySlot::new();
        slot.store(probe);
        slot.clear();
        slot.call();
        assert_eq!(CALLED.load(Ordering::Relaxed), 0);
        assert!(!slot.is_installed());
    }
}
