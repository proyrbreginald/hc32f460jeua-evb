//! 中断控制器 (INTC + NVIC) 驱动
//!
//! # 架构 (HC32F460, 对齐 DDL hc32_ll_interrupts.c)
//!
//! 三级映射: **事件源 → SEL 寄存器 → NVIC 线**
//!
//! ```text
//! 外设事件 (en_int_src_t, 如 USART1_RI=279)
//!   └→ INTC.SELx 写入事件源编号 (SEL0 偏移 0x5C, 每线 4 字节)
//!       └→ NVIC 线 INTxxx (IRQn = x, 共 144 条)
//!           └→ NVIC ISER/IPR 使能/优先级 → 向量表分发
//! ```
//!
//! - **SEL 复位值 0x1FF = 未映射**; 一条线同一时刻只能接一个事件源;
//! - INT000~INT127 通过 SEL 直接路由 (本模块支持);
//!   INT128~INT143 为**共享中断线** (VSSEL 位掩码 + 外设状态轮询),
//!   本模块不提供 (DDL 的 hc32f460_ll_interrupts_share.c 模式);
//! - 优先级: Cortex-M4 4 位 (0~15, 越小越高), 写 IPR 高半字节,
//!   默认 PRIGROUP=0 (全为抢占优先级, 无子优先级)。
//!
//! # 使用 (驱动注册标准流程, 对齐 DDL 例程)
//!
//! ```no_run
//! // 1. 外设初始化 (使能外设中断标志)
//! // 2. 注册: 路由事件源 + 装回调 + 清挂起 + 设优先级 + 使能, 一步完成
//! intc::register(intc::src::USART1_RI, intc::INT001, 8, my_isr)
//!     .expect("中断线被占用");
//! // 3. ISR 内: 读外设状态 → 处理 → 清标志; 末尾无需清 NVIC 挂起
//! ```
//!
//! 中断回调在**中断上下文**执行, 只能使用非阻塞操作。
//! 向量表分发见 [`crate::vector_table`]。
//!
//! 事件源常量与低层 API 供各外设驱动按需选用, 忽略未使用项的死代码警告。
#![allow(dead_code)]

/// 中断回调类型 (`extern "C"`, 中断上下文执行)
pub type Handler = unsafe extern "C" fn();

/// NVIC 中断线 (INT000~INT143), 编号即 IRQn
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Line(u8);

/// 常用中断线常量 (INT000~INT007 为向量表分发槽位, 任意线均可注册)
pub const INT000: Line = Line(0);
pub const INT001: Line = Line(1);
pub const INT002: Line = Line(2);
pub const INT003: Line = Line(3);
pub const INT004: Line = Line(4);
pub const INT005: Line = Line(5);
pub const INT006: Line = Line(6);
pub const INT007: Line = Line(7);

impl Line {
    /// 构造中断线 (编译期可校验 0~143)
    pub const fn new(n: u8) -> Self {
        assert!(n < 144, "中断线必须为 INT000~INT143");
        Self(n)
    }

    /// 线号 (0~143)
    pub const fn n(self) -> u8 {
        self.0
    }
}

/// 中断注册失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqError {
    /// 该中断线已被其他事件源占用 (SEL 非 0x1FF 且非本次源)
    LineTaken,
    /// 中断线越界 (注册仅支持 INT000~INT127, 共享线 INT128+ 不支持)
    LineUnsupported,
}

/// 事件源编号 (`en_int_src_t`, 参考 DDL hc32f460.h L489~812)
///
/// 每个外设中断源有固定编号, 写入 INTC.SELx 即路由到 NVIC 线 x。
pub mod src {
    // ---- GPIO 外部中断 (EIRQ0~15) ----
    pub const PORT_EIRQ0: u32 = 0;
    pub const PORT_EIRQ1: u32 = 1;
    pub const PORT_EIRQ2: u32 = 2;
    pub const PORT_EIRQ3: u32 = 3;
    pub const PORT_EIRQ4: u32 = 4;
    pub const PORT_EIRQ5: u32 = 5;
    pub const PORT_EIRQ6: u32 = 6;
    pub const PORT_EIRQ7: u32 = 7;
    pub const PORT_EIRQ8: u32 = 8;
    pub const PORT_EIRQ9: u32 = 9;
    pub const PORT_EIRQ10: u32 = 10;
    pub const PORT_EIRQ11: u32 = 11;
    pub const PORT_EIRQ12: u32 = 12;
    pub const PORT_EIRQ13: u32 = 13;
    pub const PORT_EIRQ14: u32 = 14;
    pub const PORT_EIRQ15: u32 = 15;

