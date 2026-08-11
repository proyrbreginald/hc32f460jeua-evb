//! CRC 硬件加速器驱动
//!
//! 对齐 DDL v3.3.0 `hc32_ll_crc.c/h`。多项式**硬件固定**:
//! - CRC16: 0x1021 (X25/XMODEM/CCITT 系);
//! - CRC32: 0x04C11DB7 (IEEE 802.3);
//!
//! # 架构
//!
//! - 配置寄存器 CR (协议 CRC16/32 + REFIN/REFOUT/XOROUT 开关),
//!   初值直接写结果寄存器 RESLT;
//! - 数据按 8/16/32 位写入 DAT0 即触发计算 (流水, 无需等待);
//! - 结果: CRC32 读 RESLT 全 32 位; CRC16 取低 16 位;
//!   **当 REFIN+REFOUT+XOROUT 全使能时, 结果即标准 CRC**
//!   (例程与软件按位建模逐位一致);
//! - 累加模式: 分帧 `accumulate` 后取 `result()` (可中途
//!   `set_init_value` 重置);
//! - CRC 时钟门控 FCG0.bit23, 使能受 FCG0PC 写保护 (键 0xA5A50001)。
//!
//! # 使用
//!
//! ```no_run
//! // 一次性计算 (X25: CRC16, 初值 0xFFFF, 全使能)
//! let c = crc::calculate(&data, crc::DataWidth::Byte, crc::Config::x25());
//! // 分帧累加
//! crc::init(crc::Config::x25());
//! crc::accumulate(&frame1, crc::DataWidth::Byte);
//! crc::accumulate(&frame2, crc::DataWidth::Byte);
//! let c = crc::result();
//! ```
//!
//! 部分 API (累加/校验/标准配置) 供应用按需选用, 忽略未使用项的死代码警告。
#![allow(dead_code)]

/// CRC 基址
const CRC_BASE: usize = 0x4000_8C00;
/// PWC 基址 (FCG 时钟门控)
const PWC_BASE: usize = 0x4004_8000;

// ---- 寄存器偏移 (SVD/DDL 逐项核对) ----
const CR: usize = 0x00; // 协议/格式配置
const RESLT: usize = 0x04; // 结果/初值 (bit16 = CRC16 完成标志)
const FLG: usize = 0x0C; // bit0 = CRC32 完成标志
const DAT0: usize = 0x80; // 数据输入 (写任一即触发计算)
const FCG0: usize = 0x00; // PWC.FC0: 外设时钟门控 (清位 = 使能)
const FCG0PC: usize = 0x10; // FCG0 写保护键

// ---- CR 位 ----
const CR_CRC32: u32 = 1 << 1; // 协议: 0=CRC16, 1=CRC32
const CR_REFIN: u32 = 1 << 2; // 输入位序反转
const CR_REFOUT: u32 = 1 << 3; // 输出位序反转
const CR_XOROUT: u32 = 1 << 4; // 输出异或 (全 1 掩码)

// ---- RESLT / FLG ----
const RESLT_CRCFLAG16: u32 = 1 << 16; // CRC16 完成标志
const FLG_CRCFLAG32: u32 = 1 << 0; // CRC32 完成标志

// ---- FCG ----
const FCG0_CRC: u32 = 1 << 23; // CRC 时钟门控 (清位 = 使能)
const FCG0PC_UNLOCK: u32 = 0xA5A5_0001; // PRT0=1 解除保护
const FCG0PC_LOCK: u32 = 0xA5A5_0000; // PRT0=0 恢复保护

/// CRC 协议 (多项式硬件固定)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    /// CRC16 (多项式 0x1021, X25/CCITT 系)
    Crc16,
    /// CRC32 (多项式 0x04C11DB7, IEEE 802.3)
    Crc32,
}

