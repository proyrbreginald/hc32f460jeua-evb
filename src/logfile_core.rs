//! 日志文件命名的纯逻辑部分 (主机可测): 单调有序的文件名、启动序号
//! 排序与回绕安全比较。
//!
//! # 文件名与顺序性
//!
//! 日志文件名为 `boot_<启动序号>.log` (首段) / `boot_<启动序号>_<段号>.log`
//! (后续段), 启动序号为 **u64 单调递增** (每次启动 = 上次各文件最大序号加一),
//! 以 20 位零填充。名字的字典序即时间序 —— 后续生成的文件严格有序,
//! 无需读文件内容即可判断新旧。
//!
//! 保留最近 [`crate::config::LOG_FILE_SLOTS`] 个文件: 超预算时删除
//! `(启动序号, 段号)` 最小 (最旧) 的文件。旧版本固件的 `logN.log`
//! 识别为启动序号 0 (最旧), 迁移时优先保留, 最终随预算淘汰。
//!
//! 启动序号为 u64, 实际不可能回绕; 比较器仍按回绕安全实现
//! ([`boot_newer`]), 与 littlefs generation 语义一致。

#![allow(dead_code)]

use core::fmt::Write as _;

/// 文件名最大长度 (`boot_` + 20 位序号 + `_` + 2 位段号 + `.log`)
pub const NAME_CAP: usize = 40;
/// 内容标记最大长度 (`\n----- boot #` + 20 位序号 + ` -----\n`)
pub const MARKER_CAP: usize = 48;

/// 把 (启动序号, 段号) 渲染为日志文件名 (段 0 无段号后缀)
pub fn segment_name(boot: u64, segment: u32, out: &mut [u8; NAME_CAP]) -> &[u8] {
    struct Sink<'a> {
        buf: &'a mut [u8],
        pos: usize,
    }
    impl core::fmt::Write for Sink<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let bytes = s.as_bytes();
            if self.pos + bytes.len() > self.buf.len() {
                return Err(core::fmt::Error);
            }
            self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
            self.pos += bytes.len();
            Ok(())
        }
    }
    let mut sink = Sink { buf: out, pos: 0 };
    if segment == 0 {
        let _ = write!(sink, "boot_{:020}.log", boot);
    } else {
        let _ = write!(sink, "boot_{:020}_{:02}.log", boot, segment);
    }
    let pos = sink.pos;
    &out[..pos]
}

/// 从文件名解析 `(启动序号, 段号)`; 非法/旧格式返回 `None`。
pub fn parse_segment_name(name: &str) -> Option<(u64, u32)> {
    let rest = name.strip_prefix("boot_")?;
    let digit_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digit_end == 0 || digit_end > 20 {
        return None;
    }
    let boot: u64 = rest[..digit_end].parse().ok()?;
    let tail = &rest[digit_end..];
    if tail == ".log" {
        return Some((boot, 0));
    }
    let seg = tail.strip_prefix('_')?.strip_suffix(".log")?;
    if seg.len() != 2 {
        return None; // 段号固定两位零填充
    }
    let segment: u32 = seg.parse().ok()?;
    Some((boot, segment))
}

/// 解析旧版本固件的槽文件名 `log<N>.log` → `(0, N)` (视为最旧)
pub fn parse_legacy_slot_name(name: &str) -> Option<(u64, u32)> {
    let rest = name.strip_prefix("log")?;
    let digits = rest.strip_suffix(".log")?;
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let slot: u32 = digits.parse().ok()?;
    Some((0, slot))
}

/// 启动序号回绕安全比较: `a` 是否比 `b` 更新 (语义同 littlefs generation)
pub fn boot_newer(a: u64, b: u64) -> bool {
    a != b && a.wrapping_sub(b) < 1 << 63
}

/// 段比较: 先比启动序号, 再比段号
pub fn segment_newer(a: (u64, u32), b: (u64, u32)) -> bool {
    if a.0 != b.0 {
        boot_newer(a.0, b.0)
    } else {
        a.1 > b.1
    }
}

/// 日志文件集合中的最大启动序号 (回绕安全); 空集返回 `None`
pub fn newest_boot(files: &[(u64, u32)]) -> Option<u64> {
    let mut best: Option<u64> = None;
    for &(boot, _) in files {
        let take = match best {
            Some(current) => boot_newer(boot, current),
            None => true,
        };
        if take {
            best = Some(boot);
        }
    }
    best
}

/// 最旧的文件 (启动序号最小, 段号最小); 空集返回 `None`
pub fn oldest_file(files: &[(u64, u32)]) -> Option<(u64, u32)> {
    let mut oldest: Option<(u64, u32)> = None;
    for &file in files {
        let take = match oldest {
            Some(current) => segment_newer(current, file),
            None => true,
        };
        if take {
            oldest = Some(file);
        }
    }
    oldest
}

