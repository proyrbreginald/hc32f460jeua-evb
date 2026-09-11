//! ARMv7-M 异常压栈帧的纯解码契约 (主机可测)。
//!
//! 布局 (以异常入口时的 SP 为起点, 低地址在前, 与 ARMv7-M ARM B3.3.2 /
//! Cortex-M4F TRM 一致):
//!
//! | 偏移 | 内容 |
//! |---|---|
//! | +0x00..+0x1C | r0, r1, r2, r3, r12, lr, pc, xpsr (基本帧, 8 字) |
//! | +0x20..+0x5F | s0..s15 (扩展帧; `EXC_RETURN.bit4 == 0` 时存在) |
//! | +0x60 | fpscr |
//! | +0x64 | reserved |
//!
//! 帧总长: 基本帧 0x20 字节 / 扩展帧 0x68 字节。原始 SP 始终指向基本帧
//! 首字 r0 —— 曾误实现为"SP 先指向 FP 扩展区、基本帧在其后" (把
//! s10..reserved 印刷成 r0..xpsr, PC/回溯全废), 本模块把布局索引集中
//! 定义并由主机单测锁定。

/// 基本帧字数 (r0..xpsr)
pub const BASIC_WORDS: usize = 8;
/// FP 扩展区字数 (s0..s15, fpscr, reserved)
pub const FP_WORDS: usize = 18;
/// 基本帧字节数
pub const BASIC_BYTES: usize = BASIC_WORDS * 4;
/// 扩展帧总字节数
pub const EXTENDED_BYTES: usize = (BASIC_WORDS + FP_WORDS) * 4;

/// 一帧解码结果: `fp` 仅在扩展帧 (EXC_RETURN.bit4 == 0) 时存在。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    /// 基本帧: r0, r1, r2, r3, r12, lr, pc, xpsr
    pub basic: &'a [u32; BASIC_WORDS],
    /// FP 上下文: s0..s15, fpscr, reserved (扩展帧)
    pub fp: Option<&'a [u32; FP_WORDS]>,
}

/// 从按压栈顺序排列的字数组解码 (数组首元素 = SP 处的 r0)。
///
/// `words` 须为完整的基础序列 (≥8 字; 扩展时 ≥26 字)。长度不足返回
/// `None` —— 调用方 (panic 诊断) 按"帧范围无效"处理。
pub fn try_decode<'a>(words: &'a [u32], extended: bool) -> Option<Frame<'a>> {
    if !extended {
        let basic = words.get(..BASIC_WORDS)?.try_into().ok()?;
        return Some(Frame { basic, fp: None });
    }
    let basic = words.get(..BASIC_WORDS)?.try_into().ok()?;
    let fp = words
        .get(BASIC_WORDS..BASIC_WORDS + FP_WORDS)?
        .try_into()
        .ok()?;
    Some(Frame {
        basic,
        fp: Some(fp),
    })
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::prelude::v1::*;

    /// 按 ARMv7-M 布局构造合成栈帧: [基本 8 字][s0..s15, fpscr, reserved]。
    fn synthetic_stack(extended: bool) -> Vec<u32> {
        let mut words = Vec::new();
        // 基本帧: 每个字 = 0xB0000000 | 偏移 (字节)
        for i in 0..BASIC_WORDS {
            words.push(0xB000_0000 | (i as u32) * 4);
        }
        if extended {
            // FP 区: s0..s15 = 0x5000_0000 | (i*4), fpscr = 0x6000_0000,
            // reserved = 0x7000_0000
            for i in 0..16 {
                words.push(0x5000_0000 | (i as u32) * 4);
            }
            words.push(0x6000_0000);
            words.push(0x7000_0000);
        }
        words
    }

    #[test]
    fn layout_maps_r0_at_sp_and_pc_at_0x18() {
        let stack = synthetic_stack(true);
        let frame = try_decode(&stack, true).unwrap();
        // 基本帧在低地址: r0 位于栈顶 (SP), pc 位于 +0x18
        assert_eq!(frame.basic[0], 0xB000_0000); // r0 @ +0x00
        assert_eq!(frame.basic[5], 0xB000_0014); // lr  @ +0x14
        assert_eq!(frame.basic[6], 0xB000_0018); // pc  @ +0x18
        assert_eq!(frame.basic[7], 0xB000_001C); // xpsr @ +0x1C
    }

    #[test]
    fn fp_context_follows_basic_frame() {
        let stack = synthetic_stack(true);
        let frame = try_decode(&stack, true).unwrap();
        let fp = frame.fp.unwrap();
        // s0 位于基本帧之后 (+0x20), s15 于 +0x5C, fpscr +0x60, reserved +0x64
        assert_eq!(fp[0], 0x5000_0000); // s0
        assert_eq!(fp[15], 0x5000_003C); // s15
        assert_eq!(fp[16], 0x6000_0000); // fpscr
        assert_eq!(fp[17], 0x7000_0000); // reserved
    }

    #[test]
    fn regression_never_reads_basic_frame_from_plus_0x48() {
        // 旧实现假定基本帧位于 SP+0x48 (把 s10..reserved 当成 r0..xpsr):
        // 若解码基于错误的偏移, r0 会读到 0x50000028 (s10)。
        // 本测试锁定布局, 防止该类回退。
        let stack = synthetic_stack(true);
        let frame = try_decode(&stack, true).unwrap();
        assert_ne!(frame.basic[0], 0x5000_0028, "r0 不得取自 FP 扩展区");
        assert_eq!(frame.basic[0], 0xB000_0000);
        let wrong_shifted = try_decode(&stack[18..], true);
        // 从 SP+0x48 起按扩展帧解码: 元素不足 (18 < 26), 必须拒绝
        assert!(wrong_shifted.is_none());
    }

    #[test]
    fn basic_only_frame_rejects_null_fp() {
        let stack = synthetic_stack(false);
        assert_eq!(stack.len(), BASIC_WORDS);
        let frame = try_decode(&stack, false).unwrap();
        assert!(frame.fp.is_none());
        // 非扩展帧要求 ≥8 字
        assert!(try_decode(&stack[..7], false).is_none());
        // 扩展帧要求 ≥26 字
        assert!(try_decode(&stack, true).is_none());
    }

    #[test]
    fn frame_sizes_match_architecture() {
        assert_eq!(BASIC_WORDS * 4, 0x20);
        assert_eq!(EXTENDED_BYTES, 0x68);
        assert_eq!(FP_WORDS * 4, 0x48);
    }
}
