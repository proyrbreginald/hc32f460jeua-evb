//! 片内 Flash (EFM) 控制器驱动
//!
//! 对齐 DDL v3.3.0 `hc32_ll_efm.c/h` (FWMC 模式 + 写 Flash 地址触发模型,
//! 非旧版 EFMCR.OP/START 模型)。
//!
//! # 架构
//!
//! - 主 Flash **512KB** (0x0000_0000~0x0007_FFFF), **64 个 8KB 扇区**
//!   (最小擦除单位 = 扇区, 无页擦除);
//! - 编程按**字 (4B)**: 写 Flash 地址即触发, 一次只能把 1→0;
//! - 操作模型: FAPRT 解锁 → FWMC.PEMODE=1 → 设 PEMOD 模式 → 写目标地址
//!   触发 → 等 FSR.RDY + OPTEND → 恢复只读并锁定;
//! - **BUSHLDCTL=0 (bus hold)**: 擦写期间总线被占用, CPU 取指/中断响应
//!   自动 stall, 直到操作完成 —— 从 Flash 运行也可安全执行扇区擦除/
//!   单字编程 (执行代码所在扇区未被擦除);
//! - **全片擦除/序列编程会擦除执行代码所在 Flash, 必须从 RAM 运行**
//!   (DDL 标记 `__RAM_FUNC`), 本模块不提供, 需要时请自建 RAM 函数;
//! - 操作不关中断: bus hold 期间中断挂起, 操作结束后按优先级响应。
//!
//! # 使用
//!
//! ```no_run
//! // 擦除一个扇区后写入数据 (内部自动解锁/锁回)
//! efm::sector_erase(0x0007_C000)?;
//! efm::program(0x0007_C000, b"hello")?;
//! // 读回校验 (Flash 内存映射, 任意字节可读)
//! assert_eq!(efm::read_byte(0x0007_C000), b'h');
//! ```
//!
//! 注意: 写保护/安全 (level1/2)、窗口写保护、swap、OTP 锁、序列编程、
//! 全片擦除不在本模块范围 (见 DDL `EFM_Protect_Enable` 等)。
//!
//! 常量与 API 供应用按需选用, 忽略未使用项的死代码警告。
#![allow(dead_code)]

/// 主 Flash 大小 (512KB)
pub const FLASH_SIZE: u32 = 0x0008_0000;
/// 扇区大小 (8KB, 最小擦除单位)
pub const SECTOR_SIZE: u32 = 0x0000_2000;
/// 扇区数 (64)
pub const SECTOR_COUNT: u32 = FLASH_SIZE / SECTOR_SIZE;

/// EFM 基址
const EFM_BASE: usize = 0x4001_0400;

// ---- 寄存器偏移 (SVD/DDL 逐项核对) ----
const FAPRT: usize = 0x00; // 写保护键 (0x0123→0x3210 解锁, 读回 1 = 已解锁; 0x0000 锁定)
const FSTP: usize = 0x04; // Flash 停止 (bit0, 1=停止)
const FRMC: usize = 0x08; // FLWT[7:4] 读等待, CACHE[16]
const FWMC: usize = 0x0C; // PEMODE[0] 寄存器可写, PEMOD[6:4] 操作模式, BUSHLDCTL[8]
const FSR: usize = 0x10; // 状态
const FSCLR: usize = 0x14; // 状态清除 (写 1)
const FSWP: usize = 0x1C; // swap 状态
const UQID0: usize = 0x50; // 唯一 ID (3 × 32bit)

// ---- FSR 位 ----
const FSR_PEWERR: u32 = 1 << 0; // 编程/擦除错误
const FSR_PEPRTERR: u32 = 1 << 1; // 写保护地址违例
const FSR_PGSZERR: u32 = 1 << 2; // 保护区域大小错误
const FSR_PGMISMTCH: u32 = 1 << 3; // 编程回读不匹配
const FSR_OPTEND: u32 = 1 << 4; // 操作结束
const FSR_COLERR: u32 = 1 << 5; // 读冲突 (写 Flash 期间读同一地址)
const FSR_RDY: u32 = 1 << 8; // 就绪 (可发起新操作)

