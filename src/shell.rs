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
//! `root@hc32f460:/$` — 仿 Linux PS1 风格，并显示当前工作路径。
//!
//! # 命令系统
//!
//! 命令注册在静态表 [`COMMANDS`] 中 (名称/帮助/执行函数), 分发逻辑
//! 与命令实现解耦。**新增命令 = 表内追加一项 + 加入 `CFG_SHELL_COMMANDS`
//! 启用列表**, 无需修改分发/帮助代码。
//!
//! 每个命令可单独通过 `CFG_SHELL_COMMANDS` (逗号分隔列表) 启用/禁用
//! (见 [`crate::config::cmd_enabled`]); 未列出的命令执行时提示"未启用",
//! 且不显示在 `help` 中。命令内部参数 (如 `led` 的引脚、`selftest` 的
//! 开关) 仍由各自的 `CFG_*` 配置控制。
//!
//! 当前命令: `help` / `sysinfo` / `uptime` / `ps` / `free` /
//! `echo` / `history` / `pwd` / `cd` / `ls` / `mkdir` / `rmdir` / `cat` /
//! `write` / `nano` / `sz` / `rz` / `rm` / `mv` / `stat` / `df` /
//! `fsck` / `mount` / `mkfs` / `led` / `log` / `selftest` / `soak` / `clear` /
//! `whoami` / `reboot` / `logout`。
//!
//! # 输入处理
//!
//! 命令输入仅接受 ASCII；回车提交, 退格 (BS/DEL) 删除字符, Ctrl+C 清空当前行,
//! 上下键浏览历史。输入缓冲区大小来自配置 (`CFG_SHELL_LINE_BUF`), 非 ASCII
//! 或超长命令整行拒绝执行。

#[cfg(shell_nano)]
mod editor;
mod path;
#[cfg(shell_zmodem)]
mod zmodem;

use crate::config;
use crate::heap;
use crate::print; // #[macro_export] 宏需显式引入
use crate::println;
use crate::uart_rtos::UartRtosExt;

use path::{PathError, ShellPath};

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
/// 固定容量命令历史，仅保存在 RAM 中。
const HISTORY_CAPACITY: usize = config::SHELL_HISTORY_SIZE;
/// CR 后等待可选 LF 的时间，同时兼容 CR-only 终端。
const INPUT_CRLF_TIMEOUT_MS: u32 = 25;
/// 终端探测或转义序列预读期间需要按原顺序回放的输入容量。
const PENDING_RX_CAPACITY: usize = 64;

struct InputLine {
    text: alloc::string::String,
    overflowed: bool,
    non_ascii: bool,
    rx_corrupted: bool,
}

#[derive(Clone, Copy)]
struct HistoryEntry {
    bytes: [u8; LINE_BUF],
    len: usize,
    number: u32,
}

impl HistoryEntry {
    const fn empty() -> Self {
        Self {
            bytes: [0; LINE_BUF],
            len: 0,
            number: 0,
        }
    }

    fn set(&mut self, text: &str, number: u32) {
        debug_assert!(text.len() <= self.bytes.len());
        self.bytes[..text.len()].copy_from_slice(text.as_bytes());
        self.len = text.len();
        self.number = number;
    }

    fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

struct CommandHistory {
    entries: [HistoryEntry; HISTORY_CAPACITY],
    start: usize,
    len: usize,
    next_number: u32,
}

impl CommandHistory {
    const fn new() -> Self {
        Self {
            entries: [HistoryEntry::empty(); HISTORY_CAPACITY],
            start: 0,
            len: 0,
            next_number: 1,
        }
    }

    fn push(&mut self, command: &str) {
        if command.is_empty() || self.newest(0).is_some_and(|entry| entry.text() == command) {
            return;
        }

        let index = if self.len == HISTORY_CAPACITY {
            let index = self.start;
            self.start = (self.start + 1) % HISTORY_CAPACITY;
            index
        } else {
            let index = (self.start + self.len) % HISTORY_CAPACITY;
            self.len += 1;
            index
        };
        self.entries[index].set(command, self.next_number);
        self.next_number = self.next_number.wrapping_add(1).max(1);
    }

    fn newest(&self, offset: usize) -> Option<&HistoryEntry> {
        if offset >= self.len {
            return None;
        }
        let index = (self.start + self.len - 1 - offset) % HISTORY_CAPACITY;
        Some(&self.entries[index])
    }

    fn clear(&mut self) {
        self.start = 0;
        self.len = 0;
        self.next_number = 1;
    }

