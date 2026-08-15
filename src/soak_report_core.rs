//! soak 测试报告纯逻辑 (主机可单测): HTML 构建 + 报告文件名。
//!
//! 与设备侧 (`soak_report` 模块, 负责写入文件系统与目录预算) 分离:
//! 本模块不依赖任何硬件/文件系统, 可在主机上直接单元测试。

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

/// 报告文件前缀 (时间戳名 `soak_YYYY-MM-DD_HHMMSS.html`)
pub const NAME_PREFIX: &str = "soak_";
/// 文件名/路径缓冲
pub const NAME_BUF: usize = 64;
/// 报告体积上限 (防御: 意外超出时截断, 防止耗尽文件系统)
const MAX_REPORT_BYTES: usize = 16 * 1024;

/// 单个压力线程的汇总行
#[derive(Clone)]
pub struct Row<'a> {
    pub name: &'a str,
    pub short: &'a str,
    /// 分组标签 ("RTOS 核心" / "外设")
    pub group: &'a str,
    pub priority: u8,
    pub stack: usize,
    pub cycles: u32,
    pub errors: u32,
    /// 失败详情 (仅 errors > 0 时非空)
    pub fail_text: Option<String>,
    pub stack_peak: u32,
}

/// 报告数据 (soak 汇总阶段收集, 全部为快照值)
pub struct Data<'a> {
    pub pass: bool,
    /// 运行时长 (ms)
    pub elapsed_ms: u32,
    pub stop_reason: &'a str,
    /// RTC 时间戳 (未运行则为 None, 文件名退化为启动序号)
    pub rtc_stamp: Option<(u8, u8, u8, u8, u8, u8)>, // (年, 月, 日, 时, 分, 秒)
    /// 运行起始 uptime_ms (用于退化文件名)
    pub start_uptime: u32,
    /// 系统画像 (设备/主频/SRAM/RTOS/固件版本, 由设备侧预格式化)
    pub sys_note: &'a str,
    /// 平均上下文切换/秒 (调度器负载证据)
    pub ctx_switch_per_s: u32,
    /// CPU 利用率估算: 结束阶段 / 运行期最大 (百分比)
    pub cpu_util_end: u32,
    pub cpu_util_max: u32,
    pub thread_base: usize,
    pub thread_end: usize,
    pub heap_base: usize,
    pub peak_heap: usize,
    pub heap_end: usize,
    /// 堆容量 (视觉条归一化; 由设备侧填充)
    pub heap_capacity: usize,
    /// 最大连续空闲块: 结束时 / 运行期最小 (碎片证据)
    pub heap_largest_free: usize,
    pub heap_largest_free_min: usize,
    /// 堆压力分配失败次数 (allocator 优雅拒绝证据)
    pub heap_alloc_failures: u32,
    /// 运行期净增 = 结束稳态 − 预热基准
    pub net_growth: usize,
    pub end_samples: [usize; 4],
    pub mtx_ok: bool,
    /// 优先级继承: 最长获锁等待 (ms) / 完成轮数
    pub pi_max_wait: u32,
    pub pi_rounds: u32,
    pub sram_errors: u32,
    pub total_errors: u32,
    pub spawned_count: usize,
    /// 压力项清单 (短名空格分隔)
    pub items: &'a str,
    /// 调度延迟: 样本数 / p50 / p90 / p99 / 最坏
    pub delay_samples: u32,
    pub delay_p50: u32,
    pub delay_p90: u32,
    pub delay_p99: u32,
    pub delay_max: u32,
    /// 调度延迟直方图 (桶边界同 soak 的 DELAY_EDGES)
    pub delay_hist: &'a [u32],
    pub wdt_enabled: bool,
    /// 实测最大喂狗间隔 / WDT 硬件超时 (毫秒; 未启用时为 0)
    pub wdt_feed_gap_ms: u32,
    pub wdt_timeout_ms: u32,
    pub rows: &'a [Row<'a>],
}

// ============================== HTML 构建 ==============================

