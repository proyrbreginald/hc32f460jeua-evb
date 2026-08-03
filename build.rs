//! 构建脚本: 仅注入启动横幅所需的构建日期与 rustc 版本。
//!
//! 工程配置不在本脚本中 — 全部集中在 `.cargo/config.toml` 的 `[env]`
//! 段, 由 `src/config.rs` 编译期读取。

fn main() {
    // 构建日期 (UTC, 公历)
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = unix_to_ymd(secs);
    println!("cargo:rustc-env=RTOS_BUILD_DATE={:04}-{:02}-{:02}", y, m, d);

    // rustc 版本
    if let Ok(out) = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        && let Ok(s) = String::from_utf8(out.stdout)
    {
        println!("cargo:rustc-env=RTOS_RUSTC={}", s.trim());
    }
}

/// Unix 秒 → (年, 月, 日) (Howard Hinnant 公历算法, 无依赖)
fn unix_to_ymd(secs: u64) -> (i64, u32, u32) {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
