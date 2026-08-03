//! 仿 Ubuntu 的嵌入式终端 (shell)
//!
//! # 登录
//!
//! 启动后先显示 `login:` 提示, 输入用户名 (默认 root) 与密码 (不显示
//! 回显) 后进入命令提示符; 用户名/密码/失败次数来自 `.cargo/config.toml`
//! 的 `[env]` 段 (编译期读取, 见 [`crate::config`])。
//!
//! # 命令提示符
//!
//! `root@hc32f460:~$` — 仿 Ubuntu PS1 风格 (主机名取编译期芯片型号)。
//!
//! # 命令系统
//!
//! 命令注册在静态表 [`COMMANDS`] 中 (名称/别名/帮助/执行函数), 分发逻辑
//! 与命令实现解耦。**新增命令 = 表内追加一项 + 加入 `CFG_SHELL_COMMANDS`
//! 启用列表**, 无需修改分发/帮助代码。
//!
//! 每个命令可单独通过 `CFG_SHELL_COMMANDS` (逗号分隔列表) 启用/禁用
//! (见 [`crate::config::cmd_enabled`]); 未列出的命令执行时提示"未启用",
//! 且不显示在 `help` 中。命令内部参数 (如 `led` 的引脚、`selftest` 的
//! 开关) 仍由各自的 `CFG_*` 配置控制。
//!
//! 当前命令: `help` / `sysinfo`(info) / `uptime` / `ps` / `free`(mem) /
//! `echo` / `led on|off` / `log` / `selftest` / `clear` / `whoami` /
//! `reboot` / `logout`(exit)。
//!
//! # 输入处理
//!
//! 回车提交命令, 退格 (BS/DEL) 删除字符, Ctrl+C 清空当前行;
//! 输入缓冲区大小来自配置 (`CFG_SHELL_LINE_BUF`), 超长截断。

use crate::config;
use crate::gpio::{Gpio, PortC};
use crate::heap;
use crate::print; // #[macro_export] 宏需显式引入
use crate::println;

/// 登录用户名 (.cargo/config.toml `CFG_SHELL_USERNAME`)
const SHELL_USERNAME: &str = config::SHELL_USERNAME;
/// 登录密码 (.cargo/config.toml `CFG_SHELL_PASSWORD`)
const SHELL_PASSWORD: &str = config::SHELL_PASSWORD;
/// 登录失败允许次数 (.cargo/config.toml `CFG_SHELL_LOGIN_TRIES`)
const SHELL_LOGIN_TRIES: u32 = config::SHELL_LOGIN_TRIES;
/// 主机名 (仿 Ubuntu PS1 用, 取编译期芯片型号)
const HOSTNAME: &str = config::CHIP_MODEL;

/// 输入行缓冲区大小 (.cargo/config.toml `CFG_SHELL_LINE_BUF`)
const LINE_BUF: usize = config::SHELL_LINE_BUF_SIZE;

// ============================== 命令系统 ==============================

/// 命令执行结果
#[derive(Clone, Copy, PartialEq, Eq)]
enum CmdResult {
    /// 命令执行完毕, 继续命令循环
    Ok,
    /// 退出 shell (重新登录)
    Logout,
}

/// 命令描述符: 注册在静态命令表中, 由 [`dispatch`] 统一查找/执行
struct Command {
    /// 主命令名 (`CFG_SHELL_COMMANDS` 按此名控制启用)
    name: &'static str,
    /// 别名 (可为空)
    aliases: &'static [&'static str],
    /// 帮助文本 (一行说明, 不含命令名)
    help: &'static str,
    /// 执行函数: 参数为命令名之后的剩余文本 (含前导空白)
    handler: fn(&str) -> CmdResult,
}

/// 命令表项构造器
const fn cmd(
    name: &'static str,
    aliases: &'static [&'static str],
    help: &'static str,
    handler: fn(&str) -> CmdResult,
) -> Command {
    Command {
        name,
        aliases,
        help,
        handler,
    }
}

