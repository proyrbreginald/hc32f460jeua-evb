//! 堆内存分配器 (边界标记 + 首次适配)
//!
//! 实现 [`GlobalAlloc`], 为 `Vec`/`Box`/`String` 等提供动态内存。
//!
//! # 块布局
//!
//! ```text
//! [Block header | padding | owner prefix | aligned payload | padding | footer]
//! ```
//!
//! 块头和块尾记录总长度，空闲块的 header 后内嵌 next 指针。已分配
//! payload 前的 owner prefix 保存原始块地址，使任意 2 的幂对齐都可在
//! 释放时 O(1) 找回块头。每个块的总长度始终按 8 字节对齐，因此相邻块
//! 不会逐次偏移。所有来自 [`Layout`] 的尺寸运算均使用 checked arithmetic。
//!
//! 分配与释放全程关闭中断，适用于单核线程/ISR 竞争；非法指针释放、
//! double-free 与越界写仍属于 [`GlobalAlloc`] 调用方违反契约。
#![allow(unsafe_op_in_unsafe_fn)]

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::heap_layout::{allocation_plan, checked_align_up};

/// 块头：总大小 (含全部元数据) + 使用标志。
#[repr(C)]
struct Block {
    size: usize,
    used: usize,
}

const HEADER: usize = core::mem::size_of::<Block>();
const FOOTER: usize = core::mem::size_of::<usize>();
const PREFIX: usize = core::mem::size_of::<usize>();
const OVERHEAD: usize = HEADER + FOOTER;
const BLOCK_ALIGN: usize = 8;
const MIN_BLOCK: usize =
    (OVERHEAD + core::mem::size_of::<usize>() + BLOCK_ALIGN - 1) & !(BLOCK_ALIGN - 1);
const NULL_BLOCK: usize = 0;

const _: () = assert!(BLOCK_ALIGN.is_power_of_two());
const _: () = assert!(BLOCK_ALIGN >= core::mem::align_of::<Block>());
const _: () = assert!(BLOCK_ALIGN >= core::mem::align_of::<usize>());

static FREE_HEAD: AtomicUsize = AtomicUsize::new(NULL_BLOCK);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// 全局堆分配器。
pub struct HeapAllocator;

unsafe impl GlobalAlloc for HeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        crate::critical_section::with(|_| unsafe { alloc_inner(layout) })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if !ptr.is_null() {
            crate::critical_section::with(|_| unsafe { dealloc_inner(ptr) });
        }
    }
}

