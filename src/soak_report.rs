//! soak 测试结果 → `/test/` 单文件 HTML 报告 (设备侧)。
//!
//! 压力测试运行期间控制台只刷新进度条, 测试结果/数据/关键信息全部
//! 汇总为单个自包含 HTML (内联 CSS, 无外部依赖), 写入文件系统
//! `/test/soak_<时间戳>.html`。HTML 构建与文件名等纯逻辑在
//! [`crate::soak_report_core`] (主机可单测), 本模块负责文件系统
//! 写入、启动序号分配与目录预算。
//!
//! # 跨复位排序
//!
//! RTC 未设置时报告名退化为 `soak_b<启动序号>_<uptime>.html`: 启动序号
//! 由本模块扫描 `/test` 目录取最大序号 +1 (跨复位单调), uptime 零填充;
//! 预算删除按 [`crate::soak_report_core::older_than`] 比较, 复位后
//! uptime 归零也不破坏时间序 (旧版仅按 uptime 命名, 跨复位会删错文件)。

use alloc::string::String;
use alloc::vec::Vec;

/// 报告目录名 (相对文件系统根)
const REPORT_DIR: &str = "test";
/// 目录枚举缓冲 (预算 + 冗余)
const COLLECT_CAP: usize = 32;

/// 文件系统错误 (与 logfile 模块同一类型)
type FsError = littlefs::Error<crate::filesystem::FlashError>;

/// 写入报告到 `/test/soak_<时间戳>.html`, 维护预算 (保留最新
/// [`crate::config::TEST_REPORT_SLOTS`] 个), 返回报告路径。
pub(crate) fn save(
    filesystem: &mut crate::filesystem::FileSystem,
    bytes: &[u8],
    data: &crate::soak_report_core::Data<'_>,
) -> Result<String, FsError> {
    if filesystem.stat(REPORT_DIR).is_err() {
        filesystem.mkdir(REPORT_DIR)?;
    }
    // 退化文件名的启动序号: 扫描现有报告取最大 +1 (跨复位单调)
    let boot_seq = next_boot_seq(filesystem)?;
    enforce_budget(filesystem)?;

    let mut name = [0u8; crate::soak_report_core::NAME_BUF];
    let name = crate::soak_report_core::file_name(data, boot_seq, &mut name);
    let path = alloc::format!("{REPORT_DIR}/{name}");
    filesystem.write(&path, bytes)?;
    Ok(path)
}

/// 现有退化报告名的最大启动序号 +1 (无历史时为 1)。
fn next_boot_seq(filesystem: &mut crate::filesystem::FileSystem) -> Result<u64, FsError> {
    let mut max: u64 = 0;
    filesystem.read_dir(REPORT_DIR, |name, _info| {
        if let Some((seq, _)) = crate::soak_report_core::parse_seq_name(name) {
            max = max.max(seq);
        }
    })?;
    Ok(max.saturating_add(1).max(1))
}

/// 维护报告预算: 枚举 `/test` 目录, 超出上限时删除时间序最前的
/// (即最旧) 报告文件。排序经 [`crate::soak_report_core::older_than`]:
/// RTC 名按字典序 (即时间序), 退化名按 (启动序号, uptime) —— 跨复位
/// 仍正确 (见模块文档)。
fn enforce_budget(filesystem: &mut crate::filesystem::FileSystem) -> Result<(), FsError> {
    let mut names: Vec<String> = Vec::new();
    filesystem.read_dir(REPORT_DIR, |name, _info| {
        if names.len() >= COLLECT_CAP {
            return;
        }
        if name.ends_with(".html") {
            names.push(name.into());
        }
    })?;
    // 插入排序 (≤ COLLECT_CAP=32 项): 避免链接 core 的泛型快速排序机器 (~2.4KiB)
    for i in 1..names.len() {
        let mut j = i;
        while j > 0 && crate::soak_report_core::older_than(&names[j], &names[j - 1]) {
            names.swap(j, j - 1);
            j -= 1;
        }
    }
    let slots = crate::config::TEST_REPORT_SLOTS as usize;
    while names.len() > slots {
        let oldest = names.remove(0);
        filesystem.remove(&alloc::format!("{REPORT_DIR}/{oldest}"))?;
    }
    Ok(())
}
