//! ZMODEM 文件传输协议 (设计参考 lrzsz 的 `src/zm.c` / `src/lrz.c` / `src/lsz.c`)
//!
//! 纯协议层, 零硬件依赖:
//! - 字节收发经 [`ZmPort`] 注入 (板上为控制台 UART, 主机测试为管道/通道);
//! - 文件读写经回调注入 (板上为快照文件系统, 主机测试为内存);
//! - 因此本模块可在主机上直接与真实 lrzsz (`sz`/`rz`) 互通测试。
//!
//! 会话入口:
//! - [`receive_session`]: 接收端 (对应 `rz`), 循环 ZRINIT → ZFILE → ZRPOS/ZDATA/ZEOF;
//! - [`send_session`]: 发送端 (对应 `sz`), ZRQINIT → ZRINIT → ZFILE → ZDATA → ZEOF
//!   → ZFIN 握手, 支持一批文件。
//!
//! 协议要点 (与 lrzsz 逐条对齐):
//! - 帧前缀 `ZPAD ZDLE` + 帧指示 `ZBIN`(16 位 FCS)/`ZBIN32`(32 位)/`ZHEX`;
//! - 16 位 FCS = CCITT/XMODEM CRC16 (poly 0x1021, 初值 0, 无反射);
//!   32 位 FCS = IEEE CRC32 (反射 poly 0xEDB88320, 初值 0xFFFFFFFF),
//!   接收端残差校验值 0xDEBB20E3;
//! - 转义: 仅对 ZDLE/XON/XOFF/^P (及其 +0x80 变体) 与 `@` 后的 CR 编码为
//!   `ZDLE + 字节^0x40`; 其余控制字节原样发送 (lrzsz `Zctlesc=0` 的语义);
//!   接收端既接受转义也接受裸控制字节, `ZDLE` 后跟 `ZCRCE/G/Q/W` 是数据帧结束;
//! - 数据子包: `ZDLE <ZCRCE|ZCRCG|ZCRCQ|ZCRCW>` + FCS 结束;
//!   ZCRCW 需要接收方 ZACK, ZCRCQ 触发 ZACK 但继续, ZCRCG 纯流式, ZCRCE 帧结束;
//! - 取消: 5 个连续 `CAN` (0x18) 或 `ZCAN`/`ZABORT` 帧。
//!
//! 时间模型: 所有等待以毫秒超时经 [`ZmPort::read_timeout`] 表达,
//! 由调用方提供 [`ZmConfig`]。

#![allow(dead_code)]

use core::fmt::Write as _;


// ============================== 协议常量 (对齐 zmodem.h) ==============================

/// 帧填充字符 `*`
pub const ZPAD: u8 = 0x2A;
/// ZMODEM 转义字符 (Ctrl-X)
pub const ZDLE: u8 = 0x18;
/// 转义后的 ZDLE
pub const ZDLEE: u8 = ZDLE ^ 0x40;
/// 二进制帧指示 (16 位 FCS)
pub const ZBIN: u8 = b'A';
/// 十六进制帧指示
pub const ZHEX: u8 = b'B';
/// 二进制帧指示 (32 位 FCS)
pub const ZBIN32: u8 = b'C';

/// 请求接收端初始化
pub const ZRQINIT: u8 = 0;
/// 接收端初始化
pub const ZRINIT: u8 = 1;
/// 发送端初始化序列 (可选)
pub const ZSINIT: u8 = 2;
/// 确认
pub const ZACK: u8 = 3;
/// 文件名帧
pub const ZFILE: u8 = 4;
/// 跳过一个文件
pub const ZSKIP: u8 = 5;
/// 上一帧损坏
pub const ZNAK: u8 = 6;
/// 中止批量传输
pub const ZABORT: u8 = 7;
/// 结束会话
pub const ZFIN: u8 = 8;
/// 从该位置恢复数据
pub const ZRPOS: u8 = 9;
/// 数据子包开始
pub const ZDATA: u8 = 10;
/// 文件结束
pub const ZEOF: u8 = 11;
/// 致命读写错误
pub const ZFERR: u8 = 12;
/// 请求文件 CRC 及应答
pub const ZCRC: u8 = 13;
/// 接收端挑战
pub const ZCHALLENGE: u8 = 14;
/// 请求已完成
pub const ZCOMPL: u8 = 15;
/// 对方以 CAN×5 取消会话
pub const ZCAN: u8 = 16;
/// 请求文件系统空闲字节
pub const ZFREECNT: u8 = 17;
/// 远程命令
pub const ZCOMMAND: u8 = 18;
/// 错误输出 (后随数据)
pub const ZSTDERR: u8 = 19;

/// 数据帧结束序列: CRC 后帧结束, 下一帧是头
pub const ZCRCE: u8 = b'h';
/// 数据帧结束序列: CRC 后帧继续 (不停顿)
pub const ZCRCG: u8 = b'i';
/// 数据帧结束序列: CRC 后帧继续, 期待 ZACK
pub const ZCRCQ: u8 = b'j';
/// 数据帧结束序列: CRC 后期待 ZACK, 帧结束
pub const ZCRCW: u8 = b'k';
/// 转义为 RUBOUT (0x7F)
pub const ZRUB0: u8 = b'l';
/// 转义为 RUBOUT (0xFF)
pub const ZRUB1: u8 = b'm';

/// 头内字节位置: 标志字节
pub const ZF0: usize = 3;
pub const ZF1: usize = 2;
pub const ZF2: usize = 1;
pub const ZF3: usize = 0;
/// 头内字节位置: 位置字段 (小端)
pub const ZP0: usize = 0;
pub const ZP1: usize = 1;
pub const ZP2: usize = 2;
pub const ZP3: usize = 3;

/// ZRINIT 标志: 支持全双工
pub const CANFDX: u8 = 0x01;
/// ZRINIT 标志: 磁盘 I/O 期间可接收
pub const CANOVIO: u8 = 0x02;
/// ZRINIT 标志: 可使用 32 位 FCS
pub const CANFC32: u8 = 0x20;

/// ZFILE 转换: 二进制 (禁止换行转换)
pub const ZCBIN: u8 = 1;
/// ZFILE 管理: 覆盖已有文件
pub const ZF1_ZMCLOB: u8 = 4;

/// XON (发送)
pub const XON: u8 = 0x11;
/// XOFF (发送)
pub const XOFF: u8 = 0x13;
/// 取消字符 (Ctrl-X, 与 ZDLE 同值; 5 个连续 CAN 表示取消)
pub const CAN: u8 = 0x18;

/// 32 位 FCS 残差校验值 (数据 + FCS 累加后应等于该值)
pub const CRC32_RESIDUE: u32 = 0xDEBB_20E3;

/// 帧前允许的垃圾字节上限 (lrzsz: Zrwindow + Baudrate)
const MAX_GARBAGE: u32 = 4096;
/// CAN 序列长度 (5 个连续 CAN 视为取消)
const CAN_COUNT: u32 = 5;
/// 头部 FCS 最大字节数 (16 位 2 / 32 位 4)
const MAX_FCS_BYTES: usize = 4;

// ============================== FCS (CRC16 / CRC32) ==============================

/// 生成 CRC-16/CCITT 表 (poly 0x1021, 与 lrzsz crctab.c 一致)
const fn crc16_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// 生成 CRC-32 表 (反射 poly 0xEDB88320, 与 lrzsz cr3tab 一致)
const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC16_TABLE: [u16; 256] = crc16_table();
static CRC32_TABLE: [u32; 256] = crc32_table();

/// 一步 CRC-16 (对齐 lrzsz `updcrc`)
#[inline]
fn updcrc(byte: u8, crc: u16) -> u16 {
    CRC16_TABLE[((crc >> 8) & 0xFF) as usize] ^ (crc << 8) ^ byte as u16
}

/// 一步 CRC-32 (对齐 lrzsz `UPDC32`; `crc >> 8` 已丢弃高位, 掩码是冗余的)
#[inline]
fn updc32(byte: u8, crc: u32) -> u32 {
    CRC32_TABLE[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8)
}

/// 16 位 FCS 发送值: 数据 + 两个零字节的余数 (高位在前)
#[inline]
fn crc16_tx(crc: u16) -> (u8, u8) {
    let crc = updcrc(0, updcrc(0, crc));
    ((crc >> 8) as u8, crc as u8)
}

// ============================== 错误类型 ==============================

/// ZMODEM 会话错误
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZmError {
    /// 对方以 CAN×5 / ZCAN / ZABORT 取消会话
    Cancelled,
    /// 等待超时 (对方无响应)
    Timeout,
    /// FCS 校验失败 (多次重试后仍失败)
    Crc,
    /// 帧前垃圾字节过多
    GarbageExceeded,
    /// 协议错误 (携带原因)
    Protocol(&'static str),
    /// 文件读取/写入失败 (携带原因)
    File(&'static str),
    /// 用户请求中止 (按 ESC)
    UserAbort,
}

impl core::fmt::Display for ZmError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ZmError::Cancelled => formatter.write_str("对方取消会话 (CAN)"),
            ZmError::Timeout => formatter.write_str("等待超时"),
            ZmError::Crc => formatter.write_str("FCS 校验失败"),
            ZmError::GarbageExceeded => formatter.write_str("帧前垃圾字节过多"),
            ZmError::Protocol(what) => write!(formatter, "协议错误: {}", what),
            ZmError::File(what) => write!(formatter, "文件错误: {}", what),
            ZmError::UserAbort => formatter.write_str("用户中止"),
        }
    }
}

// ============================== 端口抽象 ==============================

