//! DMA 驱动 (HC32F460 DMA1/DMA2, 每单元 4 通道)
//!
//! 参考 DDL v3.3.0 `hc32_ll_dma.c/h` 与示例 `dmac_base` / `usart_uart_dma`。
//!
//! # 硬件模型
//!
//! ```text
//! 外设事件 (如 USART1_TI = 280)
//!   └→ AOS.DMAx_TRGSELy 写入事件源编号 (AOS 基址 0x4001_0800)
//!       └→ DMA{1|2} 通道 {0..3} 触发传输
//!           ├→ 每请求移动一个"块" (BLKSIZE ≤ 1024 项)
//!           └→ 计数 (CNT ≤ 65535 次) 归零 → TC 标志 + 通道自动失能
//! ```
//!
//! - **单元**: DMA1 @ 0x4005_3000, DMA2 @ 0x4005_3400, 各含 4 通道;
//! - **通道块**: 每通道 0x40 字节寄存器组 (SAR/DAR/DTCTL/RPT/SNSEQ/DNSEQ/
//!   LLP/CHCTL + 只读 MON 影子寄存器);
//! - **软件触发**: 内存→内存搬运经 `SWREQ` (解锁键 0xA1) 发起, 无需外设;
//! - **外设触发**: 事件源经 AOS TRGSEL 路由 (与 INTC 事件源编号同空间,
//!   如 USART1_TI=280); 支持 LLC 重配置 (reconfig) 实现循环接收;
//! - **MPU**: DMA 是独立总线主设备, **MPU 限制 (SRAM XN 等) 不作用于
//!   DMA**; SRAM 无缓存, Flash 经 EFM 缓存读取对 DMA 一致, 无脏数据问题;
//! - **FCG0**: DMA1/DMA2/AOS 时钟门控位 (bit14/15/17), FCG0 受写保护,
//!   需先经 FCG0PC 键 0xA5A5 解锁 (与 clk 模块一致)。
//!
//! # Rust 设计
//!
//! - **单元/通道编码在类型中**: `Dma<UNIT, CH>` (const 泛型), 越界编译期
//!   报错; 便捷别名 [`Dma1Ch0`] 等;
//! - **单例占用**: [`Dma::take`] 一次性获取通道所有权 (全系统位图),
//!   防止同一通道被两个功能重复配置; 底层 API 另提供 [`Dma::new`];
//! - **安全边界**: `&[u8]`/`&mut [u8]` 不直接参与传输 —— 传输用裸地址
//!   (外设/缓冲区均可), 调用方负责缓冲区长生命周期; 中断回调通过原子
//!   槽位安装 (与 uart 模块的 RxNotifySlot 同模式), ISR 内只做清标志 +
//!   通知;
//! - **等待其他通道**: 硬件要求在**修改 CHEN 时不得有其他通道传输中**
//!   (对齐 DDL `DMA_ChCmd` 的忙等待), 超时返回 [`DmaError::ChannelBusy`];
//! - **回读确认**: 地址/计数写入后经 MON 影子寄存器回读确认 (对齐 DDL
//!   `DMA_SetSrcAddr` 等, 防总线写入被吞)。
//!
//! # 当前系统集成
//!
//! 1. **控制台 UART 发送卸载** ([`uart_tx_try`], DMA2/CH0, `CFG_DMA_*`):
//!    长输出 (zmodem sz / `cat` / soak 报告 / 日志) 期间 CPU 不再逐字节
//!    轮询 TXE;
//! 2. **Flash→RAM 大块读取加速** ([`copy_try`], DMA1/CH0): 文件系统读 /
//!    擦除校验 / 写后回读从逐字节/逐字循环改为 DMA 整块拷贝
//!    (Flash 带等待周期, DMA 比逐字节循环快约一个数量级)。
//!
//! 预留能力 (供未来使用): 外设→内存 (RX) 路由 [`route`] + LLC 重配置
//! [`reconfig_llp`] / [`reconfig_cmd`] / [`sw_reconfig`] 可实现循环接收;
//! 内存→内存 [`Dma::copy_blocking`] 可加速大块拷贝。
//!
//! # 暂不采用 DMA 的位置 (设计说明)
//!
//! - **UART 接收 (RX)**: 每字节中断在 115200 下约 <5% CPU; 未知帧长需
//!   RTO 超时 + reconfig 循环模式 (对齐 DDL usart_uart_dma 示例), 会
//!   重写控制台/rz 的输入路径, 收益远小于风险。需要时可直接用
//!   [`route`] + [`reconfig_llp`] 搭建;
//! - **Flash 编程/擦除 (EFM)**: 编程是"写 Flash 地址触发 + 逐字等待
//!   OPTEND", bus-hold 期间 DMA 同样被 stall, DMA 无法替代逐字流程;
//! - **CAN**: HC32F460 的 CAN 无 DMA 请求线 (外设自身带收发 FIFO)。
//!
//! 常量与 API 供应用按需选用, 忽略未使用项的死代码警告。
#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use crate::mmio::Reg;
use crate::notify::NotifySlot;

// ============================== 基址 / 偏移 ==============================

const DMA1_BASE: usize = 0x4005_3000;
const DMA2_BASE: usize = 0x4005_3400;
const AOS_BASE: usize = 0x4001_0800;
/// FCG0 时钟门控位 (清位 = 使能; 对齐 DDL PWC_FCG0_*; 写保护解锁见
/// [`crate::clk::fcg0_enable`])
const FCG0_DMA1: u32 = 1 << 14;
const FCG0_DMA2: u32 = 1 << 15;
const FCG0_AOS: u32 = 1 << 17;

/// 单元基址 (编译期校验)
const fn base(unit: u8) -> usize {
    match unit {
        1 => DMA1_BASE,
        2 => DMA2_BASE,
        _ => panic!("DMA 单元必须为 1~2"),
    }
}

/// 通道寄存器块基址 (每通道 0x40 字节, 对齐 DDL DMA_CH_REG)
const fn ch_base(unit: u8, ch: u8) -> usize {
    base(unit) + 0x40 + 0x40 * ch as usize
}