    // ---- DMA1/2 ----
    pub const DMA1_TC0: u32 = 32;
    pub const DMA1_TC1: u32 = 33;
    pub const DMA1_TC2: u32 = 34;
    pub const DMA1_TC3: u32 = 35;
    pub const DMA2_TC0: u32 = 36;
    pub const DMA2_TC1: u32 = 37;
    pub const DMA2_TC2: u32 = 38;
    pub const DMA2_TC3: u32 = 39;
    pub const DMA1_ERR: u32 = 48;
    pub const DMA2_ERR: u32 = 49;

    // ---- EFM (Flash) ----
    pub const EFM_PEERR: u32 = 50;
    pub const EFM_COLERR: u32 = 51;
    pub const EFM_OPTEND: u32 = 52;

    // ---- TIM0 ----
    pub const TMR0_1_CMP_A: u32 = 64;
    pub const TMR0_1_CMP_B: u32 = 65;
    pub const TMR0_2_CMP_A: u32 = 66;
    pub const TMR0_2_CMP_B: u32 = 67;

    // ---- RTC ----
    pub const RTC_ALM: u32 = 81;
    pub const RTC_PRD: u32 = 82;

    // ---- TIM6_1 ----
    pub const TMR6_1_GCMP_A: u32 = 96;
    pub const TMR6_1_GCMP_B: u32 = 97;
    pub const TMR6_1_GCMP_C: u32 = 98;
    pub const TMR6_1_GCMP_D: u32 = 99;
    pub const TMR6_1_GCMP_E: u32 = 100;
    pub const TMR6_1_GCMP_F: u32 = 101;
    pub const TMR6_1_OVF: u32 = 102;
    pub const TMR6_1_UDF: u32 = 103;
    pub const TMR6_1_DTE: u32 = 104;
    pub const TMR6_1_SCMP_A: u32 = 107;
    pub const TMR6_1_SCMP_B: u32 = 108;

    // ---- TIM6_2 ----
    pub const TMR6_2_GCMP_A: u32 = 112;
    pub const TMR6_2_GCMP_B: u32 = 113;
    pub const TMR6_2_GCMP_C: u32 = 114;
    pub const TMR6_2_GCMP_D: u32 = 115;
    pub const TMR6_2_GCMP_E: u32 = 116;
    pub const TMR6_2_GCMP_F: u32 = 117;
    pub const TMR6_2_OVF: u32 = 118;
    pub const TMR6_2_UDF: u32 = 119;
    pub const TMR6_2_DTE: u32 = 120;
    pub const TMR6_2_SCMP_A: u32 = 123;
    pub const TMR6_2_SCMP_B: u32 = 124;

    // ---- TIM6_3 ----
    pub const TMR6_3_GCMP_A: u32 = 128;
    pub const TMR6_3_GCMP_B: u32 = 129;
    pub const TMR6_3_GCMP_C: u32 = 130;
    pub const TMR6_3_GCMP_D: u32 = 131;
    pub const TMR6_3_GCMP_E: u32 = 132;
    pub const TMR6_3_GCMP_F: u32 = 133;
    pub const TMR6_3_OVF: u32 = 134;
    pub const TMR6_3_UDF: u32 = 135;
    pub const TMR6_3_DTE: u32 = 136;
    pub const TMR6_3_SCMP_A: u32 = 139;
    pub const TMR6_3_SCMP_B: u32 = 140;