    fn print(&self) {
        for offset in 0..self.len {
            let entry = &self.entries[(self.start + offset) % HISTORY_CAPACITY];
            println!("{:>5}  {}", entry.number, entry.text());
        }
    }
}

struct PendingRx {
    bytes: [u8; PENDING_RX_CAPACITY],
    head: usize,
    len: usize,
}

impl PendingRx {
    const fn new() -> Self {
        Self {
            bytes: [0; PENDING_RX_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn pop_front(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let byte = self.bytes[self.head];
        self.head = (self.head + 1) % PENDING_RX_CAPACITY;
        self.len -= 1;
        Some(byte)
    }

    fn push_front(&mut self, byte: u8) -> bool {
        if self.len == PENDING_RX_CAPACITY {
            return false;
        }
        self.head = (self.head + PENDING_RX_CAPACITY - 1) % PENDING_RX_CAPACITY;
        self.bytes[self.head] = byte;
        self.len += 1;
        true
    }

    #[cfg(shell_nano)]
    fn push_back(&mut self, byte: u8) -> bool {
        if self.len == PENDING_RX_CAPACITY {
            return false;
        }
        let tail = (self.head + self.len) % PENDING_RX_CAPACITY;
        self.bytes[tail] = byte;
        self.len += 1;
        true
    }

    #[cfg(shell_nano)]
    const fn is_full(&self) -> bool {
        self.len == PENDING_RX_CAPACITY
    }

    #[cfg(shell_nano)]
    const fn remaining_capacity(&self) -> usize {
        PENDING_RX_CAPACITY - self.len
    }
}

type FsError = littlefs::Error<crate::filesystem::FlashError>;

/// Shell 会话状态。文件系统实例不在此持有 —— 见 [`crate::filesystem`]
/// 的全局互斥共享模型 (shell 命令与日志落盘线程分时独占)。
struct ShellState {
    last_mount_error: Option<FsError>,
    pending_rx: PendingRx,
    history: CommandHistory,
    cwd: ShellPath,
}

impl ShellState {
    fn start() -> Self {
        match crate::filesystem::start(crate::board::BoardResources::get()) {
            Ok(crate::filesystem::Startup::Mounted) => {
                if let Some(filesystem) = crate::filesystem::mounted() {
                    let info = filesystem.info();
                    crate::log_info!(
                        "文件系统已挂载: generation={}, 条目={} 个",
                        info.generation,
                        info.entry_count
                    );
                }
                Self {
                    last_mount_error: None,
                    pending_rx: PendingRx::new(),
                    history: CommandHistory::new(),
                    cwd: ShellPath::root(),
                }
            }
            Ok(crate::filesystem::Startup::Formatted) => {
                crate::log_info!("文件系统分区为空，已创建并挂载空文件系统");
                Self {
                    last_mount_error: None,
                    pending_rx: PendingRx::new(),
                    history: CommandHistory::new(),
                    cwd: ShellPath::root(),
                }
            }
            Err(error) => {
                match &error {
                    littlefs::Error::NotFormatted => crate::log_warn!(
                        "文件系统未挂载: 分区含数据但没有有效快照; 检查后可执行 mkfs --force"
                    ),
                    _ => crate::log_error!("文件系统挂载失败: {:?}", error),
                }
                Self {
                    last_mount_error: Some(error),
                    pending_rx: PendingRx::new(),
                    history: CommandHistory::new(),
                    cwd: ShellPath::root(),
                }
            }
        }
    }

    fn remount(&mut self) -> bool {
        match crate::filesystem::remount() {
            Ok(()) => {
                self.last_mount_error = None;
                self.cwd = ShellPath::root();
                true
            }
            Err(error) => {
                self.last_mount_error = Some(error);
                false
            }
        }
    }

    fn format_unmounted(&mut self) -> bool {
        match crate::filesystem::format() {
            Ok(filesystem) => {
                drop(filesystem); // 丢弃实例并释放 token 后重新挂载到全局槽
                match crate::filesystem::remount() {
                    Ok(()) => {
                        self.last_mount_error = None;
                        self.cwd = ShellPath::root();
                        true
                    }
                    Err(error) => {
                        self.last_mount_error = Some(error);
                        false
                    }
                }
            }
            Err(error) => {
                self.last_mount_error = Some(error);
                false
            }
        }
    }
}

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
    /// 命令名 (`CFG_SHELL_COMMANDS` 按此名控制启用)
    name: &'static str,
    /// 帮助文本 (一行说明, 不含命令名)
    help: &'static str,
    /// 执行函数: Shell 状态 + 命令名之后的剩余文本 (含前导空白)
    handler: fn(&mut ShellState, &str) -> CmdResult,
}

/// 命令表项构造器
const fn cmd(
    name: &'static str,
    help: &'static str,
    handler: fn(&mut ShellState, &str) -> CmdResult,
) -> Command {
    Command {
        name,
        help,
        handler,
    }
}

/// 命令表: 新增命令 = 追加一项, 并加入 `CFG_SHELL_COMMANDS` 启用列表
static COMMANDS: &[Command] = &[
    cmd("help", "命令列表", cmd_help),
    cmd("sysinfo", "系统信息 (型号/时钟/节拍)", cmd_sysinfo),
    cmd("uptime", "运行时间", cmd_uptime),
    cmd("ps", "线程列表", cmd_ps),
    cmd("free", "堆内存统计", cmd_free),
    cmd("echo", "回显 <文本>", cmd_echo),
    cmd("history", "历史命令; -c 清空", cmd_history),
    cmd("pwd", "当前路径", cmd_pwd),
    cmd("cd", "切换路径: cd [目录]", cmd_cd),
    cmd("ls", "列出: ls [路径]", cmd_ls),
    cmd("mkdir", "建目录: mkdir <目录>", cmd_mkdir),
    cmd("rmdir", "删目录: rmdir <目录>", cmd_rmdir),
    cmd("cat", "读文件: cat <文件>", cmd_cat),
    cmd("write", "原子写: write <文件> [文本]", cmd_write),
    #[cfg(shell_nano)]
    cmd("nano", "全屏编辑: nano <文件>", cmd_nano),
    #[cfg(shell_zmodem)]
    cmd("sz", "发送 (ZMODEM): sz <文件>...", zmodem::cmd_sz),
    #[cfg(shell_zmodem)]
    cmd("rz", "接收 (ZMODEM): rz (主机 sz)", zmodem::cmd_rz),
    cmd("rm", "删文件: rm <文件>", cmd_rm),
    cmd("mv", "移动: mv <旧> <新>", cmd_mv),
    cmd("stat", "路径信息: stat <路径>", cmd_stat),
    cmd("df", "文件系统容量/状态", cmd_df),
    cmd("fsck", "校验当前快照", cmd_fsck),
    cmd("level", "磨损均衡: 快照搬到磨损最低区", cmd_level),
    cmd("mount", "重新挂载", cmd_mount),
    cmd("mkfs", "清空: mkfs --force", cmd_mkfs),
    cmd("led", "LED on|off", cmd_led),
    #[cfg(shell_selftest)]
    cmd("selftest", "自检: selftest [all|can]", cmd_selftest),
    #[cfg(shell_soak)]
    cmd("soak", "长稳: soak [分钟] [项|场景] (0=ESC)", cmd_soak),
    cmd(
        "log",
        "日志开关/级别/落盘 (on|off|level <级>|file)",
        cmd_log,
    ),
    cmd("clear", "清屏", cmd_clear),
    cmd("whoami", "当前用户", cmd_whoami),
    cmd("reboot", "软复位", cmd_reboot),
    cmd("logout", "重新登录", cmd_logout),
];

/// shell 线程入口: 启动文件系统 → 登录 → 命令循环 (永不返回)
pub extern "C" fn shell_entry(_param: usize) {
    let mut state = ShellState::start();
    loop {
        login(&mut state);
        command_loop(&mut state);
    }
}

/// 登录流程: 提示用户名/密码, 验证通过后进入 shell
fn login(state: &mut ShellState) {
    let mut tries = 0;
    loop {
        println!();
        print!("{} login: ", HOSTNAME);
        let user = read_line(&mut state.pending_rx, false, LINE_BUF, None);
        println!();
        if user.overflowed
            || user.non_ascii
            || user.rx_corrupted
            || user.text.trim() != SHELL_USERNAME
        {
            tries += 1;
            println!("Login incorrect");
        } else {
            print!("Password: ");
            let pass = read_line(&mut state.pending_rx, true, LINE_BUF, None);
            println!();
            if !pass.overflowed
                && !pass.non_ascii
                && !pass.rx_corrupted
                && pass.text == SHELL_PASSWORD
            {
                state.cwd = ShellPath::root();
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
            crate::rtos::thread_delay_ms(1000).expect("shell 延时必须在线程上下文");
            tries = 0;
        }
    }
}

/// 命令循环: 读取一行 → 解析 → 执行
fn command_loop(state: &mut ShellState) {
    loop {
        print!("{}@{}:{}$ ", SHELL_USERNAME, HOSTNAME, state.cwd);
        let line = read_line(&mut state.pending_rx, false, LINE_BUF, Some(&state.history));
        println!();
        if line.overflowed {
            println!("输入超过 {} B，命令未执行", LINE_BUF);
            continue;
        }
        if line.non_ascii {
            println!("输入包含非 ASCII 字节，命令未执行");
            continue;
        }
        if line.rx_corrupted {
            println!("串口接收期间发生丢字节或校验错误，命令未执行");
            continue;
        }
        let cmd = line.text.trim();
        if cmd.is_empty() {
            continue;
        }
        state.history.push(cmd);
        if !dispatch(state, cmd) {
            return; // logout
        }
    }
}

/// 执行命令; 返回 false 表示退出 shell (重新登录)
///
/// 在 [`COMMANDS`] 表中按命令名查找, 命中后检查 `CFG_SHELL_COMMANDS`
/// 启用列表, 通过则调用执行函数 (参数 = 命令名之后的剩余文本)。
fn dispatch(state: &mut ShellState, line: &str) -> bool {
    let mut words = line.split_whitespace();
    let Some(name) = words.next() else {
        return true;
    };
    let rest = &line[name.len()..];
    let Some(cmd) = COMMANDS.iter().find(|c| c.name == name) else {
        println!("{}: command not found (try `help`)", name);
        return true;
    };
    if !config::cmd_enabled(cmd.name) {
        println!("{}: 命令未启用 (CFG_SHELL_COMMANDS)", name);
        return true;
    }
    (cmd.handler)(state, rest) == CmdResult::Ok
}

/// 命令帮助: 仅列出 `CFG_SHELL_COMMANDS` 中启用的命令
fn cmd_help(_state: &mut ShellState, _rest: &str) -> CmdResult {
    println!("可用命令 (CFG_SHELL_COMMANDS 控制启用):");
    for c in COMMANDS {
        if !config::cmd_enabled(c.name) {
            continue;
        }
        println!("  {:<14} {}", c.name, c.help);
    }
    CmdResult::Ok
}

/// 系统信息 (sysinfo): 运行状态 + 配置摘要
fn cmd_sysinfo(state: &mut ShellState, _rest: &str) -> CmdResult {
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
    match crate::filesystem::mounted() {
        Some(filesystem) => {
            let info = filesystem.info();
            sysinfo_line(
                "文件系统",
                format_args!(
                    "已挂载, generation {}, {} 个条目, {}/{} B",
                    info.generation, info.entry_count, info.serialized_bytes, info.capacity_bytes
                ),
            );
        }
        None => sysinfo_line(
            "文件系统",
            format_args!(
                "未挂载 ({})",
                state
                    .last_mount_error
                    .as_ref()
                    .map(fs_error_summary)
                    .unwrap_or("未知状态")
            ),
        ),
    }

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
            "led P{} T{} {}B | shell P{} T{} {}B | logfile P{} T{} {}B",
            config::APP_LED_PRIORITY,
            config::APP_LED_TIMESLICE,
            config::APP_LED_STACK,
            config::APP_SHELL_PRIORITY,
            config::APP_SHELL_TIMESLICE,
            config::APP_SHELL_STACK,
            config::APP_LOGFILE_PRIORITY,
            config::APP_LOGFILE_TIMESLICE,
            config::APP_LOGFILE_STACK
        ),
    );
    CmdResult::Ok
}

/// CJK 显示宽度 (ASCII 计 1 列, 其余计 2 列)
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

/// 输出 sysinfo 分节标题 (标题 + 分隔线, 总宽 48 列)
fn sysinfo_section(title: &str) {
    print!("── {} ", title);
    for _ in display_width(title)..44 {
        print!("─");
    }
    println!();
}

/// 输出 sysinfo 一行 (标签按显示宽度对齐到 12 列)
fn sysinfo_line(label: &str, args: core::fmt::Arguments<'_>) {
    print!("  {}", label);
    for _ in display_width(label)..12 {
        print!(" ");
    }
    println!(": {}", args);
}

/// 运行时间 (仿 uptime)
fn cmd_uptime(_state: &mut ShellState, _rest: &str) -> CmdResult {
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
fn cmd_ps(_state: &mut ShellState, _rest: &str) -> CmdResult {
    let list = crate::rtos::thread_info_list();
    println!(
        "NAME                 PRIO  STATE    STACK (used/total)  (count={})",
        list.len()
    );
    for t in list {
        println!(
            "{:<20} {:>4}  {:<8} {:>6}/{:<6}",
            t.name,
            t.priority,
            crate::rtos::thread_state_name(t.state),
            t.stack_used,
            t.stack_size
        );
    }
    CmdResult::Ok
}

/// 堆内存统计 (仿 free)
fn cmd_free(_state: &mut ShellState, _rest: &str) -> CmdResult {
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
fn cmd_echo(_state: &mut ShellState, rest: &str) -> CmdResult {
    println!("{}", rest.trim());
    CmdResult::Ok
}

fn cmd_history(state: &mut ShellState, rest: &str) -> CmdResult {
    match rest.trim() {
        "" => state.history.print(),
        "-c" => state.history.clear(),
        _ => println!("用法: history [-c]"),
    }
    CmdResult::Ok
}

fn resolve_path(state: &ShellState, operation: &str, input: &str) -> Option<ShellPath> {
    match ShellPath::resolve(&state.cwd, input) {
        Ok(path) => Some(path),
        Err(PathError::Invalid) => {
            println!("{}: 路径无效", operation);
            None
        }
        Err(PathError::TooLong) => {
            println!("{}: 路径超过 {} B 上限", operation, littlefs::MAX_NAME_LEN);
            None
        }
    }
}

fn cmd_pwd(state: &mut ShellState, rest: &str) -> CmdResult {
    if !rest.trim().is_empty() {
        println!("用法: pwd");
    } else {
        println!("{}", state.cwd);
    }
    CmdResult::Ok
}

fn cmd_cd(state: &mut ShellState, rest: &str) -> CmdResult {
    let input = if rest.trim().is_empty() {
        "/"
    } else {
        let Some(input) = one_argument(rest) else {
            println!("用法: cd [目录]");
            return CmdResult::Ok;
        };
        input
    };
    let Some(path) = resolve_path(state, "cd", input) else {
        return CmdResult::Ok;
    };
    let Some(mut filesystem) = mounted_filesystem(state) else {
        return CmdResult::Ok;
    };
    match filesystem.stat(path.as_key()) {
        Ok(info) if info.kind == littlefs::EntryKind::Directory => state.cwd = path,
        Ok(_) => println!("cd: 不是目录: {}", path),
        Err(error) => print_fs_error("cd", &error),
    }
    CmdResult::Ok
}

fn cmd_mkdir(state: &mut ShellState, rest: &str) -> CmdResult {
    let Some(input) = one_argument(rest) else {
        println!("用法: mkdir <目录>");
        return CmdResult::Ok;
    };
    let Some(path) = resolve_path(state, "mkdir", input) else {
        return CmdResult::Ok;
    };
    let Some(mut filesystem) = mounted_filesystem(state) else {
        return CmdResult::Ok;
    };
    let result = filesystem.mkdir(path.as_key());
    drop(filesystem); // 释放文件系统锁, 允许 report_mutation_error 重新挂载
    match result {
        Ok(()) => println!("{}: 目录已创建", path),
        Err(error) => report_mutation_error(state, "mkdir", error),
    }
    CmdResult::Ok
}

fn cmd_rmdir(state: &mut ShellState, rest: &str) -> CmdResult {
    let Some(input) = one_argument(rest) else {
        println!("用法: rmdir <目录>");
        return CmdResult::Ok;
    };
    let Some(path) = resolve_path(state, "rmdir", input) else {
        return CmdResult::Ok;
    };
    if path.is_ancestor_of(&state.cwd) {
        println!("rmdir: 不能删除当前目录或其祖先: {}", path);
        return CmdResult::Ok;
    }
    let Some(mut filesystem) = mounted_filesystem(state) else {
        return CmdResult::Ok;
    };
    let result = filesystem.rmdir(path.as_key());
    drop(filesystem); // 释放文件系统锁, 允许 report_mutation_error 重新挂载
    match result {
        Ok(()) => println!("{}: 目录已删除", path),
        Err(error) => report_mutation_error(state, "rmdir", error),
    }
    CmdResult::Ok
}

fn one_argument(rest: &str) -> Option<&str> {
    let mut words = rest.split_whitespace();
    let argument = words.next()?;
    if words.next().is_some() {
        return None;
    }
    Some(argument)
}

fn two_arguments(rest: &str) -> Option<(&str, &str)> {
    let mut words = rest.split_whitespace();
    let first = words.next()?;
    let second = words.next()?;
    if words.next().is_some() {
        return None;
    }
    Some((first, second))
}

fn name_and_text(rest: &str) -> Option<(&str, &str)> {
    let trimmed = rest.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.find(char::is_whitespace) {
        Some(index) => Some((&trimmed[..index], trimmed[index..].trim_start())),
        None => Some((trimmed, "")),
    }
}

fn fs_error_summary(error: &FsError) -> &'static str {
    match error {
        littlefs::Error::Device(_) => "Flash 设备错误",
        littlefs::Error::DeviceBusy => "Flash 分区正被占用",
        littlefs::Error::InvalidGeometry => "分区几何参数无效",
        littlefs::Error::NotFormatted => "无有效文件系统快照",
        littlefs::Error::Corrupt => "文件系统数据损坏",
        littlefs::Error::InvalidName => "路径无效",
        littlefs::Error::NotFound => "路径不存在",
        littlefs::Error::AlreadyExists => "目标路径已存在",
        littlefs::Error::IsDirectory => "目标是目录",
        littlefs::Error::NotDirectory => "路径组件不是目录",
        littlefs::Error::DirectoryNotEmpty => "目录非空",
        littlefs::Error::InvalidMove => "目录不能移动到自身或其子目录",
        littlefs::Error::NoSpace => "文件系统空间不足",
        littlefs::Error::FileTooLarge => "文件过大",
        littlefs::Error::RecoveryRequired => "写入结果不确定，需要重新挂载",
    }
}

fn print_fs_error(operation: &str, error: &FsError) {
    match error {
        littlefs::Error::Device(error) => {
            println!("{}: Flash 设备错误: {:?}", operation, error)
        }
        _ => println!("{}: {}", operation, fs_error_summary(error)),
    }
}

fn print_mount_error(state: &ShellState, operation: &str) {
    match state.last_mount_error.as_ref() {
        Some(error) => print_fs_error(operation, error),
        None => println!("{}: 文件系统未挂载", operation),
    }
}

fn mounted_filesystem(state: &ShellState) -> Option<crate::filesystem::MountGuard> {
    let Some(filesystem) = crate::filesystem::mounted() else {
        print_mount_error(state, "文件系统");
        return None;
    };
    Some(filesystem)
}

fn report_mutation_error(state: &mut ShellState, operation: &str, error: FsError) {
    print_fs_error(operation, &error);
    let recovery_required = crate::filesystem::mounted()
        .map(|filesystem| filesystem.recovery_required())
        .unwrap_or(false);
    if !recovery_required {
        return;
    }

    println!("写入结果不确定，正在重新挂载...");
    if state.remount() {
        println!("文件系统已恢复挂载");
    } else {
        print_mount_error(state, "重新挂载");
    }
}

fn cmd_ls(state: &mut ShellState, rest: &str) -> CmdResult {
    let input = if rest.trim().is_empty() {
        "."
    } else {
        let Some(input) = one_argument(rest) else {
            println!("用法: ls [路径]");
            return CmdResult::Ok;
        };
        input
    };
    let Some(path) = resolve_path(state, "ls", input) else {
        return CmdResult::Ok;
    };
    let Some(mut filesystem) = mounted_filesystem(state) else {
        return CmdResult::Ok;
    };
    let info = match filesystem.stat(path.as_key()) {
        Ok(info) => info,
        Err(error) => {
            print_fs_error("ls", &error);
            return CmdResult::Ok;
        }
    };

    println!("TYPE        SIZE  CRC32     NAME");
    if info.kind == littlefs::EntryKind::File {
        println!("FILE  {:>10}  {:08X}  {}", info.size, info.crc32, path);
        return CmdResult::Ok;
    }

    let mut count = 0usize;
    match filesystem.read_dir(path.as_key(), |name, info| {
        count += 1;
        match info.kind {
            littlefs::EntryKind::File => {
                println!("FILE  {:>10}  {:08X}  {}", info.size, info.crc32, name)
            }
            littlefs::EntryKind::Directory => {
                println!("DIR   {:>10}  --------  {}/", "-", name)
            }
        }
    }) {
        Ok(()) if count == 0 => println!("(空)"),
        Ok(()) => {}
        Err(error) => print_fs_error("ls", &error),
    }
    CmdResult::Ok
}

fn print_file_bytes(bytes: &[u8]) {
    let mut start = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        if matches!(byte, b'\n' | b'\t' | 0x20..=0x7e) {
            continue;
        }
        if let Ok(text) = core::str::from_utf8(&bytes[start..index]) {
            print!("{}", text);
        }
        print!("\\x{:02X}", byte);
        start = index + 1;
    }
    if let Ok(text) = core::str::from_utf8(&bytes[start..]) {
        print!("{}", text);
    }
}

/// 分块读取文件；不可打印字节以 `\xNN` 显示。
fn cmd_cat(state: &mut ShellState, rest: &str) -> CmdResult {
    let Some(input) = one_argument(rest) else {
        println!("用法: cat <文件>");
        return CmdResult::Ok;
    };
    let Some(path) = resolve_path(state, "cat", input) else {
        return CmdResult::Ok;
    };
    let Some(mut filesystem) = mounted_filesystem(state) else {
        return CmdResult::Ok;
    };
    if let Err(error) = filesystem.verify() {
        print_fs_error("cat", &error);
        return CmdResult::Ok;
    }
    let info = match filesystem.stat(path.as_key()) {
        Ok(info) => info,
        Err(error) => {
            print_fs_error("cat", &error);
            return CmdResult::Ok;
        }
    };
    if info.kind == littlefs::EntryKind::Directory {
        println!("cat: 是目录: {}", path);
        return CmdResult::Ok;
    }

    let mut buffer = [0u8; 128];
    let mut offset = 0;
    let mut ends_with_newline = false;
    while offset < info.size {
        let read = match filesystem.read(path.as_key(), offset, &mut buffer) {
            Ok(0) => {
                println!();
                println!("cat: 文件提前结束");
                return CmdResult::Ok;
            }
            Ok(read) => read,
            Err(error) => {
                println!();
                print_fs_error("cat", &error);
                return CmdResult::Ok;
            }
        };
        print_file_bytes(&buffer[..read]);
        ends_with_newline = buffer[read - 1] == b'\n';
        offset += read as u32;
    }
    if info.size == 0 || !ends_with_newline {
        println!();
    }
    CmdResult::Ok
}

/// 原子创建或完整覆盖一个短文本文件。
fn cmd_write(state: &mut ShellState, rest: &str) -> CmdResult {
    let Some((input, text)) = name_and_text(rest) else {
        println!("用法: write <文件> [文本]");
        return CmdResult::Ok;
    };
    let Some(path) = resolve_path(state, "write", input) else {
        return CmdResult::Ok;
    };
    let Some(mut filesystem) = mounted_filesystem(state) else {
        return CmdResult::Ok;
    };
    let result = filesystem.write(path.as_key(), text.as_bytes());
    drop(filesystem); // 释放文件系统锁, 允许 report_mutation_error 重新挂载
    match result {
        Ok(()) => println!("{}: 已持久化 {} B", path, text.len()),
        Err(error) => report_mutation_error(state, "write", error),
    }
    CmdResult::Ok
}

/// 全屏编辑 (编译期开关 CFG_SHELL_NANO_ENABLE 控制, 关闭时命令不注册)
#[cfg(shell_nano)]
fn cmd_nano(state: &mut ShellState, rest: &str) -> CmdResult {
    let Some(input) = one_argument(rest) else {
        println!("用法: nano <文件>");
        return CmdResult::Ok;
    };
    let Some(path) = resolve_path(state, "nano", input) else {
        return CmdResult::Ok;
    };
    editor::run(state, path.as_key());
    CmdResult::Ok
}

fn cmd_rm(state: &mut ShellState, rest: &str) -> CmdResult {
    let Some(input) = one_argument(rest) else {
        println!("用法: rm <文件>");
        return CmdResult::Ok;
    };
    let Some(path) = resolve_path(state, "rm", input) else {
        return CmdResult::Ok;
    };
    let Some(mut filesystem) = mounted_filesystem(state) else {
        return CmdResult::Ok;
    };
    let result = filesystem.remove(path.as_key());
    drop(filesystem); // 释放文件系统锁, 允许 report_mutation_error 重新挂载
    match result {
        Ok(()) => println!("{}: 已删除", path),
        Err(error) => report_mutation_error(state, "rm", error),
    }
    CmdResult::Ok
}

fn cmd_mv(state: &mut ShellState, rest: &str) -> CmdResult {
    let Some((old_input, new_input)) = two_arguments(rest) else {
        println!("用法: mv <旧路径> <新路径>");
        return CmdResult::Ok;
    };
    let Some(old_path) = resolve_path(state, "mv", old_input) else {
        return CmdResult::Ok;
    };
    let Some(new_path) = resolve_path(state, "mv", new_input) else {
        return CmdResult::Ok;
    };
    let Some(mut filesystem) = mounted_filesystem(state) else {
        return CmdResult::Ok;
    };
    let source_kind = match filesystem.stat(old_path.as_key()) {
        Ok(info) => info.kind,
        Err(error) => {
            print_fs_error("mv", &error);
            return CmdResult::Ok;
        }
    };
    let result = filesystem.rename(old_path.as_key(), new_path.as_key());
    drop(filesystem); // 释放文件系统锁, 允许 report_mutation_error 重新挂载
    match result {
        Ok(()) => {
            if source_kind == littlefs::EntryKind::Directory
                && state.cwd.rebase(&old_path, &new_path).is_err()
            {
                state.cwd = ShellPath::root();
            }
            println!("{} -> {}", old_path, new_path);
        }
        Err(error) => report_mutation_error(state, "mv", error),
    }
    CmdResult::Ok
}

fn cmd_stat(state: &mut ShellState, rest: &str) -> CmdResult {
    let Some(input) = one_argument(rest) else {
        println!("用法: stat <路径>");
        return CmdResult::Ok;
    };
    let Some(path) = resolve_path(state, "stat", input) else {
        return CmdResult::Ok;
    };
    let Some(mut filesystem) = mounted_filesystem(state) else {
        return CmdResult::Ok;
    };
    match filesystem.stat(path.as_key()) {
        Ok(info) => {
            println!("路径 : {}", path);
            println!(
                "类型 : {}",
                match info.kind {
                    littlefs::EntryKind::File => "文件",
                    littlefs::EntryKind::Directory => "目录",
                }
            );
            if info.kind == littlefs::EntryKind::File {
                println!("大小 : {} B", info.size);
                println!("CRC32: {:08X}", info.crc32);
            }
        }
        Err(error) => print_fs_error("stat", &error),
    }
    CmdResult::Ok
}

fn cmd_df(state: &mut ShellState, rest: &str) -> CmdResult {
    if !rest.trim().is_empty() {
        println!("用法: df");
        return CmdResult::Ok;
    }
    let Some(filesystem) = crate::filesystem::mounted() else {
        print_mount_error(state, "df");
        return CmdResult::Ok;
    };
    let info = filesystem.info();
    let usage = info
        .serialized_bytes
        .saturating_mul(100)
        .checked_div(info.capacity_bytes)
        .unwrap_or(0);
    println!(
        "{:<14}  {:>7}  {:>7}  {:>4}  {:>7}  {:>10}  {:>6}  {:>9}  {}",
        "Filesystem",
        "Size(B)",
        "Used(B)",
        "Use%",
        "Entries",
        "Gen",
        "Blocks",
        "WearMin",
        "WearMax"
    );
    println!(
        "{:<14}  {:>7}  {:>7}  {:>3}%  {:>7}  {:>10}  {}/{}  {:>6}  {:>9}",
        "internal-flash",
        info.capacity_bytes,
        info.serialized_bytes,
        usage,
        info.entry_count,
        info.generation,
        info.active_blocks,
        crate::filesystem::BLOCK_COUNT,
        info.min_erase_count,
        info.max_erase_count
    );
    CmdResult::Ok
}

fn cmd_level(state: &mut ShellState, rest: &str) -> CmdResult {
    if !rest.trim().is_empty() {
        println!("用法: level");
        return CmdResult::Ok;
    }
    let Some(mut filesystem) = mounted_filesystem(state) else {
        return CmdResult::Ok;
    };
    let generation_before = filesystem.info().generation;
    match filesystem.level() {
        Ok(()) => {
            let (min_after, max_after) = filesystem.wear_bounds();
            if filesystem.info().generation == generation_before {
                println!("磨损已均衡 (min={min_after}, max={max_after})");
            } else {
                println!("快照已搬到磨损最低区 (min={min_after}, max={max_after})");
            }
        }
        Err(error) => print_fs_error("level", &error),
    }
    CmdResult::Ok
}

fn cmd_fsck(state: &mut ShellState, rest: &str) -> CmdResult {
    if !rest.trim().is_empty() {
        println!("用法: fsck");
        return CmdResult::Ok;
    }
    let Some(mut filesystem) = mounted_filesystem(state) else {
        return CmdResult::Ok;
    };
    match filesystem.verify() {
        Ok(()) => println!("fsck: 当前快照完整"),
        Err(error) => print_fs_error("fsck", &error),
    }
    CmdResult::Ok
}

fn cmd_mount(state: &mut ShellState, rest: &str) -> CmdResult {
    if !rest.trim().is_empty() {
        println!("用法: mount");
        return CmdResult::Ok;
    }
    if state.remount() {
        let info = crate::filesystem::mounted().unwrap().info();
        println!(
            "文件系统已挂载: generation={}, 条目={} 个",
            info.generation, info.entry_count
        );
    } else {
        print_mount_error(state, "mount");
    }
    CmdResult::Ok
}

fn cmd_mkfs(state: &mut ShellState, rest: &str) -> CmdResult {
    if rest.trim() != "--force" {
        println!("用法: mkfs --force");
        println!("警告: 该命令会清空全部文件");
        return CmdResult::Ok;
    }

    if let Some(mut filesystem) = crate::filesystem::mounted() {
        let result = filesystem.clear();
        drop(filesystem); // 释放文件系统锁, 允许 report_mutation_error 重新挂载
        match result {
            Ok(()) => {
                state.cwd = ShellPath::root();
                println!("空文件系统已提交并持久化");
            }
            Err(error) => report_mutation_error(state, "mkfs", error),
        }
    } else if state.format_unmounted() {
        println!("文件系统已格式化并挂载");
    } else {
        print_mount_error(state, "mkfs");
    }
    CmdResult::Ok
}

/// LED 控制
fn cmd_led(_state: &mut ShellState, rest: &str) -> CmdResult {
    match rest.trim() {
        "on" => {
            crate::board::BoardResources::get().set_led(true);
            println!("LED on");
        }
        "off" => {
            crate::board::BoardResources::get().set_led(false);
            println!("LED off");
        }
        _ => println!("用法: led on|off"),
    }
    CmdResult::Ok
}

/// 内核自检: **同步执行** (完成后才出下一提示符, 可按 ESC 中断)
/// (编译期开关 CFG_APP_SELFTEST_ENABLE 控制, 关闭时命令不注册)
#[cfg(shell_selftest)]
fn cmd_selftest(_state: &mut ShellState, rest: &str) -> CmdResult {
    match rest.trim() {
        "" | "all" => crate::selftest::run(),
        "can" => crate::selftest::run_can(),
        _ => println!("用法: selftest [all|can]"),
    }
    CmdResult::Ok
}

/// 长期稳定性测试: **同步执行** (期间压力线程在后台运行,
/// shell 线程兼任监控器, 可按 ESC 中断)
/// (编译期开关 CFG_SOAK_ENABLE 控制, 关闭时命令不注册)
#[cfg(shell_soak)]
fn cmd_soak(_state: &mut ShellState, rest: &str) -> CmdResult {
    crate::soak::run(rest);
    CmdResult::Ok
}

/// 清屏 (ANSI)
fn cmd_clear(_state: &mut ShellState, _rest: &str) -> CmdResult {
    println!("\x1b[2J\x1b[H");
    CmdResult::Ok
}

/// 当前用户
fn cmd_whoami(_state: &mut ShellState, _rest: &str) -> CmdResult {
    println!("{}", SHELL_USERNAME);
    CmdResult::Ok
}

/// 软复位 (AIRCR.SYSRESETREQ)
fn cmd_reboot(_state: &mut ShellState, _rest: &str) -> CmdResult {
    println!("rebooting...");
    // 先把缓冲中的日志同步落盘, 再复位 (日志线程周期刷新之外的最后一次)
    crate::log_info!("系统重启: shell reboot 命令");
    crate::logfile::flush_now();
    crate::rtos::thread_delay_ms(50).expect("shell 延时必须在线程上下文");
    crate::arch::system_reset()
}

/// 退出 shell (重新登录)
fn cmd_logout(_state: &mut ShellState, _rest: &str) -> CmdResult {
    CmdResult::Logout
}

/// 日志控制: 无参数显示状态; `on|off` 切换开关; `level <级别>` 调整阈值
///
/// 仅影响应用日志 (`log::*`), 内核打印 (横幅/shell 输出等) 不受影响;
/// 重启后恢复配置默认值 (`CFG_LOG_ENABLE` / `CFG_LOG_LEVEL`)。
fn cmd_log(_state: &mut ShellState, rest: &str) -> CmdResult {
    let mut words = rest.split_whitespace();
    match words.next() {
        None => println!(
            "日志: {} (级别阈值 = {}, 落盘 = {})",
            if crate::log::enabled() {
                "开启"
            } else {
                "关闭"
            },
            crate::log::level().name(),
            file_status()
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
        Some("file") => match words.next() {
            None => println!(
                "日志落盘: {} (待写入 {} B)",
                file_status(),
                crate::log::pending_bytes()
            ),
            Some("on") => {
                crate::log::set_file_enabled(true);
                println!("日志落盘已开启 (/log/)");
            }
            Some("off") => {
                crate::log::set_file_enabled(false);
                println!("日志落盘已关闭");
            }
            Some(_) => println!("用法: log file [on|off]"),
        },
        Some(_) => println!("用法: log [on|off|level <级>|file [on|off]]"),
    }
    CmdResult::Ok
}

fn file_status() -> &'static str {
    if crate::log::file_enabled() {
        "开启"
    } else {
        "关闭"
    }
}

fn read_pending_timeout<const U: u8>(
    pending_rx: &mut PendingRx,
    uart: &crate::uart::Uart<U>,
    timeout_ms: u32,
) -> Option<u8> {
    pending_rx
        .pop_front()
        .or_else(|| uart.read_rx_timeout_ms(timeout_ms))
}

#[derive(Clone, Copy)]
enum HistoryMove {
    Older,
    Newer,
}

fn read_shell_csi<const U: u8>(
    pending_rx: &mut PendingRx,
    uart: &crate::uart::Uart<U>,
) -> Option<HistoryMove> {
    for _ in 0..48 {
        let byte = read_pending_timeout(pending_rx, uart, INPUT_CRLF_TIMEOUT_MS)?;
        if (0x40..=0x7e).contains(&byte) {
            return match byte {
                b'A' => Some(HistoryMove::Older),
                b'B' => Some(HistoryMove::Newer),
                _ => None,
            };
        }
    }
    None
}

fn consume_shell_escape<const U: u8>(
    pending_rx: &mut PendingRx,
    uart: &crate::uart::Uart<U>,
) -> Option<HistoryMove> {
    let next = read_pending_timeout(pending_rx, uart, INPUT_CRLF_TIMEOUT_MS)?;
    match next {
        b'[' | b'O' | 0x9b => read_shell_csi(pending_rx, uart),
        byte => {
            let queued = pending_rx.push_front(byte);
            debug_assert!(queued);
            None
        }
    }
}

fn replace_input_line(line: &mut alloc::string::String, replacement: &str) {
    while line.pop().is_some() {
        print!("\x08 \x08");
    }
    line.push_str(replacement);
    print!("{}", replacement);
}

fn browse_history(
    movement: HistoryMove,
    history: &CommandHistory,
    offset: &mut Option<usize>,
    draft: &mut HistoryEntry,
    line: &mut alloc::string::String,
) {
    match movement {
        HistoryMove::Older => {
            let next = offset.map_or(0, |current| current + 1);
            let Some(entry) = history.newest(next) else {
                return;
            };
            if offset.is_none() {
                draft.set(line, 0);
            }
            replace_input_line(line, entry.text());
            *offset = Some(next);
        }
        HistoryMove::Newer => match *offset {
            Some(0) => {
                replace_input_line(line, draft.text());
                *offset = None;
            }
            Some(current) => {
                let next = current - 1;
                if let Some(entry) = history.newest(next) {
                    replace_input_line(line, entry.text());
                    *offset = Some(next);
                }
            }
            None => {}
        },
    }
}

/// 从 UART 读取一行 (阻塞, 支持退格/Ctrl+C)
///
/// `masked` 为 true 时输入不回显 (密码模式)。
/// 中断驱动: 挂起在数据到达信号量上, 由 RX ISR 唤醒, 无轮询。
fn read_line(
    pending_rx: &mut PendingRx,
    masked: bool,
    max: usize,
    history: Option<&CommandHistory>,
) -> InputLine {
    let uart = crate::board::BoardResources::get().console();
    // 错误统计窗口从上一行结束延续到本行结束。这样即使用户在上一条
    // 命令执行期间提前输入，期间发生的硬件错误或软件环溢出也不会在
    // 新提示符出现时被清掉；受影响的下一行会被完整拒绝。
    let mut line = alloc::string::String::new();
    let mut overflow = 0usize;
    let mut non_ascii = false;
    let mut history_offset = None;
    let mut draft = HistoryEntry::empty();
    loop {
        let b = pending_rx
            .pop_front()
            .unwrap_or_else(|| uart.read_rx_blocking());
        match b {
            b'\r' => {
                if let Some(next) = read_pending_timeout(pending_rx, uart, INPUT_CRLF_TIMEOUT_MS)
                    && next != b'\n'
                {
                    let queued = pending_rx.push_front(next);
                    debug_assert!(queued);
                }
                break;
            }
            b'\n' => break,
            0x1b => {
                let movement = consume_shell_escape(pending_rx, uart);
                if !masked
                    && overflow == 0
                    && let (Some(history), Some(movement)) = (history, movement)
                {
                    browse_history(
                        movement,
                        history,
                        &mut history_offset,
                        &mut draft,
                        &mut line,
                    );
                }
            }
            // A standalone C1 CSI is accepted for terminals configured in
            // 8-bit mode. Once another high byte has appeared, 0x9B may be a
            // UTF-8 continuation byte and must not consume the rest of a path.
            0x9b if !non_ascii => {
                let movement = read_shell_csi(pending_rx, uart);
                if !masked
                    && overflow == 0
                    && let (Some(history), Some(movement)) = (history, movement)
                {
                    browse_history(
                        movement,
                        history,
                        &mut history_offset,
                        &mut draft,
                        &mut line,
                    );
                }
            }
            0x08 | 0x7F => {
                // 超出缓冲区的字符没有回显，先消费对应的退格。
                if overflow > 0 {
                    overflow -= 1;
                } else if line.pop().is_some() {
                    print!("\x08 \x08");
                }
            }
            0x03 => {
                // Ctrl+C: 清空当前行
                overflow = 0;
                non_ascii = false;
                history_offset = None;
                draft = HistoryEntry::empty();
                while line.pop().is_some() {
                    print!("\x08 \x08");
                }
            }
            0x20..=0x7E => {
                if line.len() < max && overflow == 0 {
                    line.push(b as char);
                    if masked {
                        print!("*");
                    } else {
                        print!("{}", b as char);
                    }
                } else {
                    overflow = overflow.saturating_add(1);
                }
            }
            0x80..=0xFF => non_ascii = true,
            _ => {}
        }
    }
    let dropped = uart.rx_dropped_count();
    let (parity, framing, overrun) = uart.rx_error_counts();
    InputLine {
        text: line,
        overflowed: overflow != 0,
        non_ascii,
        rx_corrupted: dropped != 0 || parity != 0 || framing != 0 || overrun != 0,
    }
}
