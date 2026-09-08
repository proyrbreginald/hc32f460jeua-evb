//! 固定容量日志缓冲 (纯逻辑, 主机可测): 条目按追加顺序保存, 容量满时
//! **丢弃最旧条目**; 无内部锁, 调用方负责串行化 (板上经临界区)。
//!
//! 条目布局: `[len: u16 小端][内容字节]`。`len` 低 15 位为长度,
//! bit15 为**截断标记**: 该条目因缓冲容量被截断 (内容不完整)。
//!
//! # 增量写入 (单次格式化扇出)
//!
//! 条目经句柄 [`EntryMark`] 增量构建: [`begin_entry`] 预留长度头 →
//! [`append_to_entry`] 逐块追加 (空间不足时**截断保存已写部分**,
//! 不丢弃整条) → [`commit_entry`] 回填长度。`writing` 标记在提交前置位,
//! 排空操作会等待其清除, 避免读到半成品条目。
//!
//! 超长条目 (单条超过缓冲容量) 在 `begin` 阶段因无空间被整体放弃。
//!
//! # 损耗标记 (诚实性)
//!
//! 丢弃最旧条目、丢弃超长条目的次数累计于 `dropped`; 排空时在其
//! 发生位置输出 `[日志缓冲溢出, 丢弃 N 条]` 标记, 截断条目输出
//! `[截断]` 后缀 —— 落盘文件可区分"短"与"被截断", 不会静默丢日志。
//!
//! # 限量排空
//!
//! [`drain_into_limited`] 只取出"完整放得下"的前缀条目 (含标记),
//! 放不下的条目保留待下轮 (落盘按文件上限分块轮转用);
//! [`drain_oldest_entry`] 无条件排空首条 (超长条目独占一段时用)。

#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, Ordering};

/// 条目长度字段字节数
const HEADER: usize = 2;
/// 长度头 bit15: 条目被截断标记
const LEN_FLAG_TRUNCATED: u16 = 1 << 15;
/// 长度掩码 (低 15 位)
const LEN_MASK: u16 = 0x7FFF;

/// 丢弃标记文本 (排空时插入在发生丢弃的位置)
const DROP_MARKER_PREFIX: &[u8] = "[日志缓冲溢出, 丢弃 ".as_bytes();
const DROP_MARKER_SUFFIX: &[u8] = " 条]\n".as_bytes();
/// 截断后缀 (紧跟被截断条目内容, 换行符由排空逻辑统一补)
const TRUNC_MARKER: &[u8] = "[截断]".as_bytes();

/// 固定容量日志缓冲
pub struct LogRing<const CAP: usize> {
    buf: [u8; CAP],
    /// 第一个条目的起始偏移 (条目始终连续存放, 不绕回)
    head: usize,
    /// 已占用字节数 (含条目长度头)
    len: usize,
    /// 是否有条目正在增量构建 (提交前为 true; 排空需等待其清除)
    writing: AtomicBool,
    /// 被丢弃的条目数 (丢最旧 / 超大条目整体放弃), 排空时输出标记
    dropped: usize,
}

/// 进行中的条目标记: 由 [`LogRing::begin_entry`] 产生, 提交前保持有效
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryMark {
    /// 条目起始偏移 (长度头位置)
    start: usize,
    /// 已追加的内容字节数
    len: usize,
    /// 是否发生过截断 (append 返回 false; commit 写入长度头 bit15)
    truncated: bool,
}

