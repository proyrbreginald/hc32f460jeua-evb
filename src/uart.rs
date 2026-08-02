//! UART 驱动 (USART1~4)
//!
//! 参考 DDL `hc32_ll_usart.c` 的 `USART_UART_Init` / `USART_SetBaudrate` /
//! `USART_FuncCmd` / `USART_GetStatus` / `USART_WriteData`, 以及参考手册
//! UART 章节的波特率公式。
//!
//! 单元号通过 const 泛型编码在类型中 ([`Uart<U>`], 便捷别名 [`Uart1`]~[`Uart4`]),
//! 四个 USART 的寄存器布局、时钟域 (PCLK1) 与初始化序列完全一致,
//! 仅基址与 FCG1 时钟门控位不同 (USART_BASES / [`fcg1_usart_bit`])。
//!
//! # 时钟链
//!
//! 系统时钟 (MRC 8MHz 或外部晶振 [`crate::clk::XTAL_HZ`]) → PCLK1
//! (SCFGR.PCLK1S 分频, 运行时经 [`crate::clk::pclk1_hz`] 查询)
//! → USART_PR.PSC 预分频 (÷1/÷4/÷16/÷64) → 波特率发生器。
//! **USART 时钟 = PCLK1 / 2^(2·PSC)**。
//!
//! # 引脚 (JEUA 48pin, 用户确认)
//!
//! 各 USART 的可用引脚按封装不同, 需查数据手册表 2-1/2-2:
//! - USART1: PA9=TX (FSEL 32), PA10=RX (FSEL 33), Func_Grp1;
//! - 其他单元请按引脚表选择 (Func32~63 按引脚的 Func_Grp1/Grp2 映射)。
//!
//! Func_Grp1/Grp2 由引脚硬件固定, 无需软件配置。
//! [`Uart::init`] 只配置外设本身, 引脚复用 (PFSR.FSEL) 需单独调用
//! `gpio::Pin::set_func`。
//!
//! # 波特率 (UART 模式, 8 位过采样)
//!
//! FBME=0: B = C / (8 × (DIV_INT + 1))
//! FBME=1: B = C × (128 + FRAC) / (8 × (DIV_INT+1) × 256)
//!
//! 本模块用纯整数计算 DIV_INT/FRAC (与 DDL 浮点实现误差一致)。
//! 8MHz PCLK1 下 115200 波特率须用 PSC=0 (÷1): DIV_INT=7, FRAC=108, 误差 0.03%。
//!
//! HAL 提供完整 API (接收/16 位过采样/多级分频等), 但应用往往只使用其中一部分,
//! 因此忽略未使用项的死代码警告。
#![allow(dead_code)]

/// 内存映射寄存器 (绝对地址)
struct Reg {
    addr: usize,
}

impl Reg {
    const fn new(addr: usize) -> Self {
        Self { addr }
    }

    fn read(&self) -> u32 {
        unsafe { core::ptr::read_volatile(self.addr as *mut u32) }
    }

    fn write(&self, value: u32) {
        unsafe { core::ptr::write_volatile(self.addr as *mut u32, value) }
    }

    fn modify(&self, f: impl FnOnce(u32) -> u32) {
        self.write(f(self.read()));
    }

    fn read_u16(&self) -> u16 {
        unsafe { core::ptr::read_volatile(self.addr as *mut u16) }
    }

    fn write_u16(&self, value: u16) {
        unsafe { core::ptr::write_volatile(self.addr as *mut u16, value) }
    }
}

/// USART 单元基址表 (CM_USART1_BASE 等)
const USART_BASES: [usize; 4] = [0x4001_D000, 0x4001_D400, 0x4002_1000, 0x4002_1400];
/// PWC 外设基址 (FCG 时钟门控)
const PWC_BASE: usize = 0x4004_8000;

// ---- 中断路径 (INTC 事件源映射 + NVIC, 参考 DDL hc32_ll_interrupts.c) ----

