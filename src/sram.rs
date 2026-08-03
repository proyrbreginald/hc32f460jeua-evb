//! 片内 SRAM 控制器 (SRAMC) 驱动
//!
//! 对齐 DDL v3.3.0 `hc32_ll_sram.c/h` 与参考手册表 8-1。
//!
//! # 架构
//!
//! - 5 个 SRAM bank (总 188K + Ret 4K):
//!
//!   | Bank | 基址 | 大小 | 错误检测 |
//!   |---|---|---|---|
//!   | SRAMH | 0x1FFF_8000 | 32KB | 偶校验 (恒使能) |
//!   | SRAM1 | 0x2000_0000 | 64KB | 偶校验 |
//!   | SRAM2 | 0x2001_0000 | 64KB | 偶校验 |
//!   | SRAM3 | 0x2002_0000 | 28KB | **ECC** (可配 MD1~3) |
//!   | Ret | 0x200F_0000 | 4KB | 偶校验 |
//!
//!   (SRAMH/SRAM1/2/Ret 奇偶校验始终使能, 无使能位; SRAM3 用 ECC)
//! - 等待周期: 每 bank 独立读/写 3 位字段 (WTCR), 表 8-1:
//!   SRAMH 恒 0 等待 (0~200MHz); SRAM1/2/Ret: ≤100MHz→0, >100MHz→1;
//!   **SRAM3 恒 1 等待** (脚注: 用作堆栈时须 ≥2 CPU 周期访问 ——
//!   本工程栈顶 0x2002_7000 位于 SRAM3 末尾);
//! - 错误上报: 奇偶/ECC 错误经 **NMI** (或可配置为复位, CKCR.PYOAD/ECCOAD),
//!   标志在 CKSR (写 1 清除); 应用可轮询 [`error`]/[`clear_status`];
//! - 寄存器写保护: WTPR/CKPR 键值 0x77 解锁 / 0x76 锁定 (两者都要写)。
//!
//! # 使用
//!
//! ```no_run
//! // 切换系统时钟前按目标频率配置等待周期 (由 clk 模块调用)
//! sram::set_wait_cycles(clk::hclk_hz());
//! // 查询/清除奇偶或 ECC 错误 (正常为 None)
//! if let Some(e) = sram::error() { ...; sram::clear_status(sram::ERR_ALL); }
//! ```
//!
//! 部分 API (ECC/错误动作/状态查询) 供应用按需选用, 忽略未使用项的死代码警告。
#![allow(dead_code)]

/// SRAMC 基址
const SRAMC: usize = 0x4005_0800;

// ---- 寄存器偏移 (SVD/DDL 逐项核对) ----
const WTCR: usize = 0x00; // 等待周期 (8 × 3bit)
const WTPR: usize = 0x04; // 写保护键 (0x77 解锁 / 0x76 锁定)
const CKCR: usize = 0x08; // 错误动作/ECC 模式 (非时钟使能!)
const CKPR: usize = 0x0C; // 写保护键 (同上)
const CKSR: usize = 0x10; // 错误状态 (写 1 清除)

// ---- WTCR 字段位 ----
const WTCR_SRAM12_RWT_POS: u32 = 0;
const WTCR_SRAM12_WWT_POS: u32 = 4;
const WTCR_SRAM3_RWT_POS: u32 = 8;
const WTCR_SRAM3_WWT_POS: u32 = 12;
const WTCR_SRAMH_RWT_POS: u32 = 16;
const WTCR_SRAMH_WWT_POS: u32 = 20;
const WTCR_SRAMR_RWT_POS: u32 = 24;
const WTCR_SRAMR_WWT_POS: u32 = 28;

// ---- CKCR 位 ----
const CKCR_PYOAD: u32 = 1 << 0; // 奇偶错误动作: 0=NMI, 1=复位
const CKCR_ECCOAD: u32 = 1 << 16; // ECC 错误动作: 0=NMI, 1=复位
const CKCR_ECCMOD: u32 = 0x03 << 24; // SRAM3 ECC 模式 [25:24]