/// 传输端口: 承载 ZMODEM 字节流的半双工通道 (板上为控制台 UART)
pub trait ZmPort {
    /// 阻塞读取一个字节, `timeout_ms` 内无数据返回 `None`。
    fn read_timeout(&mut self, timeout_ms: u32) -> Option<u8>;

    /// 写入字节 (全部发出)。
    fn write(&mut self, bytes: &[u8]);

    /// 冲刷发送缓冲 (等待硬件发送完成); 默认空操作。
    fn flush(&mut self) {}

    /// 用户是否请求中止 (按 ESC)。
    ///
    /// 仅由协议在**帧间空闲点**调用 (等待帧头前), 不会在数据流中调用,
    /// 因此实现可以安全地把第一个待读字节当作中止信号消费掉。
    fn abort_requested(&mut self) -> bool {
        false
    }
}

/// 会话参数
#[derive(Clone, Copy, Debug)]
pub struct ZmConfig {
    /// 帧/字节间等待超时 (毫秒)。lrzsz 默认 10 秒。
    pub timeout_ms: u32,
    /// 发送端子包长度 (字节, 32~1024)。
    pub subpacket: usize,
}

impl Default for ZmConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 10_000,
            subpacket: 1024,
        }
    }
}

// ============================== 转义编解码 ==============================

/// 发送字符分类 (lrzsz `zsendline_tab`, `Zctlesc = 0`)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SendClass {
    /// 原样发送
    Raw,
    /// 编码为 `ZDLE + byte^0x40`
    Esc,
    /// 仅在上一发送字节为 `@` 时编码 (Telenet 换行转义)
    EscAfterAt,
}

fn send_class(byte: u8) -> SendClass {
    if byte & 0x60 != 0 {
        // 0x20..0x7F 与 0xE0..0xFF: 原样
        SendClass::Raw
    } else {
        match byte {
            ZDLE | XON | XOFF | 0x91 | 0x93 | 0x10 | 0x90 => SendClass::Esc,
            0x0D | 0x8D => SendClass::EscAfterAt,
            _ => SendClass::Raw,
        }
    }
}

/// 带转义状态的发送器 (跟踪上一发送字节以支持 `@`+CR 规则)
struct EscWriter<'a, P: ZmPort> {
    port: &'a mut P,
    last: u8,
}

impl<P: ZmPort> EscWriter<'_, P> {
    /// 编码发送一个字节 (对齐 lrzsz `zsendline`)
    fn send(&mut self, byte: u8) {
        match send_class(byte) {
            SendClass::Raw => {
                self.port.write(&[byte]);
                self.last = byte;
            }
            SendClass::Esc => {
                self.port.write(&[ZDLE, byte ^ 0x40]);
                self.last = byte ^ 0x40;
            }
            SendClass::EscAfterAt => {
                if self.last & 0x7F == b'@' {
                    self.port.write(&[ZDLE, byte ^ 0x40]);
                    self.last = byte ^ 0x40;
                } else {
                    self.port.write(&[byte]);
                    self.last = byte;
                }
            }
        }
    }

    /// 原样发送 (不转义, 不更新转义状态以外的语义; 对齐 lrzsz `xsendline`)
    fn send_raw(&mut self, byte: u8) {
        self.port.write(&[byte]);
        self.last = byte;
    }
}

/// 接收解码结果
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Zdl {
    /// 普通数据字节
    Byte(u8),
    /// `ZDLE ZCRCE/G/Q/W`: 数据帧结束序列
    FrameEnd(u8),
    /// `ZDLE CAN CAN CAN CAN`: 对方取消
    Cancelled,
}

/// 读取并解码一个字节 (对齐 lrzsz `zdlread`/`zdlread2`)。
///
/// 裸控制字节 (XON/XOFF 除外) 直接返回; `ZDLE` 后跟转义字符解回原字节;
/// `ZDLE ZCRCE/G/Q/W` 返回帧结束序列; `ZDLE` 后 4 个 `CAN` 返回取消。
fn zdlread<P: ZmPort>(port: &mut P, timeout_ms: u32) -> Result<Zdl, ZmError> {
    let first = port
        .read_timeout(timeout_ms)
        .ok_or(ZmError::Timeout)?;
    if first & 0x60 != 0 {
        return Ok(Zdl::Byte(first));
    }
    // 控制字符: 跳过 XON/XOFF, 其余原样返回
    let mut c = first;
    if c != ZDLE {
        loop {
            match c {
                ZDLE => break,
                XON | 0x91 | XOFF | 0x93 => {
                    c = port
                        .read_timeout(timeout_ms)
                        .ok_or(ZmError::Timeout)?;
                }
                _ => return Ok(Zdl::Byte(c)),
            }
        }
    }
    // ZDLE 转义序列
    loop {
        let mut c = port
            .read_timeout(timeout_ms)
            .ok_or(ZmError::Timeout)?;
        if c == CAN {
            // 最多再连续 3 个 CAN (共 4 个) → 取消
            let mut count = 0;
            while count < 3 {
                match port.read_timeout(timeout_ms) {
                    Some(CAN) => {
                        c = CAN;
                        count += 1;
                    }
                    Some(other) => {
                        c = other;
                        break;
                    }
                    None => return Err(ZmError::Timeout),
                }
            }
            if count == 3 {
                return Ok(Zdl::Cancelled);
            }
        }
        match c {
            ZCRCE | ZCRCG | ZCRCQ | ZCRCW => return Ok(Zdl::FrameEnd(c)),
            ZRUB0 => return Ok(Zdl::Byte(0x7F)),
            ZRUB1 => return Ok(Zdl::Byte(0xFF)),
            XON | 0x91 | XOFF | 0x93 => continue,
            _ if c & 0x40 != 0 => return Ok(Zdl::Byte(c ^ 0x40)),
            _ => return Err(ZmError::Protocol("非法转义序列")),
        }
    }
}

// ============================== 帧发送 ==============================

/// 把位置/计数写入头字节数组 (小端, 对齐 `stohdr`)
fn stohdr(pos: u32) -> [u8; 4] {
    [
        pos as u8,
        (pos >> 8) as u8,
        (pos >> 16) as u8,
        (pos >> 24) as u8,
    ]
}

/// 从头字节数组取回位置/计数 (对齐 `rclhdr`)
fn rclhdr(bytes: &[u8; 4]) -> u32 {
    bytes[0] as u32 | (bytes[1] as u32) << 8 | (bytes[2] as u32) << 16 | (bytes[3] as u32) << 24
}

/// 一个字节 → 两个小写十六进制字符 (对齐 `zputhex`)
fn puthex(byte: u8, out: &mut [u8; 40], pos: &mut usize) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    out[*pos] = DIGITS[(byte >> 4) as usize];
    out[*pos + 1] = DIGITS[(byte & 0x0F) as usize];
    *pos += 2;
}

/// 发送十六进制头 (对齐 `zshhdr`): `** ZPAD ZPAD ZDLE ZHEX` + 十六进制负载 + CR LF [+ XON]
pub fn send_hex_hdr<P: ZmPort>(port: &mut P, typ: u8, bytes: &[u8; 4]) -> Result<(), ZmError> {
    let mut crc = updcrc(typ, 0);
    for &b in bytes {
        crc = updcrc(b, crc);
    }
    let (hi, lo) = crc16_tx(crc);
    let mut out = [0u8; 40];
    let mut pos = 0;
    out[pos] = ZPAD;
    out[pos + 1] = ZPAD;
    out[pos + 2] = ZDLE;
    pos += 3;
    out[pos] = ZHEX;
    pos += 1;
    puthex(typ & 0x7F, &mut out, &mut pos);
    for &b in bytes {
        puthex(b, &mut out, &mut pos);
    }
    puthex(hi, &mut out, &mut pos);
    puthex(lo, &mut out, &mut pos);
    out[pos] = b'\r';
    out[pos + 1] = b'\n';
    pos += 2;
    // ZFIN/ZACK 后不加 XON
    if typ != ZFIN && typ != ZACK {
        out[pos] = XON;
        pos += 1;
    }
    port.write(&out[..pos]);
    port.flush();
    Ok(())
}

/// 发送二进制头 (对齐 `zsbhdr` / `zsbh32`), `fcs32` 选择 32 位 FCS
pub fn send_bin_hdr<P: ZmPort>(
    port: &mut P,
    typ: u8,
    bytes: &[u8; 4],
    fcs32: bool,
) -> Result<(), ZmError> {
    let mut w = EscWriter { port, last: 0 };
    w.send_raw(ZPAD);
    w.send_raw(ZDLE);
    if fcs32 {
        w.send_raw(ZBIN32);
        let mut crc: u32 = 0xFFFF_FFFF;
        w.send(typ);
        crc = updc32(typ, crc);
        for &b in bytes {
            w.send(b);
            crc = updc32(b, crc);
        }
        crc = !crc;
        for _ in 0..4 {
            w.send((crc & 0xFF) as u8);
            crc >>= 8;
        }
    } else {
        w.send_raw(ZBIN);
        let mut crc: u16 = updcrc(typ, 0);
        w.send(typ);
        for &b in bytes {
            w.send(b);
            crc = updcrc(b, crc);
        }
        let (hi, lo) = crc16_tx(crc);
        w.send(hi);
        w.send(lo);
    }
    w.port.flush();
    Ok(())
}