/// INTC 基址 (CM_INTC_TypeDef, 见 hc32f460.h):
/// NMICR@0x00, NMIENR@0x04, NMIFR@0x08, NMICFR@0x0C,
/// EIRQCR0~15@0x10..0x4C, WUPEN@0x50, EIFR@0x54, EIFCR@0x58,
/// **SEL0@0x5C** 起 (每通道 4 字节, 复位值 0x1FF = 未映射)。
const INTC_BASE: usize = 0x4005_1000;
const INTC_SEL_OFF: usize = 0x5C;
/// NVIC ISER0 基址 (Cortex-M4 内核外设)
const NVIC_ISER_BASE: usize = 0xE000_E100;
/// NVIC IPR0 基址 (优先级字节, 每中断 1 字节)
const NVIC_IPR_BASE: usize = 0xE000_E400;

/// USART1 中断事件源编号 (DDL hc32f460.h en_int_src_t)
pub const INT_SRC_USART1_RI: u32 = 279; // 接收数据寄存器非空/接收错误
#[allow(unused)]
pub const INT_SRC_USART1_EI: u32 = 278; // 接收错误
#[allow(unused)]
pub const INT_SRC_USART1_TI: u32 = 280; // 发送数据寄存器空
#[allow(unused)]
pub const INT_SRC_USART1_TCI: u32 = 281; // 发送完成

/// FCG1.USARTn 时钟门控位 (清位 = 使能): USART1=bit24 ... USART4=bit27
const fn fcg1_usart_bit(unit: u8) -> u32 {
    1 << (23 + unit)
}

/// SR 标志位
const SR_PE: u32 = 1 << 0; // 奇偶校验错误
const SR_FE: u32 = 1 << 1; // 帧错误
const SR_ORE: u32 = 1 << 3; // 过载错误
const SR_RXNE: u32 = 1 << 5; // 接收数据寄存器非空
const SR_TXE: u32 = 1 << 7; // 发送数据寄存器空

/// CR1 位
const CR1_RE: u32 = 1 << 2; // 接收使能
const CR1_TE: u32 = 1 << 3; // 发送使能
const CR1_RIE: u32 = 1 << 5; // 接收中断使能 (RXNE + 接收错误)
const CR1_OVER8: u32 = 1 << 15; // 8 位过采样
const CR1_FBME: u32 = 1 << 29; // 小数波特率使能

/// CR1 错误标志清除位 (写 1 清除, 对应 SR 的 PE/FE/ORE)
const CR1_CPE: u32 = 1 << 16; // 清除奇偶校验错误
const CR1_CFE: u32 = 1 << 17; // 清除帧错误
const CR1_CORE: u32 = 1 << 19; // 清除过载错误

/// BRR 字段
const BRR_DIV_FRACTION_MASK: u32 = 0x7F; // [6:0]
const BRR_DIV_INTEGER_POS: u32 = 8; // [15:8]
const BRR_DIV_INTEGER_MASK: u32 = 0xFF;

/// 过采样位数
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Oversample {
    /// 16 位过采样 (OVER8=0), 波特率精度更高但上限为 PCLK/16
    Sixteen,
    /// 8 位过采样 (OVER8=1), 波特率上限 PCLK/8
    Eight,
}

/// USART 时钟预分频 (PR.PSC), 分频系数 = 2^(2·PSC)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockDiv {
    /// ÷1 (PSC=0)
    Div1,
    /// ÷4 (PSC=1)
    Div4,
    /// ÷16 (PSC=2)
    Div16,
    /// ÷64 (PSC=3)
    Div64,
}

impl ClockDiv {
    fn psc(self) -> u32 {
        match self {
            ClockDiv::Div1 => 0,
            ClockDiv::Div4 => 1,
            ClockDiv::Div16 => 2,
            ClockDiv::Div64 => 3,
        }
    }