// ---- 单元全局寄存器偏移 (CM_DMA_TypeDef) ----
const EN: usize = 0x00; // 单元使能 (bit0)
const INTSTAT0: usize = 0x04; // 错误标志: TRNERR[3:0] | REQERR[19:16]
const INTSTAT1: usize = 0x08; // 完成标志: TC[3:0] | BTC[19:16]
const INTMASK0: usize = 0x0C; // 错误中断屏蔽 (1 = 屏蔽): MSKTRNERR[3:0] | MSKREQERR[19:16]
const INTMASK1: usize = 0x10; // 完成中断屏蔽 (1 = 屏蔽): MSKTC[3:0] | MSKBTC[19:16]
const INTCLR0: usize = 0x14; // 错误标志清除 (写 1): CLRTRNERR[3:0] | CLRREQERR[19:16]
const INTCLR1: usize = 0x18; // 完成标志清除 (写 1): CLRTC[3:0] | CLRBTC[19:16]
const CHEN: usize = 0x1C; // 通道使能 (CHEN[3:0], 传输完成自动清位)
const CHSTAT: usize = 0x24; // 状态: DMAACT[0] | RCFGACT[1] | CHACT[19:16]
const RCFGCTL: usize = 0x2C; // 重配置: RCFGEN[0] | RCFGLLP[1] | RCFGCHS[11:8] | SARMD[17:16] | DARMD[19:18] | CNTMD[21:20]
const SWREQ: usize = 0x30; // 软件触发 (带解锁键)

// ---- 通道寄存器偏移 (相对通道块基址) ----
const SAR: usize = 0x00;
const DAR: usize = 0x04;
const DTCTL: usize = 0x08; // BLKSIZE[9:0] | CNT[31:16]
const RPT: usize = 0x0C; // SRPT[9:0] | DRPT[25:16]
const LLP: usize = 0x18; // 链表指针 LLP[31:2]
const CHCTL: usize = 0x1C;
const MONSAR: usize = 0x20; // 影子寄存器 (只读, 回读确认用)
const MONDAR: usize = 0x24;
const MONDTCTL: usize = 0x28;

// ---- CHCTL 位域 (对齐 DDL DMA_CHCTL_*) ----
const CHCTL_SINC: u32 = 0x3; // [1:0]: 0=固定 1=递增 2=递减
const CHCTL_DINC: u32 = 0x3 << 2; // [3:2]
const CHCTL_SRPTEN: u32 = 1 << 4;
const CHCTL_DRPTEN: u32 = 1 << 5;
const CHCTL_SNSEQEN: u32 = 1 << 6;
const CHCTL_DNSEQEN: u32 = 1 << 7;
const CHCTL_HSIZE: u32 = 0x3 << 8; // [9:8]: 0=8bit 1=16bit 2=32bit
const CHCTL_LLPEN: u32 = 1 << 10;
const CHCTL_LLPRUN: u32 = 1 << 11;
const CHCTL_IE: u32 = 1 << 12;

// ---- DTCTL / RPT 位域 ----
const DTCTL_BLKSIZE: u32 = 0x3FF; // 0 编码 1024
const DTCTL_CNT: u32 = 0xFFFF << 16;
const RPT_SRPT: u32 = 0x3FF;
const RPT_DRPT: u32 = 0x3FF << 16;

// ---- 中断标志/屏蔽/清除位 (INTSTAT/INTMASK/INTCLR, 每通道 1 位) ----
const ERR_TRNERR: u32 = 0xF; // TRNERR 位于位 0~3
const ERR_REQERR: u32 = 0xF << 16; // REQERR 位于位 16~19
const TC_TC: u32 = 0xF; // TC 位于位 0~3
const TC_BTC: u32 = 0xF << 16;

// ---- CHSTAT ----
const CHSTAT_CHACT: u32 = 0xF << 16; // 每通道 1 位 (传输中)
const CHSTAT_RCFGACT: u32 = 1 << 1;

// ---- SWREQ ----
const SWREQ_KEY_POS: u32 = 16; // SWREQWP[23:16] 解锁键 0xA1
const SWREQ_KEY: u32 = 0xA1;
const SWRCFG_KEY_POS: u32 = 24; // SWRCFGWP[31:24] 解锁键 0xA2
const SWRCFG_KEY: u32 = 0xA2;
const SWREQ_SWRCFGREQ: u32 = 1 << 15;

// ---- RCFGCTL ----
const RCFGCTL_RCFGEN: u32 = 1;
const RCFGCTL_RCFGLLP: u32 = 1 << 1;
const RCFGCTL_RCFGCHS_POS: u32 = 8;
const RCFGCTL_SARMD_POS: u32 = 16;
const RCFGCTL_DARMD_POS: u32 = 18;
const RCFGCTL_CNTMD_POS: u32 = 20;

// ---- LLP ----
const LLP_MASK: u32 = 0xFFFF_FFFC; // 地址按字对齐

// ---- AOS TRGSEL (CM_AOS_TypeDef, SEL[8:0], 0x1FF = 未映射) ----
const AOS_DMA1_TRGSEL0: usize = 0x14; // DMA1 通道 0~3: +4·CH
const AOS_DMA2_TRGSEL0: usize = 0x24; // DMA2 通道 0~3: +4·CH
const AOS_DMA_RC_TRGSEL: usize = 0x34; // 重配置触发选择
const AOS_INTSFTTRG: usize = 0x00; // 软件触发寄存器 (写 1 = 触发 AOS_STRG 事件)
const AOS_TRIG_SEL_MASK: u32 = 0x1FF;

/// 外设事件源编号 (与 INTC 事件源同一编号空间, 见 [`crate::intc::src`])
pub mod event {
    /// USARTn 接收数据寄存器非空事件 (对齐 DDL EVT_SRC_USARTn_RI)
    pub const fn usart_ri(unit: u8) -> u32 {
        279 + (unit as u32 - 1) * 5
    }
    /// USARTn 发送数据寄存器空事件 (对齐 DDL EVT_SRC_USARTn_TI)
    pub const fn usart_ti(unit: u8) -> u32 {
        280 + (unit as u32 - 1) * 5
    }
    /// AOS 软件触发事件 (EVT_SRC_AOS_STRG)
    pub const AOS_STRG: u32 = 319;
}

// ============================== 通道占用表 ==============================

/// 全系统通道占用位图: bit = (UNIT-1)·4 + CH
static CHANNELS_TAKEN: AtomicU32 = AtomicU32::new(0);

const fn slot(unit: u8, ch: u8) -> usize {
    (unit as usize - 1) * 4 + ch as usize
}

// ============================== 配置类型 ==============================

/// 传输数据宽度 (对齐 DDL DMA_DATAWIDTH_*)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// 8 位
    B8,
    /// 16 位 (地址须半字对齐)
    B16,
    /// 32 位 (地址须字对齐)
    B32,
}

impl Width {
    fn bits(self) -> u32 {
        match self {
            Width::B8 => 0,
            Width::B16 => 1,
            Width::B32 => 2,
        }
    }

