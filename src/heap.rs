//! 堆内存分配器 (边界标记 + 首次适配)
//!
//! 实现 `GlobalAlloc`, 启用 Rust 标准堆数据结构 (Vec/Box/String 等)。
//!
//! # 设计
//!
//! - **块格式**: `[header 8B | payload | footer 8B]`, 每块 16 字节元数据;
//!   header 记录总大小与使用标志, footer 记录大小 —— 释放时可通过
//!   footer O(1) 定位前块并合并;
//! - **空闲链表**: 空闲块 payload 内嵌 next 指针, 首次适配 (first-fit);
//! - **合并**: 释放时向前/向后合并相邻空闲块, 防碎片;
//! - **对齐**: 8 字节 (ARM EABI); 最小块 24 字节;
//! - **中断安全**: 分配/释放全程临界区 ([`crate::critical_section`]),
//!   中断中可安全调用;
//! - **惰性初始化**: 首次分配时把整个堆初始化为一个空闲块,
//!   不依赖启动代码;
//! - **堆边界**: 由 link.ld 的 `.heap` 段符号定义 (bss 之后, 栈预留
//!   8KB 之前)。
//!
//! # 限制
//!
//! - 仅支持对齐 ≤ 8 字节的 [`Layout`] (更大对齐返回 null, 应用会 panic);
//! - 双重复用同一指针 (double-free) 与释放非法指针属于未定义行为;
//! - 堆容量 = RAM 188KB - bss - 栈预留 8KB ≈ 180KB。
//!
//! `unsafe fn` 整体即 unsafe 契约 (不变量由调用方承担), 内部不再逐段
//! 包裹 unsafe 块, 因此允许 `unsafe_op_in_unsafe_fn`。
#![allow(unsafe_op_in_unsafe_fn)]

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// 块头 (8 字节): 总大小 (含 header+footer) + 使用标志
#[repr(C)]
struct Block {
    size: usize,
    used: usize, // 0=空闲, 1=已分配
}

/// 块头大小 (payload 起始偏移)
const HEADER: usize = core::mem::size_of::<Block>();
/// footer 大小 (块尾的 size 字段)
const FOOTER: usize = core::mem::size_of::<usize>();
/// 每块元数据开销: header + footer
const OVERHEAD: usize = HEADER + FOOTER;
/// 最小块: 空闲块需容纳 next 指针
const MIN_BLOCK: usize = OVERHEAD + core::mem::size_of::<usize>();
/// 对齐 (ARM EABI)
const ALIGN: usize = 8;
/// 空闲链表结束标记 (地址 0 不在堆内)
const NULL_BLOCK: usize = 0;

/// 空闲链表头
static FREE_HEAD: AtomicUsize = AtomicUsize::new(NULL_BLOCK);
/// 惰性初始化标志 (堆整体已切分为空闲块)
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// 全局堆分配器 (注册为 `#[global_allocator]`)
pub struct HeapAllocator;

unsafe impl GlobalAlloc for HeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() > ALIGN {
            return core::ptr::null_mut();
        }
        crate::critical_section::with(|| unsafe { alloc_inner(layout.size()) })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if !ptr.is_null() {
            crate::critical_section::with(|| unsafe { dealloc_inner(ptr) });
        }
    }
}

// ---- 地址转换与块操作原语 (均为 8 字节对齐地址, 对齐访问安全) ----

/// 堆边界 (link.ld `.heap` 段符号)
#[inline]
fn heap_bounds() -> (usize, usize) {
    let start = core::ptr::addr_of!(_heap_start) as usize;
    let end = core::ptr::addr_of!(_heap_end) as usize;
    (start, end)
}

/// 堆容量 (字节, 诊断/启动横幅用)
pub fn capacity() -> usize {
    let (start, end) = heap_bounds();
    end.saturating_sub(start)
}

/// payload 指针 → 块头地址
#[inline]
fn block_of(payload: *mut u8) -> usize {
    payload as usize - HEADER
}

/// 块头地址 → payload 指针
#[inline]
fn payload_of(block: usize) -> *mut u8 {
    (block + HEADER) as *mut u8
}

/// 块总大小 (含 header+footer)
#[inline]
fn block_size(block: usize) -> usize {
    unsafe { (*(block as *const Block)).size }
}

/// 空闲块的 next 指针 (payload 开头)
#[inline]
fn next_ptr(block: usize) -> usize {
    unsafe { core::ptr::read_volatile((block + HEADER) as *const usize) }
}

#[inline]
unsafe fn set_next(block: usize, next: usize) {
    unsafe {
        core::ptr::write_volatile((block + HEADER) as *mut usize, next);
    }
}

/// 读本块前的 footer (定位前块)
#[inline]
fn read_footer(block: usize) -> usize {
    unsafe { core::ptr::read_volatile((block - FOOTER) as *const usize) }
}

#[inline]
unsafe fn write_footer(block: usize, size: usize) {
    unsafe {
        core::ptr::write_volatile((block + size - FOOTER) as *mut usize, size);
    }
}

