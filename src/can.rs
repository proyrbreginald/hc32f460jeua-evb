//! CAN 控制器驱动 (HC32F460 单 CAN 单元, CAN2.0B + TTCAN)
//!
//! 对齐 DDL v3.3.0 `hc32_ll_can.c/h` 与 `can_loopback` 例程 (寄存器偏移
//! 经 SVD 核对, 位定义与 hc32f460.h 一致)。
//!
//! # 时钟链
//!
//! - CAN 通信时钟 **can_clk = 外部高速晶振 XTAL** (参考手册 30.4.1);
//!   约束: EXCLK (CAN 控制逻辑时钟) ≥ 1.5 × can_clk (本工程
//!   EXCLK=100MHz, XTAL=8MHz ✓)。本模块 [`init`] 自动确保 XTAL 起振
//!   (未运行时经 [`crate::clk::xtal_init`] 启动);
//! - 位时间: TQ = can_clk / (PRESC+1), 位速率 = can_clk /
//!   ((PRESC+1) × (1 + SEG1 + SEG2)), 采样点 = (1+SEG1)/位时间;
//! - 外设时钟门控: FCG1.bit0 (清位使能)。
//!
//! # 引脚 (JEUA 48pin, 数据手册表 2-1/2-2)
//!
//! - **PB7 = CAN_TxD (Func_Grp2, Func50)**, **PB6 = CAN_RxD (Func51)**;
//! - **内部回环模式 (ILB) 无需任何引脚/PHY** —— 自回路测试开箱即用。
//!
//! # 收发模型 (对齐 DDL)
//!
//! - 发送: 选 PTB (TBSEL=0) → 写 TBUF[ID,CTRL,数据] → TCMD.TPE 置位
//!   → 等 RTIF.TPIF → 清除 (RTIF 需对应 RTIE 使能才会置位, init 已
//!   使能全部中断位);
//! - 接收: RCTRL.RSTAT≠0 → 读 RBUF[ID,CTRL,数据] → RCTRL.RREL 释放;
//! - 回环模式下自收帧的 RX.CTRL.TX 位 = 1 (可据此区分自发帧)。
//!
//! # 波特率计算
//!
//! [`bit_timing_for`] 按目标波特率/采样点自动搜索 (PRESC 1~64,
//! 位时间 8~25 TQ, 采样点 ≥50%)。

// 完整 API 供应用按需选用 (STB 发送/中止/诊断), 忽略未使用项
#![allow(dead_code)]

/// CAN 基址 (SVD: 0x4007_0400)
const CAN_BASE: usize = 0x4007_0400;
/// PWC 基址 (FCG1 时钟门控)
const PWC_BASE: usize = 0x4004_8000;

// ---- 寄存器偏移 (SVD 核对) ----
const RBUF: usize = 0x00; // 接收缓冲 (只读: [ID, CTRL, DATA0, DATA1])
const TBUF: usize = 0x50; // 发送缓冲 ([ID, CTRL, DATA0, DATA1])
const CFG_STAT: usize = 0xA0; // 状态/配置 (RESET/LBME/LBMI/TPSS/TSSS)
const TCMD: usize = 0xA1; // 发送命令
const TCTRL: usize = 0xA2; // 发送控制
const RCTRL: usize = 0xA3; // 接收控制
const RTIE: usize = 0xA4; // 中断使能
const RTIF: usize = 0xA5; // 中断标志
const ERRINT: usize = 0xA6; // 错误中断
const LIMIT: usize = 0xA7; // 警告限值
const SBT: usize = 0xA8; // 位时间 (32 位)
const EALCAP: usize = 0xB0; // 仲裁丢失/错误类型
const RECNT: usize = 0xB2; // 接收错误计数
const TECNT: usize = 0xB3; // 发送错误计数
const ACFCTRL: usize = 0xB4; // 验收滤波控制
const ACFEN: usize = 0xB6; // 验收滤波使能
const ACF: usize = 0xB8; // 验收滤波 (ID/掩码, 32 位)