    fn divisor(self) -> u32 {
        1 << (2 * self.psc())
    }
}

/// UART 配置 (固定 8 数据位 / 1 停止位 / 无校验, 对齐 DDL StructInit 默认)
#[derive(Clone, Copy, Debug)]
pub struct UartConfig {
    /// 目标波特率 (bps)
    pub baudrate: u32,
    /// 过采样位数
    pub oversample: Oversample,
    /// 时钟预分频
    pub clock_div: ClockDiv,
}

impl Default for UartConfig {
    fn default() -> Self {
        Self {
            baudrate: 115_200,
            oversample: Oversample::Eight,
            clock_div: ClockDiv::Div1,
        }
    }
}

/// UART 初始化失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UartError {
    /// 波特率为 0
    InvalidBaudrate,
    /// 目标波特率在所选时钟/分频/过采样组合下不可实现
    BaudrateUnsupported,
}

/// UART 句柄: USART 单元号 `U` (1~4) 编码在类型中, 越界在编译期报错。
///
/// 私有字段保证只能通过 [`Uart::take`] 构造。
/// 使用便捷别名 [`Uart1`] ~ [`Uart4`]。
pub struct Uart<const U: u8> {
    _private: (),
}

/// USART1 便捷别名
pub type Uart1 = Uart<1>;
/// USART2 便捷别名
pub type Uart2 = Uart<2>;
/// USART3 便捷别名
pub type Uart3 = Uart<3>;
/// USART4 便捷别名
pub type Uart4 = Uart<4>;

impl<const U: u8> Uart<U> {
    /// 获取 UART 句柄。`U` 越界 (非 1~4) 时:
    /// 以 `const` 方式使用会在编译期报错。
    pub const fn take() -> Self {
        assert!(U >= 1 && U <= 4, "USART 单元必须为 1..=4");
        Self { _private: () }
    }

    /// 本单元外设基址 (USART_BASES, `U` 已由 take 断言)
    fn base() -> usize {
        USART_BASES[U as usize - 1]
    }

    /// USART 寄存器 (CM_USART_TypeDef, 见 SVD/hc32f460.h)
    fn reg(&self, offset: usize) -> Reg {
        Reg::new(Self::base() + offset)
    }

    fn sr(&self) -> Reg {
        self.reg(0x00) // 状态 (只读)
    }

    fn tdr(&self) -> Reg {
        self.reg(0x04) // 发送数据 (16 位)
    }

    fn rdr(&self) -> Reg {
        self.reg(0x06) // 接收数据 (16 位, 只读)
    }

    fn brr(&self) -> Reg {
        self.reg(0x08) // 波特率: DIV_FRACTION[6:0] | DIV_INTEGER[15:8]
    }

    fn cr1(&self) -> Reg {
        self.reg(0x0C) // 控制 1
    }

    fn cr2(&self) -> Reg {
        self.reg(0x10) // 控制 2
    }

    fn cr3(&self) -> Reg {
        self.reg(0x14) // 控制 3
    }

    fn pr(&self) -> Reg {
        self.reg(0x18) // 预分频: PSC[1:0]
    }

