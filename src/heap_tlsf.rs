//! TLSF 两级隔离空闲链表状态机 (纯逻辑, 主机可测)
//!
//! 本模块与硬件无关: 状态机按注入的内存区 (arena) 工作, 固件侧由
//! [`crate::heap`] 在关中断临界区内调用, 主机侧以同样的代码路径做
//! 随机压力测试。所有方法为 `&mut self`: 互斥由调用方负责。
//!
//! # 块布局
//!
//! ```text
//! [Header: size_flags | prev_phys] [free: next/prev 链指针] [padding | owner prefix | payload]
//! ```
//!
//! - `size_flags`: bit0 = 已使用, bit1 = 前块已使用, 其余为总长度
//!   (8 字节对齐, 低 3 位空闲);
//! - `prev_phys`: 前一物理块的长度 (O(1) 找到前块合并);
//! - 空闲块的 next/prev 链指针内嵌于块头后 (+8/+12), 已分配块该区域
//!   直接归 payload 使用;
//! - owner prefix 保存原始块地址, 使任意 2 的幂对齐都可在释放时 O(1)
//!   找回块头 (布局规划见 [`crate::heap_layout::allocation_plan`]);
//!
//! # 尺寸级映射
//!
//! 位图在常数步内定位"装得下需求的最小尺寸级"并取该级链首 —— 与空闲
//! 块总数无关, 分配/释放 O(1), 关中断时间有界 (硬实时要求)。映射纯
//! 函数与边界不变量测试见 [`crate::heap_layout`]。

#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]
#[cfg(test)]
extern crate std;

use core::alloc::Layout;
use core::ptr;

use crate::heap_layout::{allocation_plan, checked_align_up, tlsf_insert_index, tlsf_search_index};

/// 块头: `size_flags` + 前块长度。总 8 字节。
#[repr(C)]
struct Block {
    /// bit0 = 已使用, bit1 = 前块已使用, 其余 = 总长度 (8 字节对齐)
    size_flags: usize,
    /// 前一物理块长度 (0 = 无前块/堆起始)
    prev_phys: usize,
}

/// 块头字节数 (随 usize 宽度: 目标 8B, 64 位主机 16B —— 全部块内偏移
/// 必须以此为基准计算, 不得硬编码)
const HEADER: usize = core::mem::size_of::<Block>();
/// owner prefix 字节数 (payload 前保存所属块地址, 释放时 O(1) 找回块头)
const PREFIX: usize = core::mem::size_of::<usize>();
/// prefix 相对 payload 的偏移: payload 恒 ≥ 8 对齐, `payload - PREFIX`
/// 对 usize 恒满足指针对齐 (目标 4 对齐, 主机 8 对齐), 且恒落在
/// 规划保留区 [块头后, payload) 内 (payload_min = HEADER + PREFIX)。
const PREFIX_OFFSET: usize = PREFIX;
/// 块地址对齐 (块长度恒为其倍数)
const BLOCK_ALIGN: usize = 8;
/// 最小块: 至少容纳头部 + 空闲链双指针 (next 于 HEADER, prev 于
/// HEADER+PREFIX, 均按指针对齐), 且不小于最小 payload 规划长度。
/// 按实际宽度计算 —— 旧版硬编码偏移在目标上把 MIN_BLOCK 算小
/// (prev 链接写穿到下一块头), 是堆损坏的根因。
const MIN_BLOCK: usize = {
    let links = HEADER + 2 * core::mem::size_of::<usize>();
    let payload = HEADER + PREFIX + 1;
    let minimum = if links > payload { links } else { payload };
    (minimum + BLOCK_ALIGN - 1) & !(BLOCK_ALIGN - 1)
};

/// size_flags 位
const USED_BIT: usize = 1 << 0;
const PREV_USED_BIT: usize = 1 << 1;
const FLAGS_MASK: usize = USED_BIT | PREV_USED_BIT;

/// TLSF 第二级数 (每级内 4 个子类, 每子类覆盖 1/4 尺寸区间)
const SL_COUNT: usize = 4;
/// 最小块的对数 (MIN_BLOCK=24 → 2^4 < 24 ≤ 2^5; 用 4 使 24B 落在 [16,32) 类)
const FL_MIN_LOG2: usize = 4;
/// 第一级数: 覆盖 log2 区间 [4, 18) —— 堆上限 188KB < 2^18
const FL_COUNT: usize = 18 - FL_MIN_LOG2;