// ---- CFG_STAT 位 ----
const CFG_STAT_RESET: u8 = 1 << 7; // 软件复位 (置位进入本地复位)
const CFG_STAT_LBME: u8 = 1 << 6; // 外部回环
const CFG_STAT_LBMI: u8 = 1 << 5; // 内部回环
const CFG_STAT_TPSS: u8 = 1 << 4; // PTB 单次发送
const CFG_STAT_TSSS: u8 = 1 << 3; // STB 单次发送

// ---- TCMD 位 ----
const TCMD_TBSEL: u8 = 1 << 7; // 发送缓冲选择 (0=PTB, 1=STB)
const TCMD_LOM: u8 = 1 << 6; // 只听模式 (静默)
const TCMD_TPE: u8 = 1 << 4; // PTB 发送请求
const TCMD_TPA: u8 = 1 << 3; // PTB 中止
const TCMD_TSONE: u8 = 1 << 2; // STB 发送一帧
const TCMD_TSALL: u8 = 1 << 1; // STB 发送全部
const TCMD_TSA: u8 = 1 << 0; // STB 中止

// ---- TCTRL 位 ----
const TCTRL_TSNEXT: u8 = 1 << 6; // STB 槽位推进
const TCTRL_TSMODE: u8 = 1 << 5; // STB 按 ID 优先
const TCTRL_TSSTAT: u8 = 0x03; // STB 填充状态

// ---- RCTRL 位 ----
const RCTRL_SACK: u8 = 1 << 7; // 自应答
const RCTRL_ROM: u8 = 1 << 6; // 溢出丢弃新帧
const RCTRL_ROV: u8 = 1 << 5; // 接收溢出
const RCTRL_RREL: u8 = 1 << 4; // 释放接收槽
const RCTRL_RBALL: u8 = 1 << 3; // 接收全部帧 (含错误帧)
const RCTRL_RSTAT: u8 = 0x03; // 接收缓冲状态

// ---- RTIE/RTIF 位 (同布局: 使能/标志) ----
const RTI_RIE: u8 = 1 << 7; // 接收中断/标志
const RTI_ROIE: u8 = 1 << 6; // 接收溢出
const RTI_RFIE: u8 = 1 << 5; // 接收缓冲满
const RTI_RAFIE: u8 = 1 << 4; // 接收近满警告
const RTI_TPIE: u8 = 1 << 3; // PTB 发送完成
const RTI_TSIE: u8 = 1 << 2; // STB 发送完成
const RTI_EIE: u8 = 1 << 1; // 错误警告限值
const RTI_TSFF: u8 = 1 << 0; // 发送缓冲满

// ---- ERRINT 位 ----
const ERRINT_EWARN: u8 = 1 << 7; // 错误计数达限 (只读)
const ERRINT_EPASS: u8 = 1 << 6; // 错误被动节点 (只读)
const ERRINT_EPIE: u8 = 1 << 5; // 被动/主动切换中断使能
const ERRINT_EPIF: u8 = 1 << 4; // 切换中断标志
const ERRINT_ALIE: u8 = 1 << 3; // 仲裁丢失中断使能
const ERRINT_ALIF: u8 = 1 << 2; // 仲裁丢失标志
const ERRINT_BEIE: u8 = 1 << 1; // 总线错误中断使能
const ERRINT_BEIF: u8 = 1 << 0; // 总线错误标志

// ---- LIMIT 位 ----
const LIMIT_AFWL_POS: u8 = 4; // 接收近满警告限值
const LIMIT_EWL: u8 = 0x0F; // 错误警告限值

// ---- SBT 位 ----
const SBT_SEG1_POS: u32 = 0; // [7:0] 段1 (写入值 = 实际-2)
const SBT_SEG2_POS: u32 = 8; // [14:8] 段2 (写入值 = 实际-1)
const SBT_SJW_POS: u32 = 16; // [22:16] 同步跳宽 (写入值 = 实际-1)
const SBT_PRESC_POS: u32 = 24; // [31:24] 预分频 (写入值 = 实际-1)