    /// 单次传输字节数
    const fn bytes(self) -> usize {
        match self {
            Width::B8 => 1,
            Width::B16 => 2,
            Width::B32 => 4,
        }
    }
}

/// 地址递增模式 (对齐 DDL DMA_SRC_ADDR_* / DMA_DEST_ADDR_*)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddrMode {
    /// 地址固定 (外设数据寄存器)
    Fix,
    /// 地址递增 (缓冲区)
    Inc,
    /// 地址递减
    Dec,
}

impl AddrMode {
    fn bits(self) -> u32 {
        match self {
            AddrMode::Fix => 0,
            AddrMode::Inc => 1,
            AddrMode::Dec => 2,
        }
    }
}

/// DMA 通道基本配置 (对齐 DDL `stc_dma_init_t` + 中断开关)
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// 传输数据宽度
    pub width: Width,
    /// 源地址 (外设寄存器地址或缓冲区地址)
    pub src_addr: usize,
    /// 目的地址
    pub dest_addr: usize,
    /// 块大小 (每次请求传输的项数; 0 编码 1024, 1~1024 合法)
    pub block_size: u16,
    /// 传输计数 (总**请求次数**; 每次请求搬运 `block_size` 项, 计数归零
    /// 置 TC 并自动失能通道; 0 = 无限, 1~65535)
    pub trans_count: u16,
    /// 源地址递增模式
    pub src_inc: AddrMode,
    /// 目的地址递增模式
    pub dest_inc: AddrMode,
    /// 使能传输完成 (TC) 中断 (需配合 [`Dma::install_tc_irq`])
    pub int_tc: bool,
    /// 使能错误中断 (需配合 [`Dma::install_err_irq`])
    pub int_err: bool,
}

impl Default for Config {
    /// 对齐 DDL `DMA_StructInit`: 8 位 / 地址 0 / 块 1 / 计数 0 / 固定地址
    fn default() -> Self {
        Self {
            width: Width::B8,
            src_addr: 0,
            dest_addr: 0,
            block_size: 1,
            trans_count: 0,
            src_inc: AddrMode::Fix,
            dest_inc: AddrMode::Fix,
            int_tc: false,
            int_err: false,
        }
    }
}

/// DMA 操作失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaError {
    /// 通道已被其他功能占用 ([`Dma::take`] 失败)
    Taken,
    /// 修改 CHEN 时其他通道仍传输中 (硬件约束, 超时; 对齐 DDL `DMA_ChCmd`)
    ChannelBusy,
    /// 地址与数据宽度不对齐 (16 位须半字, 32 位须字对齐)
    BadAlign,
    /// 寄存器写入回读确认超时 (对齐 DDL `DMA_SetSrcAddr` 等的回读)
    WriteTimeout,
    /// 等待传输完成超时
    Timeout,
    /// DMA 上报传输/请求错误 (INTSTAT0)
    TransferError,
    /// 源与目的切片长度不同。
    LengthMismatch,
}

/// 中断回调 (中断上下文执行, 必须有界、无阻塞)
pub type Callback = crate::notify::Callback;

// ============================== 核心类型 ==============================

/// DMA 通道句柄: 单元 `UNIT` (1~2) 与通道 `CH` (0~3) 编码在类型中。
///
/// 使用便捷别名 [`Dma1Ch0`] ~ [`Dma2Ch3`]。推荐经 [`Dma::take`] 获取
/// (一次性所有权); 高级用途可用 [`Dma::new`] (不检查占用)。
pub struct Dma<const UNIT: u8, const CH: u8> {
    _private: (),
}

/// DMA1 通道 0 便捷别名
pub type Dma1Ch0 = Dma<1, 0>;
/// DMA1 通道 1 便捷别名
pub type Dma1Ch1 = Dma<1, 1>;
/// DMA1 通道 2 便捷别名
pub type Dma1Ch2 = Dma<1, 2>;
/// DMA1 通道 3 便捷别名
pub type Dma1Ch3 = Dma<1, 3>;
/// DMA2 通道 0 便捷别名
pub type Dma2Ch0 = Dma<2, 0>;
/// DMA2 通道 1 便捷别名
pub type Dma2Ch1 = Dma<2, 1>;
/// DMA2 通道 2 便捷别名
pub type Dma2Ch2 = Dma<2, 2>;
/// DMA2 通道 3 便捷别名
pub type Dma2Ch3 = Dma<2, 3>;

impl<const UNIT: u8, const CH: u8> Dma<UNIT, CH> {
    /// 构造通道句柄 (不检查占用)。`UNIT`/`CH` 越界时:
    /// 以 const 方式使用会在编译期报错。
    pub(crate) const fn new() -> Self {
        assert!(UNIT == 1 || UNIT == 2, "DMA 单元必须为 1~2");
        assert!(CH <= 3, "DMA 通道必须为 0~3");
        Self { _private: () }
    }

    /// 获取通道所有权 (全系统唯一, 防止同一通道被重复配置)。
    ///
    /// 一次调用成功, 后续调用返回 `None` (除非整系统复位)。
    pub(crate) fn take() -> Option<Self> {
        let bit = 1 << slot(UNIT, CH);
        if CHANNELS_TAKEN.fetch_or(bit, Ordering::AcqRel) & bit == 0 {
            Some(Self::new())
        } else {
            None
        }
    }

    /// 单元基址 (UNIT 已由 new 断言)
    fn base() -> usize {
        base(UNIT)
    }

    /// 通道寄存器块基址
    fn ch_base() -> usize {
        ch_base(UNIT, CH)
    }

    fn unit_reg(&self, offset: usize) -> Reg {
        Reg::new(Self::base() + offset)
    }

    fn ch_reg(&self, offset: usize) -> Reg {
        Reg::new(Self::ch_base() + offset)
    }

    // ============================== 全局控制 ==============================

    /// 使能/失能整个 DMA 单元 (EN, 对齐 DDL `DMA_Cmd`)。
    ///
    /// 时钟使能 (FCG0) 由 [`crate::dma::init`] 完成; 单元使能后通道
    /// 才能响应触发。
    pub fn cmd(&self, enable: bool) {
        self.unit_reg(EN).write(enable as u32);
    }

    // ============================== 通道配置 ==============================