const _: () = assert!(BLOCK_ALIGN.is_power_of_two());
const _: () = assert!(BLOCK_ALIGN >= core::mem::align_of::<Block>());
const _: () = assert!(BLOCK_ALIGN >= core::mem::align_of::<usize>());
const _: () = assert!(MIN_BLOCK >= HEADER + 2 * core::mem::size_of::<usize>());
const _: () = assert!(MIN_BLOCK.is_multiple_of(BLOCK_ALIGN));

/// 一个尺寸级的空闲链表头 (链首插入/任意摘除, 无需尾指针)
struct FreeList {
    next: *mut Block,
}

const EMPTY_LIST: FreeList = FreeList {
    next: ptr::null_mut(),
};

/// TLSF 状态机。
pub struct Tlsf {
    /// 两级空闲链表: `lists[fl * SL_COUNT + sl]`
    lists: [FreeList; FL_COUNT * SL_COUNT],
    /// 第一级位图: bit fl = 该级存在空闲块
    fl_bitmap: u32,
    /// 第二级位图: 每级 4 位
    sl_bitmaps: [u32; FL_COUNT],
    /// 空闲总字节 (含元数据; O(1) 用量统计)
    free_bytes: usize,
    /// 内存区边界 (init 时记录, 合并/分裂的边界判断用)
    arena_start: usize,
    arena_end: usize,
    initialized: bool,
}

impl Tlsf {
    pub const fn new() -> Self {
        Self {
            lists: [EMPTY_LIST; FL_COUNT * SL_COUNT],
            fl_bitmap: 0,
            sl_bitmaps: [0; FL_COUNT],
            free_bytes: 0,
            arena_start: 0,
            arena_end: 0,
            initialized: false,
        }
    }

    /// 是否已初始化
    pub const fn initialized(&self) -> bool {
        self.initialized
    }

    /// 用注入的内存区初始化 (首次分配时自动调用亦可)。
    ///
    /// `start`/`end` 须按 [`heap_layout`] 对齐到 8 字节; 区域不足
    /// MIN_BLOCK 时初始化为空堆 (所有分配失败)。
    pub unsafe fn init(&mut self, start: usize, end: usize) {
        if self.initialized {
            return;
        }
        self.arena_start = start;
        self.arena_end = end;
        let total = end.saturating_sub(start);
        if total >= MIN_BLOCK {
            let block = start as *mut Block;
            unsafe {
                (*block).size_flags = total | PREV_USED_BIT;
                (*block).prev_phys = 0;
                self.list_insert(block);
            }
            self.free_bytes = total;
        }
        self.initialized = true;
    }

    /// 分配 `layout`, 失败返回 null。
    pub unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        if !self.initialized {
            return core::ptr::null_mut();
        }
        if self.arena_end <= self.arena_start {
            return core::ptr::null_mut();
        }

        // 需求上界 → 尺寸级 → 链首块 (search_index 保证该级任意块 ≥ 上界)
        let Some(need) = minimum_need(layout) else {
            return core::ptr::null_mut();
        };
        let Some(block) = self.find_block(need.max(MIN_BLOCK)) else {
            return core::ptr::null_mut();
        };
        // 精确规划 (checked, 见 heap_layout): 必然成功且不超过块长
        let Some(plan) = allocation_plan(
            block as usize,
            unsafe { block_size(block) },
            layout,
            HEADER,
            PREFIX,
            0,
            BLOCK_ALIGN,
        ) else {
            return core::ptr::null_mut();
        };
        if unsafe { block_size(block) } < plan.block_size {
            return core::ptr::null_mut();
        }

        unsafe { self.list_remove(block) };
        let old_size = unsafe { block_size(block) };
        let prev_used = unsafe { (*block).size_flags & PREV_USED_BIT != 0 };
        let remainder = old_size - plan.block_size;