// ---- ACFCTRL / ACFEN / ACF 位 ----
const ACFCTRL_SELMASK: u8 = 1 << 5; // 写掩码模式
const ACFCTRL_ACFADR: u8 = 0x0F; // 滤波地址
const ACF_ACODE: u32 = 0x1FFF_FFFF; // ID 码/掩码 [28:0]
const ACF_AIDE: u32 = 1 << 29; // 扩展 ID 使能
const ACF_AIDEE: u32 = 1 << 30; // 扩展 ID 使能位

/// 验收滤波类型 (对齐 DDL CAN_ID_TYPE_*)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FilterType {
    /// 接受标准与扩展 ID
    StdExt,
    /// 仅接受标准 ID
    Std,
    /// 仅接受扩展 ID
    Ext,
}

impl FilterType {
    fn acf_bits(self) -> u32 {
        match self {
            FilterType::StdExt => 0,
            FilterType::Std => ACF_AIDEE,
            FilterType::Ext => ACF_AIDEE | ACF_AIDE,
        }
    }
}

/// CAN 工作模式 (对齐 DDL CAN_WORK_MD_*)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorkMode {
    /// 正常模式 (需外部 CAN 收发器与引脚)
    Normal,
    /// 只听模式 (禁止发送)
    Silent,
    /// **内部回环**: 信号不离开芯片, 无需引脚/PHY, 自测试用
    InternalLoopback,
    /// 外部回环: 经收发器环回 (需引脚/PHY)
    ExternalLoopback,
    /// 外部回环 + 只听
    ExternalLoopbackSilent,
}

/// CAN 发送帧 (经典帧, 8 字节载荷)
#[derive(Clone, Copy, Debug)]
pub struct TxFrame {
    /// 帧 ID (11 位标准 / 29 位扩展, 由 `ide` 决定)
    pub id: u32,
    /// true = 扩展帧 (29 位 ID)
    pub ide: bool,
    /// true = 远程帧 (无数据)
    pub rtr: bool,
    /// 数据长度 (0~8)
    pub dlc: u8,
    /// 载荷 (仅 `dlc` 字节有效)
    pub data: [u8; 8],
}

/// CAN 接收帧
#[derive(Clone, Copy, Debug)]
pub struct RxFrame {
    /// 帧 ID
    pub id: u32,
    /// true = 扩展帧
    pub ide: bool,
    /// true = 远程帧
    pub rtr: bool,
    /// 数据长度
    pub dlc: u8,
    /// 载荷
    pub data: [u8; 8],
    /// 回环模式下是否为自发帧
    pub self_tx: bool,
}

/// CAN 初始化配置 (对齐 DDL `stc_can_init_t` 常用字段)
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// 工作模式
    pub mode: WorkMode,
    /// 目标波特率 (bps)
    pub baudrate: u32,
    /// 自应答使能 (回环模式必须开启)
    pub self_ack: bool,
    /// 接收全部帧 (含错误帧)
    pub rx_all_frame: bool,
    /// 验收滤波 ID / 掩码 / 类型
    pub filter_id: u32,
    pub filter_mask: u32,
    pub filter_type: FilterType,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: WorkMode::Normal,
            baudrate: 500_000,
            self_ack: false,
            rx_all_frame: false,
            filter_id: 0,
            filter_mask: 0x1FFF_FFFF, // 全接受
            filter_type: FilterType::StdExt,
        }
    }
}

/// 初始化失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanError {
    /// 波特率不可实现 (无合法位时间组合)
    BaudrateUnsupported,
    /// XTAL 未起振 (CAN 通信时钟源)
    XtalNotReady,
    /// 发送缓冲忙 (PTB 正在发送)
    TxBusy,
    /// 发送超时
    TxTimeout,
    /// 接收超时
    RxTimeout,
}

/// CAN 句柄 (唯一单元, 私有字段保证只能经 [`take`] 构造)
pub struct Can {
    _private: (),
}

impl Can {
    /// 获取 CAN 句柄
    pub fn take() -> Self {
        Self { _private: () }
    }
}

// ---- 寄存器访问 ----

fn read8(offset: usize) -> u8 {
    unsafe { core::ptr::read_volatile((CAN_BASE + offset) as *const u8) }
}

fn write8(offset: usize, value: u8) {
    unsafe { core::ptr::write_volatile((CAN_BASE + offset) as *mut u8, value) };
}

