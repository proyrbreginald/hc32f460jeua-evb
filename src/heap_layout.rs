//! Pure allocation-layout planning shared by the firmware allocator and host tests.

use core::alloc::Layout;

/// Placement of one allocation inside an allocator block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocationPlan {
    /// Aligned pointer returned to the caller.
    pub payload: usize,
    /// Bytes consumed from the start of the block, including allocator metadata.
    pub block_size: usize,
}

/// Align `value` upwards without wrapping.
pub const fn checked_align_up(value: usize, align: usize) -> Option<usize> {
    if !align.is_power_of_two() {
        return None;
    }
    match value.checked_add(align - 1) {
        Some(value) => Some(value & !(align - 1)),
        None => None,
    }
}

/// Plan an allocation within a free block.
///
/// The prefix is immediately before the returned payload and records the owning
/// block. The final block size is rounded up so the next block remains aligned.
pub fn allocation_plan(
    block: usize,
    available: usize,
    layout: Layout,
    header_size: usize,
    prefix_size: usize,
    footer_size: usize,
    block_align: usize,
) -> Option<AllocationPlan> {
    if !block_align.is_power_of_two() || !block.is_multiple_of(block_align) {
        return None;
    }

    let payload_align = layout.align().max(block_align);
    let payload_min = block.checked_add(header_size)?.checked_add(prefix_size)?;
    let payload = checked_align_up(payload_min, payload_align)?;
    let payload_end = payload.checked_add(layout.size().max(1))?;
    let raw_end = payload_end.checked_add(footer_size)?;
    let block_end = checked_align_up(raw_end, block_align)?;
    let block_size = block_end.checked_sub(block)?;

    (block_size <= available).then_some(AllocationPlan {
        payload,
        block_size,
    })
}

// ============================== TLSF 尺寸级映射 ==============================
//
// 两级隔离 (TLSF) 的核心纯函数, 供 heap.rs 与主机测试共用:
// 每级内 4 个子类 (SL_COUNT=4), 第一级索引从 `fl_min_log2` 开始,
// 每个第一级覆盖一个 2 的幂区间。

/// TLSF 每级的子类数
pub const TLSF_SL_COUNT: usize = 4;
/// log2(TLSF_SL_COUNT)
pub const TLSF_SL_SHIFT: usize = 2;

/// `x` 的对数下取整 (x > 0)
pub const fn log2_floor(x: usize) -> usize {
    usize::BITS as usize - 1 - x.leading_zeros() as usize
}

/// 尺寸级 (fl, sl) 的最小块长。
pub const fn tlsf_class_min(fl: usize, sl: usize, fl_min_log2: usize) -> usize {
    (1usize << (fl + fl_min_log2)) + (sl << (fl + fl_min_log2 - TLSF_SL_SHIFT))
}

/// 插入映射 (向下取整): 块大小 → (fl, sl)。
///
/// 调用方保证 `size` 落在 `[2^fl_min_log2, 2^(fl_min_log2+fl_count))`。
pub const fn tlsf_insert_index(size: usize, fl_min_log2: usize) -> (usize, usize) {
    let fl = log2_floor(size) - fl_min_log2;
    let sl = (size >> (fl + fl_min_log2 - TLSF_SL_SHIFT)) & (TLSF_SL_COUNT - 1);
    (fl, sl)
}

