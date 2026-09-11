//! 构建脚本: 注入启动横幅所需的构建日期与 rustc 版本, 并把
//! `.cargo/config.toml [env]` 中的功能开关翻译为 `#[cfg]` 条件
//! (实现**编译期裁剪**: 关闭的功能连代码一起不编译, 直接省 FLASH)。

fn main() {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    // 输出任意 rerun-if 指令后 Cargo 会关闭默认的整包变更追踪，因此显式
    // 列出会改变固件内容的输入，避免开发构建横幅日期长期停留在旧值。
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=.cargo/config.toml");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=crates/littlefs/Cargo.toml");
    println!("cargo:rerun-if-changed=crates/littlefs/src");

    // 功能开关: 布尔 env → `cargo:rustc-cfg=<flag>`, 非法值直接编译报错。
    // 每次开关变化都会触发 build.rs 重跑 (rerun-if-env-changed 追踪), 并
    // 因 cfg 变化导致相关代码重新编译。
    //
    // dev / release 策略: **debug 构建恒保留调试/自测配套功能**
    // (nano 编辑器 / selftest 自检 / soak 长稳), 不受 CFG_* 的 "false"
    // 影响 —— 便于在完整功能下调试; 只有 release 产物才按配置裁剪以省
    // FLASH。因此注入额外 cfg `dev_profile` 供 config::cmd_enabled 放宽
    // 命令启用列表, 与命令注册保持一致。
    let is_release = std::env::var("PROFILE").as_deref() == Ok("release");
    if !is_release {
        println!("cargo:rustc-cfg=dev_profile");
    }
    for (env, flag) in [
        ("CFG_SHELL_NANO_ENABLE", "shell_nano"),
        ("CFG_SHELL_ZMODEM_ENABLE", "shell_zmodem"),
        ("CFG_APP_SELFTEST_ENABLE", "shell_selftest"),
        ("CFG_SOAK_ENABLE", "shell_soak"),
        ("CFG_PANIC_VERBOSE", "panic_verbose"),
        // CAN 启用时一并编译 shell `can` 测试命令与 CAN 驱动
        // (默认裁剪配置下两者都不参与链接, 省 ~11.5 KiB)
        ("CFG_CAN_ENABLE", "can_enabled"),
    ] {
        let configured = match std::env::var(env).as_deref() {
            Ok("true") => true,
            Ok("false") => false,
            _ => panic!("{env} 必须为 true/false"),
        };
        // dev 恒保留的调试/自测功能; 其余 (zmodem/panic_verbose) 始终按配置
        let effective =
            if !is_release && matches!(flag, "shell_nano" | "shell_selftest" | "shell_soak") {
                true
            } else {
                configured
            };
        if effective {
            println!("cargo:rustc-cfg={flag}");
        }
        println!("cargo:rerun-if-env-changed={env}");
    }
    // soak 依赖 selftest 的 ESC 中断支持: soak 开启时强制 selftest 一并编译
    if std::env::var("CFG_SOAK_ENABLE").as_deref() == Ok("true") {
        println!("cargo:rustc-cfg=shell_selftest");
    }
    // 声明本工程自定义的 cfg 名, 避免 rustc 的 unexpected cfg 警告
    for flag in [
        "shell_nano",
        "shell_zmodem",
        "shell_selftest",
        "shell_soak",
        "panic_verbose",
        "dev_profile",
        "can_enabled",
    ] {
        println!("cargo:rustc-check-cfg=cfg({flag})");
    }

    // 构建日期 (UTC, 公历)。可复现构建由 SOURCE_DATE_EPOCH 固定时间；
    // 未设置时保留开发固件显示实际构建日的便利行为。
    let secs = match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(value) => value
            .parse::<u64>()
            .expect("SOURCE_DATE_EPOCH 必须为非负 Unix 秒数"),
        Err(std::env::VarError::NotPresent) => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        Err(std::env::VarError::NotUnicode(_)) => panic!("SOURCE_DATE_EPOCH 必须是 UTF-8"),
    };
    let (y, m, d) = unix_to_ymd(secs);
    println!("cargo:rustc-env=RTOS_BUILD_DATE={:04}-{:02}-{:02}", y, m, d);

    // rustc 版本
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    if let Ok(out) = std::process::Command::new(rustc).arg("--version").output()
        && let Ok(s) = String::from_utf8(out.stdout)
    {
        println!("cargo:rustc-env=RTOS_RUSTC={}", s.trim());
    }

    // 主栈 MPU 守卫区大小一致性: link.ld 的 MPU_GUARD_SIZE 决定
    // _heap_end 与 _main_stack_guard_base, 必须与配置
    // CFG_MPU_STACK_GUARD 一致 (不一致会导致堆上界与守卫区重叠)。
    check_mpu_guard_consistency();
}

/// 解析 link.ld 的 `MPU_GUARD_SIZE = <n>;` 并与配置比对。
fn check_mpu_guard_consistency() {
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-env-changed=CFG_MPU_STACK_GUARD");
    let link = std::fs::read_to_string("link.ld").expect("读取 link.ld 失败");
    let link_value: u32 = link
        .lines()
        .find_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("MPU_GUARD_SIZE =")?;
            rest.trim().strip_suffix(';')?.trim().parse().ok()
        })
        .unwrap_or_else(|| panic!("link.ld 缺少 MPU_GUARD_SIZE 定义"));
    let config_value: u32 = std::env::var("CFG_MPU_STACK_GUARD")
        .expect("CFG_MPU_STACK_GUARD 未定义")
        .parse()
        .expect("CFG_MPU_STACK_GUARD 必须为整数");
    assert_eq!(
        config_value, link_value,
        "CFG_MPU_STACK_GUARD ({config_value}) 与 link.ld 的 MPU_GUARD_SIZE ({link_value}) 不一致 \
         (link.ld 用它计算 _heap_end 与主栈守卫基址)"
    );
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
