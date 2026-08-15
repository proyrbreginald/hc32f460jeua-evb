//! ZMODEM 文件传输的 shell 集成: 控制台 UART 端口适配 + `sz`/`rz` 命令
//!
//! 协议核心见 [`crate::zmodem`] (纯逻辑, 主机可测)。本模块把控制台 UART
//! 适配为 [`crate::zmodem::ZmPort`], 并把快照文件系统接入收发回调:
//!
//! - `sz <文件> [文件...]`: 把板内文件发送给主机 (主机执行 `rz -y`);
//! - `rz`: 接收主机发来的文件 (主机执行 `sz <文件>`), 整文件缓存后
//!   原子写入快照文件系统。
//!
//! # 通信约束
//!
//! 控制台 UART 同时承载协议字节与状态文本。协议接收端对帧前字节一律
//! 按垃圾丢弃 (lrzsz 的 `zgethdr` 语义), 因此**仅在帧间空闲点**打印:
//! 会话开始/结束、文件开始 (决定/跳过) 与文件完成 (提交)。数据流中
//! 不打印任何内容, 避免干扰对端 (尤其流式 `ZCRCG` 阶段)。
//!
//! 中止: 帧间等待时按 ESC 取消会话 (发送 CAN×5 通知对端)。

use crate::config;
use crate::println;
use crate::uart_rtos::UartRtosExt;
use core::cell::RefCell;
use crate::zmodem::{self, SendFile, ZmConfig, ZmError, ZmPort};

use super::{mounted_filesystem, CmdResult, ShellState};

/// 进度输出节流 (毫秒)
const PROGRESS_INTERVAL_MS: u32 = 500;

// ============================== UART 端口适配 ==============================

/// 控制台 UART 的 ZMODEM 端口: 阻塞读带超时 + 原样写 + ESC 中止检测
struct UartPort {
    uart: &'static crate::config::ConsoleUart,
    /// 中止检测时暂存的非 ESC 字节 (压回给下一次读取)
    pending: Option<u8>,
}

impl UartPort {
    fn new() -> Self {
        Self {
            uart: crate::board::BoardResources::get().console(),
            pending: None,
        }
    }
}

impl ZmPort for UartPort {
    fn read_timeout(&mut self, timeout_ms: u32) -> Option<u8> {
        if let Some(byte) = self.pending.take() {
            return Some(byte);
        }
        self.uart.read_rx_timeout_ms(timeout_ms)
    }

    fn write(&mut self, bytes: &[u8]) {
        self.uart.write(bytes);
    }

    fn flush(&mut self) {
        self.uart.flush();
    }

    /// 帧间空闲时的 ESC 中止检测: 只消费一个待读字节
    ///
    /// 协议只在等待帧头前调用本方法, 不会在数据流中调用, 因此不会把
    /// 文件内容里的 0x1B 误判为中止。
    fn abort_requested(&mut self) -> bool {
        let byte = if let Some(byte) = self.pending.take() {
            Some(byte)
        } else {
            self.uart.read_rx()
        };
        match byte {
            Some(0x1B) => true,
            Some(byte) => {
                self.pending = Some(byte);
                false
            }
            None => false,
        }
    }
}

// ============================== 会话辅助 ==============================

/// 丢弃会话开始前的输入残留 (shell 行缓冲 + UART 环), 并复位错误统计
fn drain_input(state: &mut ShellState) {
    while state.pending_rx.pop_front().is_some() {}
    let uart = crate::board::BoardResources::get().console();
    while uart.read_rx().is_some() {}
    let _ = uart.rx_dropped_count();
    let _ = uart.rx_error_counts();
}

/// 把远端文件名规范为快照文件系统的合法名称 (取基础名, 去路径)
fn sanitize_name(raw: &[u8]) -> Option<&str> {
    let base = raw.rsplit(|&b| b == b'/' || b == b'\\').next()?;
    if base.is_empty() || base.len() > littlefs::MAX_NAME_LEN {
        return None;
    }
    let name = core::str::from_utf8(base).ok()?;
    if name.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return None;
    }
    Some(name)
}

fn zm_config() -> ZmConfig {
    ZmConfig {
        timeout_ms: config::ZMODEM_TIMEOUT_MS,
        subpacket: config::ZMODEM_SUBPACKET,
    }
}

/// 会话错误统一收尾: 通知对端取消 (CAN×5) 并打印原因
fn finish(port: &mut UartPort, command: &str, result: Result<(), ZmError>) {
    match result {
        Ok(()) => println!("{}: 完成", command),
        Err(ZmError::UserAbort) => {
            zmodem::cancel_session(port);
            println!("{}: 已中止 (ESC)", command);
        }
        Err(error) => {
            zmodem::cancel_session(port);
            println!("{}: {}", command, error);
        }
    }
}

// ============================== sz: 发送文件 ==============================