    /// 初始化本 USART 为 UART 模式 (对齐 DDL USART_UART_Init + FuncCmd)
    ///
    /// 序列: FCG1 时钟使能 → CR1/CR2/CR3 → PR 预分频 → BRR 波特率 →
    /// 使能发送/接收。波特率计算在 RX/TX 使能前完成 (DDL 要求)。
    ///
    /// 注意: 本方法只配置 USART 外设, 引脚复用 (PFSR.FSEL) 需按封装
    /// 引脚表另行配置 (见数据手册表 2-1)。
    pub fn init(&self, config: UartConfig) -> Result<(), UartError> {
        if config.baudrate == 0 {
            return Err(UartError::InvalidBaudrate);
        }

        // 1. 使能 USARTn 时钟 (FCG1 清位)
        let fcg1 = Reg::new(PWC_BASE + 0x04);
        fcg1.modify(|v| v & !fcg1_usart_bit(U));

        // 2. 计算波特率 (USART 时钟 = PCLK1 / 预分频, PCLK1 运行时查询)
        let usart_clk = crate::clk::pclk1_hz() / config.clock_div.divisor();
        let (div_int, div_frac, fbme) = calc_brr(usart_clk, config.baudrate, config.oversample)?;

        // 3. 控制寄存器: UART 模式 (MS=0), 8 位 (M=0), 无校验, LSB 先, 下降沿,
        //    1 停止位, 内部时钟源, 无硬件流控 (对齐 DDL 8N1 默认)
        let over8 = if config.oversample == Oversample::Eight {
            CR1_OVER8
        } else {
            0
        };
        self.cr1().write(over8);
        self.cr2().write(0);
        self.cr3().write(0);

        // 4. 预分频 (PR.PSC)
        self.pr().write(config.clock_div.psc());

        // 5. 波特率寄存器 (整数 + 小数)
        self.brr()
            .write((div_int << BRR_DIV_INTEGER_POS) | div_frac);
        if fbme {
            self.cr1().modify(|v| v | CR1_FBME);
        }

        // 6. 使能发送与接收 (对齐示例 USART_FuncCmd(USART_TX | USART_RX))
        self.cr1().modify(|v| v | CR1_TE | CR1_RE);

        Ok(())
    }

    /// 轮询发送一个字节: 等待 TDR 空 (SR.TXE=1) 后写入 (对齐 USART_WriteData)
    pub fn write_byte(&self, byte: u8) {
        while self.sr().read() & SR_TXE == 0 {
            // 等待发送数据寄存器空
        }
        self.tdr().write_u16(byte as u16);
    }

    /// 轮询发送字节串
    pub fn write(&self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_byte(byte);
        }
    }

    /// 轮询发送字符串
    pub fn write_str(&self, s: &str) {
        self.write(s.as_bytes());
    }

    /// 非阻塞读取一个字节 (SR.RXNE=1 时有数据)
    pub fn read_byte(&self) -> Option<u8> {
        if self.sr().read() & SR_RXNE != 0 {
            Some(self.rdr().read_u16() as u8)
        } else {
            None
        }
    }

    /// 使能接收中断 (对齐 DDL 示例: 注册 INTC 事件源 + NVIC + CR1.RIE)
    ///
    /// 中断 ISR ([`rx_irq_handler`], 各单元共用同一 ISR 按单元分发)
    /// 把收到的字节写入环形缓冲 [`RX_RINGS`], 应用侧用
    /// [`Uart::rx_count`] / [`Uart::read_rx`] / [`Uart::drain_rx`] 读取。
    ///
    /// - `irq_n`: INTC 通道号 (INT000~INT007, 对应向量表分发槽位);
    /// - `priority`: NVIC 抢占优先级 (0~15, 4 位, 值越小优先级越高)。
    pub fn enable_rx_interrupt(&self, irq_n: usize, priority: u8) {
        assert!(
            irq_n < 8,
            "enable_rx_interrupt: INTC 通道仅支持 INT000~INT007"
        );
        crate::vector_table::register_irq(irq_n, rx_irq_handler::<U>);
        unsafe {
            // 1. INTC 事件源映射: SEL[irq_n] = USART1_RI (对齐 INTC_IrqSignIn)
            let sel = INTC_BASE + INTC_SEL_OFF + 4 * irq_n;
            core::ptr::write_volatile(sel as *mut u32, INT_SRC_USART1_RI);

            // 2. NVIC: 清挂起 → 设优先级 → 使能 (对齐 INTC_IrqInstalHandler)。
            //    ISER 为写 1 置位 (W1S) 寄存器, 直接写入只影响本中断位;
            //    IPR 是字节寄存器, 用字节指针写 (u32 指针会因未对齐触发 UB 检查)。
            core::ptr::write_volatile(
                (NVIC_ISER_BASE + 4 * (irq_n / 32)) as *mut u32,
                1 << (irq_n % 32),
            );
            core::ptr::write_volatile(
                (NVIC_IPR_BASE + irq_n) as *mut u8,
                ((priority & 0x0F) as u8) << 4,
            );

            // 3. CR1.RIE: 接收满 + 接收错误中断使能 (对齐 USART_FuncCmd(USART_INT_RX))
            self.cr1().modify(|v| v | CR1_RIE);
        }
    }
}