/// 发送数据子包 (对齐 `zsdata` / `zsda32`)
///
/// `frameend` 为 `ZCRCE/ZCRCG/ZCRCQ/ZCRCW` 之一; 数据与 FCS 按需转义,
/// 结束序列 `ZDLE frameend` 原样发送。`ZCRCW` 结尾追加 XON 并冲刷。
pub fn send_data<P: ZmPort>(
    port: &mut P,
    data: &[u8],
    frameend: u8,
    fcs32: bool,
) -> Result<(), ZmError> {
    let mut w = EscWriter { port, last: 0 };
    if fcs32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            w.send(b);
            crc = updc32(b, crc);
        }
        w.send_raw(ZDLE);
        w.send_raw(frameend);
        crc = updc32(frameend, crc);
        crc = !crc;
        for _ in 0..4 {
            let byte = (crc & 0xFF) as u8;
            if byte & 0x40 != 0 {
                w.send_raw(byte);
            } else {
                w.send(byte);
            }
            crc >>= 8;
        }
    } else {
        let mut crc: u16 = 0;
        for &b in data {
            w.send(b);
            crc = updcrc(b, crc);
        }
        w.send_raw(ZDLE);
        w.send_raw(frameend);
        crc = updcrc(frameend, crc);
        let (hi, lo) = crc16_tx(crc);
        w.send(hi);
        w.send(lo);
    }
    if frameend == ZCRCW {
        w.send_raw(XON);
    }
    w.port.flush();
    Ok(())
}

// ============================== 帧接收 ==============================

/// 已解码的头
#[derive(Clone, Copy, Debug)]
pub struct Header {
    /// 帧类型
    pub typ: u8,
    /// 4 字节头内容 (位置字段小端, ZF0 在字节 3)
    pub bytes: [u8; 4],
    /// 帧指示: ZBIN / ZBIN32 / ZHEX (决定数据子包的 FCS 宽度)
    pub frameind: u8,
}

/// 读取一个字节并跳过 XON/XOFF (对齐 `noxrd7` 的过滤语义)
fn read_filtered<P: ZmPort>(port: &mut P, timeout_ms: u32) -> Result<u8, ZmError> {
    loop {
        let byte = port
            .read_timeout(timeout_ms)
            .ok_or(ZmError::Timeout)?;
        match byte & 0x7F {
            XON | XOFF => continue,
            _ => return Ok(byte),
        }
    }
}

/// 处理一串 CAN: 达到 5 个连续 CAN 返回取消; CAN 后跟非 CAN 视为垃圾。
///
/// 返回 `Ok(true)` 表示会话被取消; `Err(GarbageExceeded)` 用作"CAN 后跟
/// 非 CAN, 应整串丢弃并重启搜索"的标记, 由调用者按垃圾处理。
#[allow(clippy::type_complexity)]
fn can_sequence<P: ZmPort>(port: &mut P, cancount: &mut u32) -> Result<bool, ZmError> {
    loop {
        *cancount -= 1;
        if *cancount == 0 {
            return Ok(true);
        }
        match port.read_timeout(100) {
            Some(CAN) => continue,
            Some(ZCRCW) => return Err(ZmError::Protocol("CAN 后跟 ZCRCW")),
            _ => return Err(ZmError::GarbageExceeded), // 非 CAN 或超时: 视为垃圾
        }
    }
}

/// 读取一个帧头 (对齐 `zgethdr`): 跳过垃圾找 ZPAD, 支持 HEX/二进制 16/32 位 FCS。
///
/// 返回帧类型与内容; 会话取消返回 `ZmError::Cancelled`。
pub fn zgethdr<P: ZmPort>(port: &mut P, timeout_ms: u32) -> Result<Header, ZmError> {
    let mut garbage_left = MAX_GARBAGE;
    'search: loop {
        let mut cancount = CAN_COUNT;

        // ---- 等待 ZPAD (丢弃非协议字节) ----
        loop {
            let byte = match port.read_timeout(timeout_ms) {
                Some(byte) => byte,
                None => return Err(ZmError::Timeout),
            };
            match byte {
                CAN => match can_sequence(port, &mut cancount) {
                    Ok(true) => return Err(ZmError::Cancelled),
                    Ok(false) => continue,
                    // CAN 后跟非 CAN: 整串视为垃圾, 重启搜索 (对齐 lrzsz agn2)
                    Err(ZmError::GarbageExceeded) => {
                        garbage_left -= 1;
                        if garbage_left == 0 {
                            return Err(ZmError::GarbageExceeded);
                        }
                        continue 'search;
                    }
                    Err(e) => return Err(e),
                },
                ZPAD | 0xAA => break,
                _ => {
                    garbage_left -= 1;
                    if garbage_left == 0 {
                        return Err(ZmError::GarbageExceeded);
                    }
                    continue 'search;
                }
            }
        }

        // ---- 跳过更多 ZPAD, 等待 ZDLE ----
        // 注意: 本阶段 0x18 只能是 ZDLE (ZDLE 与 CAN 同值, 对齐 lrzsz 的 splat 段)
        loop {
            let byte = match read_filtered(port, timeout_ms) {
                Ok(byte) => byte,
                Err(ZmError::Timeout) => return Err(ZmError::Timeout),
                Err(e) => return Err(e),
            };
            match byte {
                ZPAD | 0xAA => continue,
                ZDLE => break,
                _ => {
                    garbage_left -= 1;
                    if garbage_left == 0 {
                        return Err(ZmError::GarbageExceeded);
                    }
                    continue 'search;
                }
            }
        }

        // ---- 帧指示 ----
        let ind = read_filtered(port, timeout_ms)?;
        match ind {
            ZBIN => return decode_bin_hdr(port, timeout_ms),
            ZBIN32 => return decode_bin32_hdr(port, timeout_ms),
            ZHEX => return decode_hex_hdr(port, timeout_ms),
            CAN => match can_sequence(port, &mut cancount) {
                Ok(true) => return Err(ZmError::Cancelled),
                Ok(false) => continue 'search,
                // CAN 后跟非 CAN: 整串视为垃圾, 重启搜索
                Err(ZmError::GarbageExceeded) => {
                    garbage_left -= 1;
                    if garbage_left == 0 {
                        return Err(ZmError::GarbageExceeded);
                    }
                    continue 'search;
                }
                Err(e) => return Err(e),
            },
            _ => {
                garbage_left -= 1;
                if garbage_left == 0 {
                    return Err(ZmError::GarbageExceeded);
                }
                continue 'search;
            }
        }
    }
}

/// 二进制 16 位 FCS 头解码 (对齐 `zrbhdr`)
fn decode_bin_hdr<P: ZmPort>(port: &mut P, timeout_ms: u32) -> Result<Header, ZmError> {
    let mut crc: u16 = 0;
    let typ = match zdlread(port, timeout_ms)? {
        Zdl::Byte(b) => b,
        _ => return Err(ZmError::Protocol("头内出现帧结束序列")),
    };
    crc = updcrc(typ, crc);
    let mut bytes = [0u8; 4];
    for slot in &mut bytes {
        let b = match zdlread(port, timeout_ms)? {
            Zdl::Byte(b) => b,
            Zdl::Cancelled => return Err(ZmError::Cancelled),
            _ => return Err(ZmError::Protocol("头内出现帧结束序列")),
        };
        *slot = b;
        crc = updcrc(b, crc);
    }
    for _ in 0..2 {
        let b = match zdlread(port, timeout_ms)? {
            Zdl::Byte(b) => b,
            Zdl::Cancelled => return Err(ZmError::Cancelled),
            _ => return Err(ZmError::Protocol("头内出现帧结束序列")),
        };
        crc = updcrc(b, crc);
    }
    if crc != 0 {
        return Err(ZmError::Crc);
    }
    Ok(Header {
        typ,
        bytes,
        frameind: ZBIN,
    })
}

/// 二进制 32 位 FCS 头解码 (对齐 `zrbhdr32`)
fn decode_bin32_hdr<P: ZmPort>(port: &mut P, timeout_ms: u32) -> Result<Header, ZmError> {
    let mut crc: u32 = 0xFFFF_FFFF;
    let typ = match zdlread(port, timeout_ms)? {
        Zdl::Byte(b) => b,
        _ => return Err(ZmError::Protocol("头内出现帧结束序列")),
    };
    crc = updc32(typ, crc);
    let mut bytes = [0u8; 4];
    for slot in &mut bytes {
        let b = match zdlread(port, timeout_ms)? {
            Zdl::Byte(b) => b,
            Zdl::Cancelled => return Err(ZmError::Cancelled),
            _ => return Err(ZmError::Protocol("头内出现帧结束序列")),
        };
        *slot = b;
        crc = updc32(b, crc);
    }
    for _ in 0..4 {
        let b = match zdlread(port, timeout_ms)? {
            Zdl::Byte(b) => b,
            Zdl::Cancelled => return Err(ZmError::Cancelled),
            _ => return Err(ZmError::Protocol("头内出现帧结束序列")),
        };
        crc = updc32(b, crc);
    }
    if crc != CRC32_RESIDUE {
        return Err(ZmError::Crc);
    }
    Ok(Header {
        typ,
        bytes,
        frameind: ZBIN32,
    })
}

/// 十六进制头解码 (对齐 `zrhhdr`)
fn decode_hex_hdr<P: ZmPort>(port: &mut P, timeout_ms: u32) -> Result<Header, ZmError> {
    let typ = gethex(port, timeout_ms)?;
    let mut crc = updcrc(typ, 0);
    let mut bytes = [0u8; 4];
    for slot in &mut bytes {
        let b = gethex(port, timeout_ms)?;
        *slot = b;
        crc = updcrc(b, crc);
    }
    let hi = gethex(port, timeout_ms)?;
    crc = updcrc(hi, crc);
    let lo = gethex(port, timeout_ms)?;
    crc = updcrc(lo, crc);
    if crc != 0 {
        return Err(ZmError::Crc);
    }
    // 丢弃可能的 CR LF (对齐 lrzsz: 读一个字节, 若是 CR 再读一个)
    match port.read_timeout(100) {
        Some(b'\r') | Some(0x8D) => {
            let _ = port.read_timeout(100);
        }
        _ => {}
    }
    Ok(Header {
        typ,
        bytes,
        frameind: ZHEX,
    })
}