// ---- CKSR 错误标志 ----
/// SRAM3 ECC 1 位错误 (可纠正)
pub const ERR_SRAM3_1: u32 = 1 << 0;
/// SRAM3 ECC 2 位错误 (不可纠正)
pub const ERR_SRAM3_2: u32 = 1 << 1;
/// SRAM1/2 奇偶错误
pub const ERR_SRAM12: u32 = 1 << 2;
/// SRAMH 奇偶错误
pub const ERR_SRAMH: u32 = 1 << 3;
/// Ret SRAM 奇偶错误
pub const ERR_SRAMR: u32 = 1 << 4;
/// 全部错误标志 (清状态用)
pub const ERR_ALL: u32 = ERR_SRAM3_1 | ERR_SRAM3_2 | ERR_SRAM12 | ERR_SRAMH | ERR_SRAMR;

/// SRAM 错误类型 (由 CKSR 映射)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SramError {
    /// SRAM3 ECC 1 位错误 (可纠正)
    Sram3Ecc1,
    /// SRAM3 ECC 2 位错误 (不可纠正)
    Sram3Ecc2,
    /// SRAM1/2 奇偶错误
    Sram12Parity,
    /// SRAMH 奇偶错误
    SramhParity,
    /// Ret SRAM 奇偶错误
    SramrParity,
}

/// SRAM 等待周期配置 (每 bank 读/写等待)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WaitCycles {
    /// SRAM1/2 读/写等待
    pub sram12: u8,
    /// SRAM3 读/写等待 (栈空间恒 1)
    pub sram3: u8,
    /// SRAMH 读/写等待 (恒 0)
    pub sramh: u8,
    /// Ret SRAM 读/写等待
    pub sramr: u8,
}

/// 表 8-1: 由 HCLK 频率推导各 SRAM 等待周期
///
/// SRAMH 恒 0 等待 (0~200MHz 全支持); SRAM1/2/Ret: HCLK ≤ 100MHz → 0,
/// \> 100MHz → 1; **SRAM3 恒 1** (脚注: 用作堆栈空间时须 1 等待 =
/// 2 个 CPU 周期以上访问, 本工程栈顶在 SRAM3 末尾)。
pub const fn wait_cycles(hclk_hz: u32) -> WaitCycles {
    let w = if hclk_hz > 100_000_000 { 1 } else { 0 };
    WaitCycles {
        sram12: w,
        sram3: 1,
        sramh: 0,
        sramr: w,
    }
}

/// 解锁 SRAMC 寄存器写保护 (WTPR 与 CKPR 都要写 0x77, 对齐 DDL
/// `SRAM_REG_Unlock`)
pub fn unlock() {
    unsafe {
        core::ptr::write_volatile((SRAMC + WTPR) as *mut u32, 0x77);
        core::ptr::write_volatile((SRAMC + CKPR) as *mut u32, 0x77);
    }
}

/// 锁定 SRAMC 寄存器写保护 (键值 0x76, 对齐 DDL `SRAM_REG_Lock`)
pub fn lock() {
    unsafe {
        core::ptr::write_volatile((SRAMC + WTPR) as *mut u32, 0x76);
        core::ptr::write_volatile((SRAMC + CKPR) as *mut u32, 0x76);
    }
}

/// 按 HCLK 频率自动配置全部 SRAM 等待周期 (由 [`crate::clk`] 切换时钟前调用)
///
/// 结果与 DDL BSP_CLK_Init 一致: ≤100MHz → SRAM3=1 其余 0;
/// >100MHz → SRAM1/2/3/Ret=1, SRAMH=0。
pub fn set_wait_cycles(hclk_hz: u32) {
    let w = wait_cycles(hclk_hz);
    let wtcr = (w.sram12 as u32) << WTCR_SRAM12_RWT_POS
        | (w.sram12 as u32) << WTCR_SRAM12_WWT_POS
        | (w.sram3 as u32) << WTCR_SRAM3_RWT_POS
        | (w.sram3 as u32) << WTCR_SRAM3_WWT_POS
        | (w.sramh as u32) << WTCR_SRAMH_RWT_POS
        | (w.sramh as u32) << WTCR_SRAMH_WWT_POS
        | (w.sramr as u32) << WTCR_SRAMR_RWT_POS
        | (w.sramr as u32) << WTCR_SRAMR_WWT_POS;
    unlock();
    unsafe {
        core::ptr::write_volatile((SRAMC + WTCR) as *mut u32, wtcr);
    }
    lock();
}