        if remainder >= MIN_BLOCK {
            // 分裂: 剩余部分成为新的空闲块
            let new_block = (block as usize + plan.block_size) as *mut Block;
            unsafe {
                (*new_block).size_flags = remainder | PREV_USED_BIT; // 前块(本块)已使用
                (*new_block).prev_phys = plan.block_size;
                set_flags(block, plan.block_size, true, prev_used);
                self.list_insert(new_block);
                // 新块后继块的 prev_phys 改指新块
                let after = (new_block as usize + remainder) as *mut Block;
                if (after as usize) < self.arena_end {
                    (*after).prev_phys = remainder;
                }
            }
            self.free_bytes = self.free_bytes.saturating_sub(plan.block_size);
        } else {
            unsafe { set_flags(block, old_size, true, prev_used) };
            // 后继块 (若存在) 的前块已使用标志
            let after = (block as usize + old_size) as *mut Block;
            if (after as usize) < self.arena_end {
                unsafe { (*after).size_flags |= PREV_USED_BIT };
            }
            self.free_bytes = self.free_bytes.saturating_sub(old_size);
        }

        // owner prefix: payload 前记录所属块地址 (8 对齐, 见 PREFIX_OFFSET)
        unsafe { core::ptr::write((plan.payload - PREFIX_OFFSET) as *mut usize, block as usize) };
        plan.payload as *mut u8
    }

    /// 释放 [`alloc`] 返回的 payload。
    pub unsafe fn dealloc(&mut self, payload: *mut u8) {
        let mut block =
            unsafe { core::ptr::read((payload as usize - PREFIX_OFFSET) as *const usize) }
                as *mut Block;
        let mut size = unsafe { block_size(block) };
        let mut prev_used = unsafe { (*block).size_flags & PREV_USED_BIT != 0 };

        // 与前块合并
        if !prev_used {
            let prev_size = unsafe { (*block).prev_phys };
            if prev_size >= MIN_BLOCK && prev_size.is_multiple_of(BLOCK_ALIGN) {
                let Some(prev_addr) = (block as usize).checked_sub(prev_size) else {
                    return;
                };
                let prev = prev_addr as *mut Block;
                if unsafe { !block_used(prev) } {
                    unsafe { self.list_remove(prev) };
                    size += prev_size;
                    // 合并后块头为 prev: 前块标志继承 prev 自身 (非原块)
                    prev_used = unsafe { (*prev).size_flags & PREV_USED_BIT != 0 };
                    block = prev;
                }
            }
        }

        // 与后块合并
        let Some(next_addr) = (block as usize).checked_add(size) else {
            return;
        };
        if next_addr < self.arena_end {
            let next = next_addr as *mut Block;
            if unsafe { !block_used(next) } {
                let next_size = unsafe { block_size(next) };
                unsafe { self.list_remove(next) };
                size += next_size;
                // 后继后继块的 prev_phys 更新
                if let Some(after_addr) = next_addr.checked_add(next_size) {
                    if after_addr < self.arena_end {
                        unsafe { (*(after_addr as *mut Block)).prev_phys = size };
                    }
                }
            } else {
                // 本块已空闲: 后继块的"前块已使用"标志清除, 且 prev_phys
                // 必须指向 (可能已与前块合并后的) 本块长度 —— 否则后继
                // 日后释放时按旧长度回退到块中间 (堆损坏/碎片化)
                unsafe {
                    (*next).size_flags &= !PREV_USED_BIT;
                    (*next).prev_phys = size;
                }
            }
        }

        // 写回合并结果 (保留块头原有的前块标志)
        unsafe { set_flags(block, size, false, prev_used) };
        unsafe { self.list_insert(block) };
        self.free_bytes = self.free_bytes.saturating_add(size);
    }

    /// 已占用字节 (含元数据; 未初始化时为 0)
    pub fn used(&self) -> usize {
        if !self.initialized {
            return 0;
        }
        self.arena_end
            .saturating_sub(self.arena_start)
            .saturating_sub(self.free_bytes)
    }

    /// 最大连续空闲块 (字节): 只检查最高非空尺寸级。
    ///
    /// 该遍历不在分配热路径上 (仅供诊断/压力测试周期采样)。
    pub fn largest_free_block(&self) -> usize {
        if !self.initialized {
            return 0;
        }
        let mut fl = FL_COUNT;
        while fl > 0 {
            fl -= 1;
            if self.fl_bitmap & (1 << fl) == 0 {
                continue;
            }
            let mut largest = 0usize;
            for sl in 0..SL_COUNT {
                let mut block = self.lists[list_index(fl, sl)].next;
                while !block.is_null() {
                    largest = largest.max(unsafe { block_size(block) });
                    block = unsafe { free_next(block) };
                }
            }
            return largest;
        }
        0
    }

    /// 调试: 按地址序 dump 全部空闲块 (测试定位碎片用)
    #[cfg(test)]
    pub fn debug_free_blocks(&self) -> std::vec::Vec<(usize, usize, usize)> {
        let mut blocks: std::vec::Vec<(usize, usize, usize)> = std::vec::Vec::new();
        for fl in 0..FL_COUNT {
            for sl in 0..SL_COUNT {
                let mut block = self.lists[list_index(fl, sl)].next;
                while !block.is_null() {
                    unsafe {
                        blocks.push((
                            block as usize,
                            block_size(block),
                            block_used(block) as usize,
                        ));
                        block = free_next(block);
                    }
                }
            }
        }
        blocks.sort_by_key(|(addr, _, _)| *addr);
        blocks
    }

    /// 定位"装得下 `size` 的最小尺寸级"并取其链首块
    ///
    /// 先在同级第二级位图中从 sl 起找, 再向上扫第一级位图 —— 与位图位数
    /// (FL_COUNT/SL_COUNT) 成正比, 与空闲块总数无关。
    fn find_block(&self, size: usize) -> Option<*mut Block> {
        let (mut fl, sl) = tlsf_search_index(size, FL_MIN_LOG2, FL_COUNT)?;
        let mut sl_map = self.sl_bitmaps[fl] & (!0u32 << sl);
        if sl_map == 0 {
            fl += 1;
            loop {
                if fl >= FL_COUNT {
                    return None;
                }
                if self.fl_bitmap & (1 << fl) != 0 {
                    break;
                }
                fl += 1;
            }
            sl_map = self.sl_bitmaps[fl];
            debug_assert!(sl_map != 0, "第一级位图与第二级位图不一致");
        }
        let sl = sl_map.trailing_zeros() as usize;
        Some(self.lists[list_index(fl, sl)].next)
    }

    /// 空闲块按尺寸级插入对应链表
    unsafe fn list_insert(&mut self, block: *mut Block) {
        let size = unsafe { block_size(block) };
        let (fl, sl) = tlsf_insert_index(size, FL_MIN_LOG2);
        let list = &mut self.lists[list_index(fl, sl)];
        unsafe {
            set_free_next(block, list.next);
            set_free_prev(block, core::ptr::null_mut());
            if !list.next.is_null() {
                set_free_prev(list.next, block);
            }
        }
        list.next = block;
        // 位图: 级可能由空转非空
        if self.sl_bitmaps[fl] & (1 << sl) == 0 {
            self.sl_bitmaps[fl] |= 1 << sl;
            self.fl_bitmap |= 1 << fl;
        }
    }

    /// 从所属链表摘除空闲块
    unsafe fn list_remove(&mut self, block: *mut Block) {
        let size = unsafe { block_size(block) };
        let (fl, sl) = tlsf_insert_index(size, FL_MIN_LOG2);
        let list = &mut self.lists[list_index(fl, sl)];
        let next = unsafe { free_next(block) };
        let prev = unsafe {
            core::ptr::read_volatile((block as usize + HEADER + PREFIX) as *const *mut Block)
        };
        if prev.is_null() {
            // 链首
            list.next = next;
        } else {
            unsafe { set_free_next(prev, next) };
        }
        if !next.is_null() {
            unsafe { set_free_prev(next, prev) };
        }
        unsafe {
            set_free_next(block, core::ptr::null_mut());
        }
        // 位图: 链表可能清空
        if list.next.is_null() {
            self.sl_bitmaps[fl] &= !(1 << sl);
            if self.sl_bitmaps[fl] == 0 {
                self.fl_bitmap &= !(1 << fl);
            }
        }
    }
}