/// 读取一个小写十六进制字节 (对齐 `zgethex`)
fn gethex<P: ZmPort>(port: &mut P, timeout_ms: u32) -> Result<u8, ZmError> {
    let hi = hexval(read_filtered(port, timeout_ms)?)?;
    let lo = hexval(read_filtered(port, timeout_ms)?)?;
    Ok((hi << 4) | lo)
}

fn hexval(byte: u8) -> Result<u8, ZmError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(ZmError::Protocol("非法十六进制字符")),
    }
}

/// 接收数据子包 (对齐 `zrdata` / `zrdat32`): 数据填入 `buf`, 直到帧结束序列。
///
/// 返回 `(数据字节数, ZCRCE/ZCRCG/ZCRCQ/ZCRCW)`。
pub fn zrdata<P: ZmPort>(
    port: &mut P,
    timeout_ms: u32,
    fcs32: bool,
    buf: &mut [u8],
) -> Result<(usize, u8), ZmError> {
    if fcs32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        let mut count = 0usize;
        loop {
            match zdlread(port, timeout_ms)? {
                Zdl::Byte(b) => {
                    if count >= buf.len() {
                        return Err(ZmError::Protocol("数据子包过长"));
                    }
                    buf[count] = b;
                    count += 1;
                    crc = updc32(b, crc);
                }
                Zdl::FrameEnd(e) => {
                    crc = updc32(e, crc);
                    for _ in 0..MAX_FCS_BYTES {
                        match zdlread(port, timeout_ms)? {
                            Zdl::Byte(b) => crc = updc32(b, crc),
                            Zdl::Cancelled => return Err(ZmError::Cancelled),
                            _ => return Err(ZmError::Protocol("FCS 处出现帧结束序列")),
                        }
                    }
                    if crc != CRC32_RESIDUE {
                        return Err(ZmError::Crc);
                    }
                    return Ok((count, e));
                }
                Zdl::Cancelled => return Err(ZmError::Cancelled),
            }
        }
    } else {
        let mut crc: u16 = 0;
        let mut count = 0usize;
        loop {
            match zdlread(port, timeout_ms)? {
                Zdl::Byte(b) => {
                    if count >= buf.len() {
                        return Err(ZmError::Protocol("数据子包过长"));
                    }
                    buf[count] = b;
                    count += 1;
                    crc = updcrc(b, crc);
                }
                Zdl::FrameEnd(e) => {
                    crc = updcrc(e, crc);
                    for _ in 0..2 {
                        match zdlread(port, timeout_ms)? {
                            Zdl::Byte(b) => crc = updcrc(b, crc),
                            Zdl::Cancelled => return Err(ZmError::Cancelled),
                            _ => return Err(ZmError::Protocol("FCS 处出现帧结束序列")),
                        }
                    }
                    if crc != 0 {
                        return Err(ZmError::Crc);
                    }
                    return Ok((count, e));
                }
                Zdl::Cancelled => return Err(ZmError::Cancelled),
            }
        }
    }
}

// ============================== 会话驱动 ==============================

/// 取消对方: 发送 5 个 CAN (对齐 `canit`)
pub fn cancel_session<P: ZmPort>(port: &mut P) {
    port.write(&[CAN, CAN, CAN, CAN, CAN]);
    port.flush();
}

/// 发送端待发送文件描述
#[derive(Clone, Copy, Debug)]
pub struct SendFile<'a> {
    /// 远程文件名 (仅基础名; 发送端不在此做路径处理)
    pub name: &'a str,
    /// 文件字节数
    pub size: u32,
}

/// 用于构建文件信息块的固定缓冲写入器
struct BufWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl core::fmt::Write for BufWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        if self.len + s.len() > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}

/// 构建 ZFILE 数据块: `name\0` + `"size 0 0 0 1 size"` + `\0`
/// (对齐 lrzsz `wctxpn` 的 `"%lu %lo %o 0 %d %ld"` 格式; mtime/mode 填 0)
fn build_file_info(name: &str, size: u32, out: &mut [u8]) -> Result<usize, ZmError> {
    let mut w = BufWriter { buf: out, len: 0 };
    w.write_str(name)
        .map_err(|_| ZmError::Protocol("文件名过长"))?;
    w.buf[w.len] = 0;
    w.len += 1;
    write!(w, "{} 0 0 0 1 {}", size, size)
        .map_err(|_| ZmError::Protocol("文件信息块过长"))?;
    if w.len >= w.buf.len() {
        return Err(ZmError::Protocol("文件信息块过长"));
    }
    w.buf[w.len] = 0;
    w.len += 1;
    Ok(w.len)
}

/// 从文件信息块解析 `(名称, 大小)`。
///
/// 名称是第一个 NUL 前的字节; 大小从其后第一个十进制数解析, 无元数据时
/// 返回 `u32::MAX` 表示未知。
fn parse_file_info(data: &[u8]) -> (&[u8], u32) {
    let name_end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let name = &data[..name_end];
    let rest = data.get(name_end + 1..).unwrap_or(&[]);
    let size = parse_decimal(rest).unwrap_or(u32::MAX);
    (name, size)
}

/// 解析前导十进制数 (允许前导/尾随空白)
fn parse_decimal(bytes: &[u8]) -> Option<u32> {
    let mut value: u64 = 0;
    let mut started = false;
    for &b in bytes {
        match b {
            b'0'..=b'9' => {
                started = true;
                value = value * 10 + (b - b'0') as u64;
                if value > u32::MAX as u64 {
                    return None;
                }
            }
            b' ' | b'\t' | 0 => {
                if started {
                    return Some(value as u32);
                }
            }
            _ => return None,
        }
    }
    if started {
        Some(value as u32)
    } else {
        None
    }
}

/// 读取一个信息数据块 (ZFILE/ZSINIT/ZCOMMAND 负载), 以 ZCRCW 结束。
fn read_info_block<P: ZmPort>(
    port: &mut P,
    config: &ZmConfig,
    fcs32: bool,
    out: &mut [u8],
) -> Result<(usize, u8), ZmError> {
    let mut len = 0usize;
    loop {
        let (count, end) = zrdata(port, config.timeout_ms, fcs32, &mut out[len..])?;
        len += count;
        match end {
            ZCRCW => return Ok((len, end)),
            _ if len < out.len() => continue,
            _ => return Err(ZmError::Protocol("信息块过长或未以 ZCRCW 结束")),
        }
    }
}

// ------------------------------ 接收端 ------------------------------

/// 接收初始化结果。`name` 位于调用方提供的 `info` 缓冲的 `[name_offset, name_offset+name_len)`。
enum Init {
    /// 会话结束 (ZFIN 握手完成或远程命令被拒绝)
    SessionDone,
    /// 收到文件请求
    File {
        name_offset: usize,
        name_len: usize,
        size: u32,
    },
}

/// 发送 ZRINIT 并等待发送端 (对齐 lrzsz `tryz`)。
///
/// 每个超时周期重发 ZRINIT (上限 [`INIT_ATTEMPTS`]); 收到 ZRQINIT 立即重发。
/// 收到 ZFILE 时调用 `decide` 决定接收或跳过 (跳过发送 ZSKIP)。
const INIT_ATTEMPTS: u32 = 10;

fn try_init<P: ZmPort, D: FnMut(&[u8], u32) -> bool>(
    port: &mut P,
    config: &ZmConfig,
    info: &mut [u8],
    decide: &mut D,
) -> Result<Init, ZmError> {
    for _ in 0..INIT_ATTEMPTS {
        if port.abort_requested() {
            return Err(ZmError::UserAbort);
        }
        // ZRINIT: 位置字段 = 0, ZF0 = 能力标志 (32 位 FCS + 全双工 + 磁盘 I/O 可接收)
        send_hex_hdr(port, ZRINIT, &[0, 0, 0, CANFC32 | CANFDX | CANOVIO])?;
        let header = zgethdr(port, config.timeout_ms)?;
        match header.typ {
            ZRQINIT | ZEOF | ZCOMPL => continue,
            ZRINIT => return Err(ZmError::Protocol("收到 ZRINIT (双方都是接收端)")),
            ZSINIT => {
                let mut attn = [0u8; 32];
                let (_, end) = read_info_block(port, config, header.frameind == ZBIN32, &mut attn)?;
                if end == ZCRCW {
                    send_hex_hdr(port, ZACK, &[1, 0, 0, 0])?;
                } else {
                    send_hex_hdr(port, ZNAK, &[0; 4])?;
                }
            }
            ZFREECNT => {
                // 报告"大量空闲" (lrzsz 同样返回 ~0)
                send_hex_hdr(port, ZACK, &[0xFF, 0xFF, 0xFF, 0x7F])?;
            }
            ZCHALLENGE => {
                send_hex_hdr(port, ZACK, &header.bytes)?;
            }
            ZCOMMAND => {
                let mut buf = [0u8; 128];
                let _ = read_info_block(port, config, header.frameind == ZBIN32, &mut buf);
                // 拒绝远程命令执行并结束会话
                send_hex_hdr(port, ZCOMPL, &[0; 4])?;
                return Ok(Init::SessionDone);
            }
            ZFILE => {
                let (len, end) =
                    read_info_block(port, config, header.frameind == ZBIN32, info)?;
                if end != ZCRCW {
                    send_hex_hdr(port, ZNAK, &[0; 4])?;
                    continue;
                }
                let (name, size) = parse_file_info(&info[..len]);
                if decide(name, size) {
                    // 名称始终从 info 缓冲起始位置开始
                    return Ok(Init::File {
                        name_offset: 0,
                        name_len: name.len(),
                        size,
                    });
                }
                send_hex_hdr(port, ZSKIP, &[0; 4])?;
            }
            ZFIN => {
                ackbibi(port, config)?;
                return Ok(Init::SessionDone);
            }
            ZCAN | ZABORT => return Err(ZmError::Cancelled),
            ZNAK => {}
            // 其余 (ZDATA/ZRPOS 等) 与 TIMEOUT: 重发 ZRINIT
            _ => {}
        }
    }
    Err(ZmError::Timeout)
}