    /// 基本配置 (对齐 DDL `DMA_Init`): 写 SAR/DAR/DTCTL + CHCTL 位域。
    ///
    /// - 16 位宽度要求地址半字对齐, 32 位要求字对齐 (对齐 DDL
    ///   `IS_DMA_DATA_WIDTH_ADDR` 断言);
    /// - 未使能的完成/错误中断在配置时**屏蔽** (INTMASK 置位), 由
    ///   [`Dma::install_tc_irq`] / [`Dma::install_err_irq`] 取消屏蔽;
    /// - 应在通道失能状态调用。
    pub(crate) unsafe fn configure(&self, cfg: &Config) -> Result<(), DmaError> {
        let align = |addr: usize| -> bool {
            match cfg.width {
                Width::B8 => true,
                Width::B16 => addr.is_multiple_of(2),
                Width::B32 => addr.is_multiple_of(4),
            }
        };
        if !align(cfg.src_addr) || !align(cfg.dest_addr) {
            return Err(DmaError::BadAlign);
        }

        self.ch_reg(SAR).write(cfg.src_addr as u32);
        self.ch_reg(DAR).write(cfg.dest_addr as u32);
        // DTCTL: BLKSIZE[9:0] | CNT[31:16] (0 编码 1024, 与 DDL 一致)
        self.ch_reg(DTCTL)
            .write((cfg.block_size as u32 & DTCTL_BLKSIZE) | ((cfg.trans_count as u32) << 16));
        // CHCTL: 只改 SINC/DINC/HSIZE/IE, 保留 LLP/重复/非序列位
        self.ch_reg(CHCTL).modify(|v| {
            (v & !(CHCTL_SINC | CHCTL_DINC | CHCTL_HSIZE | CHCTL_IE))
                | cfg.src_inc.bits()
                | (cfg.dest_inc.bits() << 2)
                | (cfg.width.bits() << 8)
                | if cfg.int_tc { CHCTL_IE } else { 0 }
        });

        // 中断屏蔽 (1 = 屏蔽): 本通道的 TC/BTC/TRNERR/REQERR 位统一按
        // 配置置位/清除 (BTC 不使用, 保持屏蔽, 防止未注册的 BTC 中断
        // 进入向量表默认处理)
        let tc_bits = (1 << CH) | (1 << (16 + CH));
        if cfg.int_tc {
            self.unit_reg(INTMASK1).modify(|v| v & !tc_bits);
        } else {
            self.unit_reg(INTMASK1).modify(|v| v | tc_bits);
        }
        let err_bits = (1 << CH) | (1 << (16 + CH));
        if cfg.int_err {
            self.unit_reg(INTMASK0).modify(|v| v & !err_bits);
        } else {
            self.unit_reg(INTMASK0).modify(|v| v | err_bits);
        }
        Ok(())
    }

    /// 设置源地址 (回读 MONSAR 确认, 对齐 DDL `DMA_SetSrcAddr`)。
    pub(crate) unsafe fn set_src_addr(&self, addr: usize) -> Result<(), DmaError> {
        self.write_verify(self.ch_reg(SAR), self.ch_reg(MONSAR), addr as u32, u32::MAX)
    }

    /// 设置目的地址 (回读 MONDAR 确认, 对齐 DDL `DMA_SetDestAddr`)。
    pub(crate) unsafe fn set_dest_addr(&self, addr: usize) -> Result<(), DmaError> {
        self.write_verify(self.ch_reg(DAR), self.ch_reg(MONDAR), addr as u32, u32::MAX)
    }

    /// 设置传输计数 (回读 MONDTCTL 确认; 0 = 无限, 1~65535)。
    pub fn set_trans_count(&self, count: u16) -> Result<(), DmaError> {
        let dtctl = self.ch_reg(DTCTL);
        let mon = self.ch_reg(MONDTCTL);
        for _ in 0..READBACK_RETRIES {
            dtctl.modify(|v| (v & !DTCTL_CNT) | ((count as u32) << 16));
            if (mon.read() & DTCTL_CNT) >> 16 == count as u32 {
                return Ok(());
            }
        }
        Err(DmaError::WriteTimeout)
    }

    /// 设置块大小 (回读 MONDTCTL 确认; 0 编码 1024)。
    pub fn set_block_size(&self, size: u16) -> Result<(), DmaError> {
        let dtctl = self.ch_reg(DTCTL);
        let mon = self.ch_reg(MONDTCTL);
        let value = size as u32 & DTCTL_BLKSIZE;
        for _ in 0..READBACK_RETRIES {
            dtctl.modify(|v| (v & !DTCTL_BLKSIZE) | value);
            if mon.read() & DTCTL_BLKSIZE == value {
                return Ok(());
            }
        }
        Err(DmaError::WriteTimeout)
    }

    /// 设置数据宽度 (对齐 DDL `DMA_SetDataWidth`)
    pub fn set_width(&self, width: Width) {
        self.ch_reg(CHCTL)
            .modify(|v| (v & !CHCTL_HSIZE) | (width.bits() << 8));
    }

    /// 通用回读确认: 写入 `reg`, 直到 `mon` 读回等于期望值 (带超时)。
    fn write_verify(&self, reg: Reg, mon: Reg, value: u32, mask: u32) -> Result<(), DmaError> {
        reg.write(value);
        for _ in 0..READBACK_RETRIES {
            if mon.read() & mask == value & mask {
                return Ok(());
            }
            reg.write(value);
        }
        Err(DmaError::WriteTimeout)
    }

    /// 链表指针 (LLP) 使能: `desc_addr` 指向描述符 (须字对齐), 传输完成
    /// 后按 `auto_run` 自动继续 (`LLPRUN`) 或等待下次请求 (`WAIT`)。
    ///
    /// 描述符为 8×u32 数组 (SAR/DAR/DTCTL/RPT/SNSEQ/DNSEQ/LLP/CHCTL,
    /// 对齐 DDL `stc_dma_llp_descriptor_t`), 由调用方持有。
    pub(crate) unsafe fn llp_enable(
        &self,
        desc_addr: usize,
        auto_run: bool,
    ) -> Result<(), DmaError> {
        if !desc_addr.is_multiple_of(4) {
            return Err(DmaError::BadAlign);
        }
        self.ch_reg(LLP).write((desc_addr as u32) & LLP_MASK);
        self.ch_reg(CHCTL)
            .modify(|v| v | CHCTL_LLPEN | if auto_run { CHCTL_LLPRUN } else { 0 });
        Ok(())
    }

    /// 失能链表指针功能。
    pub fn llp_disable(&self) {
        self.ch_reg(CHCTL).modify(|v| v & !CHCTL_LLPEN);
    }

    /// 更新链表指针地址 (运行中可改写)。
    pub(crate) unsafe fn set_llp_addr(&self, desc_addr: usize) {
        self.ch_reg(LLP).write((desc_addr as u32) & LLP_MASK);
    }