/// 命令表: 新增命令 = 追加一项, 并加入 `CFG_SHELL_COMMANDS` 启用列表
static COMMANDS: &[Command] = &[
    cmd("help", &[], "命令列表", cmd_help),
    cmd(
        "sysinfo",
        &["info"],
        "系统信息 (型号/频率/节拍/构建)",
        cmd_sysinfo,
    ),
    cmd("uptime", &[], "运行时间", cmd_uptime),
    cmd("ps", &[], "线程列表", cmd_ps),
    cmd("free", &["mem"], "堆内存统计", cmd_free),
    cmd("echo", &[], "回显 <文本>", cmd_echo),
    cmd("led", &[], "板载 LED on|off", cmd_led),
    cmd("selftest", &[], "内核自检 (rtos 功能自检)", cmd_selftest),
    cmd("log", &[], "日志开关/级别 (on|off|level <级>)", cmd_log),
    cmd("clear", &[], "清屏", cmd_clear),
    cmd("whoami", &[], "当前用户", cmd_whoami),
    cmd("reboot", &[], "软复位重启", cmd_reboot),
    cmd("logout", &["exit"], "重新登录", cmd_logout),
];

/// shell 线程入口: 登录 → 命令循环 (永不返回)
pub extern "C" fn shell_entry(_param: usize) {
    loop {
        login();
        command_loop();
    }
}

/// 登录流程: 提示用户名/密码, 验证通过后进入 shell
fn login() {
    let mut tries = 0;
    loop {
        println!();
        print!("{} login: ", HOSTNAME);
        let user = read_line(false, LINE_BUF);
        println!();
        if user.trim() != SHELL_USERNAME {
            tries += 1;
            println!("Login incorrect");
        } else {
            print!("Password: ");
            let pass = read_line(true, LINE_BUF);
            println!();
            if pass == SHELL_PASSWORD {
                println!(
                    "Welcome to RT-RUST {} ({} kernel, {}).",
                    env!("CARGO_PKG_VERSION"),
                    "RT-Thread 架构的 Rust RTOS",
                    config::CORE
                );
                // 应用日志: 登录成功属 info 级 (默认输出, 可经 `log` 关闭)
                crate::log_info!("用户 {} 登录成功", SHELL_USERNAME);
                return;
            }
            tries += 1;
            println!("Login incorrect");
        }
        if tries >= SHELL_LOGIN_TRIES {
            println!();
            println!("Too many login failures; try again later.");
            crate::rtos::thread_delay_ms(1000);
            tries = 0;
        }
    }
}

/// 命令循环: 读取一行 → 解析 → 执行
fn command_loop() {
    loop {
        print!("{}@{}:~$ ", SHELL_USERNAME, HOSTNAME);
        let line = read_line(false, LINE_BUF);
        println!();
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        if !dispatch(cmd) {
            return; // logout / exit
        }
    }
}

/// 执行命令; 返回 false 表示退出 shell (重新登录)
///
/// 在 [`COMMANDS`] 表中按主名/别名查找, 命中后检查 `CFG_SHELL_COMMANDS`
/// 启用列表, 通过则调用执行函数 (参数 = 命令名之后的剩余文本)。
fn dispatch(line: &str) -> bool {
    let mut words = line.split_whitespace();
    let Some(name) = words.next() else {
        return true;
    };
    let rest = &line[name.len()..];
    let Some(cmd) = COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
    else {
        println!("{}: command not found (try `help`)", name);
        return true;
    };
    if !config::cmd_enabled(cmd.name) {
        println!("{}: 命令未启用 (CFG_SHELL_COMMANDS)", name);
        return true;
    }
    (cmd.handler)(rest) == CmdResult::Ok
}

/// 命令帮助: 仅列出 `CFG_SHELL_COMMANDS` 中启用的命令 (含别名)
fn cmd_help(_rest: &str) -> CmdResult {
    println!("可用命令 (CFG_SHELL_COMMANDS 控制启用):");
    for c in COMMANDS {
        if !config::cmd_enabled(c.name) {
            continue;
        }
        println!("  {:<14} {}", c.name, c.help);
        for alias in c.aliases {
            println!("  {:<14} ({} 的别名)", alias, c.name);
        }
    }
    CmdResult::Ok
}

