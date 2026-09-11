//! CAN 真机测试命令 (`can`): 状态 / 初始化 / 发送 / 接收 / 监听。
//!
//! 用于配合 **CAN 转 USB 适配器** (CANable/slcan、PCAN 等) 做外部总线验证:
//! 板端 PB7(TX)/PB6(RX) 经**外部 CAN 收发器**接入总线 (本板无板载 PHY,
//! 详见 README "CAN 驱动"), PC 端用 `candump can0` / `cansend can0 ...`。
//!
//! # 与 `selftest can` 的分工
//!
//! `selftest can` 是**内部回环** (不驱动 TX 引脚、控制器自动 ACK), 只验证
//! 控制器逻辑; 本命令面向真实总线: `can init normal` 后收发都经引脚与
//! 收发器, 需要总线上的第二个节点 (适配器) 应答。
//!
//! # 生命周期
//!
//! 命令可按需 `can init` / `can deinit`, **不要求 `CFG_CAN_ENABLE=true`**
//! (该开关只决定开机时是否自动初始化应用 CAN)。若开机已由板级初始化
//! (CFG_CAN_ENABLE=true), `can status` 直接可用; `can init` 会先 deinit
//! 再按指定模式重建 (会清空硬件收发队列)。
//!
//! 注意: `selftest can` 在已初始化时会跳过 (它要求独占控制器), 因此
//! 本命令初始化后若要跑自检, 先 `can deinit`。

use super::{CmdResult, ShellState};
use crate::can::{self, Can, Config, ErrorKind, Id, RxFrame, TxBuffer, TxFrame, WorkMode};
use crate::println;

/// 接收/监听轮询间隔 (毫秒): 兼顾响应与 CPU 占用
const POLL_INTERVAL_MS: u32 = 2;
/// 发送完成后等待 PTB 结束的上限 (毫秒)。正常模式下若无对端应答,
/// 控制器会持续重发, 本上限避免命令卡死 (超时后由错误计数反映问题)。
const TX_WAIT_MS: u32 = 200;

/// 命令行文本缓冲 (栈上组装, 单次输出保证整行原子)
struct LineBuf {
    buf: [u8; 128],
    len: usize,
}

impl LineBuf {
    const fn new() -> Self {
        Self {
            buf: [0; 128],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl core::fmt::Write for LineBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        if self.len + bytes.len() > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }
}

fn board_can() -> &'static Can {
    crate::board::BoardResources::get().can()
}

fn board_clocks() -> &'static crate::clk::Clocks {
    crate::board::BoardResources::get().clocks()
}

/// 轮询 ESC (0x1B) 请求中止, 并丢弃其他待读输入 (与 selftest/soak 同语义)
fn abort_requested() -> bool {
    let mut esc = false;
    while let Some(byte) = crate::board::BoardResources::get().console().read_rx() {
        if byte == 0x1B {
            esc = true;
        }
    }
    esc
}

/// 打印一帧 (整行一次输出, 不经 `println!` 的行间间隙以便连续监听)
fn print_frame(frame: &RxFrame) {
    let mut line = LineBuf::new();
    let _ = core::fmt::write(
        &mut line,
        format_args!(
            "  {:>8}  [{}] {}{}{}{}\r\n",
            IdText(frame.id),
            frame.dlc,
            DataText(frame),
            if frame.rtr { " RTR" } else { "" },
            if frame.self_tx { " self" } else { "" },
            if frame.error == ErrorKind::None {
                ""
            } else {
                " err"
            }
        ),
    );
    crate::print!("{}", line.as_str());
}

/// 帧 ID 显示 (标准 3 位十六进制 / 扩展 8 位 + `x` 后缀)
struct IdText(Id);
impl core::fmt::Display for IdText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Id::Standard(id) => write!(f, "{id:03X}"),
            Id::Extended(id) => write!(f, "{id:08X}x"),
        }
    }
}

/// 数据字节显示 (十六进制, 空格分隔; RTR 帧无数据)
struct DataText<'a>(&'a RxFrame);
impl core::fmt::Display for DataText<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.rtr {
            return Ok(());
        }
        for index in 0..usize::from(self.0.dlc.min(8)) {
            write!(f, "{:02X} ", self.0.data[index])?;
        }
        Ok(())
    }
}

fn parse_mode(arg: Option<&str>) -> Result<WorkMode, &'static str> {
    match arg.unwrap_or("normal") {
        "normal" | "n" => Ok(WorkMode::Normal),
        "silent" | "s" => Ok(WorkMode::Silent),
        "int" | "int-loopback" | "internal" => Ok(WorkMode::InternalLoopback),
        "ext" | "ext-loopback" | "external" => Ok(WorkMode::ExternalLoopback),
        "ext-silent" | "ext-loopback-silent" => Ok(WorkMode::ExternalLoopbackSilent),
        _ => Err("模式: normal | silent | int | ext | ext-silent"),
    }
}

