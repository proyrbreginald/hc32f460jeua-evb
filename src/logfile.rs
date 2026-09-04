//! 日志落盘: 把 RAM 日志缓冲 (见 [`crate::log::drain_limited`]) 自动保存到
//! 文件系统的 `/log/` 目录。
//!
//! # 工作方式
//!
//! 独立 RTOS 线程, 每个 [`crate::config::LOG_FLUSH_MS`] 唤醒一次执行
//! [`flush_once`]; shell 的 `reboot` 命令也会在复位前同步调用
//! [`flush_now`], 保证掉电前日志已落盘。
//!
//! 一次刷新 (零中间堆分配): 日志缓冲按**条目粒度**追加到 RAM 中的
//! 当前文件镜像, 放不下即轮转 (段号 +1) 后继续 → 整文件原子写
//! `/log/boot_<序号>.log`。镜像按 [`crate::config::LOG_FILE_MAX`]
//! 分块: 单个日志文件恒不超上限 (超长单条目独占一段的例外见
//! [`flush_once`]); 缓冲丢弃最旧/截断条目由 logring 输出显式标记,
//! 落盘文件不再静默缺日志。
//!
//! # 文件名与顺序性 (见 [`crate::logfile_core`])
//!
//! 文件名 `boot_<序号>[_<段号>].log` 中序号为 **u64 单调递增** (零填充,
//! 字典序即时间序): 每次启动从上次各文件最大序号 + 1 开始, **后续生成
//! 的文件严格有序**, 重启不覆盖旧日志。保留最近
//! [`crate::config::LOG_FILE_SLOTS`] 个文件, 超预算删除最旧
//! `(序号, 段号)`; 旧固件的 `logN.log` 视为最旧并最终淘汰。
//!
//! # 并发
//!
//! 落盘状态 (游标/镜像) 与文件系统实例均经 `rtos::Mutex` 串行化;
//! [`flush_once`] 先取文件系统锁再取状态锁 (顺序固定, 不会死锁),
//! 日志线程与 reboot 的同步刷新天然互斥, 不会互相覆盖。
//!
//! # 写放大与磨损
//!
//! 快照文件系统每次提交都重写整个快照, 因此刷新间隔 (`CFG_LOG_FLUSH_MS`)
//! 不能过小; 仅当缓冲有数据时才落盘 (空闲时零 Flash 写入)。
//!
//! 掉电/复位最多丢失最近一个刷新间隔 + RAM 缓冲内的日志 (reboot 命令
//! 除外 —— 复位前会先同步落盘)。

use crate::config;
use crate::filesystem;
use crate::logfile_core;
use crate::println;
use crate::rtos::{Mutex, Timeout};
use core::fmt::Write as _;

/// 日志目录名 (相对文件系统根)
const LOG_DIR: &str = "log";
/// 路径缓冲 (`log/boot_..._99.log`)
const NAME_BUF: usize = 64;
/// 目录枚举缓冲 (文件数上限: 预算 + 迁移残留余量)
const COLLECT_CAP: usize = 32;

/// 落盘状态 (跨线程共享: 日志线程周期刷新 + shell reboot 前同步刷新)。
/// 仅经 [`STATE`] 互斥量访问; [`flush_once`] 保证持有文件系统锁。
struct FlushState {
    /// 本次启动序号 (单调递增, 见 [`crate::logfile_core`])
    boot_no: u64,
    /// 当前段号 (同一启动内镜像超过上限时递增)
    segment: u32,
    /// 当前段的文件镜像 (每次刷新整文件写, 文件内容 = 镜像)
    image: alloc::vec::Vec<u8>,
    /// 是否已完成目录扫描 (仅首次刷新时执行一次)
    scanned: bool,
}

static STATE: Mutex<FlushState> = Mutex::new(FlushState {
    boot_no: 1,
    segment: 0,
    image: alloc::vec::Vec::new(),
    scanned: false,
});

/// 枚举 `/log` 目录中全部日志文件 `(序号, 段号)` (旧格式 `logN.log`
/// 解析为 `(0, N)`; 非日志文件忽略)
fn collect_log_files(filesystem: &mut filesystem::FileSystem) -> alloc::vec::Vec<(u64, u32)> {
    let mut files: alloc::vec::Vec<(u64, u32)> = alloc::vec::Vec::new();
    let _ = filesystem.read_dir(LOG_DIR, |name, _info| {
        if files.len() >= COLLECT_CAP {
            return;
        }
        let file = logfile_core::parse_segment_name(name)
            .or_else(|| logfile_core::parse_legacy_slot_name(name));
        if let Some(file) = file {
            files.push(file);
        }
    });
    files
}