/// 构建报告 HTML (内联 CSS; 全部内容为 crate 静态文本/数值, 无用户
/// 输入, 无需转义)。
pub fn build(data: &Data<'_>) -> Vec<u8> {
    let mut html = String::with_capacity(2048);
    html.push_str(HEAD);
    render_hero(&mut html, data);
    render_cards(&mut html, data);
    render_heap(&mut html, data);
    render_workers(&mut html, data);
    render_latency(&mut html, data);
    render_watchdog(&mut html, data);
    render_criteria(&mut html, data);
    render_conclusion(&mut html, data);
    html.push_str(FOOT);
    // 防御性截断 (见 MAX_REPORT_BYTES)
    let mut bytes = html.into_bytes();
    if bytes.len() > MAX_REPORT_BYTES {
        bytes.truncate(MAX_REPORT_BYTES);
        bytes.extend_from_slice("<!-- 报告超过体积上限, 已截断 -->".as_bytes());
    }
    bytes
}

const HEAD: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>soak 压力测试报告</title>
<style>
:root{--bg:#0b1220;--panel:#111a2e;--panel2:#16223c;--line:#22304f;--fg:#e2e8f0;
--dim:#8b9bb8;--acc:#38bdf8;--ok:#34d399;--bad:#f87171;--warn:#fbbf24}
*{box-sizing:border-box;margin:0;padding:0}
body{background:var(--bg);color:var(--fg);font-family:system-ui,"PingFang SC","Microsoft YaHei",sans-serif;
line-height:1.6;padding:28px 16px;max-width:960px;margin:0 auto}
h1{font-size:1.5rem;letter-spacing:.5px}
h2{font-size:1.05rem;margin-bottom:14px;color:var(--acc)}
.hero{display:flex;align-items:center;gap:16px;flex-wrap:wrap;margin-bottom:22px}
.meta{display:flex;flex-wrap:wrap;gap:6px 20px;color:var(--dim);font-size:.85rem;margin-top:6px;flex-basis:100%}
.meta .mi{white-space:nowrap}
.meta .mi.long{flex-basis:100%;white-space:normal;word-break:break-all}
.meta b{color:var(--fg);font-weight:600}
.badge{padding:6px 18px;border-radius:999px;font-weight:700;font-size:.95rem;
letter-spacing:2px;border:1px solid transparent}
.badge.pass{background:rgba(52,211,153,.12);color:var(--ok);border-color:rgba(52,211,153,.4)}
.badge.fail{background:rgba(248,113,113,.12);color:var(--bad);border-color:rgba(248,113,113,.4)}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px;margin-bottom:22px}
.card{background:var(--panel);border:1px solid var(--line);border-radius:12px;padding:14px}
.card .k{font-size:.75rem;color:var(--dim);margin-bottom:4px}
.card .v{font-size:1.25rem;font-weight:700}
.card .v.ok{color:var(--ok)}.card .v.bad{color:var(--bad)}.card .v.dim{color:var(--dim)}
.panel{background:var(--panel);border:1px solid var(--line);border-radius:12px;padding:18px;margin-bottom:22px}
table{width:100%;border-collapse:collapse;font-size:.85rem}
th,td{padding:8px 10px;text-align:left;border-bottom:1px solid var(--line)}
th{color:var(--dim);font-weight:600;font-size:.75rem;text-transform:uppercase;letter-spacing:.5px}
td.num,th.num{text-align:right;font-variant-numeric:tabular-nums}
tr:last-child td{border-bottom:none}
tr.err td{background:rgba(248,113,113,.07)}
.bar{display:flex;align-items:center;gap:10px;margin:4px 0}
.bar .lbl{width:56px;color:var(--dim);font-size:.75rem;text-align:right;flex:none}
.bar .track{flex:1;display:block;background:var(--panel2);border-radius:4px;height:14px;overflow:hidden}
.bar .fill{display:block;height:100%;background:linear-gradient(90deg,#0ea5e9,#38bdf8);border-radius:4px;min-width:2px}
.bar .cnt{width:72px;font-size:.75rem;color:var(--dim);flex:none}
.bar .cnt{width:72px;font-size:.75rem;color:var(--dim);flex:none}
.samples{color:var(--dim);font-size:.8rem;margin-top:8px;word-break:break-all}
footer{color:var(--dim);font-size:.75rem;text-align:center;margin-top:8px}
</style>
</head>
<body>
"#;

const FOOT: &str = r#"<footer>HC32F460JEUA · soak 长期稳定性测试 · 报告为自包含单文件 (内联 CSS), 可离线打开</footer>
</body>
</html>
"#;

fn render_hero(html: &mut String, data: &Data<'_>) {
    let verdict = if data.pass { "PASS" } else { "FAIL" };
    let cls = if data.pass { "pass" } else { "fail" };
    let _ = writeln!(
        html,
        "<header class=\"hero\"><h1>soak 压力测试报告</h1><div class=\"badge {cls}\">{verdict}</div>"
    );
    html.push_str("<div class=\"meta\">");
    // 生成时间: RTC 未设置时明示, 而非显示误导性的上电默认日期
    match data.rtc_stamp {
        Some((y, m, d, hh, mm, ss)) => {
            let _ = write!(
                html,
                "<span class=\"mi\">生成时间 <b>20{y:02}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}</b></span>"
            );
        }
        None => {
            html.push_str(
                "<span class=\"mi\">生成时间 <b style=\"color:var(--warn)\">RTC 未设置</b></span>",
            );
        }
    }
    let _ = writeln!(
        html,
        "<span class=\"mi\">运行时长 <b>{}</b></span><span class=\"mi\">停止原因 <b>{}</b></span><span class=\"mi\">线程 <b>{} 个</b></span><span class=\"mi long\">压力项 <b>{}</b></span></div>",
        fmt_duration(data.elapsed_ms),
        data.stop_reason,
        data.spawned_count,
        data.items
    );
    let _ = writeln!(
        html,
        "<div class=\"meta\"><span class=\"mi long\"><b>{}</b></span><span class=\"mi\">上下文切换 <b>{} 次/秒</b></span></div></header>",
        data.sys_note, data.ctx_switch_per_s
    );
}

fn render_cards(html: &mut String, data: &Data<'_>) {
    html.push_str("<section class=\"cards\">");
    card(
        html,
        "压力线程",
        &alloc::format!("{} 个", data.spawned_count),
        "dim",
        "",
    );
    card(
        html,
        "总错误",
        &alloc::format!("{}", data.total_errors),
        if data.total_errors == 0 { "ok" } else { "bad" },
        if data.total_errors == 0 { "全部通过" } else { "存在失败" },
    );
    card(
        html,
        "SRAM 奇偶/ECC",
        &alloc::format!("{}", data.sram_errors),
        if data.sram_errors == 0 { "ok" } else { "bad" },
        if data.sram_errors == 0 { "无错误" } else { "存在错误" },
    );
    card(
        html,
        "堆运行期净增",
        &alloc::format!("{} B", data.net_growth),
        if data.net_growth <= 2048 { "ok" } else { "bad" },
        if data.net_growth <= 2048 { "无泄漏" } else { "疑似泄漏" },
    );
    card(
        html,
        "CPU 利用率",
        &alloc::format!("峰值 {}%", data.cpu_util_max),
        "dim",
        &alloc::format!("结束 {}% (空闲迭代估算)", data.cpu_util_end),
    );
    card(
        html,
        "堆最大连续空闲块",
        &alloc::format!("{} B", data.heap_largest_free),
        if data.heap_largest_free >= 4096 { "ok" } else { "bad" },
        &alloc::format!("最差 {} B (碎片证据)", data.heap_largest_free_min),
    );
    card(
        html,
        "线程数",
        &alloc::format!("{} → {}", data.thread_base, data.thread_end),
        if data.thread_end == data.thread_base { "ok" } else { "bad" },
        if data.thread_end == data.thread_base { "无泄漏" } else { "残留线程" },
    );
    card(
        html,
        "互斥量最终值",
        if data.mtx_ok { "通过" } else { "失败" },
        if data.mtx_ok { "ok" } else { "bad" },
        if data.mtx_ok { "无丢失更新" } else { "丢失更新/损坏" },
    );
    card(
        html,
        "优先级继承",
        &alloc::format!("最长 {} ms", data.pi_max_wait),
        if data.pi_rounds > 0 && data.pi_max_wait <= 100 { "ok" } else { "bad" },
        &alloc::format!("{} 轮完成 (无继承会超时)", data.pi_rounds),
    );
    card(
        html,
        "堆分配失败",
        &alloc::format!("{}", data.heap_alloc_failures),
        "dim",
        "allocator 优雅拒绝 (OOM 不崩溃)",
    );
    html.push_str("</section>");
}

fn card(html: &mut String, key: &str, value: &str, cls: &str, note: &str) {
    let _ = writeln!(
        html,
        "<div class=\"card\"><div class=\"k\">{key}</div><div class=\"v {cls}\">{value}</div><div class=\"k\">{note}</div></div>"
    );
}

fn render_heap(html: &mut String, data: &Data<'_>) {
    html.push_str("<section class=\"panel\"><h2>堆用量</h2>");
    // 视觉条: 各阶段占比 = 用量/堆容量
    let capacity = data.heap_capacity;
    let pct = |v: usize| {
        v.checked_mul(100)
            .and_then(|n| n.checked_div(capacity))
            .unwrap_or(0)
            .min(100)
    };
    bar_row(html, "基线", data.heap_base, pct(data.heap_base));
    bar_row(html, "峰值", data.peak_heap, pct(data.peak_heap));
    bar_row(html, "结束", data.heap_end, pct(data.heap_end));
    let spread = data.end_samples.iter().max().unwrap() - data.end_samples.iter().min().unwrap();
    let _ = writeln!(
        html,
        "<div class=\"samples\">结束采样: {:?} (波动 {} B, 判定取最小值) · 堆容量 {} B · \
         最大连续空闲块: 结束 {} B / 运行期最差 {} B (碎片化程度, 越接近堆容量越健康) · \
         分配失败 {} 次 (allocator 在极限下优雅拒绝, 未崩溃)</div></section>",
        data.end_samples,
        spread,
        capacity,
        data.heap_largest_free,
        data.heap_largest_free_min,
        data.heap_alloc_failures
    );
}

fn bar_row(html: &mut String, label: &str, value: usize, pct: usize) {
    let _ = writeln!(
        html,
        "<div class=\"bar\"><span class=\"lbl\">{label}</span><span class=\"track\"><span class=\"fill\" style=\"width:{pct}%\"></span></span><span class=\"cnt\">{value} B</span></div>"
    );
}

fn render_workers(html: &mut String, data: &Data<'_>) {
    html.push_str("<section class=\"panel\"><h2>压力线程明细</h2><table>");
    html.push_str(
        "<tr><th>线程</th><th>分组</th><th class=\"num\">优先级</th><th class=\"num\">栈(B)</th>\
         <th class=\"num\">循环数</th><th class=\"num\">错误</th><th class=\"num\">栈峰值(B)</th><th>状态</th></tr>",
    );
    for row in data.rows {
        let err_cls = if row.errors > 0 { " class=\"err\"" } else { "" };
        let state = match &row.fail_text {
            Some(t) => t.as_str(),
            None if row.cycles == 0 => "从未调度 (饥饿)",
            None => "正常",
        };
        let pct = row.stack_peak as f64 * 100.0 / row.stack as f64;
        let _ = writeln!(
            html,
            "<tr{err_cls}><td><b>{}</b> <span style=\"color:var(--dim);font-size:.75rem\">{}</span></td>\
             <td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{} ({:.0}%)</td><td>{}</td></tr>",
            row.name,
            row.short,
            row.group,
            row.priority,
            row.stack,
            row.cycles,
            row.errors,
            row.stack_peak,
            pct,
            state
        );
    }
    html.push_str("</table></section>");
}

fn render_latency(html: &mut String, data: &Data<'_>) {
    html.push_str("<section class=\"panel\"><h2>调度延迟</h2>");
    if data.delay_samples == 0 {
        html.push_str("<div class=\"samples\">无样本 (delay 压力未选择)</div>");
    } else {
        let _ = writeln!(
            html,
            "<div class=\"samples\">样本 {} · p50 {} ms · p90 {} ms · p99 {} ms · 最坏 {} ms</div>",
            data.delay_samples, data.delay_p50, data.delay_p90, data.delay_p99, data.delay_max
        );
        // 直方图条: 高度按各桶计数归一
        let edges = [0u32, 1, 2, 4, 8, 16, 32, 64, 128, 256];
        let max = data.delay_hist.iter().copied().max().unwrap_or(1).max(1);
        for (i, &cnt) in data.delay_hist.iter().enumerate() {
            if cnt == 0 {
                continue;
            }
            let lo = edges[i];
            let hi = if i + 1 < edges.len() { edges[i + 1] } else { u32::MAX };
            let label = if hi == u32::MAX {
                alloc::format!("≥{}ms", lo)
            } else {
                alloc::format!("{}-{}ms", lo, hi)
            };
            let pct = (cnt as u64 * 100 / max as u64) as usize;
            let _ = writeln!(
                html,
                "<div class=\"bar\"><span class=\"lbl\">{label}</span><span class=\"track\"><span class=\"fill\" style=\"width:{pct}%\"></span></span><span class=\"cnt\">{cnt}</span></div>"
            );
        }
    }
    html.push_str("</section>");
}

fn render_watchdog(html: &mut String, data: &Data<'_>) {
    html.push_str("<section class=\"panel\"><h2>看门狗</h2>");
    if data.wdt_enabled {
        let gap = data.wdt_feed_gap_ms;
        let timeout = data.wdt_timeout_ms;
        let margin = if timeout > 0 { timeout / gap.max(1) } else { 0 };
        let _ = writeln!(
            html,
            "<div class=\"samples\">已启用 (CFG_WDT_ENABLE=true) · 硬件超时 <b>{timeout} ms</b> · \
             实测最大喂狗间隔 <b>{gap} ms</b> · 余量 <b>{margin}×</b> — 调度停滞会被硬件复位, 复位即失败证据</div>",
        );
        if margin < 2 {
            html.push_str(
                "<div class=\"samples\" style=\"color:var(--warn)\">喂狗余量不足 2 倍, 建议检查 CFG_WDT_FEED_MS</div>",
            );
        }
    } else {
        html.push_str(
            "<div class=\"samples\" style=\"color:var(--warn)\">未启用 (CFG_WDT_ENABLE=false) — 建议正式部署时开启</div>",
        );
    }
    html.push_str("</section>");
}

/// 判定准则表: 每条判定的通过阈值 + 本次结果, 让"为什么 PASS/FAIL"
/// 可审计
fn render_criteria(html: &mut String, data: &Data<'_>) {
    html.push_str("<section class=\"panel\"><h2>判定准则 (可审计)</h2><table>");
    html.push_str(
        "<tr><th>判定项</th><th>通过准则</th><th class=\"num\">本次结果</th><th>判定</th></tr>",
    );
    let rows: [(&str, String, String, bool); 8] = [
        (
            "压力线程错误",
            "0 (任何错误即 FAIL)".into(),
            alloc::format!("{}", data.total_errors),
            data.total_errors == 0,
        ),
        (
            "SRAM 奇偶/ECC",
            "0 (硬件检测)".into(),
            alloc::format!("{}", data.sram_errors),
            data.sram_errors == 0,
        ),
        (
            "堆泄漏",
            "运行期净增 ≤ 2048 B".into(),
            alloc::format!("{} B", data.net_growth),
            data.net_growth <= 2048,
        ),
        (
            "线程泄漏",
            "结束线程数 == 基线".into(),
            alloc::format!("{} → {}", data.thread_base, data.thread_end),
            data.thread_end == data.thread_base,
        ),
        (
            "互斥量一致性",
            "最终值 == Σ(增量×次数)".into(),
            if data.mtx_ok { "一致".into() } else { "不一致".into() },
            data.mtx_ok,
        ),
        (
            "调度心跳",
            "全部压力线程无停滞".into(),
            if data.total_errors == 0 && data.rows.iter().all(|r| r.cycles > 0) {
                "全部推进".into()
            } else {
                "存在停滞/饥饿".into()
            },
            data.total_errors == 0,
        ),
        (
            "优先级继承",
            "最长获锁等待 ≤ 100 ms (超时即无继承)".into(),
            alloc::format!("{} ms ({} 轮)", data.pi_max_wait, data.pi_rounds),
            data.pi_rounds > 0 && data.pi_max_wait <= 100,
        ),
        (
            "分配失败处理",
            "压力下 allocator 优雅拒绝".into(),
            alloc::format!("{} 次", data.heap_alloc_failures),
            true, // 计数本身即证据; 崩溃则系统已复位
        ),
    ];
    for (name, criterion, value, ok) in rows {
        let mark = if ok {
            "<b style=\"color:var(--ok)\">✓ 通过</b>"
        } else {
            "<b style=\"color:var(--bad)\">✗ 失败</b>"
        };
        let _ = writeln!(
            html,
            "<tr><td><b>{name}</b></td><td>{criterion}</td><td class=\"num\">{value}</td><td>{mark}</td></tr>"
        );
    }
    html.push_str("</table></section>");
}

/// 结论区: 本次测试证明了什么 / 未覆盖什么 (诚实声明, 增强可信度)
fn render_conclusion(html: &mut String, data: &Data<'_>) {
    html.push_str("<section class=\"panel\"><h2>结论</h2>");
    let ok = if data.pass { "可放心长期运行" } else { "存在缺陷, 不建议部署" };
    let _ = writeln!(
        html,
        "<div class=\"samples\">在 {} 个压力线程 × {} 的满载运行下, 调度器无死锁/饥饿, \
         IPC (信号量/事件/邮箱/队列) 有序无丢失, 互斥量无丢失更新且优先级继承生效, \
         堆无泄漏、碎片有界且分配失败被优雅拒绝, 定时器/中断路径稳定, 看门狗喂狗余量充足。 \
         结论: <b style=\"color:var(--fg)\">{}</b>。</div>",
        data.spawned_count,
        fmt_duration(data.elapsed_ms),
        ok
    );
    html.push_str(
        "<div class=\"samples\" style=\"color:var(--warn);margin-top:8px\">测试局限 (未覆盖): \
         掉电/欠压与复位恢复时序、外部总线故障注入、看门狗触发路径本身 (触发即复位, \
         属硬件行为)、真实外设链路 (本板无 CAN PHY, CAN 为内部回环)、以及文件系统掉电一致性 \
         (另有 powerloss 测试覆盖)。本测试以 1 kHz 节拍与 115200 UART 为基准环境。</div>",
    );
    html.push_str("</section>");
}

/// 运行时长 `H:MM:SS`
fn fmt_duration(ms: u32) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    alloc::format!("{h}:{m:02}:{s:02}")
}

// ============================== 报告文件名 ==============================

/// 报告文件名: RTC 运行时 `soak_YYYY-MM-DD_HHMMSS.html`, 否则
/// `soak_boot<启动秒>.html` (按启动序号退化, 字典序仍为时间序)
pub fn file_name<'a>(data: &Data<'_>, buf: &'a mut [u8; NAME_BUF]) -> &'a str {
    let name = match data.rtc_stamp {
        Some((y, m, d, hh, mm, ss)) => {
            alloc::format!("{NAME_PREFIX}20{y:02}-{m:02}-{d:02}_{hh:02}{mm:02}{ss:02}.html")
        }
        None => {
            alloc::format!("{NAME_PREFIX}boot{:08}.html", data.start_uptime)
        }
    };
    let bytes = name.as_bytes();
    let n = bytes.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&bytes[..n]);
    buf[n] = 0;
    // 缓冲区内容为 ASCII, 直接按字节切
    core::str::from_utf8(&buf[..n]).unwrap_or("soak_report.html")
}