/// 读取当前等待周期配置
pub fn wait_cycles_now() -> WaitCycles {
    let v = unsafe { core::ptr::read_volatile((SRAMC + WTCR) as *const u32) };
    WaitCycles {
        sram12: ((v >> WTCR_SRAM12_RWT_POS) & 0x7) as u8,
        sram3: ((v >> WTCR_SRAM3_RWT_POS) & 0x7) as u8,
        sramh: ((v >> WTCR_SRAMH_RWT_POS) & 0x7) as u8,
        sramr: ((v >> WTCR_SRAMR_RWT_POS) & 0x7) as u8,
    }
}

// ============================== 错误检测 (奇偶/ECC) ==============================

/// 读取错误状态 (CKSR, 对齐 DDL `SRAM_GetStatus`)
pub fn status() -> u32 {
    unsafe { core::ptr::read_volatile((SRAMC + CKSR) as *const u32) }
}

/// 清除错误标志 (写 CKSR, 写 1 清除, 对齐 DDL `SRAM_ClearStatus`)
pub fn clear_status(flags: u32) {
    unsafe { core::ptr::write_volatile((SRAMC + CKSR) as *mut u32, flags) };
}

/// 查询错误 (按优先级返回最高位错误, 无错误返回 None)
///
/// 错误经 NMI 上报 (或 CKCR 配置为复位); 本 API 供应用在正常流程
/// 中轮询检测 (读取未初始化 SRAM 会触发奇偶错误, 属预期行为)。
pub fn error() -> Option<SramError> {
    let s = status();
    if s & ERR_SRAM3_2 != 0 {
        Some(SramError::Sram3Ecc2)
    } else if s & ERR_SRAM3_1 != 0 {
        Some(SramError::Sram3Ecc1)
    } else if s & ERR_SRAM12 != 0 {
        Some(SramError::Sram12Parity)
    } else if s & ERR_SRAMH != 0 {
        Some(SramError::SramhParity)
    } else if s & ERR_SRAMR != 0 {
        Some(SramError::SramrParity)
    } else {
        None
    }
}

/// SRAM3 ECC 模式 (CKCR.ECCMOD, 对齐 DDL `SRAM_SetEccMode`)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EccMode {
    /// 无效 (复位默认)
    Invalid = 0,
    /// 模式 1
    Md1 = 1,
    /// 模式 2
    Md2 = 2,
    /// 模式 3
    Md3 = 3,
}

/// 错误上报目标 (对齐 DDL `SRAM_SetExceptionType` 的 checkSram)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckTarget {
    /// 奇偶错误 (SRAMH/1/2/Ret)
    Parity,
    /// ECC 错误 (SRAM3)
    Ecc,
}

/// 错误动作 (对齐 DDL `SRAM_SetExceptionType` 的 type)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultAction {
    /// NMI 中断上报
    Nmi,
    /// 复位
    Reset,
}

/// 设置错误动作 (CKCR.PYOAD/ECCOAD: 0=NMI, 1=复位)
pub fn set_fault_action(target: CheckTarget, action: FaultAction) {
    let bit = match target {
        CheckTarget::Parity => CKCR_PYOAD,
        CheckTarget::Ecc => CKCR_ECCOAD,
    };
    unlock();
    unsafe {
        let v = core::ptr::read_volatile((SRAMC + CKCR) as *const u32);
        core::ptr::write_volatile(
            (SRAMC + CKCR) as *mut u32,
            if action == FaultAction::Reset { v | bit } else { v & !bit },
        );
    }
    lock();
}

/// 设置 SRAM3 ECC 模式 (CKCR.ECCMOD)
pub fn set_ecc_mode(mode: EccMode) {
    unlock();
    unsafe {
        let v = core::ptr::read_volatile((SRAMC + CKCR) as *const u32);
        core::ptr::write_volatile(
            (SRAMC + CKCR) as *mut u32,
            (v & !CKCR_ECCMOD) | ((mode as u32) << 24),
        );
    }
    lock();
}

// ============================== Bank 布局 ==============================

/// SRAMH 基址 (32KB, 高速, 恒 0 等待)
pub const SRAMH_ADDR: u32 = 0x1FFF_8000;
/// SRAM1 基址 (64KB)
pub const SRAM1_ADDR: u32 = 0x2000_0000;
/// SRAM2 基址 (64KB)
pub const SRAM2_ADDR: u32 = 0x2001_0000;
/// SRAM3 基址 (28KB, 栈空间, ECC)
pub const SRAM3_ADDR: u32 = 0x2002_0000;
/// Ret SRAM 基址 (4KB, 低功耗保持)
pub const RETRAM_ADDR: u32 = 0x200F_0000;