/// 惰性初始化: 把整个堆切分为一个空闲块
unsafe fn init_heap(heap_start: usize, heap_end: usize) {
    if !INITIALIZED.load(Ordering::Relaxed) {
        let total = heap_end - heap_start;
        if total >= MIN_BLOCK {
            unsafe {
                (*(heap_start as *mut Block)) = Block {
                    size: total,
                    used: 0,
                };
                write_footer(heap_start, total);
                set_next(heap_start, NULL_BLOCK);
            }
            FREE_HEAD.store(heap_start, Ordering::Relaxed);
        }
        INITIALIZED.store(true, Ordering::Relaxed);
    }
}

/// 分裂块: 前 `need` 字节留作分配, 剩余成为新空闲块 (继承原 next)
unsafe fn split_block(block: usize, need: usize) -> usize {
    let new_block = block + need;
    let new_size = block_size(block) - need;
    unsafe {
        (*(new_block as *mut Block)) = Block {
            size: new_size,
            used: 0,
        };
        write_footer(new_block, new_size);
        set_next(new_block, next_ptr(block));
        (*(block as *mut Block)).size = need;
    }
    new_block
}

/// 首次适配分配 (调用方必须在临界区内)
unsafe fn alloc_inner(size: usize) -> *mut u8 {
    let (heap_start, heap_end) = heap_bounds();
    if heap_end <= heap_start {
        return core::ptr::null_mut();
    }
    init_heap(heap_start, heap_end);

    // 需求总大小 (8 字节对齐 + 元数据)
    let need = ((size + ALIGN - 1) & !(ALIGN - 1)) + OVERHEAD;

    let mut prev: usize = NULL_BLOCK;
    let mut cur = FREE_HEAD.load(Ordering::Relaxed);
    while cur != NULL_BLOCK {
        if block_size(cur) >= need {
            if block_size(cur) - need >= MIN_BLOCK {
                // 分裂: 前段分配, 后段入链表
                let remainder = split_block(cur, need);
                if prev == NULL_BLOCK {
                    FREE_HEAD.store(remainder, Ordering::Relaxed);
                } else {
                    set_next(prev, remainder);
                }
            } else {
                // 整块分配: 从空闲链表移除
                let next = next_ptr(cur);
                if prev == NULL_BLOCK {
                    FREE_HEAD.store(next, Ordering::Relaxed);
                } else {
                    set_next(prev, next);
                }
            }
            // 标记已分配
            unsafe {
                (*(cur as *mut Block)).used = 1;
                write_footer(cur, block_size(cur));
            }
            return payload_of(cur);
        }
        prev = cur;
        cur = next_ptr(cur);
    }
    core::ptr::null_mut() // 堆耗尽
}

/// 释放并合并相邻空闲块 (调用方必须在临界区内)
unsafe fn dealloc_inner(payload: *mut u8) {
    let (heap_start, heap_end) = heap_bounds();

    let mut block = block_of(payload);
    let mut total_size = block_size(block);

    // 合并前块: 通过本块前的 footer 定位
    if block > heap_start {
        let prev_size = read_footer(block);
        let prev = block - prev_size;
        // 合法性: 前块起始在堆内, 大小与 header 一致, 且为空闲
        if prev >= heap_start
            && prev_size >= MIN_BLOCK
            && prev_size == block_size(prev)
            && unsafe { (*(prev as *const Block)).used == 0 }
        {
            remove_from_free_list(prev);
            total_size += prev_size;
            block = prev;
        }
    }

    // 标记本块空闲
    unsafe {
        (*(block as *mut Block)).used = 0;
    }

    // 合并后块: 通过 size 定位
    let next = block + total_size;
    if next + MIN_BLOCK <= heap_end && unsafe { (*(next as *const Block)).used == 0 } {
        let next_size = block_size(next);
        remove_from_free_list(next);
        total_size += next_size;
    }

    // 写回大小/footer 并插入空闲链表头
    unsafe {
        (*(block as *mut Block)).size = total_size;
        write_footer(block, total_size);
        set_next(block, FREE_HEAD.load(Ordering::Relaxed));
    }
    FREE_HEAD.store(block, Ordering::Relaxed);
}

/// 从空闲链表移除指定块 (调用方必须在临界区内)
unsafe fn remove_from_free_list(target: usize) {
    let mut prev: usize = NULL_BLOCK;
    let mut cur = FREE_HEAD.load(Ordering::Relaxed);
    while cur != NULL_BLOCK {
        if cur == target {
            let next = next_ptr(cur);
            if prev == NULL_BLOCK {
                FREE_HEAD.store(next, Ordering::Relaxed);
            } else {
                set_next(prev, next);
            }
            return;
        }
        prev = cur;
        cur = next_ptr(cur);
    }
}

// 堆区边界 (link.ld `.heap` 段)
unsafe extern "C" {
    static _heap_start: u8;
    static _heap_end: u8;
}