    // ---- TMRA (高级定时器 1~6) ----
    pub const TMRA_1_OVF: u32 = 256;
    pub const TMRA_1_UDF: u32 = 257;
    pub const TMRA_1_CMP: u32 = 258;
    pub const TMRA_2_OVF: u32 = 259;
    pub const TMRA_2_UDF: u32 = 260;
    pub const TMRA_2_CMP: u32 = 261;
    pub const TMRA_3_OVF: u32 = 262;
    pub const TMRA_3_UDF: u32 = 263;
    pub const TMRA_3_CMP: u32 = 264;
    pub const TMRA_4_OVF: u32 = 265;
    pub const TMRA_4_UDF: u32 = 266;
    pub const TMRA_4_CMP: u32 = 267;
    pub const TMRA_5_OVF: u32 = 268;
    pub const TMRA_5_UDF: u32 = 269;
    pub const TMRA_5_CMP: u32 = 270;
    pub const TMRA_6_OVF: u32 = 271;
    pub const TMRA_6_UDF: u32 = 272;
    pub const TMRA_6_CMP: u32 = 273;

    // ---- USBFS ----
    pub const USBFS_GLB: u32 = 275;

    // ---- USART1~4 (EI/RI/TI/TCI/RTO) ----
    pub const USART1_EI: u32 = 278; // 接收错误
    pub const USART1_RI: u32 = 279; // 接收数据寄存器非空
    pub const USART1_TI: u32 = 280; // 发送数据寄存器空
    pub const USART1_TCI: u32 = 281; // 发送完成
    pub const USART1_RTO: u32 = 282; // 接收超时
    pub const USART2_EI: u32 = 283;
    pub const USART2_RI: u32 = 284;
    pub const USART2_TI: u32 = 285;
    pub const USART2_TCI: u32 = 286;
    pub const USART2_RTO: u32 = 287;
    pub const USART3_EI: u32 = 288;
    pub const USART3_RI: u32 = 289;
    pub const USART3_TI: u32 = 290;
    pub const USART3_TCI: u32 = 291;
    pub const USART3_RTO: u32 = 292;
    pub const USART4_EI: u32 = 293;
    pub const USART4_RI: u32 = 294;
    pub const USART4_TI: u32 = 295;
    pub const USART4_TCI: u32 = 296;
    pub const USART4_RTO: u32 = 297;

    // ---- 比较器/串行接口/电源监测等 ----
    pub const CMP1: u32 = 416;
    pub const CMP2: u32 = 417;
    pub const CMP3: u32 = 418;
    pub const I2C1_RXI: u32 = 420;
    pub const I2C1_TXI: u32 = 421;
    pub const I2C1_TEI: u32 = 422;
    pub const I2C2_RXI: u32 = 424;
    pub const I2C2_TXI: u32 = 425;
    pub const I2C2_TEI: u32 = 426;
    pub const I2C3_RXI: u32 = 428;
    pub const I2C3_TXI: u32 = 429;
    pub const I2C3_TEI: u32 = 430;
    pub const LVD1: u32 = 433;
    pub const LVD2: u32 = 434;
    pub const OTS: u32 = 435; // OTS 采样完成 (独立线 INT110, 见 ots 模块)
    pub const WDT_REFUDF: u32 = 439;
    pub const ADC1_EOCA: u32 = 448;
    pub const ADC1_EOCB: u32 = 449;
    pub const ADC2_EOCA: u32 = 452;
    pub const ADC2_EOCB: u32 = 453;
    pub const TRNG_END: u32 = 456;
}

// ============================== INTC (SEL 路由) ==============================

/// INTC 基址 (SEL0 偏移 0x5C, 每线 4 字节, 与 DDL CM_INTC 布局一致)
const INTC_BASE: usize = 0x4005_1000;
const INTC_SEL0_OFF: usize = 0x5C;
/// SEL 复位值: 0x1FF = 未映射 (DDL INTSEL_RST_VALUE)
const SEL_UNMAPPED: u32 = 0x1FF;

fn sel_addr(n: u8) -> *mut u32 {
    (INTC_BASE + INTC_SEL0_OFF + 4 * n as usize) as *mut u32
}

/// 路由事件源到中断线 (写 INTC.SELx = 事件源编号)
fn route(source: u32, line: Line) {
    unsafe { core::ptr::write_volatile(sel_addr(line.n()), source) };
}