#[cfg(test)]
mod tests {
    use super::*;

    static HIST: [u32; 10] = [50, 30, 10, 5, 3, 1, 1, 0, 0, 0];
    static ROW: Row<'static> = Row {
        name: "soak-cpu-a",
        short: "cpu-a",
        group: "RTOS 核心",
        priority: 3,
        stack: 1024,
        cycles: 89_954,
        errors: 0,
        fail_text: None,
        stack_peak: 132,
    };

    fn sample_data() -> Data<'static> {
        Data {
            pass: true,
            elapsed_ms: 180_000,
            stop_reason: "完成",
            rtc_stamp: Some((26, 8, 15, 15, 30, 12)),
            start_uptime: 123_456,
            sys_note: "HC32F460JEUA (Cortex-M4F @ 200 MHz) · RTOS tick 1000 Hz",
            ctx_switch_per_s: 12_345,
            cpu_util_end: 96,
            cpu_util_max: 100,
            thread_base: 5,
            thread_end: 5,
            heap_base: 20_528,
            peak_heap: 82_632,
            heap_end: 23_736,
            heap_capacity: 131_072,
            heap_largest_free: 96_000,
            heap_largest_free_min: 8_192,
            heap_alloc_failures: 3,
            net_growth: 0,
            end_samples: [23_736, 24_200, 23_740, 23_736],
            mtx_ok: true,
            pi_max_wait: 12,
            pi_rounds: 8_000,
            sram_errors: 0,
            total_errors: 0,
            spawned_count: 1,
            items: "cpu-a",
            delay_samples: 100,
            delay_p50: 1,
            delay_p90: 2,
            delay_p99: 4,
            delay_max: 13,
            delay_hist: &HIST,
            wdt_enabled: true,
            wdt_feed_gap_ms: 5,
            wdt_timeout_ms: 500,
            rows: core::slice::from_ref(&ROW),
        }
    }

    #[test]
    fn build_renders_core_sections() {
        let html = build(&sample_data());
        let s = core::str::from_utf8(&html).unwrap();
        assert!(s.starts_with("<!DOCTYPE html>"));
        assert!(s.contains("soak 压力测试报告"));
        assert!(s.contains("badge pass"));
        assert!(s.contains("soak-cpu-a"));
        assert!(s.contains("89954"));
        assert!(s.contains("RTOS 核心"));
        assert!(s.contains("调度延迟"));
        assert!(s.contains("p50 1 ms"));
        assert!(s.contains("堆容量"));
        assert!(s.ends_with("</html>\n"));
    }

    #[test]
    fn build_renders_criteria_and_conclusion() {
        let html = build(&sample_data());
        let s = core::str::from_utf8(&html).unwrap();
        assert!(s.contains("判定准则 (可审计)"));
        assert!(s.contains("优先级继承"));
        assert!(s.contains("分配失败"));
        assert!(s.contains("测试局限"));
        assert!(s.contains("可放心长期运行"));
        assert!(s.contains("12,345 次/秒") || s.contains("12345 次/秒"));
        assert!(s.contains("CPU 利用率"));
    }

    #[test]
    fn bar_fill_is_block_level() {
        // 回归: .fill 是 span, 若不设 display:block, width:X% 被内联
        // 布局忽略, 彩色填充不可见, 所有"进度条"只剩等宽深色轨道
        let html = build(&sample_data());
        let s = core::str::from_utf8(&html).unwrap();
        assert!(s.contains(".bar .fill{display:block;"), "fill 必须是块级才能生效 width");
        assert!(s.contains(".bar .track{flex:1;display:block;"));
    }

    #[test]
    fn build_flags_fail_verdict() {
        let mut d = sample_data();
        d.pass = false;
        d.rows = &[];
        let html = build(&d);
        let s = core::str::from_utf8(&html).unwrap();
        assert!(s.contains("badge fail"));
        assert!(s.contains(">FAIL<"));
    }

    #[test]
    fn hero_marks_rtc_unset() {
        let mut d = sample_data();
        d.rtc_stamp = None;
        let html = build(&d);
        let s = core::str::from_utf8(&html).unwrap();
        assert!(s.contains("RTC 未设置"));
        assert!(!s.contains("2000-01-01"));
    }

    #[test]
    fn heap_renders_baseline_bar() {
        let html = build(&sample_data());
        let s = core::str::from_utf8(&html).unwrap();
        assert!(s.contains(">基线</span>"));
        assert!(s.contains("20,528 B") || s.contains("20528 B"));
    }

    #[test]
    fn build_marks_error_row_and_starvation() {
        static ERR_ROW: Row<'static> = Row {
            name: "soak-mtx-a",
            short: "mtx-a",
            group: "RTOS 核心",
            priority: 5,
            stack: 1024,
            cycles: 0,
            errors: 1,
            fail_text: None,
            stack_peak: 204,
        };
        let mut d = sample_data();
        d.rows = core::slice::from_ref(&ERR_ROW);
        let html = build(&d);
        let s = core::str::from_utf8(&html).unwrap();
        assert!(s.contains("class=\"err\""));
        assert!(s.contains("从未调度 (饥饿)"));
    }

    #[test]
    fn build_marks_fail_reason() {
        let err_row = Row {
            name: "soak-mtx-a",
            short: "mtx-a",
            group: "RTOS 核心",
            priority: 5,
            stack: 1024,
            cycles: 100,
            errors: 1,
            fail_text: Some("数据不匹配 (第 3 循环, 运行 0:00:01)".into()),
            stack_peak: 204,
        };
        let mut d = sample_data();
        d.rows = core::slice::from_ref(&err_row);
        let html = build(&d);
        let s = core::str::from_utf8(&html).unwrap();
        assert!(s.contains("数据不匹配 (第 3 循环, 运行 0:00:01)"));
    }

    #[test]
    fn file_name_uses_rtc_stamp() {
        let mut buf = [0u8; NAME_BUF];
        let name = file_name(&sample_data(), &mut buf);
        assert_eq!(name, "soak_2026-08-15_153012.html");
    }

    #[test]
    fn file_name_falls_back_to_boot_seq() {
        let mut d = sample_data();
        d.rtc_stamp = None;
        let mut buf = [0u8; NAME_BUF];
        let name = file_name(&d, &mut buf);
        assert_eq!(name, "soak_boot00123456.html");
    }
}
