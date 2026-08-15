//! 固定容量日志缓冲 (纯逻辑, 主机可测): 条目按追加顺序保存, 容量满时
//! **丢弃最旧条目**; 无内部锁, 调用方负责串行化 (板上经临界区)。
//!
//! 条目布局: `[len: u16 小端][内容字节]`。
//!
//! # 增量写入 (单次格式化扇出)
//!
//! 条目经句柄 [`EntryMark`] 增量构建: [`begin_entry`] 预留长度头 →
//! [`append_to_entry`] 逐块追加 (空间不足时**截断保存已写部分**,
//! 不丢弃整条) → [`commit_entry`] 回填长度。`writing` 标记在提交前置位,
//! 排空操作会等待其清除, 避免读到半成品条目。
//!
//! 超长条目 (单条超过缓冲容量) 在 `begin` 阶段因无空间被整体放弃。

#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, Ordering};

/// 条目长度字段字节数
const HEADER: usize = 2;

/// 固定容量日志缓冲
pub struct LogRing<const CAP: usize> {
    buf: [u8; CAP],
    /// 第一个条目的起始偏移 (条目始终连续存放, 不绕回)
    head: usize,
    /// 已占用字节数 (含条目长度头)
    len: usize,
    /// 是否有条目正在增量构建 (提交前为 true; 排空需等待其清除)
    writing: AtomicBool,
}

/// 进行中的条目标记: 由 [`LogRing::begin_entry`] 产生, 提交前保持有效
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryMark {
    /// 条目起始偏移 (长度头位置)
    start: usize,
    /// 已追加的内容字节数
    len: usize,
}