/// 解除路由 (SEL 恢复 0x1FF)
fn unroute(line: Line) {
    unsafe { core::ptr::write_volatile(sel_addr(line.n()), SEL_UNMAPPED) };
}

// ============================== NVIC 操作 (对齐 CMSIS NVIC_*) ==============================

const NVIC_ISER: usize = 0xE000_E100; // 中断使能 (W1S)
const NVIC_ICER: usize = 0xE000_E180; // 中断失能 (W1C)
const NVIC_ISPR: usize = 0xE000_E200; // 挂起置位 (W1S)
const NVIC_ICPR: usize = 0xE000_E280; // 挂起清除 (W1C)
const NVIC_IPR: usize = 0xE000_E400; // 优先级 (每线 1 字节)

fn nvic_reg(addr: usize, line: Line) -> *mut u32 {
    (addr + 4 * (line.n() as usize / 32)) as *mut u32
}

fn nvic_bit(line: Line) -> u32 {
    1 << (line.n() % 32)
}

/// 使能中断 (NVIC ISER, 对齐 CMSIS `NVIC_EnableIRQ`)
pub fn enable(line: Line) {
    unsafe {
        core::ptr::write_volatile(nvic_reg(NVIC_ISER, line), nvic_bit(line));
    }
}

/// 失能中断 (NVIC ICER)
pub fn disable(line: Line) {
    unsafe {
        core::ptr::write_volatile(nvic_reg(NVIC_ICER, line), nvic_bit(line));
    }
}

/// 挂起中断 (NVIC ISPR; 也可作为**软件触发**测试手段)
pub fn pend(line: Line) {
    unsafe {
        core::ptr::write_volatile(nvic_reg(NVIC_ISPR, line), nvic_bit(line));
    }
}

/// 清除中断挂起 (NVIC ICPR; 注册/初始化时先清残留挂起)
pub fn clear_pend(line: Line) {
    unsafe {
        core::ptr::write_volatile(nvic_reg(NVIC_ICPR, line), nvic_bit(line));
    }
}

/// 设置中断优先级 (NVIC IPR, 0~15, 越小越高; 写高半字节)
pub fn set_priority(line: Line, priority: u8) {
    assert!(priority <= 15, "优先级必须为 0~15");
    unsafe {
        core::ptr::write_volatile(
            (NVIC_IPR + line.n() as usize) as *mut u8,
            (priority & 0x0F) << 4,
        );
    }
}

/// 读取中断优先级 (0~15)
pub fn priority(line: Line) -> u8 {
    unsafe { core::ptr::read_volatile((NVIC_IPR + line.n() as usize) as *const u8) >> 4 }
}

// ============================== 注册 API ==============================

/// 注册中断 (对齐 DDL 例程流程, 一步完成):
/// 路由事件源 → 安装回调 → 清挂起 → 设优先级 → 使能
///
/// - `source`: 事件源编号 (见 [`src`] 模块);
/// - `line`: NVIC 线 (仅 INT000~INT127, 共享线 INT128+ 不支持);
/// - `priority`: 0~15 (越小越高);
/// - `handler`: 中断回调 (中断上下文执行, 末尾会返回, 无需清挂起)。
///
/// 失败 ([`IrqError::LineTaken`]) 表示该线已被其他事件源占用,
/// 应更换中断线或先 [`unregister`]。
pub fn register(source: u32, line: Line, priority: u8, handler: Handler) -> Result<(), IrqError> {
    let n = line.n();
    if n >= 128 {
        return Err(IrqError::LineUnsupported);
    }
    let sel = unsafe { core::ptr::read_volatile(sel_addr(n)) };
    if sel != SEL_UNMAPPED && sel != source {
        return Err(IrqError::LineTaken);
    }
    route(source, line);
    crate::vector_table::register_irq(n as usize, handler);
    clear_pend(line);
    set_priority(line, priority);
    enable(line);
    Ok(())
}

/// 注销中断: 失能 → 解除路由 → 移除回调 (复位 SEL 为 0x1FF)
pub fn unregister(line: Line) {
    let n = line.n();
    if n >= 128 {
        return;
    }
    disable(line);
    unroute(line);
    crate::vector_table::unregister_irq(n as usize);
}