/// ZFIN 握手 (接收端, 对齐 `ackbibi`): 发 ZFIN, 等 `OO`
fn ackbibi<P: ZmPort>(port: &mut P, config: &ZmConfig) -> Result<(), ZmError> {
    for _ in 0..3 {
        send_hex_hdr(port, ZFIN, &[0; 4])?;
        match port.read_timeout(config.timeout_ms) {
            Some(b'O') => {
                let _ = port.read_timeout(config.timeout_ms);
                return Ok(());
            }
            None => return Ok(()), // 对方已离开
            Some(_) => continue,
        }
    }
    Ok(())
}

/// 接收一个文件的全部数据 (对齐 lrzsz `rzfile` 的数据部分)。
///
/// 流程: ZRPOS(已收) → 等 ZDATA → 累积子包 (ZCRCW/ZCRCQ 回 ZACK) →
/// 直到 ZEOF(位置一致) → 调用 `commit` 提交完整文件。
fn receive_file<P: ZmPort, C: FnMut(&[u8], u32, &[u8]) -> Result<(), ZmError>, G: FnMut(u32, u32)>(
    port: &mut P,
    config: &ZmConfig,
    buffer: &mut [u8],
    name: &[u8],
    declared_size: u32,
    commit: &mut C,
    progress: &mut G,
) -> Result<(), ZmError> {
    let mut received: u32 = 0;
    const MAX_ERRORS: u32 = 20;

    'top: loop {
        if port.abort_requested() {
            return Err(ZmError::UserAbort);
        }
        send_hex_hdr(port, ZRPOS, &stohdr(received))?;

        let mut errors: u32 = 0;
        // 等待下一帧 (ZDATA 数据结束后也回到这里, 不再发 ZRPOS)
        loop {
            let header = match zgethdr(port, config.timeout_ms) {
                Ok(header) => header,
                Err(ZmError::Timeout) => {
                    errors += 1;
                    if errors > MAX_ERRORS {
                        return Err(ZmError::Timeout);
                    }
                    continue 'top;
                }
                Err(e) => return Err(e),
            };
            match header.typ {
                ZDATA if rclhdr(&header.bytes) == received => {
                    let fcs32 = header.frameind == ZBIN32;
                    // 数据子包循环 (ZCRCG 连续; ZCRCQ/ZCRCW 回 ZACK)
                    loop {
                        let start = received as usize;
                        match zrdata(port, config.timeout_ms, fcs32, &mut buffer[start..]) {
                            Ok((count, end)) => {
                                received += count as u32;
                                progress(received, declared_size);
                                match end {
                                    ZCRCE => break,
                                    ZCRCG => continue,
                                    ZCRCQ => {
                                        send_hex_hdr(port, ZACK, &stohdr(received))?;
                                        continue;
                                    }
                                    ZCRCW => {
                                        send_hex_hdr(port, ZACK, &stohdr(received))?;
                                        break;
                                    }
                                    _ => unreachable!(),
                                }
                            }
                            Err(ZmError::Cancelled) => return Err(ZmError::Cancelled),
                            Err(e) => {
                                // FCS/超时/协议错误: 限制重试后放弃, 否则 ZRPOS 重新同步
                                errors += 1;
                                if errors > MAX_ERRORS {
                                    return Err(e);
                                }
                                continue 'top;
                            }
                        }
                    }
                }
                ZDATA => {
                    // 位置失配: 忽略 (发送方稍后会重传)
                    errors += 1;
                    if errors > MAX_ERRORS {
                        return Err(ZmError::Protocol("ZDATA 位置失配"));
                    }
                    continue 'top;
                }
                ZEOF if rclhdr(&header.bytes) == received => {
                    commit(name, declared_size, &buffer[..received as usize])?;
                    return Ok(());
                }
                ZEOF => continue,
                ZFILE => {
                    // 发送方重新协商: 读取其信息块后重新同步
                    let mut info = [0u8; 256];
                    let _ = read_info_block(port, config, header.frameind == ZBIN32, &mut info);
                    continue 'top;
                }
                ZSKIP => return Ok(()), // 发送方跳过本文件
                ZNAK => {
                    errors += 1;
                    if errors > MAX_ERRORS {
                        return Err(ZmError::Protocol("收到过多 ZNAK"));
                    }
                    continue 'top;
                }
                ZCAN | ZABORT => return Err(ZmError::Cancelled),
                ZFIN => return Err(ZmError::Protocol("文件传输中收到 ZFIN")),
                // 其余未知类型视为协议错误
                _ => {
                    errors += 1;
                    if errors > MAX_ERRORS {
                        return Err(ZmError::Protocol("未知帧类型"));
                    }
                }
            }
        }
    }
}

/// 接收会话: 等待并接收一批文件, 直到发送端 ZFIN 结束会话。
///
/// - `decide(名称, 声明大小)` 在文件数据到达前调用; 返回 `false` 跳过该文件;
///   名称可能不是合法 UTF-8, 大小未知时为 `u32::MAX`。
/// - `commit(名称, 声明大小, 完整数据)` 在文件接收完成后调用 (数据缓冲为
///   `file_buffer` 的前缀); 返回错误将中止整个会话。
/// - `progress(已收, 声明大小)` 每收到一个子包调用一次。
pub fn receive_session<
    P: ZmPort,
    D: FnMut(&[u8], u32) -> bool,
    C: FnMut(&[u8], u32, &[u8]) -> Result<(), ZmError>,
    G: FnMut(u32, u32),
>(
    port: &mut P,
    config: &ZmConfig,
    file_buffer: &mut [u8],
    decide: &mut D,
    commit: &mut C,
    progress: &mut G,
) -> Result<(), ZmError> {
    let mut info = [0u8; 256];
    loop {
        match try_init(port, config, &mut info, decide)? {
            Init::SessionDone => return Ok(()),
            Init::File {
                name_offset,
                name_len,
                size,
            } => {
                let name = &info[name_offset..name_offset + name_len];
                receive_file(port, config, file_buffer, name, size, commit, progress)?;
            }
        }
    }
}

// ------------------------------ 发送端 ------------------------------'