impl<const CAP: usize> LogRing<CAP> {
    /// 空缓冲
    pub const fn new() -> Self {
        Self {
            buf: [0; CAP],
            head: 0,
            len: 0,
            writing: AtomicBool::new(false),
            dropped: 0,
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
        let entry_len = self.entry_len_at(self.head);
        let step = HEADER + entry_len;
        debug_assert!(step <= self.len);
        self.head += step;
        self.len -= step;
        self.dropped += 1;
        if self.head >= self.buf.len() {
            self.head = 0;
            self.len = 0;
        }
        true
    }

    /// 偏移处的条目长度 (低 15 位)
    #[inline]
    fn entry_len_at(&self, pos: usize) -> usize {
        (u16::from_le_bytes([self.buf[pos], self.buf[pos + 1]]) & LEN_MASK) as usize
    }

    /// 偏移处的截断标记
    #[inline]
    fn entry_truncated_at(&self, pos: usize) -> bool {
        u16::from_le_bytes([self.buf[pos], self.buf[pos + 1]]) & LEN_FLAG_TRUNCATED != 0
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
                    truncated: false,
                };
                self.len += HEADER;
                self.writing.store(true, Ordering::Release);
                return Some(mark);
            }
            if !self.drop_oldest() {
                // 缓冲全空仍放不下长度头: 超大条目, 整体放弃并计数
                self.dropped += 1;
                return None;
            }
        }
    }

    /// 向条目追加字节。
    ///
    /// 尾部空间不足时先压缩、再丢弃**本条目之前**的最旧条目腾空间
    /// (与整条写入一致的丢弃最旧策略, 保证最近日志不被挤出);
    /// 本条目已成为唯一条目仍放不下时返回 `false` (条目保持已截断状态,
    /// 截断标记在提交时写入长度头)。
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
                mark.truncated = true;
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
        let entry_len = self.entry_len_at(self.head);
        let step = HEADER + entry_len;
        if self.head + step > before {
            return false; // 最旧条目就是本条目 (之前已无条目)
        }
        self.head += step;
        self.len -= step;
        self.dropped += 1;
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
        let header = (mark.len as u16 & LEN_MASK)
            | if mark.truncated {
                LEN_FLAG_TRUNCATED
            } else {
                0
            };
        self.buf[mark.start..mark.start + HEADER].copy_from_slice(&header.to_le_bytes());
        self.writing.store(false, Ordering::Release);
    }

    /// 等待进行中的条目提交 (排空前置条件; 有界轮询, 不依赖临界区)
    fn wait_writer_idle(&self) -> bool {
        for _ in 0..10_000 {
            if !self.writing.load(Ordering::Acquire) {
                return true;
            }
        }
        false
    }

    /// 把所有条目拷入 `out` (每条末尾补 `\n`), 然后清空缓冲。
    ///
    /// 若存在进行中的条目 (增量写入), 有界等待其提交后读取; 极端情况
    /// 下仍未提交则放弃本轮 (返回 0), 保证不读到半成品。
    /// 返回拷贝的字节数 (含换行符与损耗标记)。
    pub fn drain_into(&mut self, out: &mut alloc::vec::Vec<u8>) -> usize {
        if !self.wait_writer_idle() {
            return 0;
        }
        self.drain_locked_limited(out, usize::MAX)
    }

    /// 把"完整放得下"的前缀条目拷入 `out` (损耗标记按序在前),
    /// 放不下的条目保留待下轮。`limit` 为本次可追加的字节上限
    /// (含换行符与标记; 单条超限时不截断、不取出)。
    ///
    /// 同样有界等待进行中的条目; 返回拷贝的字节数 (0 = 无待排或
    /// 首条放不下)。
    pub fn drain_into_limited(&mut self, out: &mut alloc::vec::Vec<u8>, limit: usize) -> usize {
        if !self.wait_writer_idle() {
            return 0;
        }
        self.drain_locked_limited(out, limit)
    }

    /// 无条件排空首条 (含前置损耗标记, 不受 `limit` 约束)。
    ///
    /// 供"超长条目独占一段"场景使用 (落盘轮转时首条比整个段还长,
    /// 限量排空永远为 0, 必须强制取出保证进度)。返回拷贝的字节数。
    pub fn drain_oldest_entry(&mut self, out: &mut alloc::vec::Vec<u8>) -> usize {
        if !self.wait_writer_idle() {
            return 0;
        }
        let mut copied = self.emit_drop_marker(out);
        if self.len == 0 {
            return copied;
        }
        let entry_len = self.entry_len_at(self.head);
        let truncated = self.entry_truncated_at(self.head);
        let step = HEADER + entry_len;
        if entry_len != 0 {
            out.extend_from_slice(&self.buf[self.head + HEADER..self.head + HEADER + entry_len]);
            copied += entry_len;
            if truncated {
                out.extend_from_slice(TRUNC_MARKER);
                copied += TRUNC_MARKER.len();
            }
            out.push(b'\n');
            copied += 1;
        }
        self.head += step;
        self.len -= step;
        if self.head >= self.buf.len() {
            self.head = 0;
            self.len = 0;
        }
        copied
    }

    /// 输出损耗标记 (存在且未输出过时), 返回追加字节数
    fn emit_drop_marker(&mut self, out: &mut alloc::vec::Vec<u8>) -> usize {
        if self.dropped == 0 {
            return 0;
        }
        let dropped = self.dropped;
        out.extend_from_slice(DROP_MARKER_PREFIX);
        push_usize(out, dropped);
        out.extend_from_slice(DROP_MARKER_SUFFIX);
        self.dropped = 0;
        DROP_MARKER_PREFIX.len() + DROP_MARKER_SUFFIX.len() + digits(dropped)
    }

    /// 限量排空 (调用方已保证无进行中的条目; 板上在临界区内调用)
    fn drain_locked_limited(&mut self, out: &mut alloc::vec::Vec<u8>, limit: usize) -> usize {
        let mut copied = 0;
        // 损耗标记: 标记描述的是"先于现存条目"的丢失, 必须先行输出;
        // 放不下则整轮放弃 (否则条目会越过标记, 顺序错乱)
        let dropped = self.dropped;
        if dropped > 0 {
            let marker_len = DROP_MARKER_PREFIX.len() + DROP_MARKER_SUFFIX.len() + digits(dropped);
            if copied + marker_len > limit {
                return 0;
            }
            copied += self.emit_drop_marker(out);
        }
        // 逐条取出完整条目 (不截断, 放不下即停止)
        let end = self.head + self.len;
        let mut pos = self.head;
        while pos < end {
            let entry_len = self.entry_len_at(pos);
            let truncated = self.entry_truncated_at(pos);
            let step = HEADER + entry_len;
            if entry_len != 0 {
                let total = step + 1 + if truncated { TRUNC_MARKER.len() } else { 0 };
                if copied + total > limit {
                    break;
                }
                out.extend_from_slice(&self.buf[pos + HEADER..pos + HEADER + entry_len]);
                copied += entry_len;
                if truncated {
                    out.extend_from_slice(TRUNC_MARKER);
                    copied += TRUNC_MARKER.len();
                }
                out.push(b'\n');
                copied += 1;
            }
            pos += step;
        }
        // 消费已取出的前缀
        let consumed = pos - self.head;
        self.head += consumed;
        self.len -= consumed;
        if self.head >= self.buf.len() {
            self.head = 0;
            self.len = 0;
        }
        copied
    }
}