    /// 重配置功能: 使能/失能本通道的 LLP 重配置 (`RCFGCTL.RCFGCHS` +
    /// `RCFGLLP`, 对齐 DDL `DMA_ReconfigLlpCmd`)。
    ///
    /// 循环接收流程 (对齐 DDL 示例 usart_uart_dma):
    /// 配置描述符 (LLP 自指) → [`Self::reconfig_llp`](true) →
    /// [`Self::reconfig_cmd`](true) → AOS 把重配置请求路由到
    /// [`event::AOS_STRG`] → 帧结束/超时后 [`Self::sw_reconfig`] 把
    /// SAR/DAR/DTCTL 等从描述符重载, 通道回到初始状态继续接收。
    pub fn reconfig_llp(&self, enable: bool) {
        self.unit_reg(RCFGCTL).modify(|v| {
            (v & !(RCFGCTL_RCFGLLP | (0xF << RCFGCTL_RCFGCHS_POS)))
                | ((CH as u32) << RCFGCTL_RCFGCHS_POS)
                | if enable { RCFGCTL_RCFGLLP } else { 0 }
        });
    }

    /// 使能/失能重配置功能 (`RCFGCTL.RCFGEN`, 对齐 DDL `DMA_ReconfigCmd`)。
    pub fn reconfig_cmd(&self, enable: bool) {
        self.unit_reg(RCFGCTL).modify(|v| {
            if enable {
                v | RCFGCTL_RCFGEN
            } else {
                v & !RCFGCTL_RCFGEN
            }
        });
    }

    // ============================== 触发与使能 ==============================

    /// 软件触发本通道 (SWREQ 带解锁键 0xA1, 对齐 DDL `DMA_MxChSWTrigger`)。
    /// 用于内存→内存搬运或调试验证。
    pub fn sw_trigger(&self) {
        self.unit_reg(SWREQ)
            .write((SWREQ_KEY << SWREQ_KEY_POS) | (1 << CH));
    }

    /// 软件触发重配置 (SWREQ 带解锁键 0xA2, 对齐 DDL `DMA_SWReconfig`)。
    pub fn sw_reconfig(&self) {
        self.unit_reg(SWREQ)
            .write((SWRCFG_KEY << SWRCFG_KEY_POS) | SWREQ_SWRCFGREQ);
    }

    /// 使能本通道 (CHEN 置位)。
    ///
    /// 硬件约束 (对齐 DDL `DMA_ChCmd`): 写 CHEN 前必须等待**其他通道**
    /// 传输结束, 否则写可能被硬件忽略; 超时返回 [`DmaError::ChannelBusy`]。
    /// 应在通道已完成上次传输 (或已 disable) 后调用。
    pub fn enable(&self) -> Result<(), DmaError> {
        self.wait_other_channels_idle()?;
        self.unit_reg(CHEN)
            .write(self.unit_reg(CHEN).read() | (1 << CH));
        Ok(())
    }

    /// 失能本通道 (CHEN 清位)。
    pub fn disable(&self) -> Result<(), DmaError> {
        self.wait_other_channels_idle()?;
        self.unit_reg(CHEN)
            .write(self.unit_reg(CHEN).read() & !(1 << CH));
        Ok(())
    }

    /// 确认通道停止后才返回。
    ///
    /// 安全切片 API 的异常路径必须满足此后置条件，否则 DMA 仍可能在借用
    /// 结束后访问缓冲区。硬件无法停止时宁可停留在恢复路径，也不能返回。
    fn stop_blocking(&self) {
        while self.is_enabled() || self.is_busy() {
            let _ = self.disable();
        }
    }

    /// 等待其他通道传输结束 (CHEN 写前置条件, 2ms 墙钟超时)。
    ///
    /// 判定源为 **CHEN 使能位** (对齐 DDL `DMA_ChCmd`: 写 CHEN 前等待
    /// 其他通道的 CHEN 位清零)。通道"已使能但等待外设触发"时
    /// `CHSTAT.CHACT` 可能为 0 而 CHEN 仍置位 —— 按 CHACT 判定会放行
    /// 被硬件忽略的写入。超时时钟经 DWT 周期计数 (与 RTOS 节拍解耦,
    /// 调度器启动前同样有效)。
    fn wait_other_channels_idle(&self) -> Result<(), DmaError> {
        let mask = 0x0Fu32 & !(1u32 << (CH as u32));
        let start = crate::arch::cycles_now();
        while self.unit_reg(CHEN).read() & mask != 0 {
            if elapsed_us_since(start) >= 2_000 {
                return Err(DmaError::ChannelBusy);
            }
        }
        Ok(())
    }

    // ============================== 状态查询 ==============================

    /// 本通道是否传输中 (CHSTAT.CHACT)。
    pub fn is_busy(&self) -> bool {
        self.unit_reg(CHSTAT).read() & (1 << (16 + CH)) != 0
    }

    /// 本通道是否已使能 (CHEN; 传输完成会自动清位)。
    pub fn is_enabled(&self) -> bool {
        self.unit_reg(CHEN).read() & (1 << CH) != 0
    }

    /// 传输完成 (TC) 标志是否置位 (INTSTAT1)。
    pub fn tc_pending(&self) -> bool {
        self.unit_reg(INTSTAT1).read() & (1 << CH) != 0
    }

    /// 错误标志是否置位 (INTSTAT0: TRNERR/REQERR)。
    pub fn error_pending(&self) -> bool {
        let v = self.unit_reg(INTSTAT0).read();
        v & (1 << CH) != 0 || v & (1 << (16 + CH)) != 0
    }

    /// 剩余传输计数 (MONDTCTL.CNT)。
    pub fn remaining(&self) -> u16 {
        (self.ch_reg(MONDTCTL).read() >> 16) as u16
    }

    /// 清除传输完成标志 (写 INTCLR1, 对齐 DDL `DMA_ClearTransCompleteStatus`)。
    pub fn clear_tc(&self) {
        self.unit_reg(INTCLR1).write(1 << CH);
    }

    /// 清除错误标志 (写 INTCLR0, 对齐 DDL `DMA_ClearErrStatus`)。
    pub fn clear_errors(&self) {
        self.unit_reg(INTCLR0).write((1 << CH) | (1 << (16 + CH)));
    }