impl<const CAP: usize> LogRing<CAP> {
    /// 空缓冲
    pub const fn new() -> Self {
        Self {
            buf: [0; CAP],
            head: 0,
            len: 0,
            writing: AtomicBool::new(false),
        }
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 已占用的字节数 (含条目头, 不含换行符)
    pub fn bytes_pending(&self) -> usize {
        self.len
    }

    /// 把空闲空间压缩到缓冲头部 (entries 前移)
    fn compact(&mut self) {
        if self.head > 0 {
            self.buf.copy_within(self.head..self.head + self.len, 0);
            self.head = 0;
        }
    }

    /// 丢弃最旧条目; 缓冲为空时返回 `false`
    fn drop_oldest(&mut self) -> bool {
        if self.len == 0 {
            return false;
        }
        let entry_len = u16::from_le_bytes([self.buf[self.head], self.buf[self.head + 1]]) as usize;
        let step = HEADER + entry_len;
        debug_assert!(step <= self.len);
        self.head += step;
        self.len -= step;
        if self.head >= self.buf.len() {
            self.head = 0;
            self.len = 0;
        }
        true
    }

    /// 开始一个条目: 预留长度头并置位 `writing`。
    ///
    /// 空间不足时压缩并丢弃最旧条目; 单条无法容纳 (缓冲全空仍不够) 时
    /// 返回 `None` (调用方应放弃本条, 控制台输出不受影响)。
    pub fn begin_entry(&mut self) -> Option<EntryMark> {
        debug_assert!(
            !self.writing.load(Ordering::Relaxed),
            "begin_entry: 已有条目在构建中"
        );
        loop {
            self.compact();
            if CAP - self.len >= HEADER {
                let mark = EntryMark {
                    start: self.head + self.len,
                    len: 0,
                };
                self.len += HEADER;
                self.writing.store(true, Ordering::Release);
                return Some(mark);
            }
            if !self.drop_oldest() {
                return None;
            }
        }
    }

    /// 向条目追加字节。
    ///
    /// 尾部空间不足时先压缩、再丢弃**本条目之前**的最旧条目腾空间
    /// (与整条写入一致的丢弃最旧策略, 保证最近日志不被挤出);
    /// 本条目已成为唯一条目仍放不下时返回 `false` (条目保持已截断状态)。
    pub fn append_to_entry(&mut self, mark: &mut EntryMark, bytes: &[u8]) -> bool {
        loop {
            if self.head > 0 {
                // 压缩会前移全部条目: 同步修正本条目起始偏移
                let shift = self.head;
                self.compact();
                mark.start -= shift;
            }
            let tail = self.head + self.len;
            if bytes.len() <= CAP - tail {
                self.buf[tail..tail + bytes.len()].copy_from_slice(bytes);
                self.len += bytes.len();
                mark.len += bytes.len();
                return true;
            }
            if !self.drop_oldest_before(mark.start) {
                return false;
            }
        }
    }

    /// 丢弃最旧条目, 但不越过 `before` 偏移 (进行中条目的起始)。
    /// 无更旧的条目可丢时返回 `false`。
    fn drop_oldest_before(&mut self, before: usize) -> bool {
        if self.len == 0 || self.head >= before {
            return false;
        }
        let entry_len = u16::from_le_bytes([self.buf[self.head], self.buf[self.head + 1]]) as usize;
        let step = HEADER + entry_len;
        if self.head + step > before {
            return false; // 最旧条目就是本条目 (之前已无条目)
        }
        self.head += step;
        self.len -= step;
        if self.head >= self.buf.len() {
            self.head = 0;
            self.len = 0;
        }
        true
    }

    /// 提交条目: 回填长度头并清除 `writing`。
    pub fn commit_entry(&mut self, mark: &EntryMark) {
        debug_assert!(
            self.writing.load(Ordering::Relaxed),
            "commit_entry: 无进行中的条目"
        );
        debug_assert!(mark.start + HEADER + mark.len <= self.head + self.len);
        self.buf[mark.start..mark.start + HEADER].copy_from_slice(&(mark.len as u16).to_le_bytes());
        self.writing.store(false, Ordering::Release);
    }

    /// 把所有条目拷入 `out` (每条末尾补 `\n`), 然后清空缓冲。
    ///
    /// 若存在进行中的条目 (增量写入), 有界等待其提交后读取; 极端情况
    /// 下仍未提交则放弃本轮 (返回 0), 保证不读到半成品。
    /// 返回拷贝的字节数 (含换行符)。
    pub fn drain_into(&mut self, out: &mut alloc::vec::Vec<u8>) -> usize {
        // 等待增量条目提交 (写入方在微秒级完成): 轮询 `writing` 标志,
        // 不能在临界区内等待 (写入方需要临界区完成提交)。
        for _ in 0..10_000 {
            if !self.writing.load(Ordering::Acquire) {
                break;
            }
        }
        if self.writing.load(Ordering::Acquire) {
            return 0; // 仍在构建 (极端情况): 本轮放弃, 下轮再排
        }
        self.drain_locked(out)
    }

    /// 排空 (调用方已保证无进行中的条目; 板上在临界区内调用)
    fn drain_locked(&mut self, out: &mut alloc::vec::Vec<u8>) -> usize {
        let mut copied = 0;
        let end = self.head + self.len;
        let mut pos = self.head;
        while pos < end {
            let entry_len = u16::from_le_bytes([self.buf[pos], self.buf[pos + 1]]) as usize;
            if entry_len != 0 {
                let bytes = &self.buf[pos + HEADER..pos + HEADER + entry_len];
                out.extend_from_slice(bytes);
                out.push(b'\n');
                copied += entry_len + 1;
            }
            pos += HEADER + entry_len;
        }
        self.head = 0;
        self.len = 0;
        copied
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::prelude::v1::*;

    /// 便捷: 整条写入 (begin + append + commit)
    fn push<const CAP: usize>(ring: &mut LogRing<CAP>, text: &str) -> bool {
        let Some(mut mark) = ring.begin_entry() else {
            return false;
        };
        ring.append_to_entry(&mut mark, text.as_bytes());
        ring.commit_entry(&mark);
        true
    }

    #[test]
    fn append_and_drain_roundtrip() {
        let mut ring = LogRing::<64>::new();
        assert!(ring.is_empty());
        assert!(push(&mut ring, "hello"));
        assert!(push(&mut ring, "world"));
        assert_eq!(ring.bytes_pending(), 2 + 5 + 2 + 5);
        let mut out = Vec::new();
        assert_eq!(ring.drain_into(&mut out), 12);
        assert_eq!(out, b"hello\nworld\n");
        assert!(ring.is_empty());
    }

    #[test]
    fn overflow_drops_oldest_entries() {
        let mut ring = LogRing::<32>::new();
        // 3 × (2 + 8) = 30 ≤ 32, 全放得下
        assert!(push(&mut ring, "entry-01"));
        assert!(push(&mut ring, "entry-02"));
        assert!(push(&mut ring, "entry-03"));
        assert_eq!(ring.bytes_pending(), 30);
        // 第 4 条需要 30 + 10 = 40 > 32: 丢最旧后重试
        assert!(push(&mut ring, "entry-04"));
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out, b"entry-02\nentry-03\nentry-04\n");
    }

    #[test]
    fn oversized_entry_evicts_oldest_then_truncates() {
        let mut ring = LogRing::<16>::new();
        assert!(push(&mut ring, "small"));
        // 单条内容 32B > 缓冲: 先丢最旧条目 (small) 尝试腾空间,
        // 仍放不下 → 截断为空条目; 排空时跳过空条目
        let Some(mut mark) = ring.begin_entry() else {
            panic!("begin 失败");
        };
        assert!(!ring.append_to_entry(&mut mark, b"this entry is far too long"));
        ring.commit_entry(&mark);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn incremental_entry_is_truncated_at_capacity() {
        let mut ring = LogRing::<16>::new();
        let Some(mut mark) = ring.begin_entry() else {
            panic!("begin 失败");
        };
        assert!(ring.append_to_entry(&mut mark, b"0123456789")); // 10/12
        assert!(!ring.append_to_entry(&mut mark, b"ABCDEFGHIJ")); // 超出 → 截断
        ring.commit_entry(&mark);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out, b"0123456789\n");
    }

    #[test]
    fn compaction_preserves_order() {
        let mut ring = LogRing::<16>::new();
        assert!(push(&mut ring, "aaaa")); // 2+4
        assert!(push(&mut ring, "bbbb")); // 2+4
        assert_eq!(ring.bytes_pending(), 12);
        // 触发压缩 + 丢最旧
        assert!(push(&mut ring, "cccc")); // 需要 2+4=6; 尾空 4 < 6 → 压缩后仍不够 → 丢最旧
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out, b"bbbb\ncccc\n");
    }

    #[test]
    fn drain_waits_for_inflight_entry() {
        // 增量条目进行中: 排空应等待提交后再读, 不读到半成品
        let mut ring = LogRing::<64>::new();
        let Some(mut mark) = ring.begin_entry() else {
            panic!("begin 失败");
        };
        ring.append_to_entry(&mut mark, b"in-flight");
        // 模拟另一线程调用排空 (writing 已置位)
        let mut out = Vec::new();
        let n = ring.drain_into(&mut out);
        assert_eq!(n, 0);
        ring.commit_entry(&mark);
        assert_eq!(ring.drain_into(&mut out), 10);
        assert_eq!(out, b"in-flight\n");
    }

    #[test]
    fn append_then_drain_repeatedly() {
        let mut ring = LogRing::<32>::new();
        for i in 0..10 {
            let text = format!("line-{i:02}");
            assert!(push(&mut ring, &text));
            let mut out = Vec::new();
            ring.drain_into(&mut out);
            assert_eq!(out, format!("{text}\n").into_bytes());
        }
    }
}

impl<const CAP: usize> Default for LogRing<CAP> {
    fn default() -> Self {
        Self::new()
    }
}