/// 接收环形缓冲大小 (字节)
pub const RX_BUF_SIZE: usize = 512;

/// 接收环形缓冲: ISR (单生产者) 写 head, 应用 (单消费者) 读 tail
struct RxRing {
    buf: [u8; RX_BUF_SIZE],
    head: usize,
    tail: usize,
}

impl RxRing {
    const fn new() -> Self {
        Self {
            buf: [0; RX_BUF_SIZE],
            head: 0,
            tail: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        let next = (self.head + 1) % RX_BUF_SIZE;
        if next == self.tail {
            return; // 缓冲满: 丢弃新字节
        }
        self.buf[self.head] = byte;
        self.head = next;
    }

    fn pop(&mut self) -> Option<u8> {
        if self.head == self.tail {
            return None;
        }
        let b = self.buf[self.tail];
        self.tail = (self.tail + 1) % RX_BUF_SIZE;
        Some(b)
    }

    fn count(&self) -> usize {
        (self.head + RX_BUF_SIZE - self.tail) % RX_BUF_SIZE
    }
}

/// 各 USART 单元的接收环形缓冲容器
struct RxRingCell(core::cell::UnsafeCell<RxRing>);

// 访问路径: ISR (rx_irq_handler) 与 critical_section 保护的读侧,
// 无共享引用越权访问, Send/Sync 安全。
unsafe impl Sync for RxRingCell {}

static RX_RINGS: [RxRingCell; 4] = [
    RxRingCell(core::cell::UnsafeCell::new(RxRing::new())),
    RxRingCell(core::cell::UnsafeCell::new(RxRing::new())),
    RxRingCell(core::cell::UnsafeCell::new(RxRing::new())),
    RxRingCell(core::cell::UnsafeCell::new(RxRing::new())),
];

/// 接收中断 ISR (对齐 DDL 示例 USART_RxFull_IrqCallback + USART_RxError_IrqCallback)
///
/// - RXNE: 读 RDR 取数据入环形缓冲 (读 RDR 自动清 RXNE);
/// - 错误 (ORE/FE/PE): 读 RDR 丢弃出错字节, 写 CR1 清除位 (对齐
///   `USART_ClearStatus(USART_FLAG_PARITY_ERR|FRAME_ERR|OVERRUN)`)。
///
/// 仅做缓冲写入, 不调用任何 RTOS/打印 API (中断上下文安全)。
unsafe extern "C" fn rx_irq_handler<const U: u8>() {
    unsafe {
        let base = USART_BASES[U as usize - 1];
        let sr = core::ptr::read_volatile((base + 0x00) as *const u32);
        if sr & (SR_RXNE | SR_PE | SR_FE | SR_ORE) != 0 {
            // 读 RDR: 清 RXNE/ORE, 同时取出数据 (对齐示例先读再判错)
            let byte = core::ptr::read_volatile((base + 0x06) as *const u16) as u8;
            let ring = &mut *RX_RINGS[U as usize - 1].0.get();
            if sr & SR_RXNE != 0 {
                ring.push(byte);
            }
            if sr & (SR_PE | SR_FE | SR_ORE) != 0 {
                // 读-改-写 CR1 清除错误标志 (CPE/CFE/CORE, 对齐 USART_ClearStatus)
                let cr1 = core::ptr::read_volatile((base + 0x0C) as *const u32);
                core::ptr::write_volatile(
                    (base + 0x0C) as *mut u32,
                    cr1 | CR1_CPE | CR1_CFE | CR1_CORE,
                );
            }
        }
    }
}

impl<const U: u8> Uart<U> {
    /// 环形缓冲中待读取的字节数
    pub fn rx_count(&self) -> usize {
        crate::critical_section::with(|| unsafe { (*RX_RINGS[U as usize - 1].0.get()).count() })
    }