/// 按模式构造初始化参数: 基础参数取配置, 仅覆盖模式与自 ACK
/// (自 ACK 只对外部回环有意义, 且由 `CFG_CAN_SELF_ACK` 决定)
fn config_for(mode: WorkMode) -> Config {
    let external_loopback = matches!(
        mode,
        WorkMode::ExternalLoopback | WorkMode::ExternalLoopbackSilent
    );
    Config {
        mode,
        self_ack: external_loopback && crate::config::CAN_SELF_ACK,
        ..crate::config::CAN_CONFIG
    }
}

fn print_usage() {
    println!("CAN 测试命令 (外部总线需经收发器接入, 板端 PB7=TX/PB6=RX):");
    println!("  can status                        状态/位时序/错误计数/FIFO");
    println!("  can init [mode]                   初始化 (默认 normal)");
    println!("      mode: normal|silent|int|ext|ext-silent");
    println!("  can deinit                        关闭 CAN (释放控制器时钟)");
    println!("  can send <id> [--ext] [--rtr] [--dlc=N] [字节...]");
    println!("      例: can send 123 DE AD BE EF     标准帧");
    println!("          can send 18FF50E5 --ext --dlc=2 11 22   扩展帧");
    println!("          can send 123 --rtr --dlc=8       远程帧");
    println!("  can recv [帧数] [超时ms]          收帧并打印 (默认 1 帧/3000ms)");
    println!("  can listen [秒数] [normal|silent] 连续监听 (默认 10s, silent)");
    println!("  监听/接收期间按 ESC 中止; 数据以十六进制解析 (可省略 0x)");
    println!("  提示: 要作为正常节点接收 (发 ACK) 用 normal; silent 只听不 ACK,");
    println!("        对端会因无应答而重发/报错, 仅适合嗅探总线");
}

fn cmd_status() {
    let can = board_can();
    let Some(mode) = can.work_mode() else {
        println!("CAN: 未初始化 (先 `can init [mode]`, 或配置 CFG_CAN_ENABLE=true)");
        return;
    };
    let clocks = board_clocks();
    let hz = clocks.xtal_hz();
    let timing = crate::config::CAN_BIT_TIMING;
    println!("CAN: 已初始化, 模式 {}", mode.name());
    println!(
        "  位时序: {} bps (实际 {}, 误差 {} ppm), 采样点 {}‰, {} TQ",
        crate::config::CAN_BITRATE,
        timing.actual_bitrate(hz),
        timing.error_ppm(hz, crate::config::CAN_BITRATE),
        timing.sample_point_permille(),
        timing.total_time_quanta()
    );
    println!(
        "  段配置: PRESC={} SEG1={} SEG2={} SJW={} (XTAL {} Hz)",
        timing.prescaler, timing.time_seg1, timing.time_seg2, timing.sjw, hz
    );
    let info = can.error_info();
    println!(
        "  错误计数: TEC={} REC={}, 仲裁丢失位置={}, 最近错误={:?}",
        info.tx_count, info.rx_count, info.arbitration_lost_position, info.kind
    );
    println!(
        "  RX FIFO: {:?} (10 槽), TX PTB 忙={}, TX STB 水位={:?}",
        can.rx_buffer_status(),
        can.tx_pending(TxBuffer::Primary),
        can.tx_buffer_status()
    );
    let status = can.status();
    let bits = status.bits();
    println!("  状态位: 0x{bits:08X}");
    for (flag, name) in [
        (can::Status::BUS_OFF, "BUS_OFF"),
        (can::Status::ERROR_PASSIVE_NODE, "ERROR_PASSIVE"),
        (can::Status::ERROR_COUNT_WARNING, "ERROR_WARN"),
        (can::Status::RX_OVERFLOW, "RX_OVERFLOW"),
        (can::Status::RX_OVERRUN, "RX_OVERRUN"),
        (can::Status::ARBITRATION_LOST, "ARBITRATION_LOST"),
        (can::Status::BUS_ERROR, "BUS_ERROR"),
    ] {
        if bits & flag.bits() != 0 {
            println!("    ! {name}");
        }
    }
}