/// 维护文件预算: 即将写入的新段文件 (若不存在) 会使总数 +1,
/// 超过 [`config::LOG_FILE_SLOTS`] 时删除最旧文件直至满足
fn enforce_budget(filesystem: &mut filesystem::FileSystem, boot_no: u64, segment: u32) {
    let mut name = [0u8; logfile_core::NAME_CAP];
    let name = core::str::from_utf8(logfile_core::segment_name(boot_no, segment, &mut name))
        .expect("段名 ASCII");
    let mut path_buf = [0u8; NAME_BUF];
    let slot = slot_path(name, &mut path_buf);
    let is_new = filesystem.stat(slot).is_err();
    if !is_new {
        return;
    }
    loop {
        let files = collect_log_files(filesystem);
        if files.len() < config::LOG_FILE_SLOTS {
            break;
        }
        let Some(oldest) = logfile_core::oldest_file(&files) else {
            break;
        };
        let mut name = [0u8; logfile_core::NAME_CAP];
        let name = core::str::from_utf8(logfile_core::segment_name(oldest.0, oldest.1, &mut name))
            .expect("段名 ASCII");
        let mut path_buf = [0u8; NAME_BUF];
        let slot = slot_path(name, &mut path_buf);
        if filesystem.remove(slot).is_err() {
            break;
        }
    }
}

/// 段文件名 → `/log/<名>` (栈缓冲)
fn slot_path<'a>(name: &str, out: &'a mut [u8; NAME_BUF]) -> &'a str {
    let mut w = FmtSlice { buf: out, pos: 0 };
    let _ = write!(w, "{}/{}", LOG_DIR, name);
    let pos = w.pos;
    core::str::from_utf8(&out[..pos]).unwrap_or("")
}

/// 固定切片写入器 (渲染短路径)
struct FmtSlice<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl core::fmt::Write for FmtSlice<'_> {
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

/// 执行一次完整的"排空缓冲 → 落盘"循环。
///
/// 由日志线程周期性调用, 也可由其他线程 (shell `reboot`) 同步调用;
/// 两者经文件系统互斥量 + [`STATE`] 串行化, 不会互相覆盖。
///
/// # 分块轮转 (文件上限保证)
///
/// 缓冲按**条目粒度**切块落盘: 每条追加前检查段镜像剩余空间, 放得下
/// 才追加; 放不下即先落盘当前段、段号 +1 另起新段。单个日志文件
/// **恒 ≤ [`config::LOG_FILE_MAX`]**, 唯一的例外是"单条日志 + 段头
/// 标记本身就超过上限"的病态场景 (此时该条目独占一段, 超限部分 =
/// 单条长度, 且会被后续轮转吸收 —— 不再出现旧版"一次刷入整个缓冲
/// 导致段文件超限一个环大小"的问题)。
pub fn flush_once() {
    if !crate::log::file_enabled() {
        return;
    }
    let Some(mut filesystem) = filesystem::mounted() else {
        return;
    };
    // 无待落盘日志时直接返回 (镜像已在文件中)
    if crate::log::pending_bytes() == 0 {
        return;
    }

    let mut state = STATE
        .lock(Timeout::Forever)
        .expect("flush_once 必须在线程上下文");

    // 首次刷新: 扫描目录确定本次启动序号
    if !state.scanned {
        let files = collect_log_files(&mut filesystem);
        state.boot_no = logfile_core::next_boot(logfile_core::newest_boot(&files));
        state.segment = 0;
        state.scanned = true;
    }

    // 分块排空: 段镜像按条目粒度填满即轮转, 单文件恒 ≤ LOG_FILE_MAX
    loop {
        // 新段起始 (启动首写或轮转后首写): 内容标记 (含芯片唯一编号,
        // 日志自含设备身份) + 维护文件预算
        if state.image.is_empty() {
            let mut marker_buf = [0u8; logfile_core::MARKER_CAP];
            let mut uid_buf = [0u8; crate::efm::UID_HEX_CAP];
            let uid = crate::efm::uid_hex(&mut uid_buf);
            let marker = logfile_core::format_boot_marker(state.boot_no, uid, &mut marker_buf);
            state.image.extend_from_slice(marker);
            enforce_budget(&mut filesystem, state.boot_no, state.segment);
        }
        if crate::log::pending_bytes() == 0 {
            break;
        }
        let free = config::LOG_FILE_MAX.saturating_sub(state.image.len());
        if crate::log::drain_limited(&mut state.image, free) == 0 {
            // 首条放不进剩余空间: 强制排出 (超长条目独占一段, 见模块文档)
            if crate::log::drain_oldest(&mut state.image) == 0 {
                // 排空失败 (极端: 条目构建中): 放弃本轮, 下轮再试
                return;
            }
            if write_segment_retry(&mut filesystem, state.boot_no, state.segment, &state.image)
                .is_err()
            {
                return;
            }
            state.segment += 1;
            state.image.clear();
            continue;
        }
        if crate::log::pending_bytes() > 0 {
            // 本段已填满 (或已达上限): 落盘并轮转
            if write_segment_retry(&mut filesystem, state.boot_no, state.segment, &state.image)
                .is_err()
            {
                return;
            }
            state.segment += 1;
            state.image.clear();
        }
    }
    // 收尾: 最后一段 (未满) 也落盘
    if !state.image.is_empty() {
        let _ = write_segment_retry(&mut filesystem, state.boot_no, state.segment, &state.image);
    }
}