    /// 从环形缓冲非阻塞读取一个字节 (中断接收模式下使用)
    pub fn read_rx(&self) -> Option<u8> {
        crate::critical_section::with(|| unsafe { (*RX_RINGS[U as usize - 1].0.get()).pop() })
    }

    /// 从环形缓冲读取多个字节到 `buf`, 返回读取数量 (非阻塞)
    pub fn drain_rx(&self, buf: &mut [u8]) -> usize {
        let mut n = 0;
        while n < buf.len() {
            match self.read_rx() {
                Some(b) => {
                    buf[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// 读取 SR 状态寄存器 (诊断用)
    pub fn sr_value(&self) -> u32 {
        self.sr().read()
    }
}

/// 实现 `core::fmt::Write`, 使任意 UART 可作为格式化输出目标
/// (配合 `core::fmt::write` / `write_fmt` 使用, 见 `console` 模块的
/// `print!`/`println!` 宏)。
impl<const U: u8> core::fmt::Write for Uart<U> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write(s.as_bytes());
        Ok(())
    }
}

/// 计算 BRR 的 DIV_INTEGER/DIV_FRACTION 与小数使能标志
///
/// 8 位过采样: scale = 8; 16 位过采样: scale = 16
/// DIV_INT = C / (scale·B) - 1; 余数非零时用小数补偿:
/// FRAC = round(256·scale·(DIV_INT+1)·B / C) - 128
///
/// 实现为纯整数运算, 与 DDL UART_CalculateBrr 的结果一致。
fn calc_brr(
    usart_clk: u32,
    baudrate: u32,
    oversample: Oversample,
) -> Result<(u32, u32, bool), UartError> {
    let over8 = if oversample == Oversample::Eight {
        1
    } else {
        0
    };
    let scale = 8 * (2 - over8);

    let d = baudrate
        .checked_mul(scale)
        .ok_or(UartError::BaudrateUnsupported)?;

    let n = usart_clk / d; // DIV_INT + 1
    if n == 0 {
        return Err(UartError::BaudrateUnsupported); // 时钟过低
    }
    let div_int = n - 1;
    if div_int > BRR_DIV_INTEGER_MASK {
        return Err(UartError::BaudrateUnsupported); // DIV_INT 溢出
    }

    if usart_clk % d == 0 {
        // 精确波特率, 无需小数
        return Ok((div_int, 0, false));
    }

    // 小数补偿 (u64 防溢出): FRAC = round(256·scale·n·B / C) - 128
    let t = (256u64 * u64::from(scale) * u64::from(n) * u64::from(baudrate)
        + u64::from(usart_clk / 2))
        / u64::from(usart_clk);
    if t < 128 {
        return Err(UartError::BaudrateUnsupported);
    }
    let frac = t - 128;
    if frac <= BRR_DIV_FRACTION_MASK as u64 {
        return Ok((div_int, frac as u32, true));
    }

    // 小数越界: 整数舍入回退 (DDL 的 rounding-off 分支)
    let div_int_round = ((u64::from(usart_clk) * 10 + u64::from(d) * 5) / (u64::from(d) * 10)) - 1;
    if div_int_round > u64::from(BRR_DIV_INTEGER_MASK) {
        return Err(UartError::BaudrateUnsupported);
    }
    Ok((div_int_round as u32, 0, false))
}