    /// 阻塞等待传输完成 (轮询 TC 标志, 墙钟超时)。
    ///
    /// 截止时间取自 DWT 周期计数 (见 [`elapsed_us_since`], 与 RTOS 节拍
    /// 解耦, 调度器启动前同样有效, 回绕安全)。返回 true = 传输完成
    /// (TC 置位); false = 超时。超时后应检查 [`Dma::error_pending`]
    /// 区分错误与硬件停滞, 并 [`Dma::disable`] 中止残留传输。
    ///
    /// 注意: 超时值应远小于 DWT 32 位回绕周期 (200MHz 下约 21.5s)。
    pub fn wait_done(&self, timeout_us: u32) -> bool {
        let start = crate::arch::cycles_now();
        loop {
            if self.tc_pending() {
                return true;
            }
            if elapsed_us_since(start) >= timeout_us {
                return false;
            }
        }
    }

    // ============================== 中断 ==============================

    /// 安装传输完成 (TC) 中断: 路由 DMA{U}_TC{CH} 事件 → `line`,
    /// 装回调 → 清挂起 → 设优先级 → 使能 (经 [`crate::intc::register`])。
    ///
    /// 回调在中断上下文执行, 须有界且无阻塞; ISR 已清 TC 标志。
    /// 同一通道只允许安装一次。
    pub fn install_tc_irq(
        &self,
        line: crate::intc::Line,
        priority: u8,
        callback: Callback,
    ) -> Result<(), crate::intc::IrqError> {
        TC_CALLBACKS[slot(UNIT, CH)].store(callback);
        let source = 32 + (UNIT as u32 - 1) * 4 + CH as u32; // DMA1_TC0=32
        crate::intc::register(source, line, priority, tc_isr::<UNIT, CH>)?;
        self.unit_reg(INTMASK1).modify(|v| v & !(1 << CH)); // 取消屏蔽
        Ok(())
    }

    /// 安装单元错误中断 (DMA{U}_ERR 每单元一个, ISR 按 INTSTAT0 分发到
    /// 对应通道的回调槽位)。回调须有界且无阻塞。
    pub fn install_err_irq(
        &self,
        line: crate::intc::Line,
        priority: u8,
        callback: Callback,
    ) -> Result<(), crate::intc::IrqError> {
        ERR_CALLBACKS[slot(UNIT, CH)].store(callback);
        let source = 48 + (UNIT as u32 - 1); // DMA1_ERR=48
        crate::intc::register(source, line, priority, err_isr::<UNIT>)?;
        self.unit_reg(INTMASK0)
            .modify(|v| v & !((1 << CH) | (1 << (16 + CH)))); // 取消屏蔽
        Ok(())
    }

    // ============================== 高层操作 ==============================

    /// 阻塞式内存→内存拷贝 (软件触发, 对齐 DDL dmac_base 示例)。
    ///
    /// - 源/目的均字对齐且长度 4 的倍数 → 32 位传输, 否则 8 位;
    /// - 每次请求最多 1024 项 (块大小上限), 大拷贝自动分块
    ///   (每块 `block_size=chunk, trans_count=1`: 单次 SW 请求搬完整块);
    /// - 全程阻塞等待完成 (CPU 可被更高优先级线程/中断抢占),
    ///   适用于把 CPU 从逐字节 memcpy 中解放的场景 (异步化后更佳)。
    pub fn copy_blocking(&self, src: &[u8], dst: &mut [u8]) -> Result<(), DmaError> {
        if src.len() != dst.len() {
            return Err(DmaError::LengthMismatch);
        }
        // The borrows remain live until the blocking transfer has stopped.
        unsafe { self.copy_blocking_raw(src.as_ptr(), dst.as_mut_ptr(), src.len()) }
    }

    /// 裸地址阻塞拷贝。
    ///
    /// # Safety
    ///
    /// `src..src+len` 必须在整个调用期间可读，`dst..dst+len` 必须唯一可写，
    /// 两个区域不得重叠；地址还必须可由当前 DMA 主设备访问。
    pub(crate) unsafe fn copy_blocking_raw(
        &self,
        src: *const u8,
        dst: *mut u8,
        len: usize,
    ) -> Result<(), DmaError> {
        if len == 0 {
            return Ok(());
        }
        let (width, items) = if (src as usize).is_multiple_of(4)
            && (dst as usize).is_multiple_of(4)
            && len.is_multiple_of(4)
        {
            (Width::B32, len / 4)
        } else {
            (Width::B8, len)
        };

        let mut remaining = items;
        let mut s = src as usize;
        let mut d = dst as usize;
        while remaining > 0 {
            let chunk = remaining.min(1024);
            // 计数语义: DTCTL.CNT 计的是**请求次数**, 每次请求搬运
            // BLKSIZE 项, 计数归零才置 TC 并自动失能通道 (对照 DDL
            // dmac_base 示例 BC=5/TC=4: 4 次软件触发各搬 5 项)。
            // 故每块配 count=1: 单次 SW 触发搬完整个块, 1→0 触发 TC。
            unsafe {
                self.configure(&Config {
                    width,
                    src_addr: s,
                    dest_addr: d,
                    block_size: chunk as u16,
                    trans_count: 1,
                    src_inc: AddrMode::Inc,
                    dest_inc: AddrMode::Inc,
                    int_tc: false,
                    int_err: false,
                })?
            };
            self.clear_tc();
            self.enable()?;
            self.sw_trigger();
            if !self.wait_done(COPY_CHUNK_TIMEOUT_US) {
                self.stop_blocking();
                if self.error_pending() {
                    return Err(DmaError::TransferError);
                }
                return Err(DmaError::Timeout);
            }
            self.stop_blocking();
            self.clear_tc();
            s += chunk * width.bytes();
            d += chunk * width.bytes();
            remaining -= chunk;
        }
        Ok(())
    }
}

/// 寄存器回读重试次数 (对齐 DDL DMATIMEOUT2 量级)
const READBACK_RETRIES: u32 = 0x1000;
/// 单块拷贝超时 (µs; 1024 项 · 4B @ 200MHz 远小于此值)
const COPY_CHUNK_TIMEOUT_US: u32 = 10_000;

// ============================== 中断回调槽 ==============================

/// 各通道传输完成回调槽 (索引 = (单元-1)·4 + 通道; 原子回调槽见
/// [`crate::notify::NotifySlot`])
static TC_CALLBACKS: [NotifySlot; 8] = [
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
];
/// 各通道错误回调槽 (由单元错误 ISR 按 INTSTAT0 分发)
static ERR_CALLBACKS: [NotifySlot; 8] = [
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
    NotifySlot::new(),
];

/// TC 中断 ISR: 清标志后通知对应通道槽位。
unsafe extern "C" fn tc_isr<const U: u8, const C: u8>() {
    Reg::new(base(U) + INTCLR1).write(1 << C);
    TC_CALLBACKS[slot(U, C)].call();
}