/// 本次启动序号 = 最大序号 + 1 (无历史时为 1; u64 实际不可能回绕到 0)
pub fn next_boot(newest: Option<u64>) -> u64 {
    newest.map(|n| n.wrapping_add(1)).unwrap_or(1).max(1)
}

/// 把启动序号渲染为内容标记 `\n----- boot #N -----\n`
pub fn format_boot_marker(boot: u64, out: &mut [u8; MARKER_CAP]) -> &[u8] {
    struct Sink<'a> {
        buf: &'a mut [u8],
        pos: usize,
    }
    impl core::fmt::Write for Sink<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let bytes = s.as_bytes();
            if self.pos + bytes.len() > self.buf.len() {
                return Err(core::fmt::Error);
            }
            self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
            self.pos += bytes.len();
            Ok(())
        }
    }
    let mut sink = Sink { buf: out, pos: 0 };
    let _ = write!(sink, "\n----- boot #{} -----\n", boot);
    let pos = sink.pos;
    &out[..pos]
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::prelude::v1::*;

    #[test]
    fn segment_names_are_zero_padded_and_lexicographically_ordered() {
        let mut buf = [0u8; NAME_CAP];
        // 名字: 字典序 = 时间序
        let mut names = Vec::new();
        for (boot, seg) in [(1u64, 0u32), (1, 2), (2, 0), (42, 7), (999, 99)] {
            let name = core::str::from_utf8(segment_name(boot, seg, &mut buf)).unwrap();
            names.push(name.to_string());
        }
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        // 往返
        assert_eq!(
            parse_segment_name("boot_00000000000000000001.log"),
            Some((1, 0))
        );
        assert_eq!(
            parse_segment_name("boot_00000000000000000042_07.log"),
            Some((42, 7))
        );
        assert_eq!(
            core::str::from_utf8(segment_name(42, 7, &mut buf)).unwrap(),
            "boot_00000000000000000042_07.log"
        );
    }

    #[test]
    fn parse_rejects_invalid_names() {
        assert_eq!(parse_segment_name(""), None);
        assert_eq!(parse_segment_name("boot_.log"), None);
        assert_eq!(parse_segment_name("boot_abc.log"), None);
        assert_eq!(parse_segment_name("boot_00000000000000000001.txt"), None);
        assert_eq!(parse_segment_name("boot_00000000000000000001_1.log"), None); // 段号未补零
        assert_eq!(parse_segment_name("boot_00000000000000000001.log "), None);
        // 旧格式
        assert_eq!(parse_legacy_slot_name("log0.log"), Some((0, 0)));
        assert_eq!(parse_legacy_slot_name("log12.log"), Some((0, 12)));
        assert_eq!(parse_legacy_slot_name("log.log"), None);
        assert_eq!(
            parse_legacy_slot_name("boot_00000000000000000001.log"),
            None
        );
    }

    #[test]
    fn boot_ordering_wraps() {
        assert!(boot_newer(2, 1));
        assert!(!boot_newer(1, 2));
        assert!(!boot_newer(1, 1));
        assert!(boot_newer(0, u64::MAX)); // 回绕: 0 比 MAX 新
        assert!(!boot_newer(u64::MAX, 0));
    }

    #[test]
    fn newest_oldest_and_next_boot() {
        let files = [(5u64, 0u32), (6, 1), (6, 2), (7, 0)];
        assert_eq!(newest_boot(&files), Some(7));
        assert_eq!(oldest_file(&files), Some((5, 0)));
        assert_eq!(next_boot(newest_boot(&files)), 8);
        assert_eq!(next_boot(None), 1);
        // 段内新旧: 同 boot 比段号
        assert!(segment_newer((6, 2), (6, 1)));
        assert!(segment_newer((7, 0), (6, 99)));
        // 回绕集合: 4294967295 → 0 → 1 之后, 最旧是 4294967294
        let wrapped = [(u64::MAX - 1, 0), (u64::MAX, 0), (1, 0), (2, 0)];
        assert_eq!(newest_boot(&wrapped), Some(2));
        assert_eq!(oldest_file(&wrapped), Some((u64::MAX - 1, 0)));
        assert_eq!(next_boot(newest_boot(&wrapped)), 3);
    }

    #[test]
    fn marker_roundtrip() {
        let mut buf = [0u8; MARKER_CAP];
        for boot in [1u64, 42, u64::MAX] {
            let marker = format_boot_marker(boot, &mut buf);
            assert!(marker.starts_with(b"\n----- boot #"));
            assert!(marker.ends_with(b" -----\n"));
        }
    }
}