/// 全部错误标志 (供 [`clear_status`] / [`last_error`] 用)
const FSR_ERRORS: u32 = FSR_PEWERR | FSR_PEPRTERR | FSR_PGSZERR | FSR_PGMISMTCH | FSR_COLERR;

// ---- FWMC ----
const FWMC_PEMODE: u32 = 1 << 0;
const FWMC_PEMOD_POS: u32 = 4;
const FWMC_BUSHLDCTL: u32 = 1 << 8;

/// 操作模式 (FWMC.PEMOD, 与 DDL `EFM_MD_*` 一致)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpMode {
    /// 只读 (复位默认)
    ReadOnly = 0,
    /// 单字编程 (每次写一个 4 字节字)
    Program = 1,
    /// 编程 + 回读校验 (写后检查 PGMISMTCH)
    ProgramReadBack = 2,
    /// 序列编程 (连续 512B, 需 RAM 运行, 本模块不提供)
    SequenceProgram = 3,
    /// 扇区擦除 (8KB)
    SectorErase = 4,
    /// 全片擦除 (需 RAM 运行, 本模块不提供)
    ChipErase = 5,
}

/// 操作失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EfmError {
    /// 等待就绪/操作结束超时
    Timeout,
    /// 编程/擦除错误 (FSR.PEWERR)
    ProgramEraseError,
    /// 写保护地址违例 (FSR.PEPRTERR)
    ProtectViolation,
    /// 保护区域大小错误 (FSR.PGSZERR)
    SizeError,
    /// 编程回读不匹配 (FSR.PGMISMTCH)
    Mismatch,
    /// 读冲突 (FSR.COLERR)
    ReadCollision,
    /// 地址非法 (非字对齐或超出 Flash 范围)
    InvalidAddr,
}

/// 由 FSR 错误位映射错误 (无错误返回 None)
fn map_error(status: u32) -> Option<EfmError> {
    if status & FSR_PEWERR != 0 {
        Some(EfmError::ProgramEraseError)
    } else if status & FSR_PEPRTERR != 0 {
        Some(EfmError::ProtectViolation)
    } else if status & FSR_PGSZERR != 0 {
        Some(EfmError::SizeError)
    } else if status & FSR_PGMISMTCH != 0 {
        Some(EfmError::Mismatch)
    } else if status & FSR_COLERR != 0 {
        Some(EfmError::ReadCollision)
    } else {
        None
    }
}

/// 读取 FSR 状态 (对齐 DDL `EFM_GetStatus`)
pub fn status() -> u32 {
    unsafe { core::ptr::read_volatile((EFM_BASE + FSR) as *const u32) }
}

/// 清除状态标志 (写 FSCLR, 对齐 DDL `EFM_ClearStatus`)
pub fn clear_status(flags: u32) {
    unsafe { core::ptr::write_volatile((EFM_BASE + FSCLR) as *mut u32, flags) };
}

/// 是否就绪 (FSR.RDY=1, 可发起新操作)
pub fn ready() -> bool {
    status() & FSR_RDY != 0
}

/// 等待就绪 (带超时, 循环次数按 HCLK 折算 ~50µs, 对齐 DDL EFM_WAIT_FLAG)
pub fn wait_ready() -> bool {
    let timeout = crate::clk::hclk_hz() / 20_000;
    let mut i = 0u32;
    while !ready() {
        i += 1;
        if i > timeout {
            return false;
        }
    }
    true
}

/// 解锁 EFM 寄存器写保护 (FAPRT 键 0x0123→0x3210, 对齐 DDL `EFM_REG_Unlock`)
pub fn unlock() {
    unsafe {
        core::ptr::write_volatile(EFM_BASE as *mut u32, 0x0123);
        core::ptr::write_volatile(EFM_BASE as *mut u32, 0x3210);
    }
}