/// 系统信息 (info/sysinfo): 运行状态 + 配置摘要
fn cmd_sysinfo(_rest: &str) -> CmdResult {
    println!(
        "{} v{} — RT-Thread 架构的 Rust RTOS",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    // ---- 系统信息 (运行时状态) ----
    sysinfo_section("系统信息");
    sysinfo_line(
        "芯片",
        format_args!(
            "{} ({}) @ {} MHz [{}]",
            config::CHIP_MODEL,
            config::CORE,
            crate::clk::system_clock_hz() / 1_000_000,
            config::CLOCK_SOURCE.name()
        ),
    );
    sysinfo_line(
        "总线",
        format_args!(
            "HCLK {} | PCLK0 {} | PCLK1 {} | PCLK2 {} | PCLK3 {} | PCLK4 {} MHz",
            crate::clk::hclk_hz() / 1_000_000,
            crate::clk::pclk0_hz() / 1_000_000,
            crate::clk::pclk1_hz() / 1_000_000,
            crate::clk::pclk2_hz() / 1_000_000,
            crate::clk::pclk3_hz() / 1_000_000,
            crate::clk::pclk4_hz() / 1_000_000
        ),
    );
    sysinfo_line(
        "节拍",
        format_args!(
            "{} ms ({} Hz), 优先级 {} 级 (空闲 {})",
            1000 / crate::rtos::TICKS_PER_SEC,
            crate::rtos::TICKS_PER_SEC,
            crate::rtos::PRIORITY_MAX,
            crate::rtos::IDLE_PRIORITY
        ),
    );
    let ms = crate::rtos::uptime_ms();
    sysinfo_line(
        "运行",
        format_args!(
            "{:02}:{:02}:{:02}, 就绪 {} 线程",
            ms / 3_600_000,
            (ms / 60_000) % 60,
            (ms / 1000) % 60,
            crate::rtos::sched::ready_thread_count()
        ),
    );
    sysinfo_line(
        "构建",
        format_args!(
            "v{} [{}] {}, {}",
            env!("CARGO_PKG_VERSION"),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            env!("RTOS_BUILD_DATE"),
            env!("RTOS_RUSTC")
        ),
    );

    // ---- 配置 (编译期常量, 来源 .cargo/config.toml) ----
    sysinfo_section("配置 (config)");
    sysinfo_line(
        "时钟源",
        format_args!(
            "{} — MPLL: 源={}, ÷{} ×{} ÷{}",
            config::CLOCK_SOURCE.name(),
            if config::PLL_SRC == 0 { "XTAL" } else { "HRC" },
            config::PLL_M + 1,
            config::PLL_N + 1,
            config::PLL_P + 1
        ),
    );
    sysinfo_line(
        "振荡器",
        format_args!(
            "XTAL {} MHz (稳定 {}, 驱动 {}), HRC {} MHz (复位{})",
            config::XTAL_HZ / 1_000_000,
            config::XTAL_STABLE_TIME,
            match config::XTAL_DRV {
                0 => "high",
                1 => "mid",
                2 => "low",
                _ => "ulow",
            },
            config::HRC_FREQ_MHZ,
            if config::HRC_STOP { "停止" } else { "振荡" }
        ),
    );
    sysinfo_line(
        "分频",
        format_args!(
            "HCLK÷{} PCLK0÷{} PCLK1÷{} PCLK2÷{} PCLK3÷{} PCLK4÷{} EXCLK÷{}",
            config::DIV_HCLK,
            config::DIV_PCLK0,
            config::DIV_PCLK1,
            config::DIV_PCLK2,
            config::DIV_PCLK3,
            config::DIV_PCLK4,
            config::DIV_EXCLK
        ),
    );
    // UART 帧格式缩写 (如 8N1) 与参数
    use crate::uart::{ClockDiv, DataBits, FlowControl, Oversample, Parity, StopBits};
    let (db, par, sb) = (
        match config::UART_DATA_BITS {
            DataBits::Eight => "8",
            DataBits::Nine => "9",
        },
        match config::UART_PARITY {
            Parity::None => "N",
            Parity::Even => "E",
            Parity::Odd => "O",
        },
        match config::UART_STOP_BITS {
            StopBits::One => "1",
            StopBits::Two => "2",
        },
    );
    let (os, cd, fc) = (
        match config::UART_OVERSAMPLE {
            Oversample::Eight => "8",
            Oversample::Sixteen => "16",
        },
        match config::UART_CLOCK_DIV {
            ClockDiv::Div1 => "1",
            ClockDiv::Div4 => "4",
            ClockDiv::Div16 => "16",
            ClockDiv::Div64 => "64",
        },
        match config::UART_FLOW_CTRL {
            FlowControl::None => "无",
            FlowControl::Cts => "CTS",
        },
    );
    sysinfo_line(
        "UART",
        format_args!(
            "USART{} {} bps {}{}{} (过采样 {}, 分频 {}, 流控 {}, 噪声滤波 {})",
            config::UART_UNIT,
            config::UART_BAUDRATE,
            db,
            par,
            sb,
            os,
            cd,
            fc,
            if config::UART_NOISE_FILTER {
                "开"
            } else {
                "关"
            }
        ),
    );
    sysinfo_line(
        "接收",
        format_args!(
            "缓冲 {} B, 中断 INT{:03} (NVIC {})",
            crate::uart::RX_BUF_SIZE,
            config::UART_RX_IRQ_CHANNEL,
            config::UART_RX_IRQ_PRIORITY
        ),
    );
    sysinfo_line(
        "LED",
        format_args!(
            "PC{} (初始{})",
            config::LED_PIN,
            if config::LED_INITIAL_LEVEL == crate::gpio::Level::High {
                "高电平"
            } else {
                "低电平"
            }
        ),
    );
    sysinfo_line(
        "日志",
        format_args!(
            "默认{}, 阈值 {}",
            if config::LOG_ENABLE {
                "开启"
            } else {
                "关闭"
            },
            config::LOG_LEVEL.name()
        ),
    );
    sysinfo_line(
        "终端",
        format_args!(
            "用户 {}, 失败次数 {}",
            config::SHELL_USERNAME,
            config::SHELL_LOGIN_TRIES
        ),
    );
    sysinfo_line(
        "线程",
        format_args!(
            "led P{} T{} {}B | shell P{} T{} {}B | 自检 P{} {}B",
            config::APP_LED_PRIORITY,
            config::APP_LED_TIMESLICE,
            config::APP_LED_STACK,
            config::APP_SHELL_PRIORITY,
            config::APP_SHELL_TIMESLICE,
            config::APP_SHELL_STACK,
            config::APP_SELFTEST_PRIORITY,
            config::APP_SELFTEST_STACK
        ),
    );
    CmdResult::Ok
}

/// CJK 显示宽度 (ASCII 计 1 列, 其余计 2 列)
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

/// 输出 info 分节标题 (标题 + 分隔线, 总宽 48 列)
fn sysinfo_section(title: &str) {
    print!("── {} ", title);
    for _ in display_width(title)..44 {
        print!("─");
    }
    println!();
}

/// 输出 info 一行 (标签按显示宽度对齐到 12 列)
fn sysinfo_line(label: &str, args: core::fmt::Arguments<'_>) {
    print!("  {}", label);
    for _ in display_width(label)..12 {
        print!(" ");
    }
    println!(": {}", args);
}

/// 运行时间 (仿 uptime)
fn cmd_uptime(_rest: &str) -> CmdResult {
    let ms = crate::rtos::uptime_ms();
    let (h, m, s) = (ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60);
    println!(
        "up {:02}:{:02}:{:02}, 节拍 {} ({} Hz), 就绪线程 {}",
        h,
        m,
        s,
        crate::rtos::tick(),
        crate::rtos::TICKS_PER_SEC,
        crate::rtos::sched::ready_thread_count()
    );
    CmdResult::Ok
}

/// 线程列表 (仿 ps)
fn cmd_ps(_rest: &str) -> CmdResult {
    let list = crate::rtos::thread_info_list();
    println!("NAME                 PRIO  STATE   (count={})", list.len());
    for t in list {
        println!(
            "{:<20} {:>4}  {}",
            t.name,
            t.priority,
            crate::rtos::thread_state_name(t.state)
        );
    }
    CmdResult::Ok
}

/// 堆内存统计 (仿 free)
fn cmd_free(_rest: &str) -> CmdResult {
    let total = heap::capacity();
    let used = heap::used();
    // 整数百分比 (避免浮点格式化在 no_std 下的问题)
    let pct = (100 * used + total / 2).checked_div(total).unwrap_or(0);
    println!("                total         used         free");
    println!(
        "Mem:      {:>10} B  {:>10} B  {:>10} B  ({}% used)",
        total,
        used,
        total - used,
        pct
    );
    CmdResult::Ok
}

/// 回显剩余参数
fn cmd_echo(rest: &str) -> CmdResult {
    println!("{}", rest.trim());
    CmdResult::Ok
}

/// LED 控制
fn cmd_led(rest: &str) -> CmdResult {
    let gpio = Gpio::take();
    let led = gpio.pin::<PortC, { config::LED_PIN }>();
    match rest.trim() {
        "on" => {
            led.set_high();
            println!("LED on");
        }
        "off" => {
            led.set_low();
            println!("LED off");
        }
        _ => println!("用法: led on|off"),
    }
    CmdResult::Ok
}

/// 内核自检: 通过命令手动启动 (受 CFG_APP_SELFTEST_ENABLE 控制)
fn cmd_selftest(_rest: &str) -> CmdResult {
    if config::APP_SELFTEST_ENABLE {
        crate::start_selftest();
    } else {
        println!("selftest 未启用 (CFG_APP_SELFTEST_ENABLE=false)");
    }
    CmdResult::Ok
}

/// 清屏 (ANSI)
fn cmd_clear(_rest: &str) -> CmdResult {
    println!("\x1b[2J\x1b[H");
    CmdResult::Ok
}

/// 当前用户
fn cmd_whoami(_rest: &str) -> CmdResult {
    println!("{}", SHELL_USERNAME);
    CmdResult::Ok
}

/// 软复位 (AIRCR.SYSRESETREQ)
fn cmd_reboot(_rest: &str) -> CmdResult {
    println!("rebooting...");
    crate::rtos::thread_delay_ms(50);
    unsafe {
        core::ptr::write_volatile(0xE000_ED0C as *mut u32, 0x05FA_0004);
    }
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

/// 退出 shell (重新登录)
fn cmd_logout(_rest: &str) -> CmdResult {
    CmdResult::Logout
}

/// 日志控制: 无参数显示状态; `on|off` 切换开关; `level <级别>` 调整阈值
///
/// 仅影响应用日志 (`log::*`), 内核打印 (横幅/shell 输出等) 不受影响;
/// 重启后恢复配置默认值 (`CFG_LOG_ENABLE` / `CFG_LOG_LEVEL`)。
fn cmd_log(rest: &str) -> CmdResult {
    let mut words = rest.split_whitespace();
    match words.next() {
        None => println!(
            "日志: {} (级别阈值 = {})",
            if crate::log::enabled() {
                "开启"
            } else {
                "关闭"
            },
            crate::log::level().name()
        ),
        Some("on") => {
            crate::log::set_enabled(true);
            println!("日志已开启");
        }
        Some("off") => {
            crate::log::set_enabled(false);
            println!("日志已关闭");
        }
        Some("level") => match words.next().and_then(crate::log::Level::from_name) {
            Some(l) => {
                crate::log::set_level(l);
                println!("日志级别阈值: {}", l.name());
            }
            None => println!("用法: log level error|warn|info|debug|trace"),
        },
        Some(_) => println!("用法: log [on|off|level <error|warn|info|debug|trace>]"),
    }
    CmdResult::Ok
}

/// 从 UART 读取一行 (阻塞, 支持退格/Ctrl+C)
///
/// `masked` 为 true 时输入不回显 (密码模式)。
/// 中断驱动: 挂起在数据到达信号量上, 由 RX ISR 唤醒, 无轮询。
fn read_line(masked: bool, max: usize) -> alloc::string::String {
    let uart = config::ConsoleUart::take();
    let mut line = alloc::string::String::new();
    loop {
        let b = uart.read_rx_blocking();
        match b {
            b'\r' | b'\n' => break,
            0x08 | 0x7F => {
                // 退格: 删除最后一个字符
                if line.pop().is_some() {
                    print!("\x08 \x08");
                }
            }
            0x03 => {
                // Ctrl+C: 清空当前行
                while line.pop().is_some() {
                    print!("\x08 \x08");
                }
            }
            0x20..=0x7E if line.len() < max => {
                line.push(b as char);
                if masked {
                    print!("*");
                } else {
                    print!("{}", b as char);
                }
            }
            _ => {}
        }
    }
    line
}