fn cmd_init(mode_arg: Option<&str>) {
    let mode = match parse_mode(mode_arg) {
        Ok(mode) => mode,
        Err(usage) => {
            println!("can init: 模式无效 ({usage})");
            return;
        }
    };
    let can = board_can();
    if can.is_initialized() {
        can.deinit();
    }
    match can.init(board_clocks(), config_for(mode)) {
        Ok(timing) => {
            let hz = board_clocks().xtal_hz();
            println!(
                "CAN: 已初始化, 模式 {} — {} bps (实际 {}, 采样点 {}‰, PRESC={} SEG1={} SEG2={})",
                mode.name(),
                crate::config::CAN_BITRATE,
                timing.actual_bitrate(hz),
                timing.sample_point_permille(),
                timing.prescaler,
                timing.time_seg1,
                timing.time_seg2
            );
            if matches!(mode, WorkMode::Normal | WorkMode::Silent) {
                println!("  提示: 正常/静默模式需外部收发器与第二个节点 (适配器) 应答 ACK");
            }
        }
        Err(error) => println!("CAN: 初始化失败: {error:?} (检查收发器接线/XTAL 起振)"),
    }
}

fn cmd_deinit() {
    let can = board_can();
    if !can.is_initialized() {
        println!("CAN: 未初始化");
        return;
    }
    can.deinit();
    println!("CAN: 已关闭 (控制器时钟已释放)");
}

/// 解析十六进制 ID (可带 0x 前缀)
fn parse_id(text: &str, extended: bool) -> Option<Id> {
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    let value = u32::from_str_radix(digits, 16).ok()?;
    if extended {
        (value <= 0x1FFF_FFFF).then_some(Id::Extended(value))
    } else {
        (value <= 0x7FF).then_some(Id::Standard(value as u16))
    }
}

/// 解析数据字节: 连续十六进制串 (如 `DEADBEEF`) 或单个字节
fn parse_data(text: &str, data: &mut [u8; 8], len: &mut usize) -> bool {
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    if text.is_empty()
        || !text.len().is_multiple_of(2)
        || !text.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return false;
    }
    let mut index = 0;
    while index < text.len() {
        if *len >= data.len() {
            return false;
        }
        let Ok(byte) = u8::from_str_radix(&text[index..index + 2], 16) else {
            return false;
        };
        data[*len] = byte;
        *len += 1;
        index += 2;
    }
    true
}

fn cmd_send<'a>(mut words: impl Iterator<Item = &'a str>) {
    let can = board_can();
    if !can.is_initialized() {
        println!("can send: CAN 未初始化 (先 `can init normal`)");
        return;
    }
    // silent 模式 TCMD.LOM 置位: 控制器不上总线也不发 ACK, 直接拦下避免误判
    if matches!(
        can.work_mode(),
        Some(WorkMode::Silent | WorkMode::ExternalLoopbackSilent)
    ) {
        println!("can send: 当前为 silent (只听) 模式, 不会上总线; 先 `can init normal`");
        return;
    }
    let mut id_arg: Option<&str> = None;
    let mut extended = false;
    let mut rtr = false;
    let mut dlc_override: Option<u8> = None;
    let mut data = [0u8; 8];
    let mut len = 0usize;

    for word in words.by_ref() {
        match word {
            "--ext" | "--extended" => extended = true,
            "--rtr" => rtr = true,
            _ if word.starts_with("--dlc=") => match word[6..].parse::<u8>() {
                Ok(value) if value <= 8 => dlc_override = Some(value),
                _ => {
                    println!("can send: --dlc 应为 0~8");
                    return;
                }
            },
            _ if id_arg.is_none() => id_arg = Some(word),
            _ => {
                if !parse_data(word, &mut data, &mut len) {
                    println!("can send: 数据无效 (每字节两位十六进制, 最多 8 字节)");
                    return;
                }
            }
        }
    }

    let Some(id_arg) = id_arg else {
        println!("用法: can send <id> [--ext] [--rtr] [--dlc=N] [字节...]");
        return;
    };
    let Some(id) = parse_id(id_arg, extended) else {
        println!(
            "can send: ID 无效 (标准 ≤0x7FF, 扩展 ≤0x1FFFFFFF{}; 十六进制)",
            if extended { "" } else { ", 扩展请加 --ext" }
        );
        return;
    };
    let dlc = dlc_override.unwrap_or(len as u8);
    if dlc as usize > 8 {
        println!("can send: DLC 超出 8");
        return;
    }
    let frame = if rtr {
        TxFrame::remote(id, dlc)
    } else {
        TxFrame::data(id, dlc, data)
    };

    match can.try_transmit_ptb(&frame) {
        Ok(()) => {
            let start = crate::rtos::uptime_ms();
            while can.tx_pending(TxBuffer::Primary) {
                if crate::rtos::uptime_ms().wrapping_sub(start) >= TX_WAIT_MS {
                    break;
                }
                crate::rtos::thread_delay_ms(1).ok();
            }
            let info = can.error_info();
            if info.kind == ErrorKind::None && info.tx_count == 0 {
                println!("CAN: 已发送 (TEC=0, 对端已应答)");
            } else {
                println!(
                    "CAN: 已发出但总线异常: 最近错误={:?}, TEC={} REC={} (检查对端/终端电阻/位速率)",
                    info.kind, info.tx_count, info.rx_count
                );
            }
        }
        Err(error) => println!("CAN: 发送失败: {error:?}"),
    }
}