#[inline]
fn list_index(fl: usize, sl: usize) -> usize {
    fl * SL_COUNT + sl
}

/// 一个布局所需块长的**全局上界** (checked)。
///
/// payload 相对块头的偏移 ≤ align + BLOCK_ALIGN (块 8 对齐, 最坏位相下
/// align_up 至多跨过一个对齐边界), 故需求上界 = 该偏移 + 尺寸, 按
/// 块对齐上取整。任何实际块上的精确规划值都不超过该上界, 因此
/// [`find_block`] 返回的块必然装得下。
fn minimum_need(layout: Layout) -> Option<usize> {
    let align = layout.align().max(BLOCK_ALIGN);
    let offset = align.checked_add(BLOCK_ALIGN)?;
    let end = offset.checked_add(layout.size().max(1))?;
    checked_align_up(end, BLOCK_ALIGN)
}

#[inline]
unsafe fn block_size(block: *mut Block) -> usize {
    #[cfg(test)]
    if block as usize & 7 != 0 {
        std::panic!(
            "misaligned block ptr {:#x} (from {:p})",
            block as usize,
            block
        );
    }
    unsafe { (*block).size_flags & !FLAGS_MASK }
}

#[inline]
unsafe fn block_used(block: *mut Block) -> bool {
    unsafe { (*block).size_flags & USED_BIT != 0 }
}

