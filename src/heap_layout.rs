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
}