/// 发送端初始化: ZRQINIT → ZRINIT, 返回对方是否支持 32 位 FCS
fn getzrxinit<P: ZmPort>(port: &mut P, config: &ZmConfig) -> Result<bool, ZmError> {
    let mut resends = 0u32;
    for attempt in 0..INIT_ATTEMPTS {
        if port.abort_requested() {
            return Err(ZmError::UserAbort);
        }
        if attempt > 0 && resends < 4 {
            send_hex_hdr(port, ZRQINIT, &[0; 4])?;
            resends += 1;
        }
        match zgethdr(port, config.timeout_ms) {
            Ok(header) => match header.typ {
                ZCHALLENGE => {
                    send_hex_hdr(port, ZACK, &header.bytes)?;
                }
                ZRINIT => {
                    let flags = header.bytes[ZF0];
                    return Ok(flags & CANFC32 != 0);
                }
                ZRQINIT => {}
                _ => {
                    send_hex_hdr(port, ZNAK, &[0; 4])?;
                }
            },
            Err(ZmError::Timeout) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ZmError::Timeout)
}

/// 等待发送端数据阶段的确认 (ZCRCW 之后), 对齐 `getinsync` 的语义
///
/// 返回 `Ok(true)` 表示发送方要求从 [`sent`] 位置重新开始 (`ZRPOS`)。
#[allow(clippy::too_many_arguments)]
fn wait_ack<P: ZmPort>(
    port: &mut P,
    config: &ZmConfig,
    sent: &mut u32,
    errors: &mut u32,
) -> Result<bool, ZmError> {
    const MAX_ERRORS: u32 = 20;
    loop {
        let header = match zgethdr(port, config.timeout_ms) {
            Ok(header) => header,
            Err(ZmError::Timeout) => {
                *errors += 1;
                if *errors > MAX_ERRORS {
                    return Err(ZmError::Timeout);
                }
                continue;
            }
            Err(e) => return Err(e),
        };
        match header.typ {
            // 位置相符的 ZACK 才算确认; 过期 ZACK (位置不符) 忽略,
            // 继续等待真实响应 (对齐 lrzsz getinsync: `bytes_sent == rxpos` 判断)
            ZACK if *sent == rclhdr(&header.bytes) => return Ok(false),
            ZACK => {
                *errors += 1;
                if *errors > MAX_ERRORS {
                    return Err(ZmError::Protocol("ZACK 位置持续不符"));
                }
            }
            ZRPOS => {
                *sent = rclhdr(&header.bytes);
                return Ok(true);
            }
            ZRINIT => return Err(ZmError::Protocol("接收端要求重新初始化")),
            ZSKIP => return Err(ZmError::Protocol("接收端跳过了文件")),
            ZCAN | ZABORT | ZFIN => return Err(ZmError::Cancelled),
            _ => {
                *errors += 1;
                if *errors > MAX_ERRORS {
                    return Err(ZmError::Protocol("等待确认收到过多无效帧"));
                }
                send_hex_hdr(port, ZNAK, &[0; 4])?;
            }
        }
    }
}

/// 发送单个文件的数据 (对齐 `zsendfdata` + ZEOF 阶段)。
///
/// 从 `start` 位置开始, 每个子包以 ZCRCW 结束并等待 ZACK (乒乓流控,
/// 板上接收端 UART 环形缓冲小, 不使用 ZCRCG 流水); 最后一块 ZCRCE。
/// 每次 ZACK/ZRPOS 后重发 ZDATA 头 (接收端在 ZACK 后回到帧头等待,
/// 对齐 lrzsz `waitack` 后的 `zsbhdr(ZDATA, ...)`)。
/// ZEOF 后等待接收端 ZRINIT/ZSKIP (本文件完成) 或 ZRPOS (重传)。
#[allow(clippy::too_many_arguments)]
fn send_file_data<
    P: ZmPort,
    R: FnMut(usize, u32, &mut [u8]) -> Result<usize, ()>,
    G: FnMut(usize, u32, u32),
>(
    port: &mut P,
    config: &ZmConfig,
    index: usize,
    file: &SendFile<'_>,
    start: u32,
    fcs32: bool,
    read_at: &mut R,
    progress: &mut G,
    chunk: &mut [u8],
) -> Result<(), ZmError> {
    let subpacket = config.subpacket.clamp(32, chunk.len());
    let mut sent = start;
    const MAX_ERRORS: u32 = 20;

    'zdata: loop {
        if port.abort_requested() {
            return Err(ZmError::UserAbort);
        }
        send_bin_hdr(port, ZDATA, &stohdr(sent), fcs32)?;
        let mut errors: u32 = 0;

        // 发送一个子包: 最后一块以 ZCRCE 结束并进入 ZEOF 阶段
        let remaining = file.size.saturating_sub(sent);
        let want = subpacket.min(remaining as usize);
        let count = if want == 0 {
            0
        } else {
            read_at(index, sent, &mut chunk[..want])
                .map_err(|()| ZmError::File("读取文件失败"))?
        };
        // want == 0: 文件正好结束于子包边界 (发送空 ZCRCE 子包, 对齐 lrzsz)
        let last = want == 0 || count < want;
        send_data(port, &chunk[..count], if last { ZCRCE } else { ZCRCW }, fcs32)?;
        sent += count as u32;
        progress(index, sent, file.size);

        if !last {
            // ZCRCW → 等 ZACK / ZRPOS, 然后重发 ZDATA 头
            match wait_ack(port, config, &mut sent, &mut errors)? {
                false => continue 'zdata, // ZACK: 下一子包
                true => continue 'zdata,  // ZRPOS: 从新位置重发
            }
        }

        // ZEOF 阶段: 等接收端确认本文件完成
        let mut eof_errors: u32 = 0;
        loop {
            if port.abort_requested() {
                return Err(ZmError::UserAbort);
            }
            send_bin_hdr(port, ZEOF, &stohdr(sent), fcs32)?;
            let header = match zgethdr(port, config.timeout_ms) {
                Ok(header) => header,
                Err(ZmError::Timeout) => {
                    eof_errors += 1;
                    if eof_errors > MAX_ERRORS {
                        return Err(ZmError::Timeout);
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            match header.typ {
                ZACK => {
                    // 位置不符: 重发 ZEOF
                }
                ZRPOS => {
                    sent = rclhdr(&header.bytes);
                    continue 'zdata;
                }
                ZRINIT => return Ok(()),
                ZSKIP => return Ok(()),
                ZCAN | ZABORT | ZFIN => return Err(ZmError::Cancelled),
                _ => {
                    eof_errors += 1;
                    if eof_errors > MAX_ERRORS {
                        return Err(ZmError::Protocol("等待 ZEOF 确认收到过多无效帧"));
                    }
                }
            }
        }
    }
}

/// 发送一个文件: ZFILE + 信息块 → 等 ZRPOS/ZCRC/ZSKIP → 数据 → ZEOF
#[allow(clippy::too_many_arguments)]
fn send_one<
    P: ZmPort,
    R: FnMut(usize, u32, &mut [u8]) -> Result<usize, ()>,
    G: FnMut(usize, u32, u32),
>(
    port: &mut P,
    config: &ZmConfig,
    index: usize,
    file: &SendFile<'_>,
    fcs32: bool,
    read_at: &mut R,
    progress: &mut G,
    chunk: &mut [u8],
) -> Result<(), ZmError> {
    const MAX_ERRORS: u32 = 20;

    // ZFILE + 文件信息 (ZCRCW), 仅发送一次; 之后循环等待响应。
    // 注: 超时/无效帧时重发 ZFILE, 但**过期的 ZRINIT (初始握手竞争)
    // 不重发** —— ZFILE 已在飞行中, 接收端会以 ZRPOS/ZSKIP 回应,
    // 重发会造成过期帧连锁 (对齐 lrzsz: 接收端不会在文件协商中主动重启)。
    let mut info = [0u8; 192];
    let info_len = build_file_info(file.name, file.size, &mut info)?;
    // ZFILE 头: ZF0 = 转换 (二进制), ZF1 = 管理 (覆盖)
    send_bin_hdr(port, ZFILE, &[0, 0, ZF1_ZMCLOB, ZCBIN], fcs32)?;
    send_data(port, &info[..info_len], ZCRCW, fcs32)?;

    let mut errors: u32 = 0;
    loop {
        if port.abort_requested() {
            return Err(ZmError::UserAbort);
        }
        let header = match zgethdr(port, config.timeout_ms) {
            Ok(header) => header,
            Err(ZmError::Timeout) => {
                errors += 1;
                if errors > MAX_ERRORS {
                    return Err(ZmError::Timeout);
                }
                continue; // 重发 ZFILE
            }
            Err(e) => return Err(e),
        };
        match header.typ {
            ZRPOS => {
                return send_file_data(
                    port, config, index, file, rclhdr(&header.bytes), fcs32, read_at, progress,
                    chunk,
                );
            }
            ZCRC => {
                // 计算并应答文件 CRC (对齐 lrzsz zsendfile 的 ZCRC 分支)
                let mut crc: u32 = 0xFFFF_FFFF;
                let mut remaining = if rclhdr(&header.bytes) == 0 {
                    file.size
                } else {
                    rclhdr(&header.bytes).min(file.size)
                };
                let mut offset = 0u32;
                let mut scratch = [0u8; 256];
                while remaining > 0 {
                    let want = remaining.min(256) as usize;
                    let count = read_at(index, offset, &mut scratch[..want])
                        .map_err(|()| ZmError::File("读取文件失败"))?;
                    if count == 0 {
                        break;
                    }
                    for &b in &scratch[..count] {
                        crc = updc32(b, crc);
                    }
                    offset += count as u32;
                    remaining -= count as u32;
                }
                send_bin_hdr(port, ZCRC, &stohdr(!crc), fcs32)?;
            }
            ZSKIP => return Ok(()),
            ZRINIT => {
                // 过期 ZRINIT: 忽略, 继续等待真实响应
            }
            ZRQINIT => return Err(ZmError::Protocol("对方是发送端")),
            ZCAN | ZABORT | ZFIN => return Err(ZmError::Cancelled),
            _ => {
                errors += 1;
                if errors > MAX_ERRORS {
                    return Err(ZmError::Protocol("等待 ZRPOS 收到过多无效帧"));
                }
            }
        }
    }
}

/// ZFIN 握手 (发送端, 对齐 `saybibi`)
fn saybibi<P: ZmPort>(port: &mut P, config: &ZmConfig) -> Result<(), ZmError> {
    for _ in 0..3 {
        send_hex_hdr(port, ZFIN, &[0; 4])?;
        match zgethdr(port, config.timeout_ms) {
            Ok(header) if header.typ == ZFIN => {
                port.write(b"OO");
                port.flush();
                return Ok(());
            }
            Ok(_) => continue,
            Err(ZmError::Cancelled) => return Ok(()),
            Err(ZmError::Timeout) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 发送会话: 握手 (ZRQINIT → ZRINIT) → 逐个发送文件 → ZFIN 握手。
///
/// - `read_at(文件序号, 偏移, 缓冲)` 读取文件数据, 返回实际字节数 (0 = 结尾);
/// - `progress(文件序号, 已发, 总字节)` 每个子包调用一次。
pub fn send_session<
    P: ZmPort,
    R: FnMut(usize, u32, &mut [u8]) -> Result<usize, ()>,
    G: FnMut(usize, u32, u32),
>(
    port: &mut P,
    config: &ZmConfig,
    files: &[SendFile<'_>],
    read_at: &mut R,
    progress: &mut G,
    chunk: &mut [u8],
) -> Result<(), ZmError> {
    if files.is_empty() {
        return Ok(());
    }
    send_hex_hdr(port, ZRQINIT, &[0; 4])?;
    let fcs32 = getzrxinit(port, config)?;
    for (index, file) in files.iter().enumerate() {
        send_one(port, config, index, file, fcs32, read_at, progress, chunk)?;
    }
    saybibi(port, config)
}

// ============================== 主机单测 ==============================

#[cfg(test)]
mod tests {
    extern crate std;
    use std::prelude::v1::*;
    use super::*;
    use std::io::{Read, Write as _};
    use std::sync::mpsc;
    use std::time::Duration;

    // ---------- FCS 向量 (与 lrzsz crctab.c 实测一致) ----------

    #[test]
    fn crc16_vectors() {
        // 裸累加 "123456789" = 0xBEEF (lrzsz updcrc 实测)
        let mut crc: u16 = 0;
        for &b in b"123456789" {
            crc = updcrc(b, crc);
        }
        assert_eq!(crc, 0xBEEF);
        // 补两个零字节后 = 0x31C3 (XMODEM 发送值)
        crc = updcrc(0, updcrc(0, crc));
        assert_eq!(crc, 0x31C3);
        // 残差: 数据 + 发送值 (高字节在前) 累加后为 0
        let mut acc: u16 = 0;
        for &b in b"123456789" {
            acc = updcrc(b, acc);
        }
        acc = updcrc((0x31C3 >> 8) as u8, acc);
        acc = updcrc((0x31C3 & 0xFF) as u8, acc);
        assert_eq!(acc, 0);
    }

    #[test]
    fn crc32_vectors() {
        // "123456789" → 标准 CRC32 0xCBF43926 (与 lrzsz 实测一致)
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in b"123456789" {
            crc = updc32(b, crc);
        }
        assert_eq!(!crc, 0xCBF4_3926);
        // 残差: 数据 + FCS (小端) 累加后 = 0xDEBB20E3
        let mut acc: u32 = 0xFFFF_FFFF;
        for &b in b"abc" {
            acc = updc32(b, acc);
        }
        let tx = !acc;
        for i in 0..4 {
            acc = updc32((tx >> (8 * i)) as u8, acc);
        }
        assert_eq!(acc, CRC32_RESIDUE);
    }

    // ---------- 转义 ----------

    #[test]
    fn escape_roundtrip() {
        // 发送分类 + 接收解码往返: 除 ZDLE/XON/XOFF/^P 及 '@'+CR 外均原样
        let (mut a, mut b) = loopback();
        let mut w = EscWriter {
            port: &mut a,
            last: 0,
        };
        for byte in 0..=255u8 {
            w.send(byte);
        }
        drop(w);
        for byte in 0..=255u8 {
            let got = zdlread(&mut b, 1000).unwrap();
            assert!(
                matches!(got, Zdl::Byte(_)),
                "字节 0x{byte:02X} 解码为 {got:?}"
            );
            let Zdl::Byte(got) = got else { unreachable!() };
            assert_eq!(got, byte, "字节 0x{byte:02X} 往返不一致");
        }
        // 直接验证发送分类 (lrzsz zsendline 行为)
        assert_eq!(send_class(ZDLE), SendClass::Esc);
        assert_eq!(send_class(XON), SendClass::Esc);
        assert_eq!(send_class(XOFF), SendClass::Esc);
        assert_eq!(send_class(0x91), SendClass::Esc);
        assert_eq!(send_class(0x93), SendClass::Esc);
        assert_eq!(send_class(0x10), SendClass::Esc);
        assert_eq!(send_class(0x90), SendClass::Esc);
        assert_eq!(send_class(0x0D), SendClass::EscAfterAt);
        assert_eq!(send_class(0x8D), SendClass::EscAfterAt);
        assert_eq!(send_class(0x1B), SendClass::Raw);
        assert_eq!(send_class(0x00), SendClass::Raw);
        assert_eq!(send_class(0x20), SendClass::Raw);
        assert_eq!(send_class(0x7F), SendClass::Raw);
        assert_eq!(send_class(0xE0), SendClass::Raw);
    }

    // ---------- 内存端口 (单测双端) ----------

    struct MemPort {
        rx: mpsc::Receiver<u8>,
        tx: mpsc::Sender<u8>,
    }

    impl MemPort {
        fn idle() -> MemPort {
            let (tx, rx) = mpsc::channel();
            MemPort { rx, tx }
        }
    }

    impl ZmPort for MemPort {
        fn read_timeout(&mut self, timeout_ms: u32) -> Option<u8> {
            self.rx
                .recv_timeout(Duration::from_millis(timeout_ms as u64))
                .ok()
        }
        fn write(&mut self, bytes: &[u8]) {
            for &b in bytes {
                let _ = self.tx.send(b);
            }
        }
    }

    fn loopback() -> (MemPort, MemPort) {
        let (a_tx, a_rx) = mpsc::channel();
        let (b_tx, b_rx) = mpsc::channel();
        (
            MemPort { rx: a_rx, tx: b_tx },
            MemPort { rx: b_rx, tx: a_tx },
        )
    }

    // ---------- 帧往返 ----------

    #[test]
    fn header_roundtrip() {
        for &fcs32 in &[false, true] {
            let (mut a, mut b) = loopback();
            for typ in [ZRINIT, ZFILE, ZDATA, ZEOF, ZRPOS, ZFIN, ZACK, ZCRC] {
                let bytes = stohdr(0x1234_5678);
                send_bin_hdr(&mut a, typ, &bytes, fcs32).unwrap();
                let h = zgethdr(&mut b, 1000).unwrap();
                assert_eq!(h.typ, typ);
                assert_eq!(h.bytes, bytes);
                assert_eq!(h.frameind, if fcs32 { ZBIN32 } else { ZBIN });
            }
        }
        // 十六进制头 (含可选 XON)
        let (mut a, mut b) = loopback();
        for typ in [ZRINIT, ZRPOS, ZACK, ZFIN] {
            send_hex_hdr(&mut a, typ, &stohdr(0x89AB_CDEF)).unwrap();
            let h = zgethdr(&mut b, 1000).unwrap();
            assert_eq!(h.typ, typ);
            assert_eq!(h.bytes, stohdr(0x89AB_CDEF));
            assert_eq!(h.frameind, ZHEX);
        }
    }

    #[test]
    fn data_roundtrip() {
        for &fcs32 in &[false, true] {
            let (mut a, mut b) = loopback();
            // 包含各种需要转义的字节
            let data: Vec<u8> = (0..=255u8).collect();
            for end in [ZCRCG, ZCRCQ, ZCRCW, ZCRCE] {
                send_data(&mut a, &data, end, fcs32).unwrap();
                let mut buf = [0u8; 300];
                let (count, got_end) = zrdata(&mut b, 1000, fcs32, &mut buf).unwrap();
                assert_eq!(count, 256);
                assert_eq!(&buf[..count], &data[..]);
                assert_eq!(got_end, end);
                // ZCRCW 后应收到 XON
                if end == ZCRCW {
                    assert_eq!(b.rx.recv_timeout(Duration::from_millis(500)).unwrap(), XON);
                }
            }
        }
    }

    // ---------- 双端回环 (本实现收发互测) ----------

    fn random_data(len: usize, seed: u32) -> Vec<u8> {
        // 简单 LCG, 覆盖所有字节值 (含转义/控制字符)
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn pair_transfer_basic() {
        let data = random_data(32 * 1024, 42);
        let (mut a, mut b) = loopback();

        let receiver = std::thread::spawn(move || {
            let mut buffer = vec![0u8; 64 * 1024];
            let mut name = Vec::new();
            let mut received = Vec::new();
            receive_session(
                &mut b,
                &ZmConfig::default(),
                &mut buffer,
                &mut |_n, _s| true,
                &mut |n, _s, data| {
                    name = n.to_vec();
                    received.extend_from_slice(data);
                    Ok(())
                },
                &mut |_, _| {},
            )
            .unwrap();
            (name, received)
        });

        let files = [SendFile {
            name: "pair.bin",
            size: data.len() as u32,
        }];
        let mut chunk = vec![0u8; 1024];
        send_session(
            &mut a,
            &ZmConfig::default(),
            &files,
            &mut |_i, offset, buf| {
                let start = offset as usize;
                if start >= data.len() {
                    return Ok(0);
                }
                let count = (data.len() - start).min(buf.len());
                buf[..count].copy_from_slice(&data[start..start + count]);
                Ok(count)
            },
            &mut |_i, _s, _t| {},
            &mut chunk,
        )
        .unwrap();

        let (name, received) = receiver.join().unwrap();
        assert_eq!(name, b"pair.bin");
        assert_eq!(received, data);
    }


    #[test]
    fn pair_transfer_multifile_and_skip() {
        // 接收端跳过第二个文件 (超容量), 会话继续接收第三个
        let file_a = random_data(4096, 1);
        let file_c = random_data(2048, 3);
        let (mut a, mut b) = loopback();

        let receiver = std::thread::spawn(move || {
            let mut buffer = vec![0u8; 4 * 1024]; // 仅第一个文件放得下
            let mut files: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            receive_session(
                &mut b,
                &ZmConfig::default(),
                &mut buffer,
                &mut |_n, size| size <= 4 * 1024, // 只接受 ≤ 4K 的文件
                &mut |n, _s, data| {
                    files.push((n.to_vec(), data.to_vec()));
                    Ok(())
                },
                &mut |_, _| {},
            )
            .unwrap();
            files
        });

        let files = [
            SendFile {
                name: "a.bin",
                size: file_a.len() as u32,
            },
            SendFile {
                name: "b.bin",
                size: 6000, // 超 4K, 接收端跳过
            },
            SendFile {
                name: "c.bin",
                size: file_c.len() as u32,
            },
        ];
        let mut chunk = vec![0u8; 1024];
        send_session(
            &mut a,
            &ZmConfig::default(),
            &files,
            &mut |i, offset, buf| {
                let data = if i == 0 { &file_a } else { &file_c };
                let start = offset as usize;
                if start >= data.len() {
                    return Ok(0);
                }
                let count = (data.len() - start).min(buf.len());
                buf[..count].copy_from_slice(&data[start..start + count]);
                Ok(count)
            },
            &mut |_i, _s, _t| {},
            &mut chunk,
        )
        .unwrap();

        let received = receiver.join().unwrap();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].0, b"a.bin");
        assert_eq!(received[0].1, file_a);
        assert_eq!(received[1].0, b"c.bin");
        assert_eq!(received[1].1, file_c);
    }

    #[test]
    fn pair_transfer_empty_file() {
        let (mut a, mut b) = loopback();
        let receiver = std::thread::spawn(move || {
            let mut buffer = vec![0u8; 1024];
            let mut name = Vec::new();
            let mut received = Vec::new();
            receive_session(
                &mut b,
                &ZmConfig::default(),
                &mut buffer,
                &mut |_n, _s| true,
                &mut |n, _s, data| {
                    name = n.to_vec();
                    received.extend_from_slice(data);
                    Ok(())
                },
                &mut |_, _| {},
            )
            .unwrap();
            (name, received)
        });
        let files = [SendFile {
            name: "empty.txt",
            size: 0,
        }];
        let mut chunk = vec![0u8; 1024];
        send_session(
            &mut a,
            &ZmConfig::default(),
            &files,
            &mut |_i, _offset, _buf| Ok(0),
            &mut |_i, _s, _t| {},
            &mut chunk,
        )
        .unwrap();
        let (name, received) = receiver.join().unwrap();
        assert_eq!(name, b"empty.txt");
        assert!(received.is_empty());
    }

    #[test]
    fn receiver_skips_when_declared_oversized() {
        // 声明大小超缓冲 → decide 拒绝 → 发送端收到 ZSKIP 后继续下一个文件
        let data = random_data(2048, 7);
        let (mut a, mut b) = loopback();
        let receiver = std::thread::spawn(move || {
            let mut buffer = vec![0u8; 1024];
            let mut names: Vec<Vec<u8>> = Vec::new();
            receive_session(
                &mut b,
                &ZmConfig::default(),
                &mut buffer,
                &mut |_n, size| size <= 1024,
                &mut |n, _s, data| {
                    names.push(n.to_vec());
                    assert_eq!(data.len(), 1024);
                    Ok(())
                },
                &mut |_, _| {},
            )
            .unwrap();
            names
        });
        let files = [
            SendFile {
                name: "big.bin",
                size: 4096, // 被跳过
            },
            SendFile {
                name: "ok.bin",
                size: 1024,
            },
        ];
        let mut chunk = vec![0u8; 512];
        send_session(
            &mut a,
            &ZmConfig::default(),
            &files,
            &mut |_i, offset, buf| {
                let start = offset as usize;
                if start >= data.len() {
                    return Ok(0);
                }
                let count = (data.len() - start).min(buf.len());
                buf[..count].copy_from_slice(&data[start..start + count]);
                Ok(count)
            },
            &mut |_i, _s, _t| {},
            &mut chunk,
        )
        .unwrap();
        assert_eq!(receiver.join().unwrap(), vec![b"ok.bin".to_vec()]);
    }

    // ---------- 与真实 lrzsz 互通 ----------

    /// 把 stdio 子进程包装为 ZmPort (读取线程 → 通道)
    struct StdioPort {
        rx: mpsc::Receiver<u8>,
        tx: std::process::ChildStdin,
    }

    impl ZmPort for StdioPort {
        fn read_timeout(&mut self, timeout_ms: u32) -> Option<u8> {
            self.rx
                .recv_timeout(Duration::from_millis(timeout_ms as u64))
                .ok()
        }
        fn write(&mut self, bytes: &[u8]) {
            let _ = self.tx.write_all(bytes);
        }
        fn flush(&mut self) {
            let _ = self.tx.flush();
        }
    }

    fn pipe_stdio(
        child: &mut std::process::Child,
    ) -> (StdioPort, std::thread::JoinHandle<()>) {
        let stdout = child.stdout.take().expect("子进程 stdout");
        let stdin = child.stdin.take().expect("子进程 stdin");
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let mut stdout = stdout;
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        for &b in &buf[..n] {
                            if tx.send(b).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });
        (StdioPort { rx, tx: stdin }, reader)
    }

    fn lrzsz_available() -> bool {
        std::path::Path::new("/usr/bin/sz").exists() && std::path::Path::new("/usr/bin/rz").exists()
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zmodem-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 真实 `sz` 发送 → 本实现接收
    #[test]
    fn interop_real_sz_to_rust_receiver() {
        if !lrzsz_available() {
            eprintln!("skipped: /usr/bin/sz or /usr/bin/rz not found");
            return;
        }
        let dir = temp_dir("sz2rx");
        let source = dir.join("blob.bin");
        let data = random_data(24 * 1024, 99);
        std::fs::write(&source, &data).unwrap();

        let mut child = std::process::Command::new("/usr/bin/sz")
            .arg("-q")
            .arg(&source)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let (mut port, reader) = pipe_stdio(&mut child);

        let mut buffer = vec![0u8; 64 * 1024];
        let mut name = Vec::new();
        let mut received = Vec::new();
        receive_session(
            &mut port,
            &ZmConfig::default(),
            &mut buffer,
            &mut |_n, _s| true,
            &mut |n, _s, data| {
                name = n.to_vec();
                received.extend_from_slice(data);
                Ok(())
            },
            &mut |_, _| {},
        )
        .unwrap();

        let status = child.wait().unwrap();
        assert!(status.success(), "sz 退出状态: {status}");
        reader.join().unwrap();
        // 名称应为文件基础名
        assert!(name.ends_with(b"blob.bin"));
        assert_eq!(received, data);
    }

    /// 本实现发送 → 真实 `rz -y` 接收
    #[test]
    fn interop_rust_sender_to_real_rz() {
        if !lrzsz_available() {
            eprintln!("skipped: /usr/bin/sz or /usr/bin/rz not found");
            return;
        }
        let dir = temp_dir("tx2rz");
        let data = random_data(17 * 1024, 1234);

        let mut child = std::process::Command::new("/usr/bin/rz")
            .arg("-y")
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let (mut port, reader) = pipe_stdio(&mut child);

        let files = [SendFile {
            name: "sent.bin",
            size: data.len() as u32,
        }];
        let mut chunk = vec![0u8; 1024];
        send_session(
            &mut port,
            &ZmConfig::default(),
            &files,
            &mut |_i, offset, buf| {
                let start = offset as usize;
                if start >= data.len() {
                    return Ok(0);
                }
                let count = (data.len() - start).min(buf.len());
                buf[..count].copy_from_slice(&data[start..start + count]);
                Ok(count)
            },
            &mut |_i, _s, _t| {},
            &mut chunk,
        )
        .unwrap();

        let status = child.wait().unwrap();
        assert!(status.success(), "rz 退出状态: {status}");
        reader.join().unwrap();
        let received = std::fs::read(dir.join("sent.bin")).unwrap();
        assert_eq!(received, data);
    }

    /// 本实现发送多个文件 → 真实 `rz -y` 接收
    #[test]
    fn interop_rust_sender_multifile_to_real_rz() {
        if !lrzsz_available() {
            eprintln!("skipped: /usr/bin/sz or /usr/bin/rz not found");
            return;
        }
        let dir = temp_dir("tx2rz-multi");
        let data_a = random_data(5000, 11);
        let data_b = random_data(0, 12); // 空文件
        let data_c = random_data(9 * 1024, 13);

        let mut child = std::process::Command::new("/usr/bin/rz")
            .arg("-y")
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let (mut port, reader) = pipe_stdio(&mut child);

        let datas: [&[u8]; 3] = [&data_a, &data_b, &data_c];
        let files = [
            SendFile {
                name: "multi_a.bin",
                size: data_a.len() as u32,
            },
            SendFile {
                name: "multi_b.bin",
                size: 0,
            },
            SendFile {
                name: "multi_c.bin",
                size: data_c.len() as u32,
            },
        ];
        let mut chunk = vec![0u8; 1024];
        send_session(
            &mut port,
            &ZmConfig::default(),
            &files,
            &mut |i, offset, buf| {
                let data = datas[i];
                let start = offset as usize;
                if start >= data.len() {
                    return Ok(0);
                }
                let count = (data.len() - start).min(buf.len());
                buf[..count].copy_from_slice(&data[start..start + count]);
                Ok(count)
            },
            &mut |_i, _s, _t| {},
            &mut chunk,
        )
        .unwrap();

        let status = child.wait().unwrap();
        assert!(status.success(), "rz 退出状态: {status}");
        reader.join().unwrap();
        assert_eq!(std::fs::read(dir.join("multi_a.bin")).unwrap(), data_a);
        assert_eq!(std::fs::read(dir.join("multi_b.bin")).unwrap(), data_b);
        assert_eq!(std::fs::read(dir.join("multi_c.bin")).unwrap(), data_c);
    }

    // ---------- 解析辅助 ----------

    #[test]
    fn parse_file_info_works() {
        // "name\0 123 0 0 0 1 123\0"
        let mut data = b"name.txt\0 12345 0 100644 0 1 12345\0".to_vec();
        let (name, size) = parse_file_info(&data);
        assert_eq!(name, b"name.txt");
        assert_eq!(size, 12345);
        // 无元数据
        data = b"name.txt\0".to_vec();
        let (name, size) = parse_file_info(&data);
        assert_eq!(name, b"name.txt");
        assert_eq!(size, u32::MAX);
        // 无 NUL (非法块)
        data = b"name.txt".to_vec();
        let (name, size) = parse_file_info(&data);
        assert_eq!(name, b"name.txt");
        assert_eq!(size, u32::MAX);
    }

    #[test]
    fn build_file_info_roundtrip() {
        let mut out = [0u8; 192];
        let len = build_file_info("doc.txt", 777, &mut out).unwrap();
        let (name, size) = parse_file_info(&out[..len]);
        assert_eq!(name, b"doc.txt");
        assert_eq!(size, 777);
    }
}