fn modify8(offset: usize, f: impl FnOnce(u8) -> u8) {
    write8(offset, f(read8(offset)));
}

fn read32(offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((CAN_BASE + offset) as *const u32) }
}

fn write32(offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile((CAN_BASE + offset) as *mut u32, value) };
}

/// 使能 CAN 外设时钟 (FCG1.bit0 清位; FCG1 不受 FCG0PC 保护)
fn clock_enable() {
    let fcg1 = unsafe { core::ptr::read_volatile((PWC_BASE + 0x04) as *const u32) };
    unsafe {
        core::ptr::write_volatile((PWC_BASE + 0x04) as *mut u32, fcg1 & !(1 << 0));
    }
}

/// 等待 RTIF 标志置位 (带超时, 按 HCLK 折算 ~10ms)
fn wait_flag(flag: u8, timeout_hclk: u32) -> bool {
    let mut i = 0u32;
    while read8(RTIF) & flag == 0 {
        i += 1;
        if i > timeout_hclk {
            return false;
        }
    }
    true
}

/// 由 (can_clk, 波特率, 采样点百分比) 计算位时间寄存器值
///
/// 搜索 PRESC 1~64: 位时间 = can_clk/(PRESC·波特率) ∈ [8, 25] TQ,
/// 段2 ≥ 1 TQ, 采样点 ≥ 目标值。返回 (SEG1, SEG2, SJW, PRESC) 的
/// **寄存器编码** (实际值-1/-2, 对齐 DDL CAN_Init 的写入)。
pub fn bit_timing_for(
    can_clk: u32,
    baudrate: u32,
    sample_point_pct: u32,
) -> Option<(u32, u32, u32, u32)> {
    for presc in 1..=64u32 {
        let tq = can_clk / presc;
        if tq == 0 {
            continue;
        }
        let bit_time = tq / baudrate;
        if !(8..=25).contains(&bit_time) || tq % baudrate > baudrate / 10 {
            continue; // 位时间需整数且误差 <10%
        }
        // 段2 = 位时间 × (1 - 采样点), 至少 1 TQ; 段1 = 其余
        let seg2 = (bit_time * (100 - sample_point_pct.min(95)) + 50) / 100;
        let seg2 = seg2.max(1).min(bit_time - 2);
        let seg1 = bit_time - 1 - seg2;
        if seg1 < 1 {
            continue;
        }
        let sjw = seg2.min(4);
        return Some((seg1 - 2, seg2 - 1, sjw - 1, presc - 1));
    }
    None
}

// ---- 初始化 ----