/// 发送文件: `sz <文件> [文件...]`
///
/// 在主机侧执行 `rz -y` (或 `lrz`) 接收。会话开始先与主机完成
/// ZRQINIT → ZRINIT 握手, 再逐个发送文件, 最后 ZFIN 握手。
pub(super) fn cmd_sz(state: &mut ShellState, rest: &str) -> CmdResult {
    let mut send: alloc::vec::Vec<(super::ShellPath, u32)> = alloc::vec::Vec::new();
    for input in rest.split_whitespace() {
        let Some(path) = super::resolve_path(state, "sz", input) else {
            continue;
        };
        let Some(filesystem) = mounted_filesystem(state) else {
            return CmdResult::Ok;
        };
        match filesystem.stat(path.as_key()) {
            Ok(info) if info.kind == littlefs::EntryKind::File => send.push((path, info.size)),
            Ok(_) => println!("sz: 不是文件: {}", path),
            Err(error) => super::print_fs_error("sz", &error),
        }
    }
    if send.is_empty() {
        println!("用法: sz <文件> [文件...]");
        return CmdResult::Ok;
    }

    let mut chunk: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    if chunk.try_reserve_exact(config::ZMODEM_SUBPACKET).is_err() {
        println!("sz: 内存不足 (子包缓冲 {} B)", config::ZMODEM_SUBPACKET);
        return CmdResult::Ok;
    }
    chunk.resize(config::ZMODEM_SUBPACKET, 0);

    drain_input(state);
    println!(
        "sz: 发送 {} 个文件, 在主机执行 `rz -y` 接收, 按 ESC 中止",
        send.len()
    );

    let mut port = UartPort::new();
    let files: alloc::vec::Vec<SendFile<'_>> = send
        .iter()
        .map(|(path, size)| SendFile {
            name: path.as_key(),
            size: *size,
        })
        .collect();

    let mut last_report = 0u32;
    let result = {
        let Some(filesystem) = state.filesystem.as_mut() else {
            println!("sz: 文件系统未挂载");
            return CmdResult::Ok;
        };
        let mut read_at = |index: usize, offset: u32, buf: &mut [u8]| -> Result<usize, ()> {
            let path = &send[index].0;
            filesystem
                .read(path.as_key(), offset, buf)
                .map_err(|_| ())
        };
        let mut progress = |_index: usize, sent: u32, total: u32| {
            let now = crate::rtos::uptime_ms();
            if now.saturating_sub(last_report) >= PROGRESS_INTERVAL_MS {
                last_report = now;
                let pct = if total == 0 {
                    100
                } else {
                    (100 * sent).checked_div(total).unwrap_or(0)
                };
                crate::print!("\r 已发送 {}/{} B ({}%)", sent, total, pct);
            }
        };
        zmodem::send_session(&mut port, &zm_config(), &files, &mut read_at, &mut progress, &mut chunk)
    };
    println!();
    finish(&mut port, "sz", result);
    CmdResult::Ok
}

// ============================== rz: 接收文件 ==============================

/// 接收文件: `rz`
///
/// 在主机侧执行 `sz <文件>` 发送。文件整块缓存在 RAM (上限
/// `CFG_ZMODEM_RX_MAX`), 完整接收后原子写入快照文件系统;
/// 超出上限或名称非法的文件发送 ZSKIP 跳过。
pub(super) fn cmd_rz(state: &mut ShellState, rest: &str) -> CmdResult {
    if !rest.trim().is_empty() {
        println!("用法: rz");
        return CmdResult::Ok;
    }

    let mut buffer: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    if buffer.try_reserve_exact(config::ZMODEM_RX_MAX).is_err() {
        println!("rz: 内存不足 (接收缓冲 {} B)", config::ZMODEM_RX_MAX);
        return CmdResult::Ok;
    }
    buffer.resize(config::ZMODEM_RX_MAX, 0);

    drain_input(state);
    println!("rz: 等待接收 (主机执行 `sz <文件>`), 按 ESC 中止");

    let mut port = UartPort::new();
    let result = {
        let Some(filesystem) = state.filesystem.as_mut() else {
            println!("rz: 文件系统未挂载");
            return CmdResult::Ok;
        };
        // 两个回调 (decide/commit) 分时独占文件系统句柄, 用 RefCell 共享
        let filesystem = RefCell::new(filesystem);
        let capacity = buffer.len();
        let mut decide = |raw_name: &[u8], size: u32| {
            let Some(name) = sanitize_name(raw_name) else {
                println!("rz: 跳过: 文件名无效");
                return false;
            };
            if size > capacity as u32 {
                println!("rz: 跳过 {} ({} B 超过 {} B 上限)", name, size, capacity);
                return false;
            }
            // 预检文件系统剩余容量 (整文件原子写入)
            match filesystem.borrow_mut().max_write_size(name) {
                Ok(available) if size as usize <= available as usize => true,
                Ok(available) => {
                    println!("rz: 跳过 {} (文件系统剩余 {} B)", name, available);
                    false
                }
                Err(error) => {
                    println!(
                        "rz: 跳过 {} ({})",
                        name,
                        super::fs_error_summary(&error)
                    );
                    false
                }
            }
        };
        let mut commit = |raw_name: &[u8], _size: u32, data: &[u8]| -> Result<(), ZmError> {
            let Some(name) = sanitize_name(raw_name) else {
                return Err(ZmError::Protocol("文件名无效"));
            };
            filesystem.borrow_mut().write(name, data).map_err(|error| {
                println!("rz: 写入失败: {}", super::fs_error_summary(&error));
                ZmError::File("文件系统写入失败")
            })?;
            println!("rz: 已接收 {} ({} B)", name, data.len());
            Ok(())
        };
        let mut progress = |_received: u32, _total: u32| {
            // 数据流中不打印 (见模块说明); 完成状态由 commit 输出
        };
        zmodem::receive_session(
            &mut port,
            &zm_config(),
            &mut buffer,
            &mut decide,
            &mut commit,
            &mut progress,
        )
    };
    finish(&mut port, "rz", result);
    CmdResult::Ok
}