/// 锁定 EFM 寄存器写保护 (FAPRT=0, 对齐 DDL `EFM_REG_Lock`)
pub fn lock() {
    unsafe { core::ptr::write_volatile(EFM_BASE as *mut u32, 0x0000) };
}

/// 使能擦写模式 (FWMC.PEMODE=1, 对齐 DDL `EFM_FWMC_Cmd(ENABLE)`)
pub fn enable_program_mode() {
    unsafe {
        let v = core::ptr::read_volatile((EFM_BASE + FWMC) as *const u32);
        core::ptr::write_volatile((EFM_BASE + FWMC) as *mut u32, v | FWMC_PEMODE);
    }
}

/// 退出擦写模式 (FWMC.PEMODE=0)
pub fn disable_program_mode() {
    unsafe {
        let v = core::ptr::read_volatile((EFM_BASE + FWMC) as *const u32);
        core::ptr::write_volatile((EFM_BASE + FWMC) as *mut u32, v & !FWMC_PEMODE);
    }
}

/// 设置操作模式 (FWMC.PEMOD, 对齐 DDL `EFM_SetOperateMode`)
fn set_op_mode(mode: OpMode) {
    unsafe {
        let v = core::ptr::read_volatile((EFM_BASE + FWMC) as *const u32);
        let v = (v & !(0x7 << FWMC_PEMOD_POS)) | ((mode as u32) << FWMC_PEMOD_POS);
        core::ptr::write_volatile((EFM_BASE + FWMC) as *mut u32, v);
    }
}

/// 等待操作结束: RDY 置位 → OPTEND 置位并清除 (对齐 DDL `EFM_WaitEnd`)
fn wait_end(timeout: u32) -> Result<(), EfmError> {
    let mut i = 0u32;
    while status() & FSR_RDY == 0 {
        i += 1;
        if i > timeout {
            return Err(EfmError::Timeout);
        }
    }
    if status() & FSR_OPTEND == 0 {
        return Err(EfmError::Timeout);
    }
    clear_status(FSR_OPTEND);
    Ok(())
}

/// 校验地址: 字对齐且在 Flash 范围内
fn check_addr(addr: u32) -> Result<(), EfmError> {
    if !addr.is_multiple_of(4) || addr >= FLASH_SIZE {
        return Err(EfmError::InvalidAddr);
    }
    Ok(())
}

// ============================== 读 ==============================

/// 读取 Flash 字节 (Flash 内存映射, 任意字节地址)
pub fn read_byte(addr: u32) -> u8 {
    unsafe { core::ptr::read_volatile(addr as *const u8) }
}

/// 读取 Flash 字 (4 字节, 需字对齐)
pub fn read_word(addr: u32) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// 读取唯一 ID (UQID0~2, 96 位)
pub fn uid() -> [u32; 3] {
    unsafe {
        [
            core::ptr::read_volatile((EFM_BASE + UQID0) as *const u32),
            core::ptr::read_volatile((EFM_BASE + UQID0 + 4) as *const u32),
            core::ptr::read_volatile((EFM_BASE + UQID0 + 8) as *const u32),
        ]
    }
}

// ============================== 擦除 ==============================

/// 扇区擦除 (8KB, 对齐 DDL `EFM_SectorErase`)
///
/// `addr` 只需字对齐, 擦除其所在整个扇区。操作期间总线被占用
/// (bus hold), CPU stall 直至完成 (~ms 级); 结束后按错误位返回。
pub fn sector_erase(addr: u32) -> Result<(), EfmError> {
    check_addr(addr)?;
    if !wait_ready() {
        return Err(EfmError::Timeout);
    }
    unlock();
    enable_program_mode();
    clear_status(FSR_ERRORS | FSR_OPTEND);
    set_op_mode(OpMode::SectorErase);

    // 触发: 向目标地址写 0 (擦除 = 全 1, 任意值均可, DDL 用 0)
    let result = unsafe {
        core::ptr::write_volatile(addr as *mut u32, 0);
        // 扇区擦除 ~ms 级, 超时按 HCLK 折算 ~20ms (对齐 DDL EFM_ERASE_TIMEOUT)
        wait_end(crate::clk::hclk_hz() / 50)
    };

    set_op_mode(OpMode::ReadOnly);
    disable_program_mode();
    lock();
    result?;
    map_error(status()).map_or(Ok(()), Err)
}