/// 初始化 CAN (对齐 DDL CAN_Init + CAN_FilterConfig + CAN_IntCmd(ALL))
///
/// 序列: 确保 XTAL 起振 (CANCLK) → FCG1 时钟使能 → 本地复位 →
/// SBT 位时间 → STB 优先模式 → 验收滤波 → 退出复位 → 工作模式 →
/// 单次发送关 → 警告限值 → RCTRL (自应答/溢出) → 滤波使能 → 中断全开
/// (RTIF 标志需对应 RTIE 使能才会置位, 供轮询)。
pub fn init(cfg: Config) -> Result<(), CanError> {
    // 1. CANCLK = XTAL: 未起振时启动 (无晶振板子 CAN 不可用)
    if !crate::clk::xtal_stable() {
        crate::clk::xtal_init().map_err(|_| CanError::XtalNotReady)?;
    }
    // 2. FCG1.CAN 时钟使能
    clock_enable();

    // 3. 本地复位 (位时间/滤波等仅复位态可写)
    modify8(CFG_STAT, |v| v | CFG_STAT_RESET);

    // 4. 位时间 (SBT, 仅复位态可写)
    let (seg1, seg2, sjw, presc) = bit_timing_for(crate::clk::XTAL_HZ, cfg.baudrate, 75)
        .ok_or(CanError::BaudrateUnsupported)?;
    write32(
        SBT,
        seg1 | (seg2 << SBT_SEG2_POS) | (sjw << SBT_SJW_POS) | (presc << SBT_PRESC_POS),
    );

    // 5. STB 优先模式 = 按顺序 (FIFO)
    modify8(TCTRL, |v| v & !TCTRL_TSMODE);

    // 6. 验收滤波: 滤波1, ID 码 + 掩码 (对齐 CAN_FilterConfig)
    write8(ACFCTRL, 0); // 滤波地址 0
    write32(ACF, cfg.filter_id & ACF_ACODE);
    modify8(ACFCTRL, |v| v | ACFCTRL_SELMASK);
    write32(ACF, (cfg.filter_mask & ACF_ACODE) | cfg.filter_type.acf_bits());

    // 7. 退出本地复位 → 进入 CAN 通信
    modify8(CFG_STAT, |v| v & !CFG_STAT_RESET);

    // 8. 工作模式 (对齐 CAN_SetWorkMode)
    match cfg.mode {
        WorkMode::Normal => {
            modify8(CFG_STAT, |v| v & !(CFG_STAT_LBMI | CFG_STAT_LBME));
            modify8(TCMD, |v| v & !TCMD_LOM);
        }
        WorkMode::Silent => modify8(TCMD, |v| v | TCMD_LOM),
        WorkMode::InternalLoopback => modify8(CFG_STAT, |v| (v | CFG_STAT_LBMI) & !CFG_STAT_LBME),
        WorkMode::ExternalLoopback => modify8(CFG_STAT, |v| (v | CFG_STAT_LBME) & !CFG_STAT_LBMI),
        WorkMode::ExternalLoopbackSilent => {
            modify8(CFG_STAT, |v| (v | CFG_STAT_LBME) & !CFG_STAT_LBMI);
            modify8(TCMD, |v| v | TCMD_LOM);
        }
    }

    // 9. 单次发送关 (自动重发, 对齐 DDL StructInit 默认)
    modify8(CFG_STAT, |v| v & !(CFG_STAT_TPSS | CFG_STAT_TSSS));

    // 10. 警告限值: 接收近满 = 3, 错误警告 = 7 (默认)
    write8(LIMIT, (3 << LIMIT_AFWL_POS) | 7);

    // 11. RCTRL: 溢出丢弃新帧 + 自应答 (回环必需)
    let mut rctrl = if cfg.rx_all_frame { RCTRL_RBALL } else { 0 };
    rctrl |= RCTRL_ROM; // 溢出丢弃新帧
    if cfg.self_ack {
        rctrl |= RCTRL_SACK;
    }
    write8(RCTRL, rctrl);

    // 12. 使能验收滤波 1
    write8(ACFEN, 0x01);

    // 13. 中断全开 (RTIF 标志依赖 RTIE 使能; 对齐例程 CAN_IntCmd(ALL))
    write8(
        RTIE,
        RTI_RIE | RTI_ROIE | RTI_RFIE | RTI_RAFIE | RTI_TPIE | RTI_TSIE | RTI_EIE,
    );
    write8(
        ERRINT,
        ERRINT_BEIE | ERRINT_ALIE | ERRINT_EPIE,
    );

    Ok(())
}

/// 进入本地复位 (软件复位, 对齐 CAN_EnterLocalReset)
pub fn local_reset() {
    modify8(CFG_STAT, |v| v | CFG_STAT_RESET);
}

/// 退出本地复位
pub fn exit_local_reset() {
    modify8(CFG_STAT, |v| v & !CFG_STAT_RESET);
}

// ---- 发送 ----