/// 接收若干帧 (`can recv [帧数] [超时ms]`), 返回收到帧数
fn receive_loop(limit: u32, timeout_ms: u32) -> u32 {
    let can = board_can();
    let start = crate::rtos::uptime_ms();
    let mut got = 0u32;
    loop {
        while let Some(frame) = can.try_receive() {
            print_frame(&frame);
            got += 1;
            if got >= limit {
                return got;
            }
        }
        if crate::rtos::uptime_ms().wrapping_sub(start) >= timeout_ms {
            return got;
        }
        if abort_requested() {
            println!("CAN: 已中止 (ESC)");
            return got;
        }
        crate::rtos::thread_delay_ms(POLL_INTERVAL_MS).ok();
    }
}

fn cmd_recv<'a>(mut words: impl Iterator<Item = &'a str>) {
    let can = board_can();
    if !can.is_initialized() {
        println!("can recv: CAN 未初始化 (先 `can init normal`)");
        return;
    }
    let limit = words
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1)
        .max(1);
    let timeout_ms = words
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(3_000)
        .max(1);
    let start = crate::rtos::uptime_ms();
    let got = receive_loop(limit, timeout_ms);
    let elapsed = crate::rtos::uptime_ms().wrapping_sub(start);
    println!("CAN: 收到 {got} 帧 ({elapsed} ms)");
}

fn cmd_listen<'a>(mut words: impl Iterator<Item = &'a str>) {
    let seconds = words
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(10);
    let mode = match parse_mode(Some(words.next().unwrap_or("silent"))) {
        Ok(mode) => mode,
        Err(usage) => {
            println!("can listen: 模式无效 ({usage})");
            return;
        }
    };
    let can = board_can();
    if !can.is_initialized() || can.work_mode() != Some(mode) {
        if can.is_initialized() {
            can.deinit();
        }
        match can.init(board_clocks(), config_for(mode)) {
            Ok(_) => {}
            Err(error) => {
                println!("can listen: 初始化失败: {error:?}");
                return;
            }
        }
    }
    println!(
        "CAN: 监听 {}s (模式 {}) — 按 ESC 中止; 对端: candump can0",
        seconds,
        mode.name()
    );
    let duration_ms = seconds.saturating_mul(1_000);
    let start = crate::rtos::uptime_ms();
    let mut frames = 0u32;
    let mut last_report = start;
    loop {
        let elapsed = crate::rtos::uptime_ms().wrapping_sub(start);
        while let Some(frame) = can.try_receive() {
            print_frame(&frame);
            frames += 1;
        }
        if duration_ms != 0 && elapsed >= duration_ms {
            break;
        }
        if crate::rtos::uptime_ms().wrapping_sub(last_report) >= 1_000 {
            let info = can.error_info();
            let status = can.status();
            println!(
                "  [{}s] 累计 {} 帧, TEC={} REC={}, 状态=0x{:08X}",
                elapsed / 1_000,
                frames,
                info.tx_count,
                info.rx_count,
                status.bits()
            );
            last_report = crate::rtos::uptime_ms();
        }
        if abort_requested() {
            println!("CAN: 监听中止 (ESC)");
            break;
        }
        crate::rtos::thread_delay_ms(POLL_INTERVAL_MS).ok();
    }
    let info = can.error_info();
    println!(
        "CAN: 监听结束 — 共 {} 帧, 最近错误={:?}, TEC={} REC={}, RX FIFO={:?}",
        frames,
        info.kind,
        info.tx_count,
        info.rx_count,
        can.rx_buffer_status()
    );
}

/// `can` 命令入口 (子命令分发)
pub(super) fn cmd_can(_state: &mut ShellState, rest: &str) -> CmdResult {
    let mut words = rest.split_whitespace();
    match words.next() {
        Some("status") => cmd_status(),
        Some("init") => cmd_init(words.next()),
        Some("deinit") | Some("off") => cmd_deinit(),
        Some("send") => cmd_send(words),
        Some("recv") | Some("receive") => cmd_recv(words),
        Some("listen") | Some("monitor") => cmd_listen(words),
        _ => print_usage(),
    }
    CmdResult::Ok
}