/// 单元错误 ISR: 遍历 INTSTAT0, 清标志并分发到出错通道的槽位。
unsafe extern "C" fn err_isr<const U: u8>() {
    let stat = Reg::new(base(U) + INTSTAT0).read();
    for ch in 0..4 {
        if stat & ((1 << ch) | (1 << (16 + ch))) != 0 {
            Reg::new(base(U) + INTCLR0).write((1 << ch) | (1 << (16 + ch)));
            ERR_CALLBACKS[slot(U, ch)].call();
        }
    }
}

// ============================== AOS 触发路由 ==============================

/// 把外设事件 `source` 路由到 DMA 通道 (写 AOS TRGSEL, 对齐 DDL
/// `AOS_SetTriggerEventSrc`)。
///
/// 例如 USART1 TX 卸载: `route::<2, 0>(event::usart_ti(1))` →
/// USART1_TI 事件触发 DMA2 通道 0。
pub fn route<const UNIT: u8, const CH: u8>(source: u32) {
    let trgsel = if UNIT == 1 {
        AOS_DMA1_TRGSEL0
    } else {
        AOS_DMA2_TRGSEL0
    } + 4 * CH as usize;
    Reg::new(AOS_BASE + trgsel).modify(|v| (v & !AOS_TRIG_SEL_MASK) | (source & AOS_TRIG_SEL_MASK));
}

/// 解除 DMA 通道的触发路由 (SEL 恢复 0x1FF = 未映射)。
pub fn unroute<const UNIT: u8, const CH: u8>() {
    route::<UNIT, CH>(AOS_TRIG_SEL_MASK);
}

/// 把重配置请求事件路由到 AOS 重配置触发 (`DMA_RC_TRGSEL`)。
pub fn route_reconfig(source: u32) {
    Reg::new(AOS_BASE + AOS_DMA_RC_TRGSEL)
        .modify(|v| (v & !AOS_TRIG_SEL_MASK) | (source & AOS_TRIG_SEL_MASK));
}

/// AOS 软件触发 (写 INTSFTTRG, 对齐 DDL `AOS_SW_Trigger`):
/// 发出 `EVT_SRC_AOS_STRG` 事件, 用于重配置流程的软件请求。
pub fn reconfig_sw_trigger() {
    Reg::new(AOS_BASE + AOS_INTSFTTRG).write(1);
}

// ============================== 全局初始化 ==============================

static CLOCKS_READY: AtomicU8 = AtomicU8::new(0);

/// 使能 DMA 外设时钟 (FCG0: DMA1/DMA2/AOS, 经 [`crate::clk::fcg0_enable`]
/// 自动处理 FCG0 写保护解锁)。
///
/// - 幂等: 重复调用无副作用;
/// - 由 board 初始化在时钟链就绪后调用; 注意运行期切换系统时钟会
///   备份/恢复 FCG0 (clk 模块), 若此后才调用本函数, DMA 时钟保持使能。
pub fn init() {
    if CLOCKS_READY.load(Ordering::Acquire) != 0 {
        return;
    }
    crate::clk::fcg0_enable(FCG0_DMA1 | FCG0_DMA2 | FCG0_AOS);
    // 使能两个 DMA 单元 (EN=1); 并屏蔽全部完成/错误中断
    // (INTMASK 复位值未知, 显式写满确保无未注册中断误入向量表,
    // 需要时由 install_* 逐通道取消屏蔽)
    for unit in 1..=2 {
        let base = base(unit);
        Reg::new(base + EN).write(1);
        Reg::new(base + INTMASK0).write(0xFFFF_FFFF);
        Reg::new(base + INTMASK1).write(0xFFFF_FFFF);
    }
    CLOCKS_READY.store(1, Ordering::Release);
}

// ============================== 控制台 UART TX 卸载 ==============================

static UART_TX_READY: AtomicBool = AtomicBool::new(false);
static UART_TX_BUSY: AtomicBool = AtomicBool::new(false);
/// DMA TX 超时次数 (诊断: 正常情况下应为 0)
static UART_TX_TIMEOUTS: AtomicU32 = AtomicU32::new(0);
/// 启动边沿前等待上次发送结束 (SR.TC) 的超时 (ms)
const UART_TX_ARM_TIMEOUT_MS: u32 = 10;

/// 控制台 TX 专用通道 (编译期配置, 见 `CFG_DMA_TX_*`)
type UartTxDma = Dma<{ crate::config::DMA_TX_UNIT }, { crate::config::DMA_TX_CHANNEL }>;