#[inline]
unsafe fn set_flags(block: *mut Block, size: usize, used: bool, prev_used: bool) {
    unsafe {
        (*block).size_flags =
            size | if used { USED_BIT } else { 0 } | if prev_used { PREV_USED_BIT } else { 0 };
    }
}

/// 空闲链指针内嵌于块头之后 (next 于 +8, prev 于 +16; 均按指针对齐)
#[inline]
unsafe fn free_next(block: *mut Block) -> *mut Block {
    unsafe { core::ptr::read_volatile((block as usize + HEADER) as *const *mut Block) }
}

#[inline]
unsafe fn set_free_next(block: *mut Block, next: *mut Block) {
    unsafe { core::ptr::write_volatile((block as usize + HEADER) as *mut *mut Block, next) };
}

#[inline]
unsafe fn set_free_prev(block: *mut Block, prev: *mut Block) {
    unsafe {
        core::ptr::write_volatile((block as usize + HEADER + PREFIX) as *mut *mut Block, prev)
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::prelude::v1::*;

    /// 64KB 测试内存区 (8 字节对齐; 每个测试独立分配, 并行互不干扰)
    type Arena = [u64; 8192];
    const ARENA_BYTES: usize = 64 * 1024;

    fn arena_bounds(arena: &Arena) -> (usize, usize) {
        let start = arena.as_ptr() as usize;
        let raw_end = start + ARENA_BYTES;
        let end = raw_end & !(BLOCK_ALIGN - 1);
        (start, end)
    }

    /// 简易 xorshift PRNG (确定性可复现)
    struct Rng(u32);
    impl Rng {
        fn next(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u32) -> u32 {
            self.next() % n
        }
    }

    #[test]
    fn alloc_free_reuses_memory_without_corruption() {
        let arena = std::boxed::Box::new([0u64; 8192]);
        let (start, end) = arena_bounds(&arena);
        let mut tlsf = Tlsf::new();
        unsafe { tlsf.init(start, end) };

        let layouts = [
            Layout::from_size_align(1, 1).unwrap(),
            Layout::from_size_align(7, 8).unwrap(),
            Layout::from_size_align(24, 8).unwrap(),
            Layout::from_size_align(512, 8).unwrap(),
            Layout::from_size_align(2048, 8).unwrap(),
            Layout::from_size_align(1056, 32).unwrap(), // 线程栈 (含守卫)
            Layout::from_size_align(544, 32).unwrap(),
            Layout::from_size_align(4096, 256).unwrap(),
        ];
        let mut allocations: Vec<(usize, Layout)> = Vec::new();

        for round in 0..200 {
            let layout = layouts[(round * 3 + 1) % layouts.len()];
            let ptr = unsafe { tlsf.alloc(layout) };
            if !ptr.is_null() {
                // 写入模式, 验证不越界写坏邻居
                for (i, byte) in unsafe { core::slice::from_raw_parts_mut(ptr, layout.size()) }
                    .iter_mut()
                    .enumerate()
                {
                    *byte = (round as u8).wrapping_add(i as u8);
                }
                allocations.push((ptr as usize, layout));
            }
            // 随机释放一半
            if allocations.len() > 16 && round % 2 == 1 {
                let index = round % allocations.len();
                let (ptr, layout) = allocations.swap_remove(index);
                unsafe { tlsf.dealloc(ptr as *mut u8) };
                let _ = layout;
            }
        }
        // 释放全部: 必须回到单一大块, used() == 0
        for (ptr, _) in allocations.drain(..) {
            unsafe { tlsf.dealloc(ptr as *mut u8) };
        }
        assert_eq!(tlsf.used(), 0, "空闲块残留: {:?}", tlsf.debug_free_blocks());
        assert_eq!(
            tlsf.largest_free_block(),
            end - start,
            "未完全合并: {:?}",
            tlsf.debug_free_blocks()
        );
    }

    #[test]
    fn random_churn_keeps_neighbors_intact() {
        let arena = std::boxed::Box::new([0u64; 8192]);
        let (start, end) = arena_bounds(&arena);
        let mut tlsf = Tlsf::new();
        unsafe { tlsf.init(start, end) };
        let mut rng = Rng(0x1234_5678);

        const ALIGNS: [usize; 7] = [1, 2, 4, 8, 16, 64, 256];
        let mut live: Vec<(usize, usize, u8)> = Vec::new(); // (ptr, size, tag)

        for round in 0..5000u32 {
            match rng.below(10) {
                0..=6 => {
                    // 分配
                    let size = 1 + rng.below(1500) as usize;
                    let align = ALIGNS[rng.below(ALIGNS.len() as u32) as usize];
                    let Ok(layout) = Layout::from_size_align(size, align) else {
                        continue;
                    };
                    let ptr = unsafe { tlsf.alloc(layout) };
                    if ptr.is_null() {
                        continue; // 满则跳过 (压力下优雅拒绝)
                    }
                    let tag = (round & 0xFF) as u8;
                    for byte in unsafe { core::slice::from_raw_parts_mut(ptr, size) } {
                        *byte = tag;
                    }
                    live.push((ptr as usize, size, tag));
                }
                _ => {
                    // 释放并校验内容
                    if !live.is_empty() {
                        let index = rng.below(live.len() as u32) as usize;
                        let (ptr, size, tag) = live.swap_remove(index);
                        for byte in unsafe { core::slice::from_raw_parts(ptr as *const u8, size) } {
                            assert_eq!(*byte, tag, "邻居块被写坏 (round={round})");
                        }
                        unsafe { tlsf.dealloc(ptr as *mut u8) };
                    }
                }
            }
        }
        for (ptr, size, tag) in live.drain(..) {
            for byte in unsafe { core::slice::from_raw_parts(ptr as *const u8, size) } {
                assert_eq!(*byte, tag, "幸存块被写坏");
            }
            unsafe { tlsf.dealloc(ptr as *mut u8) };
        }
        assert_eq!(tlsf.used(), 0);
        assert_eq!(tlsf.largest_free_block(), end - start);
    }

    #[test]
    fn thread_stack_and_tcb_churn_matches_kernel_pattern() {
        let arena = std::boxed::Box::new([0u64; 8192]);
        let (start, end) = arena_bounds(&arena);
        let mut tlsf = Tlsf::new();
        unsafe { tlsf.init(start, end) };

        // 与 thread_create 完全一致的布局: 栈 (含 32B 守卫) + TCB 类分配
        let stack_layout = Layout::from_size_align(512 + 32, 32).unwrap();
        let tcb_layout = Layout::from_size_align(224, 8).unwrap();
        let mut stacks: Vec<usize> = Vec::new();
        let mut tcbs: Vec<usize> = Vec::new();

        for _ in 0..3000 {
            // 创建线程: TCB + 栈
            let tcb = unsafe { tlsf.alloc(tcb_layout) };
            assert!(!tcb.is_null());
            tcbs.push(tcb as usize);
            let stack = unsafe { tlsf.alloc(stack_layout) };
            assert!(!stack.is_null());
            unsafe { core::ptr::write_bytes(stack, 0xA5, stack_layout.size()) };
            stacks.push(stack as usize);
            // 退出线程: 栈 + TCB 回收 (idle 顺序)
            if stacks.len() >= 8 {
                let stack = stacks.remove(0);
                let tcb = tcbs.remove(0);
                unsafe { tlsf.dealloc(stack as *mut u8) };
                unsafe { tlsf.dealloc(tcb as *mut u8) };
            }
        }
        for stack in stacks {
            unsafe { tlsf.dealloc(stack as *mut u8) };
        }
        for tcb in tcbs {
            unsafe { tlsf.dealloc(tcb as *mut u8) };
        }
        assert_eq!(tlsf.used(), 0);
        assert_eq!(tlsf.largest_free_block(), end - start);
    }

    #[test]
    fn coalescing_merges_all_directions() {
        let arena = std::boxed::Box::new([0u64; 8192]);
        let (start, end) = arena_bounds(&arena);
        let mut tlsf = Tlsf::new();
        unsafe { tlsf.init(start, end) };

        let l = Layout::from_size_align(256, 8).unwrap();
        let a = unsafe { tlsf.alloc(l) };
        let b = unsafe { tlsf.alloc(l) };
        let c = unsafe { tlsf.alloc(l) };
        assert!(!a.is_null() && !b.is_null() && !c.is_null());
        // 三块连续: a < b < c (首次适配/类首分配保持地址序)
        assert!(a < b && b < c);
        // 释放中间块 → 与前后合并? (前后均为使用中, 不合并)
        unsafe { tlsf.dealloc(b) };
        // 释放 a → a+b 合并
        unsafe { tlsf.dealloc(a) };
        // 释放 c → 全部合并为单块
        unsafe { tlsf.dealloc(c) };
        assert_eq!(tlsf.used(), 0);
        assert_eq!(tlsf.largest_free_block(), end - start);
        // 再次分配大块成功
        let big = unsafe { tlsf.alloc(Layout::from_size_align(32 * 1024, 8).unwrap()) };
        assert!(!big.is_null());
        unsafe { tlsf.dealloc(big) };
        assert_eq!(tlsf.used(), 0);
    }

    #[test]
    fn high_alignment_payloads_round_trip() {
        let arena = std::boxed::Box::new([0u64; 8192]);
        let (start, end) = arena_bounds(&arena);
        let mut tlsf = Tlsf::new();
        unsafe { tlsf.init(start, end) };
        let mut rng = Rng(0xDEAD_BEEF);

        let mut live: Vec<(usize, usize)> = Vec::new();
        for _ in 0..2000 {
            let align = 1usize << rng.below(11); // 1..1024
            let size = 1 + rng.below(256) as usize;
            let Ok(layout) = Layout::from_size_align(size, align) else {
                continue;
            };
            let ptr = unsafe { tlsf.alloc(layout) };
            if ptr.is_null() {
                continue;
            }
            assert_eq!(ptr as usize % align, 0, "对齐不满足");
            live.push((ptr as usize, align));
            if live.len() > 64 {
                let index = rng.below(live.len() as u32) as usize;
                let (p, _) = live.swap_remove(index);
                unsafe { tlsf.dealloc(p as *mut u8) };
            }
        }
        for (p, _) in live {
            unsafe { tlsf.dealloc(p as *mut u8) };
        }
        assert_eq!(tlsf.used(), 0);
        assert_eq!(tlsf.largest_free_block(), end - start);
    }

    #[test]
    fn all_free_orders_coalesce_to_single_block() {
        let arena = std::boxed::Box::new([0u64; 8192]);
        let (start, end) = arena_bounds(&arena);
        let l = Layout::from_size_align(256, 8).unwrap();
        // 覆盖全部释放顺序: 三个相邻块以任意次序释放都必须合并为单块
        let orders: [[usize; 3]; 6] = [
            [0, 1, 2],
            [2, 1, 0],
            [1, 0, 2],
            [0, 2, 1],
            [2, 0, 1],
            [1, 2, 0],
        ];
        for order in orders {
            let mut tlsf = Tlsf::new();
            unsafe { tlsf.init(start, end) };
            let a = unsafe { tlsf.alloc(l) };
            let b = unsafe { tlsf.alloc(l) };
            let c = unsafe { tlsf.alloc(l) };
            assert!(!a.is_null() && !b.is_null() && !c.is_null());
            let ptrs = [a, b, c];
            for index in order {
                unsafe { tlsf.dealloc(ptrs[index]) };
            }
            assert_eq!(
                tlsf.largest_free_block(),
                end - start,
                "order {order:?}: {:?}",
                tlsf.debug_free_blocks()
            );
            assert_eq!(tlsf.used(), 0);
        }
    }

    #[test]
    fn allocation_failure_is_graceful_when_full() {
        let arena = std::boxed::Box::new([0u64; 8192]);
        let (start, _end) = arena_bounds(&arena);
        let mut tlsf = Tlsf::new();
        // 仅 1KB 区域: 请求 2KB 应失败且不损坏状态
        unsafe { tlsf.init(start, start + 1024) };
        let big = unsafe { tlsf.alloc(Layout::from_size_align(2048, 8).unwrap()) };
        assert!(big.is_null());
        // 小分配仍可用
        let small = unsafe { tlsf.alloc(Layout::from_size_align(64, 8).unwrap()) };
        assert!(!small.is_null());
        unsafe { tlsf.dealloc(small) };
        assert_eq!(tlsf.used(), 0);
    }
}