/// 落盘一个段文件; 失败时删最旧日志文件重试一次 (仍失败则报错)
fn write_segment_retry(
    filesystem: &mut filesystem::FileSystem,
    boot_no: u64,
    segment: u32,
    image: &[u8],
) -> Result<(), ()> {
    if write_segment(filesystem, boot_no, segment, image).is_ok() {
        return Ok(());
    }
    // 空间不足等: 删最旧日志文件后重试一次, 仍失败则丢弃本轮
    if free_oldest(filesystem).is_err() {
        println!("logfile: 无法释放日志文件");
        return Err(());
    }
    if let Err(error) = write_segment(filesystem, boot_no, segment, image) {
        println!("logfile: 日志落盘失败: {}", fs_error_summary(&error));
        return Err(());
    }
    Ok(())
}

/// 立即把缓冲中的日志落盘 (供 `reboot` 在复位前调用; 落盘开关关闭时为无操作)
pub fn flush_now() {
    flush_once();
}

/// 日志落盘线程入口 (在 `main` 中创建; 永不返回)
pub extern "C" fn logfile_entry(_param: usize) {
    loop {
        crate::rtos::thread_delay_ms(config::LOG_FLUSH_MS).ok();
        flush_once();
    }
}

/// 整文件原子写一个日志段 (确保 `/log` 目录存在)
fn write_segment(
    filesystem: &mut filesystem::FileSystem,
    boot_no: u64,
    segment: u32,
    image: &[u8],
) -> Result<(), FsError> {
    if filesystem.stat(LOG_DIR).is_err() {
        filesystem.mkdir(LOG_DIR)?;
    }
    let mut name = [0u8; logfile_core::NAME_CAP];
    let name = core::str::from_utf8(logfile_core::segment_name(boot_no, segment, &mut name))
        .expect("段名 ASCII");
    let mut path = [0u8; NAME_BUF];
    let path = slot_path(name, &mut path);
    filesystem.write(path, image)
}

/// 删除最旧的日志文件 (按 `(序号, 段号)` 判断; 旧格式文件视为最旧)
fn free_oldest(filesystem: &mut filesystem::FileSystem) -> Result<(), FsError> {
    let files = collect_log_files(filesystem);
    let Some(oldest) = logfile_core::oldest_file(&files) else {
        return Err(FsError::NotFound);
    };
    let mut name = [0u8; logfile_core::NAME_CAP];
    let name = core::str::from_utf8(logfile_core::segment_name(oldest.0, oldest.1, &mut name))
        .expect("段名 ASCII");
    let mut path_buf = [0u8; NAME_BUF];
    let path = slot_path(name, &mut path_buf);
    filesystem.remove(path)?;
    Ok(())
}

type FsError = littlefs::Error<crate::filesystem::FlashError>;

fn fs_error_summary(error: &FsError) -> &'static str {
    match error {
        littlefs::Error::Device(_) => "Flash 设备错误",
        littlefs::Error::NoSpace => "文件系统空间不足",
        littlefs::Error::AlreadyExists => "目标已存在",
        littlefs::Error::NotFound => "未找到",
        littlefs::Error::IsDirectory => "目标是目录",
        _ => "其他文件系统错误",
    }
}