/// 初始化控制台 UART 发送卸载:
/// AOS 路由 USART{`CFG_UART_UNIT`}_TI → DMA 通道 + 通道基础配置
/// (8 位 / 目的 = USART TDR 固定 / 源递增 / 块 1 / 计数每次发送时设置)。
///
/// 由 board 初始化在控制台 UART 就绪后调用一次。
pub fn uart_tx_init() {
    let dma = UartTxDma::new();
    let unit = crate::config::UART_UNIT;
    route::<{ crate::config::DMA_TX_UNIT }, { crate::config::DMA_TX_CHANNEL }>(event::usart_ti(
        unit,
    ));
    unsafe {
        dma.configure(&Config {
            width: Width::B8,
            src_addr: 0,
            dest_addr: crate::uart::tdr_addr(unit),
            block_size: 1,
            trans_count: 0,
            src_inc: AddrMode::Inc,
            dest_inc: AddrMode::Fix,
            int_tc: false,
            int_err: false,
        })
    }
    .expect("控制台 DMA TX 通道配置失败");
    dma.clear_tc();
    dma.clear_errors();
    UART_TX_READY.store(true, Ordering::Release);
}
/// 尝试用 DMA 发送控制台 UART 数据 (由 [`crate::uart::Uart::write`] 调用)。
///
/// 返回 true = 已接管发送 (含超时放弃: 避免轮询重发导致字节重复);
/// false = 未接管, 调用方回退逐字节轮询。放弃条件:
/// - DMA 未启用 / 未初始化 / 目标不是控制台 UART;
/// - 中断上下文 (panic/fault 诊断路径保持无锁轮询);
/// - 长度过短 ([`crate::config::DMA_TX_MIN`] 以下) 或超 16 位计数;
/// - 其他 DMA TX 正在进行 (互斥) 或修改 CHEN 的等待超时。
///
/// # 触发边沿 (关键!)
///
/// DMA 请求为**边沿捕获**: 通道使能时 TXE 已静默为高 (无新上升沿) 则
/// 传输不会开始, 会一直等到超时丢字节。因此每次发送按 DDL 示例
/// usart_uart_dma 的序列执行:
/// 等待 SR.TC (上次发送完全结束) → `TE=0` (TXE 复位, 清陈旧请求) →
/// 使能 DMA 通道 → `TE=1` (新的 TXE 上升沿 → 触发首字节) → 后续字节
/// 由每次 TXE 上升沿自然驱动。失败路径会先恢复 TE 再回退轮询。
///
/// 发送期间 CPU 只在等 TC 时轮询状态寄存器, 不再逐字节等 TXE;
/// 传输完成后通道自动失能, 与后续轮询发送无缝衔接。
pub fn uart_tx_try<const U: u8>(bytes: &[u8]) -> bool {
    if !crate::config::DMA_ENABLE || U != crate::config::UART_UNIT {
        return false;
    }
    if !UART_TX_READY.load(Ordering::Acquire) {
        return false;
    }
    if crate::critical_section::in_isr() {
        return false;
    }
    let len = bytes.len();
    if len < crate::config::DMA_TX_MIN || len > u16::MAX as usize {
        return false;
    }
    // 一次只允许一个 DMA TX (打印锁串行化控制台, 但 zmodem 等在锁外发送)
    if UART_TX_BUSY
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }

    let dma = UartTxDma::new();
    let uart = crate::uart::Uart::<U>::take();
    let mut consumed = true;
    if unsafe { dma.set_src_addr(bytes.as_ptr() as usize) }.is_ok()
        && dma.set_trans_count(len as u16).is_ok()
    {
        dma.clear_tc();
        // 启动边沿: 空闲 → TE 停 → 使能通道 → TE 起 (见函数文档)
        if uart.dma_tx_arm(UART_TX_ARM_TIMEOUT_MS, monotonic_ms) {
            if dma.enable().is_ok() {
                uart.dma_tx_fire();
                // 超时 = 波特率耗时 ×2 + 1ms 余量 (u64 防长包溢出; 墙钟
                // 截止见 [`Dma::wait_done`])
                let timeout_us = (u64::from(len as u32) * 10 * 1_000_000
                    / u64::from(crate::config::UART_BAUDRATE))
                    as u32
                    * 2
                    + 1_000;
                if !dma.wait_done(timeout_us) {
                    UART_TX_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                }
                // 返回前必须确认 DMA 已停止，维持 `bytes` 借用的安全边界。
                dma.stop_blocking();
            } else {
                uart.dma_tx_fire(); // 恢复发射器后回退轮询
                consumed = false;
            }
        } else {
            consumed = false; // 上次发送未结束 (异常) → 轮询, TE 未动
        }
    } else {
        consumed = false; // 配置写入回读失败 → 轮询
    }
    dma.clear_tc();
    UART_TX_BUSY.store(false, Ordering::Release);
    consumed
}

/// DMA TX 超时累计计数 (诊断用)。
pub fn uart_tx_timeout_count() -> u32 {
    UART_TX_TIMEOUTS.load(Ordering::Relaxed)
}

// ============================== 共享内存拷贝 (Flash→RAM / RAM→RAM) ==============================

/// 共享拷贝通道 (编译期配置, 见 `CFG_DMA_COPY_*`; 与 TX 通道分属不同
/// 通道, 已由 [`crate::config`] 编译期校验不冲突)。
type CopyDma = Dma<{ crate::config::DMA_COPY_UNIT }, { crate::config::DMA_COPY_CHANNEL }>;

static COPY_BUSY: AtomicBool = AtomicBool::new(false);
/// copy_try 回退计数 (诊断: 通道忙/未初始化/失败时递增, 正常应为 0)
static COPY_FALLBACKS: AtomicU32 = AtomicU32::new(0);

/// 尝试用 DMA 整块拷贝 `src → dst` (阻塞等待完成)。
///
/// 返回 true = DMA 已接管并完成; false = 未接管, 调用方回退
/// 逐字节/逐字拷贝。放弃条件: DMA 未启用/未初始化、中断上下文、
/// 长度低于 [`crate::config::DMA_COPY_MIN`]、通道正忙 (并发拷贝互斥)。
///
/// 用途: Flash→RAM 大块读取 (文件系统读/擦除校验/写后回读) 与
/// RAM→RAM 搬运。Flash 内存映射读带等待周期, DMA 32 位传输比逐字节
/// 循环快约一个数量级; 源/目的未对齐时自动降为 8 位宽度
/// (见 [`Dma::copy_blocking`])。
pub fn copy_try(src: &[u8], dst: &mut [u8]) -> bool {
    if !crate::config::DMA_ENABLE || CLOCKS_READY.load(Ordering::Acquire) == 0 {
        return false;
    }
    if crate::critical_section::in_isr() {
        return false;
    }
    if src.len() != dst.len() || src.len() < crate::config::DMA_COPY_MIN {
        return false;
    }
    if COPY_BUSY
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        COPY_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let ok = CopyDma::new().copy_blocking(src, dst).is_ok();
    COPY_BUSY.store(false, Ordering::Release);
    if !ok {
        COPY_FALLBACKS.fetch_add(1, Ordering::Relaxed);
    }
    ok
}

/// copy_try 回退计数 (诊断用)。
pub fn copy_fallback_count() -> u32 {
    COPY_FALLBACKS.load(Ordering::Relaxed)
}

// ============================== 单调时钟 (与 RTOS 节拍解耦) ==============================

/// 自 `start` (CYCCNT) 起的微秒数。
///
/// 按当前 HCLK 换算周期数; DWT 与 SysTick/调度器无关, 启动阶段
/// (单执行流) 同样有效, 差值回绕安全。仅用于毫秒/微秒级超时窗口
/// (须远小于 DWT 32 位回绕周期: 200MHz 下约 21.5s)。
fn elapsed_us_since(start: u32) -> u32 {
    let hz = crate::clk::hclk_hz().max(1);
    (u64::from(crate::arch::cycles_now().wrapping_sub(start)) * 1_000_000 / u64::from(hz)) as u32
}

/// 单调毫秒时钟 (DWT 按 HCLK 换算; 语义同 [`crate::rtos::uptime_ms`] 的
/// 差分用法, 但调度器启动前同样有效)。供外部注入 [`crate::uart`]
/// 的等待时钟使用。
pub fn monotonic_ms() -> u32 {
    let hz = crate::clk::hclk_hz().max(1);
    (u64::from(crate::arch::cycles_now()) * 1_000 / u64::from(hz)) as u32
}
