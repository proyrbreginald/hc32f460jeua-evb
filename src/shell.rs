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
//! # 命令
//!
//! - `help` — 命令列表;
//! - `sysinfo` — 系统信息 (型号/CPU 频率/节拍/构建/版本);
//! - `uptime` — 运行时间;
//! - `ps` — 线程列表 (仿 ps);
//! - `free` — 堆内存统计 (仿 free);
//! - `echo` — 回显参数;
//! - `led on|off` — 控制板载 LED;
//! - `clear` — 清屏;
//! - `logout` / `exit` — 重新登录;
//! - `reboot` — 软复位重启;
//! - `whoami` — 显示当前用户。
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
        println!("{} login: ", HOSTNAME);
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
                println!();
                println!(
                    "Welcome to RT-RUST {} ({} kernel, {}).",
                    env!("CARGO_PKG_VERSION"),
                    "RT-Thread 架构的 Rust RTOS",
                    config::CORE
                );
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
fn dispatch(line: &str) -> bool {
    let mut words = line.split_whitespace();
    let Some(cmd) = words.next() else {
        return true;
    };
    match cmd {
        "help" => cmd_help(),
        "sysinfo" | "info" => cmd_sysinfo(),
        "uptime" => cmd_uptime(),
        "ps" => cmd_ps(),
        "free" | "mem" => cmd_free(),
        "echo" => {
            let rest: alloc::string::String = words.collect::<alloc::vec::Vec<_>>().join(" ");
            println!("{}", rest);
        }
        "led" => cmd_led(words.next()),
        "clear" => cmd_clear(),
        "whoami" => println!("{}", SHELL_USERNAME),
        "reboot" => cmd_reboot(),
        "logout" | "exit" => return false,
        _ => println!("{}: command not found (try `help`)", cmd),
    }
    true
}

/// 命令帮助
fn cmd_help() {
    println!("可用命令:");
    println!("  sysinfo         系统信息 (型号/频率/节拍/构建)");
    println!("  uptime          运行时间");
    println!("  ps              线程列表");
    println!("  free            堆内存统计");
    println!("  echo <文本>     回显");
    println!("  led on|off      板载 LED");
    println!("  clear           清屏");
    println!("  whoami          当前用户");
    println!("  reboot          软复位重启");
    println!("  logout|exit     重新登录");
}

/// 系统信息
fn cmd_sysinfo() {
    println!(
        "{}  v{}  —  RT-Thread 架构的 Rust RTOS",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    println!("芯片型号   : {}", config::CHIP_MODEL);
    println!("内核       : {}", config::CORE);
    println!(
        "CPU 频率   : {} MHz",
        crate::clk::system_clock_hz() / 1_000_000
    );
    println!(
        "节拍       : {} ms ({} Hz)",
        1000 / crate::rtos::TICKS_PER_SEC,
        crate::rtos::TICKS_PER_SEC
    );
    println!(
        "优先级     : {} 级 (空闲 = {})",
        crate::rtos::PRIORITY_MAX,
        crate::rtos::IDLE_PRIORITY
    );
    println!(
        "构建       : {}  [{}]  {}",
        env!("RTOS_BUILD_DATE"),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        env!("RTOS_RUSTC")
    );
}

/// 运行时间 (仿 uptime)
fn cmd_uptime() {
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
}

/// 线程列表 (仿 ps)
fn cmd_ps() {
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
}

/// 堆内存统计 (仿 free)
fn cmd_free() {
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
}

/// LED 控制
fn cmd_led(arg: Option<&str>) {
    let gpio = Gpio::take();
    let led = gpio.pin::<PortC, 13>();
    match arg {
        Some("on") => {
            led.set_high();
            println!("LED on");
        }
        Some("off") => {
            led.set_low();
            println!("LED off");
        }
        _ => println!("用法: led on|off"),
    }
}

/// 清屏 (ANSI)
fn cmd_clear() {
    println!("\x1b[2J\x1b[H");
}

/// 软复位 (AIRCR.SYSRESETREQ)
fn cmd_reboot() {
    println!("rebooting...");
    crate::rtos::thread_delay_ms(50);
    unsafe {
        core::ptr::write_volatile(0xE000_ED0C as *mut u32, 0x05FA_0004);
    }
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
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
