//! 全局堆分配器 (固件适配层: TLSF 状态机 + 关中断临界区)
//!
//! 实现 [`GlobalAlloc`], 为 `Vec`/`Box`/`String` 等提供动态内存。
//! TLSF 两级隔离状态机位于 [`crate::heap_tlsf`] (主机可测, 随机压力
//! 测试覆盖随机 churn/线程栈模式/对齐往返/合并), 本模块只负责:
//!
//! - 链接脚本堆边界 ([`heap_bounds`], 防御性对齐);
//! - 关中断临界区串行化 (线程/ISR 竞争);
//! - 诊断查询 ([`capacity`]/[`used`]/[`largest_free_block`])。
//!
//! 分配/释放 O(1), 与空闲块总数无关 —— 碎片化堆上最坏关中断时间
//! 依然有界 (硬实时要求)。

#![allow(unsafe_op_in_unsafe_fn)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

use crate::heap_tlsf::Tlsf;

/// 全局 TLSF 状态机 (可变访问仅发生在关中断临界区内)
struct TlsfCell(UnsafeCell<Tlsf>);

// 与内核 KCell 同理: 访问全部经临界区串行化
unsafe impl Sync for TlsfCell {}

// `Tlsf::new()` 为 const fn, 静态初始化即可 (无需中间 const, 避免
// 内部可变对象的 const 副本语义)。
static TLSF: TlsfCell = TlsfCell(UnsafeCell::new(Tlsf::new()));

/// 全局堆分配器。
pub struct HeapAllocator;

unsafe impl GlobalAlloc for HeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        crate::critical_section::with(|_| {
            let tlsf = unsafe { &mut *TLSF.0.get() };
            let (start, end) = heap_bounds();
            if !tlsf.initialized() {
                unsafe { tlsf.init(start, end) };
            }
            unsafe { tlsf.alloc(layout) }
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if !ptr.is_null() {
            crate::critical_section::with(|_| {
                let tlsf = unsafe { &mut *TLSF.0.get() };
                unsafe { tlsf.dealloc(ptr) };
            });
        }
    }
}

// ============================== 堆边界 ==============================

unsafe extern "C" {
    static _heap_start: u8;
    static _heap_end: u8;
}

/// 对链接脚本边界做防御性对齐。`_heap_end` 前方是主栈 canary, 不能
/// 向上扩展; 最多舍弃首尾各 7 字节。
#[inline]
fn heap_bounds() -> (usize, usize) {
    let raw_start = core::ptr::addr_of!(_heap_start) as usize;
    let raw_end = core::ptr::addr_of!(_heap_end) as usize;
    let end = raw_end & !7;
    let start = crate::heap_layout::checked_align_up(raw_start, 8)
        .unwrap_or(end)
        .min(end);
    (start, end)
}

/// 堆可用容量 (字节)。
pub fn capacity() -> usize {
    let (start, end) = heap_bounds();
    end.saturating_sub(start)
}

/// 已占用堆空间 (包含已分配块的内部元数据/对齐填充)。
///
/// 空闲总量经状态机 O(1) 跟踪, 无需遍历空闲链表。
pub fn used() -> usize {
    crate::critical_section::with(|_| unsafe { (*TLSF.0.get()).used() })
}

/// 最大连续空闲块 (字节)。
///
/// 只检查最高非空尺寸级 (该级最小尺寸即其他所有级的块都不超过的量级;
/// 最大块必在该级内)。该遍历只出现在 soak 1Hz 监控路径, 不在分配
/// 热路径上。
#[cfg(shell_soak)]
pub fn largest_free_block() -> usize {
    crate::critical_section::with(|_| unsafe { (*TLSF.0.get()).largest_free_block() })
}