/// 数据输入宽度 (每元素字节数, 对齐 DDL CRC_DATA_WIDTH_*)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataWidth {
    /// 按字节输入
    Byte = 1,
    /// 按半字 (16 位, 大端内存序) 输入
    HalfWord = 2,
    /// 按字 (32 位, 大端内存序) 输入
    Word = 4,
}

/// CRC 配置 (对齐 DDL `stc_crc_init_t`)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// 协议 (CRC16/CRC32)
    pub protocol: Protocol,
    /// 初值 (写入 RESLT, CRC16 用低 16 位)
    pub init_value: u32,
    /// 输入位序反转 (REFIN)
    pub ref_in: bool,
    /// 输出位序反转 (REFOUT)
    pub ref_out: bool,
    /// 输出异或 (XOROUT, 掩码硬件固定全 1)
    pub xor_out: bool,
}

impl Config {
    /// X25: CRC16, 初值 0xFFFF, REFIN+REFOUT+XOROUT 全使能
    pub const fn x25() -> Self {
        Self {
            protocol: Protocol::Crc16,
            init_value: 0xFFFF,
            ref_in: true,
            ref_out: true,
            xor_out: true,
        }
    }

    /// CCITT-FALSE: CRC16, 初值 0xFFFF, 全禁用
    pub const fn ccitt_false() -> Self {
        Self {
            protocol: Protocol::Crc16,
            init_value: 0xFFFF,
            ref_in: false,
            ref_out: false,
            xor_out: false,
        }
    }

    /// CRC-32 (IEEE 802.3): CRC32, 初值 0xFFFFFFFF, 全使能
    pub const fn crc32() -> Self {
        Self {
            protocol: Protocol::Crc32,
            init_value: 0xFFFF_FFFF,
            ref_in: true,
            ref_out: true,
            xor_out: true,
        }
    }

    /// CRC-32/MPEG-2: CRC32, 初值 0xFFFFFFFF, 全禁用
    pub const fn crc32_mpeg2() -> Self {
        Self {
            protocol: Protocol::Crc32,
            init_value: 0xFFFF_FFFF,
            ref_in: false,
            ref_out: false,
            xor_out: false,
        }
    }
}

// ============================== 底层寄存器访问 ==============================

fn read32(offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((CRC_BASE + offset) as *const u32) }
}

fn write32(offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile((CRC_BASE + offset) as *mut u32, value) };
}

fn write_u16(offset: usize, value: u16) {
    unsafe { core::ptr::write_volatile((CRC_BASE + offset) as *mut u16, value) };
}

/// 当前协议 (读 CR.bit1, 对齐 DDL CRC_GetResult 的 READ_REG32_BIT)
fn protocol_now() -> Protocol {
    if read32(CR) & CR_CRC32 != 0 {
        Protocol::Crc32
    } else {
        Protocol::Crc16
    }
}

/// 使能 CRC 时钟 (FCG0.bit23 清位; FCG0 受 FCG0PC 写保护)
fn clock_enable() {
    unsafe {
        core::ptr::write_volatile((PWC_BASE + FCG0PC) as *mut u32, FCG0PC_UNLOCK);
        let fcg0 = core::ptr::read_volatile((PWC_BASE + FCG0) as *const u32);
        core::ptr::write_volatile((PWC_BASE + FCG0) as *mut u32, fcg0 & !FCG0_CRC);
        core::ptr::write_volatile((PWC_BASE + FCG0PC) as *mut u32, FCG0PC_LOCK);
    }
}

// ============================== 配置与计算 ==============================

