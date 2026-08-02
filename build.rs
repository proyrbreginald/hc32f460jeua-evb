//! 构建脚本: 为启动横幅 (banner) 生成构建日期与 rustc 版本等元数据
//!
//! 通过 `cargo:rustc-env` 注入编译期环境变量, crate 内以
//! `env!("RTOS_BUILD_DATE")` / `env!("RTOS_RUSTC")` 读取。
//! 无第三方依赖。

fn main() {
    // 构建日期 (UTC, 公历)
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = unix_to_ymd(secs);
    println!(
        "cargo:rustc-env=RTOS_BUILD_DATE={:04}-{:02}-{:02}",
        y, m, d
    );

    // rustc 版本
    if let Ok(out) = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        && let Ok(s) = String::from_utf8(out.stdout)
    {
        println!("cargo:rustc-env=RTOS_RUSTC={}", s.trim());
    }

    // shell 配置: 读取源码目录下的 shell.conf (KEY=VALUE 行, # 为注释)
    inject_shell_conf();
}

/// 读取 `shell.conf` 并注入编译期环境变量 (缺失时用内置默认值)
fn inject_shell_conf() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("shell.conf");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut values: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            values.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    // 密码等敏感字段不随编译命令输出
    let username = values.get("USERNAME").cloned().unwrap_or_else(|| "root".into());
    let password = values.get("PASSWORD").cloned().unwrap_or_else(|| "root123".into());
    let tries = values.get("LOGIN_TRIES").cloned().unwrap_or_else(|| "3".into());
    println!("cargo:rustc-env=SHELL_USERNAME={}", username);
    println!("cargo:rustc-env=SHELL_PASSWORD={}", password);
    println!("cargo:rustc-env=SHELL_LOGIN_TRIES={}", tries);
    println!("cargo:rerun-if-changed=shell.conf");
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