/// 对链接脚本边界做防御性对齐。`_heap_end` 前方是主栈 canary，不能
/// 向上扩展；最多舍弃首尾各 7 字节。
#[inline]
fn heap_bounds() -> (usize, usize) {
    let raw_start = core::ptr::addr_of!(_heap_start) as usize;
    let raw_end = core::ptr::addr_of!(_heap_end) as usize;
    let end = raw_end & !(BLOCK_ALIGN - 1);
    let start = checked_align_up(raw_start, BLOCK_ALIGN)
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
pub fn used() -> usize {
    crate::critical_section::with(|_| {
        let (start, end) = heap_bounds();
        if !INITIALIZED.load(Ordering::Relaxed) {
            return 0;
        }
        let mut free = 0usize;
        let mut cur = FREE_HEAD.load(Ordering::Relaxed);
        while cur != NULL_BLOCK {
            free = free.saturating_add(block_size(cur));
            cur = next_ptr(cur);
        }
        end.saturating_sub(start).saturating_sub(free)
    })
}

/// 最大连续空闲块 (字节)。
///
/// 碎片化最直接的度量: 用量相同, 碎片越严重最大连续空闲块越小,
/// 越难满足大块分配。压力测试周期采样可证明长期运行不会因碎片
/// 耗尽可用大块。
pub fn largest_free_block() -> usize {
    crate::critical_section::with(|_| {
        if !INITIALIZED.load(Ordering::Relaxed) {
            return 0;
        }
        let mut largest = 0usize;
        let mut cur = FREE_HEAD.load(Ordering::Relaxed);
        while cur != NULL_BLOCK {
            largest = largest.max(block_size(cur));
            cur = next_ptr(cur);
        }
        largest
    })
}

#[inline]
fn block_of(payload: *mut u8) -> usize {
    unsafe { core::ptr::read((payload as usize - PREFIX) as *const usize) }
}

#[inline]
unsafe fn write_prefix(payload: usize, block: usize) {
    unsafe { core::ptr::write((payload - PREFIX) as *mut usize, block) };
}

#[inline]
fn block_size(block: usize) -> usize {
    unsafe { (*(block as *const Block)).size }
}

#[inline]
fn next_ptr(block: usize) -> usize {
    unsafe { core::ptr::read_volatile((block + HEADER) as *const usize) }
}

#[inline]
unsafe fn set_next(block: usize, next: usize) {
    unsafe { core::ptr::write_volatile((block + HEADER) as *mut usize, next) };
}

#[inline]
fn read_footer(block: usize) -> usize {
    unsafe { core::ptr::read_volatile((block - FOOTER) as *const usize) }
}

#[inline]
unsafe fn write_footer(block: usize, size: usize) {
    unsafe { core::ptr::write_volatile((block + size - FOOTER) as *mut usize, size) };
}

unsafe fn init_heap(heap_start: usize, heap_end: usize) {
    if INITIALIZED.load(Ordering::Relaxed) {
        return;
    }

    let total = heap_end.saturating_sub(heap_start);
    if total >= MIN_BLOCK {
        unsafe {
            (heap_start as *mut Block).write(Block {
                size: total,
                used: 0,
            });
            write_footer(heap_start, total);
            set_next(heap_start, NULL_BLOCK);
        }
        FREE_HEAD.store(heap_start, Ordering::Relaxed);
    }
    INITIALIZED.store(true, Ordering::Relaxed);
}

/// 前 `need` 字节用于当前分配，后段继承原空闲链表位置。
unsafe fn split_block(block: usize, need: usize) -> usize {
    let new_block = block + need;
    let new_size = block_size(block) - need;
    unsafe {
        (new_block as *mut Block).write(Block {
            size: new_size,
            used: 0,
        });
        write_footer(new_block, new_size);
        set_next(new_block, next_ptr(block));
        (*(block as *mut Block)).size = need;
    }
    new_block
}

unsafe fn alloc_inner(layout: Layout) -> *mut u8 {
    let (heap_start, heap_end) = heap_bounds();
    if heap_end <= heap_start {
        return core::ptr::null_mut();
    }
    unsafe { init_heap(heap_start, heap_end) };

    let mut prev = NULL_BLOCK;
    let mut cur = FREE_HEAD.load(Ordering::Relaxed);
    while cur != NULL_BLOCK {
        let available = block_size(cur);
        let Some(plan) =
            allocation_plan(cur, available, layout, HEADER, PREFIX, FOOTER, BLOCK_ALIGN)
        else {
            prev = cur;
            cur = next_ptr(cur);
            continue;
        };

        if available - plan.block_size >= MIN_BLOCK {
            let remainder = unsafe { split_block(cur, plan.block_size) };
            if prev == NULL_BLOCK {
                FREE_HEAD.store(remainder, Ordering::Relaxed);
            } else {
                unsafe { set_next(prev, remainder) };
            }
        } else {
            let next = next_ptr(cur);
            if prev == NULL_BLOCK {
                FREE_HEAD.store(next, Ordering::Relaxed);
            } else {
                unsafe { set_next(prev, next) };
            }
        }

        unsafe {
            (*(cur as *mut Block)).used = 1;
            write_footer(cur, block_size(cur));
            write_prefix(plan.payload, cur);
        }
        return plan.payload as *mut u8;
    }
    core::ptr::null_mut()
}

unsafe fn dealloc_inner(payload: *mut u8) {
    let (heap_start, heap_end) = heap_bounds();
    let mut block = block_of(payload);
    let mut total_size = block_size(block);

    if block > heap_start {
        let prev_size = read_footer(block);
        if prev_size >= MIN_BLOCK
            && prev_size.is_multiple_of(BLOCK_ALIGN)
            && let Some(prev) = block.checked_sub(prev_size)
            && prev >= heap_start
            && prev_size == block_size(prev)
            && unsafe { (*(prev as *const Block)).used == 0 }
        {
            unsafe { remove_from_free_list(prev) };
            total_size += prev_size;
            block = prev;
        }
    }

    unsafe { (*(block as *mut Block)).used = 0 };

    if let Some(next) = block.checked_add(total_size)
        && let Some(next_min_end) = next.checked_add(MIN_BLOCK)
        && next_min_end <= heap_end
        && unsafe { (*(next as *const Block)).used == 0 }
    {
        let next_size = block_size(next);
        unsafe { remove_from_free_list(next) };
        total_size += next_size;
    }

    unsafe {
        (*(block as *mut Block)).size = total_size;
        write_footer(block, total_size);
        set_next(block, FREE_HEAD.load(Ordering::Relaxed));
    }
    FREE_HEAD.store(block, Ordering::Relaxed);
}

unsafe fn remove_from_free_list(target: usize) {
    let mut prev = NULL_BLOCK;
    let mut cur = FREE_HEAD.load(Ordering::Relaxed);
    while cur != NULL_BLOCK {
        if cur == target {
            let next = next_ptr(cur);
            if prev == NULL_BLOCK {
                FREE_HEAD.store(next, Ordering::Relaxed);
            } else {
                unsafe { set_next(prev, next) };
            }
            return;
        }
        prev = cur;
        cur = next_ptr(cur);
    }
}

unsafe extern "C" {
    static _heap_start: u8;
    static _heap_end: u8;
}