/// 初始化 CRC 引擎: 时钟使能 → 配置协议/格式 → 写入初值
/// (对齐 DDL `CRC_Init`)
pub fn init(cfg: Config) {
    clock_enable();
    let mut cr = 0u32;
    if cfg.ref_in {
        cr |= CR_REFIN;
    }
    if cfg.ref_out {
        cr |= CR_REFOUT;
    }
    if cfg.xor_out {
        cr |= CR_XOROUT;
    }
    // 两步写入 (对齐 DDL): 先写格式位 (协议位 = CRC16), 再写协议位,
    // 避免格式位与协议位同次写入的硬件时序约束
    write32(CR, cr);
    if cfg.protocol == Protocol::Crc32 {
        write32(CR, cr | CR_CRC32);
    }
    // 初值写入宽度按协议区分 (对齐 DDL CRC_Init: CRC16 用 16 位写,
    // CRC32 用 32 位写, 避免触碰 RESLT 高半字的 CRCFLAG16 等位)
    match cfg.protocol {
        Protocol::Crc16 => write_u16(RESLT, cfg.init_value as u16),
        Protocol::Crc32 => write32(RESLT, cfg.init_value),
    }
}

/// 设置初值 (累加模式中途重置, 对齐 DDL `CRC_SetInitValue`:
/// CRC16 掩 0xFFFF, CRC32 全写)
pub fn set_init_value(value: u32) {
    match protocol_now() {
        Protocol::Crc16 => write32(RESLT, value & 0xFFFF),
        Protocol::Crc32 => write32(RESLT, value),
    }
}

/// 累加一段数据 (按指定宽度写 DAT0, 对齐 DDL `CRC_*_AccumulateData`)
///
/// **关键**: 必须以对应宽度的总线访问写 DAT0 (8/16/32 位)——硬件按
/// 总线字节选通判断元素宽度, 统一 32 位写会把 8 位数据误算成 32 位
/// 元素。尾部不完整元素忽略 (与 DDL 按元素计数的语义一致)。
pub fn accumulate(data: &[u8], width: DataWidth) {
    match width {
        DataWidth::Byte => {
            for &b in data {
                unsafe { core::ptr::write_volatile((CRC_BASE + DAT0) as *mut u8, b) };
            }
        }
        DataWidth::HalfWord => {
            for chunk in data.chunks_exact(2) {
                let v = u16::from_le_bytes([chunk[0], chunk[1]]);
                unsafe { core::ptr::write_volatile((CRC_BASE + DAT0) as *mut u16, v) };
            }
        }
        DataWidth::Word => {
            for chunk in data.chunks_exact(4) {
                let mut b = [0u8; 4];
                b.copy_from_slice(chunk);
                let v = u32::from_le_bytes(b);
                unsafe { core::ptr::write_volatile((CRC_BASE + DAT0) as *mut u32, v) };
            }
        }
    }
}

/// 读取计算结果 (对齐 DDL `CRC_GetResult`: CRC32 读全 32 位;
/// CRC16 掩 0xFFFF —— RESLT 高半字含 CRCFLAG16 完成标志位, 不掩会
/// 混入结果)。
///
/// 当 REFIN+REFOUT+XOROUT 全使能时即标准 CRC 值。
pub fn result() -> u32 {
    match protocol_now() {
        Protocol::Crc16 => read32(RESLT) & 0xFFFF,
        Protocol::Crc32 => read32(RESLT),
    }
}

/// 计算完成标志 (对齐 DDL `CRC_GetResultStatus`)
pub fn result_ready(protocol: Protocol) -> bool {
    match protocol {
        Protocol::Crc16 => read32(RESLT) & RESLT_CRCFLAG16 != 0,
        Protocol::Crc32 => read32(FLG) & FLG_CRCFLAG32 != 0,
    }
}

/// 一次性计算 (初始化 → 累加 → 取结果)
pub fn calculate(data: &[u8], width: DataWidth, cfg: Config) -> u32 {
    init(cfg);
    accumulate(data, width);
    result()
}

/// 校验: 计算数据 CRC 并与期望值比较
pub fn check(data: &[u8], width: DataWidth, cfg: Config, expected: u32) -> bool {
    let c = calculate(data, width, cfg);
    let mask = match cfg.protocol {
        Protocol::Crc16 => 0xFFFF,
        Protocol::Crc32 => 0xFFFF_FFFF,
    };
    c & mask == expected & mask
}