// ============================== 编程 ==============================

/// 编程数据到 Flash (单字模式, 对齐 DDL `EFM_Program`)
///
/// - `addr` 必须 4 字节对齐; 长度任意, 尾部不足 4 字节用 0xFF 补齐
///   (擦除态为 0xFF, 补齐字节不改变);
/// - 只能把 1→0 (先擦后写); 操作期间 bus hold, CPU stall 至完成;
/// - 逐字等待结束, 失败返回对应错误。
pub fn program(addr: u32, data: &[u8]) -> Result<(), EfmError> {
    if !addr.is_multiple_of(4) || addr + data.len() as u32 > FLASH_SIZE {
        return Err(EfmError::InvalidAddr);
    }
    if !wait_ready() {
        return Err(EfmError::Timeout);
    }
    unlock();
    enable_program_mode();
    clear_status(FSR_ERRORS | FSR_OPTEND);
    set_op_mode(OpMode::Program);

    let mut result = Ok(());
    for (i, chunk) in data.chunks(4).enumerate() {
        // 组装字: 实际字节 + 尾部 0xFF 填充
        let mut word = 0xFFFF_FFFFu32;
        for (j, &b) in chunk.iter().enumerate() {
            word &= !(0xFFu32 << (8 * j));
            word |= (b as u32) << (8 * j);
        }
        unsafe {
            core::ptr::write_volatile((addr + 4 * i as u32) as *mut u32, word);
        }
        // 单字编程 ~µs 级, 超时按 HCLK 折算 ~53µs (对齐 DDL EFM_PGM_TIMEOUT)
        result = wait_end(crate::clk::hclk_hz() / 20_000);
        if result.is_err() {
            break;
        }
    }

    set_op_mode(OpMode::ReadOnly);
    disable_program_mode();
    lock();
    result?;
    map_error(status()).map_or(Ok(()), Err)
}

/// 编程一个字 (4 字节, 对齐 DDL `EFM_ProgramWord`)
pub fn program_word(addr: u32, word: u32) -> Result<(), EfmError> {
    program(addr, &word.to_le_bytes())
}

// ============================== FLASH 读等待周期 ==============================

/// 表 7-1: CPU 时钟频率 → FLASH 读等待周期 (普通读模式)
///
/// 从 clk 模块迁入: 闪存控制器寄存器归本模块所有 (见 [`set_wait_cycle`])。
pub const fn wait_cycle(hclk_hz: u32) -> u32 {
    if hclk_hz <= 33_000_000 {
        0
    } else if hclk_hz <= 66_000_000 {
        1
    } else if hclk_hz <= 99_000_000 {
        2
    } else if hclk_hz <= 132_000_000 {
        3
    } else if hclk_hz <= 168_000_000 {
        4
    } else {
        5
    }
}

/// 配置 FLASH 读等待周期 (FRMC.FLWT, 对齐 DDL `EFM_SetWaitCycle`)
///
/// 由 [`crate::clk`] 在切换系统时钟前调用; 回读确认写入生效。
pub fn set_wait_cycle(hclk_hz: u32) {
    const FRMC_FLWT_MASK: u32 = 0x0000_00F0;
    let cycles = wait_cycle(hclk_hz) << 4;

    unlock();
    unsafe {
        let frmc = core::ptr::read_volatile((EFM_BASE + FRMC) as *const u32);
        core::ptr::write_volatile(
            (EFM_BASE + FRMC) as *mut u32,
            (frmc & !FRMC_FLWT_MASK) | cycles,
        );
        // 回读确认配置生效
        while core::ptr::read_volatile((EFM_BASE + FRMC) as *const u32) & FRMC_FLWT_MASK != cycles {
            // 等待
        }
    }
    lock();
}