/// 查找映射 (向上取整): 需求大小 → 保证"该级任意块都装得下"的最小
/// (fl, sl)。返回 None 表示超出尺寸级上限 `fl_count`。
///
/// 不变量: 对任意 `size`, 返回级的最小块长 ≥ `size` 且 `fl < fl_count`。
pub const fn tlsf_search_index(
    size: usize,
    fl_min_log2: usize,
    fl_count: usize,
) -> Option<(usize, usize)> {
    let (fl, sl) = tlsf_insert_index(size, fl_min_log2);
    if fl >= fl_count {
        return None;
    }
    let class_min = tlsf_class_min(fl, sl, fl_min_log2);
    if size <= class_min {
        return Some((fl, sl));
    }
    if sl + 1 < TLSF_SL_COUNT {
        Some((fl, sl + 1))
    } else if fl + 1 < fl_count {
        Some((fl + 1, 0))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{allocation_plan, checked_align_up};
    use core::alloc::Layout;

    const HEADER: usize = 8;
    const PREFIX: usize = 4;
    const FOOTER: usize = 4;
    const BLOCK_ALIGN: usize = 8;

    #[test]
    fn sequential_blocks_preserve_payload_and_block_alignment() {
        let mut block = 0x2000_0000usize;
        let arena_end = block + 64 * 1024;

        for (index, align) in [1, 2, 4, 8, 16, 32, 64, 256, 4096]
            .into_iter()
            .cycle()
            .take(128)
            .enumerate()
        {
            let layout = Layout::from_size_align(index % 97 + 1, align).unwrap();
            let plan = allocation_plan(
                block,
                arena_end - block,
                layout,
                HEADER,
                PREFIX,
                FOOTER,
                BLOCK_ALIGN,
            )
            .unwrap();

            assert_eq!(plan.payload % align, 0);
            assert_eq!(plan.block_size % BLOCK_ALIGN, 0);
            assert!(plan.payload >= block + HEADER + PREFIX);
            block += plan.block_size;
            assert_eq!(block % BLOCK_ALIGN, 0);
        }
    }

    #[test]
    fn high_alignment_padding_is_accounted_for() {
        let block = 0x2000_0008;
        let layout = Layout::from_size_align(33, 1024).unwrap();
        let plan =
            allocation_plan(block, 4096, layout, HEADER, PREFIX, FOOTER, BLOCK_ALIGN).unwrap();

        assert_eq!(plan.payload % 1024, 0);
        assert!(plan.block_size >= plan.payload - block + 33 + FOOTER);
        assert_eq!(plan.block_size % BLOCK_ALIGN, 0);
    }

    #[test]
    fn insufficient_or_overflowing_layouts_fail_closed() {
        let layout = Layout::from_size_align(128, 32).unwrap();
        assert!(
            allocation_plan(0x2000_0000, 64, layout, HEADER, PREFIX, FOOTER, BLOCK_ALIGN,)
                .is_none()
        );
        assert!(
            allocation_plan(
                usize::MAX - 7,
                usize::MAX,
                layout,
                HEADER,
                PREFIX,
                FOOTER,
                BLOCK_ALIGN,
            )
            .is_none()
        );
        assert_eq!(checked_align_up(usize::MAX, 8), None);
        assert_eq!(checked_align_up(1, 3), None);
    }

    #[test]
    fn zero_sized_layout_still_gets_a_stable_internal_slot() {
        let layout = Layout::from_size_align(0, 64).unwrap();
        let plan = allocation_plan(
            0x2000_0000,
            1024,
            layout,
            HEADER,
            PREFIX,
            FOOTER,
            BLOCK_ALIGN,
        )
        .unwrap();

        assert_eq!(plan.payload % 64, 0);
        assert!(plan.block_size > HEADER + PREFIX + FOOTER);
    }

    // ---- TLSF 尺寸级不变量 ----

    const FL_MIN: usize = 4; // 最小块 24B ∈ [16, 32) → 级 4
    const FL_COUNT: usize = 14; // 覆盖到 2^18 (256KB)

    #[test]
    fn tlsf_insert_index_places_size_within_its_class() {
        // 覆盖堆上可能出现的全部块长 (24B ~ 192KB, 含非对齐值)
        for size in 24..=200_000 {
            let (fl, sl) = super::tlsf_insert_index(size, FL_MIN);
            let min = super::tlsf_class_min(fl, sl, FL_MIN);
            let max = super::tlsf_class_min(fl, sl + 1, FL_MIN);
            assert!(
                size >= min && size < max,
                "size={size} 不在其类 [{min}, {max}) 内 (fl={fl}, sl={sl})"
            );
            assert!(fl < FL_COUNT);
        }
    }

    #[test]
    fn tlsf_search_index_guarantees_class_fits() {
        for size in 24..=200_000 {
            let Some((fl, sl)) = super::tlsf_search_index(size, FL_MIN, FL_COUNT) else {
                panic!("size={size} 应存在可容纳的尺寸级");
            };
            let min = super::tlsf_class_min(fl, sl, FL_MIN);
            assert!(min >= size, "size={size} 被映射到更小的级 (min={min})");
            // 返回级是最小可容纳级: 前一级的最小块长 < size (前级不能
            // 保证装得下任意 size 需求)
            if fl > 0 || sl > 0 {
                let (pfl, psl) = if sl > 0 {
                    (fl, sl - 1)
                } else {
                    (fl - 1, super::TLSF_SL_COUNT - 1)
                };
                let prev_min = super::tlsf_class_min(pfl, psl, FL_MIN);
                assert!(
                    prev_min < size,
                    "size={size} 应落入更小级 (前级最小 {prev_min})"
                );
            }
        }
    }

    #[test]
    fn tlsf_search_index_reports_overflow_beyond_fl_count() {
        // 需求接近 2^18 且向上取整越过末级 → None
        let top = 1usize << (FL_MIN + FL_COUNT - 1); // 2^17
        assert!(super::tlsf_search_index(top, FL_MIN, FL_COUNT).is_some());
        // 末级上限 (2^18) 起不可容纳
        assert!(
            super::tlsf_search_index(1usize << (FL_MIN + FL_COUNT), FL_MIN, FL_COUNT).is_none()
        );
    }

    #[test]
    fn tlsf_mapping_is_monotonic_in_size() {
        let mut prev = (0usize, 0usize);
        for size in 24..=200_000 {
            let (fl, sl) = super::tlsf_insert_index(size, FL_MIN);
            assert!((fl, sl) >= prev, "映射非单调: size={size}");
            prev = (fl, sl);
        }
    }
}