/// 十进制数字位数
fn digits(mut v: usize) -> usize {
    if v == 0 {
        return 1;
    }
    let mut n = 0;
    while v > 0 {
        v /= 10;
        n += 1;
    }
    n
}

/// 追加十进制无符号数 (无格式化机器, 纯逻辑模块)
fn push_usize(out: &mut alloc::vec::Vec<u8>, mut v: usize) {
    if v == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    out.extend_from_slice(&buf[i..]);
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

    const DROP_1: &[u8] = "[日志缓冲溢出, 丢弃 1 条]\n".as_bytes();

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
    fn overflow_drops_oldest_entries_with_marker() {
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
        let mut expected = DROP_1.to_vec();
        expected.extend_from_slice(b"entry-02\nentry-03\nentry-04\n");
        assert_eq!(out, expected);
    }

    #[test]
    fn oversized_entry_evicts_oldest_then_truncates() {
        let mut ring = LogRing::<16>::new();
        assert!(push(&mut ring, "small"));
        // 单条内容 32B > 缓冲: 先丢最旧条目 (small) 尝试腾空间,
        // 仍放不下 → 截断为空条目; 排空时跳过空条目, 但保留丢弃标记
        let mut mark = ring.begin_entry().expect("begin 失败");
        assert!(!ring.append_to_entry(&mut mark, b"this entry is far too long"));
        ring.commit_entry(&mark);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out, DROP_1);
    }

    #[test]
    fn incremental_entry_is_truncated_at_capacity() {
        let mut ring = LogRing::<16>::new();
        let mut mark = ring.begin_entry().expect("begin 失败");
        assert!(ring.append_to_entry(&mut mark, b"0123456789")); // 10/12
        assert!(!ring.append_to_entry(&mut mark, b"ABCDEFGHIJ")); // 超出 → 截断
        ring.commit_entry(&mark);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out, "0123456789[截断]\n".as_bytes());
    }

    #[test]
    fn truncated_flag_roundtrips_through_commit() {
        let mut ring = LogRing::<24>::new();
        let mut mark = ring.begin_entry().expect("begin 失败");
        assert!(ring.append_to_entry(&mut mark, b"abcdefghij"));
        assert!(!ring.append_to_entry(&mut mark, b"klmnopqrstuvwxyz"));
        ring.commit_entry(&mark);
        // 长度头: 内容 10, bit15 置位
        let header = u16::from_le_bytes([ring.buf[0], ring.buf[1]]);
        assert_eq!(header & LEN_MASK, 10);
        assert_ne!(header & LEN_FLAG_TRUNCATED, 0);
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
        let mut expected = DROP_1.to_vec();
        expected.extend_from_slice(b"bbbb\ncccc\n");
        assert_eq!(out, expected);
    }

    #[test]
    fn drain_waits_for_inflight_entry() {
        // 增量条目进行中: 排空应等待提交后再读, 不读到半成品
        let mut ring = LogRing::<64>::new();
        let mut mark = ring.begin_entry().expect("begin 失败");
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

    #[test]
    fn limited_drain_keeps_entries_that_do_not_fit() {
        let mut ring = LogRing::<64>::new();
        assert!(push(&mut ring, "entry-01")); // 输出 8+1 = 9
        assert!(push(&mut ring, "entry-02")); // 9
        assert!(push(&mut ring, "entry-03")); // 9
        let mut out = Vec::new();
        // 限 25 字节: 前两条 (18) 放得下, 第三条放不下 → 保留
        assert_eq!(ring.drain_into_limited(&mut out, 25), 18);
        assert_eq!(out, b"entry-01\nentry-02\n");
        assert!(!ring.is_empty());
        // 剩余条目下一轮取出
        let mut out2 = Vec::new();
        assert_eq!(ring.drain_into(&mut out2), 9);
        assert_eq!(out2, b"entry-03\n");
    }

    #[test]
    fn limited_drain_respects_drop_marker_order() {
        let mut ring = LogRing::<16>::new();
        assert!(push(&mut ring, "aaaa"));
        assert!(push(&mut ring, "bbbb"));
        assert!(push(&mut ring, "cccc")); // 触发丢弃 aaaa
        let mut out = Vec::new();
        // 标记 (27B) 放不进 20B 限内 → 保留, 条目也因余量不足保留
        assert_eq!(ring.drain_into_limited(&mut out, 20), 0);
        let mut out2 = Vec::new();
        ring.drain_into(&mut out2);
        let mut expected = DROP_1.to_vec();
        expected.extend_from_slice(b"bbbb\ncccc\n");
        assert_eq!(out2, expected);
    }

    #[test]
    fn oldest_entry_drain_forces_progress() {
        let mut ring = LogRing::<32>::new();
        assert!(push(&mut ring, "first"));
        assert!(push(&mut ring, "second"));
        let mut out = Vec::new();
        // 超长条目独占段场景: 无视 limit 强制取首条
        assert!(ring.drain_oldest_entry(&mut out) > 0);
        assert_eq!(out, b"first\n");
        let mut out2 = Vec::new();
        ring.drain_into(&mut out2);
        assert_eq!(out2, b"second\n");
    }

    #[test]
    fn oversized_begin_is_counted_as_dropped() {
        let mut ring = LogRing::<1>::new();
        // 缓冲连长度头都放不下: begin 直接放弃并计数
        assert!(ring.begin_entry().is_none());
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out, DROP_1);
    }
}

impl<const CAP: usize> Default for LogRing<CAP> {
    fn default() -> Self {
        Self::new()
    }
}