/// 发送一帧 (PTB, 轮询等待完成, 对齐例程 CanTx)
///
/// 超时按 HCLK 折算 ~10ms (回环模式一帧 <1ms)。
pub fn send(frame: &TxFrame) -> Result<(), CanError> {
    // PTB 忙检查 (对齐 CAN_FillTxFrame 的 LL_ERR_BUSY 判定)
    if read8(TCMD) & TCMD_TPE != 0 {
        return Err(CanError::TxBusy);
    }

    // 选择 PTB (TBSEL=0) 并写入缓冲 [ID, CTRL, DATA0, DATA1]
    // CTRL 位布局 (对齐 stc_can_tx_frame_t): DLC[3:0], BRS@4, FDF@5,
    // RTR@6, IDE@7 (经典帧 FDF=0, BRS=0)
    modify8(TCMD, |v| v & !TCMD_TBSEL);
    let ctrl = (frame.dlc as u32 & 0x0F)
        | if frame.ide { 1 << 7 } else { 0 }
        | if frame.rtr { 1 << 6 } else { 0 };
    let w0 = u32::from_le_bytes([frame.data[0], frame.data[1], frame.data[2], frame.data[3]]);
    let w1 = u32::from_le_bytes([frame.data[4], frame.data[5], frame.data[6], frame.data[7]]);
    write32(TBUF, frame.id);
    write32(TBUF + 4, ctrl);
    write32(TBUF + 8, w0);
    write32(TBUF + 12, w1);

    // 启动 PTB 发送
    modify8(TCMD, |v| v | TCMD_TPE);

    // 等待发送完成 (RTIF.TPIF, 对齐例程 while(CAN_FLAG_PTB_TX==RESET))
    if !wait_flag(RTI_TPIE, crate::clk::hclk_hz() / 100) {
        return Err(CanError::TxTimeout);
    }
    write8(RTIF, RTI_TPIE); // 清标志
    Ok(())
}

// ---- 接收 ----

/// 读取一帧 (非阻塞, 对齐 CAN_GetRxFrame: RSTAT≠0 → 读 RBUF → RREL)
pub fn recv() -> Option<RxFrame> {
    if read8(RCTRL) & RCTRL_RSTAT == 0 {
        return None;
    }
    let id = read32(RBUF);
    let ctrl = read32(RBUF + 4);
    let w0 = read32(RBUF + 8);
    let w1 = read32(RBUF + 12);

    // CTRL 位布局 (对齐 stc_can_rx_frame_t): DLC[3:0], BRS@4, FDF@5,
    // RTR@6, IDE@7, TX@12 (回环自发帧)
    let ide = ctrl & (1 << 7) != 0;
    let rtr = ctrl & (1 << 6) != 0;
    let dlc = (ctrl & 0x0F).min(8) as u8;
    let id = if ide { id & 0x1FFF_FFFF } else { id & 0x7FF };

    let mut data = [0u8; 8];
    data[0..4].copy_from_slice(&w0.to_le_bytes());
    data[4..8].copy_from_slice(&w1.to_le_bytes());

    // 释放当前接收槽 (指向下一槽)
    modify8(RCTRL, |v| v | RCTRL_RREL);

    Some(RxFrame {
        id,
        ide,
        rtr,
        dlc,
        data,
        self_tx: ctrl & (1 << 12) != 0, // CTRL.TX (回环自发帧)
    })
}

/// 阻塞读取一帧 (带超时, 按 HCLK 折算 ~10ms)
pub fn recv_timeout() -> Result<RxFrame, CanError> {
    let mut i = 0u32;
    let timeout = crate::clk::hclk_hz() / 100;
    loop {
        if let Some(f) = recv() {
            return Ok(f);
        }
        i += 1;
        if i > timeout {
            return Err(CanError::RxTimeout);
        }
    }
}

// ---- 状态/诊断 ----

/// 读取指定状态标志 (RTIF 位, 对齐 CAN_GetStatus)
pub fn status(flag: u8) -> bool {
    read8(RTIF) & flag != 0
}

/// 清除指定状态标志 (写 RTIF, 对齐 CAN_ClearStatus)
pub fn clear_status(flags: u8) {
    write8(RTIF, flags);
}

/// 接收/发送错误计数 (RECNT/TECNT)
pub fn error_counts() -> (u8, u8) {
    (read8(RECNT), read8(TECNT))
}

/// 总线关闭状态 (CFG_STAT.BUSOFF)
pub fn bus_off() -> bool {
    read8(CFG_STAT) & 0x01 != 0
}

/// 接收缓冲状态 (RCTRL.RSTAT: 0=空, 1=非空, 2=近满, 3=满)
pub fn rx_buf_status() -> u8 {
    read8(RCTRL) & RCTRL_RSTAT
}
