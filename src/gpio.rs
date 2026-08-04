//! HC32F460 GPIO 硬件抽象层 (HAL)
//!
//! 寄存器布局依据 `doc/chip/HC32F460.svd` (GPIO 外设, 基地址 `0x4005_3800`),
//! 功能说明参考 HC32F460 系列参考手册 Rev1.71 GPIO 章节。
//!
//! # 设计分层
//!
//! | 层 | 类型 | 职责 |
//! |----|------|------|
//! | 寄存器层 | [`Reg`] | 偏移量编码在类型中的零开销易失性寄存器 |
//! | 端口层 | [`Port`] trait + `PortA`~`PortH` | 端口的寄存器布局元数据 |
//! | 引脚层 | [`Pin<P, N>`] | 端口/引脚号编码在类型中, 越界在编译期报错 |
//! | 接口层 | [`OutputPin`] / [`InputPin`] | 引脚操作的抽象 trait |
//! | 所有权层 | [`Gpio`] | 句柄, 只能通过 `take()` 构造 |
//! | 保护层 | [`with_unlocked`] | PWPR 写保护自动解锁/加锁 + 临界区 |
//!
//! # 并发安全
//!
//! `Pin` 实现了 `Send + Sync`, 允许主循环与中断共享同一引脚。soundness
//! 论证如下:
//! - [`Pin::set_high`]/[`Pin::set_low`]/[`Pin::toggle`]: 写 POSR/PORR/POTR,
//!   硬件"写 1 生效"原子操作, 并发安全;
//! - [`Pin::configure`]: 解锁-写-加锁序列, 全部在 [`with_unlocked`] 的
//!   临界区 (PRIMASK) 内执行, 中断无法穿插;
//! - [`Pin::is_high`] 等读操作: 只读 PIDR, 无副作用。
//!
//! HAL 提供完整 API, 但应用往往只使用其中一部分,
//! 因此忽略未使用项 (输入/输出接口, 单个引脚方法等) 的死代码警告。
#![allow(dead_code)]

use core::marker::PhantomData;

/// GPIO 外设基地址 (SVD: GPIO.baseAddress)
const GPIO_BASE: usize = 0x4005_3800;

/// PWPR 解除写保护: WE=1, WP=0xA5 (与 DDL GPIO_REG_UNLOCK_KEY 一致)
const PWPR_UNLOCK: u16 = 0xA501;
/// PWPR 恢复写保护: WE=0, WP=0xA5 (与 DDL GPIO_REG_LOCK_KEY 一致)
const PWPR_LOCK: u16 = 0xA500;

/// 端口数据寄存器组内各寄存器偏移 (每组 0x10 字节, 16 位寄存器)
const PIDR_OFF: usize = 0x00; // 输入数据 (只读)
const PODR_OFF: usize = 0x04; // 输出数据
const POER_OFF: usize = 0x06; // 输出使能
const POSR_OFF: usize = 0x08; // 置位 (写 1)
const PORR_OFF: usize = 0x0A; // 复位 (写 1)
const POTR_OFF: usize = 0x0C; // 翻转 (写 1)

/// 引脚控制寄存器 PCR 字段位 (SVD: PCRA0, resetMask 0xD377)
const PCR_POUT: u16 = 1 << 0; // 输出数据
const PCR_POUTE: u16 = 1 << 1; // 输出使能
const PCR_NOD: u16 = 1 << 2; // 开漏
const PCR_DRV_SHIFT: u16 = 4; // 驱动能力 (2 位)
const PCR_DRV_MASK: u16 = 0x30;
const PCR_PUU: u16 = 1 << 6; // 内部上拉
const PCR_INVE: u16 = 1 << 9; // 输入反相
const PCR_DDIS: u16 = 1 << 15; // 关闭数字输入 (模拟模式)

// ================================ 功能复用号 (表 2-2) ================================

/// 周边复用功能号 (PFSR.FSEL, 数据手册表 2-2, 与 DDL 的 GPIO_FUNC_* 一致)。
///
/// Func32~63 按引脚的 **Func_Grp1/Grp2 分组**映射 (组由硬件固定, 与引脚
/// 相关): 同一功能号在不同引脚可能对应不同外设, 使用前请查数据手册
/// "引脚功能表"。USART1/2 在 Grp1 (32~47), USART3/4 在 Grp2 (48~63)。
pub mod func {
    // ---- Func_Grp1: USART1 / USART2 / SPI1 / SPI2 ----
    /// USART1_TX (如 PA9, PH1)
    pub const USART1_TX: u8 = 32;
    /// USART1_RX (如 PA10, PC1)
    pub const USART1_RX: u8 = 33;
    /// USART1_RTS
    pub const USART1_RTS: u8 = 34;
    /// USART1_CTS
    pub const USART1_CTS: u8 = 35;
    /// USART2_TX
    pub const USART2_TX: u8 = 36;
    /// USART2_RX
    pub const USART2_RX: u8 = 37;
    /// USART2_RTS
    pub const USART2_RTS: u8 = 38;
    /// USART2_CTS
    pub const USART2_CTS: u8 = 39;
    /// SPI1_MOSI
    pub const SPI1_MOSI: u8 = 40;
    /// SPI1_MISO
    pub const SPI1_MISO: u8 = 41;
    /// SPI1_SS0
    pub const SPI1_SS0: u8 = 42;
    /// SPI1_SCK
    pub const SPI1_SCK: u8 = 43;
    /// SPI2_MOSI
    pub const SPI2_MOSI: u8 = 44;
    /// SPI2_MISO
    pub const SPI2_MISO: u8 = 45;
    /// SPI2_SS0
    pub const SPI2_SS0: u8 = 46;
    /// SPI2_SCK
    pub const SPI2_SCK: u8 = 47;

    // ---- Func_Grp2: USART3 / USART4 / SPI3 / SPI4 ----
    /// USART3_TX
    pub const USART3_TX: u8 = 48;
    /// USART3_RX
    pub const USART3_RX: u8 = 49;
    /// USART3_RTS
    pub const USART3_RTS: u8 = 50;
    /// USART3_CTS
    pub const USART3_CTS: u8 = 51;
    /// USART4_TX
    pub const USART4_TX: u8 = 52;
    /// USART4_RX
    pub const USART4_RX: u8 = 53;
    /// USART4_RTS
    pub const USART4_RTS: u8 = 54;
    /// USART4_CTS
    pub const USART4_CTS: u8 = 55;
    /// SPI3_MOSI
    pub const SPI3_MOSI: u8 = 56;
    /// SPI3_MISO
    pub const SPI3_MISO: u8 = 57;
    /// SPI3_SS0
    pub const SPI3_SS0: u8 = 58;
    /// SPI3_SCK
    pub const SPI3_SCK: u8 = 59;
    /// SPI4_MOSI
    pub const SPI4_MOSI: u8 = 60;
    /// SPI4_MISO
    pub const SPI4_MISO: u8 = 61;
    /// SPI4_SS0
    pub const SPI4_SS0: u8 = 62;
    /// SPI4_SCK
    pub const SPI4_SCK: u8 = 63;
}

// ================================ 寄存器层 ================================

/// 内存映射寄存器。
///
/// 偏移量在单态化后是常量表达式 (如 `P::PCR_OFFSET + N as usize * 4`),
/// LLVM 会将其折叠为直接的易失性 load/store, 零运行时开销。
struct Reg<T> {
    offset: usize,
    _marker: PhantomData<T>,
}

impl<T> Reg<T> {
    const fn new(offset: usize) -> Self {
        Self {
            offset,
            _marker: PhantomData,
        }
    }

    fn addr(&self) -> *mut T {
        (GPIO_BASE + self.offset) as *mut T
    }
}

impl<T: Copy> Reg<T> {
    /// 读取寄存器
    fn read(&self) -> T {
        unsafe { core::ptr::read_volatile(self.addr()) }
    }

    /// 写入寄存器
    fn write(&self, value: T) {
        unsafe { core::ptr::write_volatile(self.addr(), value) }
    }

    /// 读-改-写寄存器
    fn modify(&self, f: impl FnOnce(T) -> T) {
        self.write(f(self.read()));
    }
}

/// PWPR: 端口写保护控制寄存器
fn pwpr() -> Reg<u16> {
    Reg::new(0x3FC)
}

/// 在解除 PWPR 写保护的状态下执行 `f`, 完成后立即恢复写保护。
///
/// 受 PWPR 保护的寄存器: PSPCR / PCCR / PINAER / PCR / PFSR 等,
/// 写保护未解除时对这些寄存器的写入会被硬件忽略。
///
/// # 中断安全
///
/// 整个解锁-执行-加锁窗口处于临界区 ([`crate::critical_section`]) 内:
/// - 若中断在此窗口内也操作 GPIO, 嵌套的解锁/加锁会使主循环的写入
///   被中断的加锁"提前锁死"而静默丢弃, 因此窗口内禁止中断;
/// - 临界区嵌套安全: 外层已处于临界区时, 内层不会重新开中断。
fn with_unlocked<T>(f: impl FnOnce() -> T) -> T {
    crate::critical_section::with(|_| {
        pwpr().write(PWPR_UNLOCK);
        let result = f();
        pwpr().write(PWPR_LOCK);
        result
    })
}

// ================================ 端口层 ================================

mod sealed {
    pub trait Sealed {}
}

/// 端口寄存器布局元数据。
///
/// 每个端口在 GPIO 外设中占用两组寄存器:
/// - 端口数据寄存器组 (PIDR/PODR/POER/POSR/PORR/POTR), 每组 0x10 字节;
/// - 引脚控制寄存器组 (PCRn/PFSRn), 每个引脚占 4 字节。
pub trait Port: sealed::Sealed {
    /// 端口数据寄存器组偏移 (PIDR 相对 GPIO 基地址)
    const DATA_OFFSET: usize;
    /// 引脚控制寄存器组基址 (PCR0 相对 GPIO 基地址)
    const PCR_OFFSET: usize;
    /// 该端口在 JEUA (LQFP48) 封装上的引脚存在性表 `[pin0 .. pin15]`
    ///
    /// 引脚表见数据手册表 2-1: JEUA 共 38 个 GPIO
    /// (PA0~15, PB0~15, PC13~15, PH0~2), 其余引脚不存在。
    const VALID_PINS: [bool; 16];
}

macro_rules! port {
    ($name:ident, $data:expr, $pcr:expr, $valid:expr) => {
        /// 端口标记类型
        pub struct $name;

        impl sealed::Sealed for $name {}

        impl Port for $name {
            const DATA_OFFSET: usize = $data;
            const PCR_OFFSET: usize = $pcr;
            const VALID_PINS: [bool; 16] = $valid;
        }
    };
}

/// 仅引脚 13~15 存在 (PortC: PC13~PC15)
const PC_PINS: [bool; 16] = {
    let mut t = [false; 16];
    t[13] = true;
    t[14] = true;
    t[15] = true;
    t
};

/// 仅引脚 0~2 存在 (PortH: PH0~PH2)
const PH_PINS: [bool; 16] = {
    let mut t = [false; 16];
    t[0] = true;
    t[1] = true;
    t[2] = true;
    t
};

// JEUA (LQFP48) 封装引脚表 (数据手册表 2-1):
// 仅 PA0~15、PB0~15、PC13~15、PH0~2 存在, 共 38 个 GPIO
port!(PortA, 0x000, 0x400, [true; 16]);
port!(PortB, 0x010, 0x440, [true; 16]);
port!(PortC, 0x020, 0x480, PC_PINS);
port!(PortD, 0x030, 0x4C0, [false; 16]); // JEUA 无 PD 引脚
port!(PortE, 0x040, 0x500, [false; 16]); // JEUA 无 PE 引脚
port!(PortH, 0x050, 0x540, PH_PINS);

// ================================ 引脚层 ================================

/// 输出电平
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// 低电平
    Low,
    /// 高电平
    High,
}

/// 引脚工作模式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 数字输入
    Input,
    /// 推挽输出
    Output,
    /// 开漏输出 (PCR.NOD=1)
    OpenDrain,
    /// 模拟功能 (PCR.DDIS=1, 关闭数字输入缓冲)
    Analog,
}

/// 驱动能力 (PCR.DRV), 编码与 DDL 的 PIN_LOW_DRV/PIN_MID_DRV/PIN_HIGH_DRV 一致
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Drive {
    /// 低速
    Low = 0b00,
    /// 中速
    Medium = 0b01,
    /// 高速
    High = 0b10,
}

/// 引脚配置
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// 工作模式
    pub mode: Mode,
    /// 内部上拉使能 (PCR.PUU=1)。
    /// 注: HC32F460 无内部下拉 (无 PUD 位), 下拉需外部电阻。
    pub pull_up: bool,
    /// 驱动能力 (PCR.DRV, 仅输出模式有效)
    pub drive: Drive,
    /// 配置完成时的初始输出电平 (PCR.POUT, 仅输出模式有效)。
    /// 输出数据位与输出使能位同一次写入, 使能瞬间即为目标电平, 无毛刺。
    pub initial_level: Level,
    /// 输入反相 (PCR.INVE, 对齐 DDL GPIO_Init 的 u16Invert)
    pub invert: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Input,
            pull_up: false,
            drive: Drive::Medium,
            initial_level: Level::Low,
            invert: false,
        }
    }
}

/// 引脚类型: 端口 `P` 与引脚号 `N` 均编码在类型系统中, 零运行时开销。
pub struct Pin<P: Port, const N: u8> {
    _port: PhantomData<P>,
}

impl<P: Port, const N: u8> Clone for Pin<P, N> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<P: Port, const N: u8> Copy for Pin<P, N> {}

unsafe impl<P: Port, const N: u8> Send for Pin<P, N> {}
unsafe impl<P: Port, const N: u8> Sync for Pin<P, N> {}

impl<P: Port, const N: u8> Pin<P, N> {
    /// 创建引脚。
    ///
    /// 当以 `const` 方式使用 (如 `const LED: Pin<PortC, 13> = Pin::new();`)
    /// 时, 引脚不存在于 JEUA 封装会在编译期报错。
    pub const fn new() -> Self {
        assert!(
            P::VALID_PINS[N as usize],
            "pin does not exist on the JEUA package"
        );
        Self { _port: PhantomData }
    }

    /// 输入数据寄存器 PIDR (只读, 实时引脚电平)
    fn pidr(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + PIDR_OFF)
    }
    /// 输出数据寄存器 PODR
    fn podr(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + PODR_OFF)
    }
    /// 输出使能寄存器 POER
    fn poer(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + POER_OFF)
    }
    /// 置位寄存器 POSR (写 1 输出高电平, 原子操作)
    fn posr(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + POSR_OFF)
    }
    /// 复位寄存器 PORR (写 1 输出低电平, 原子操作)
    fn porr(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + PORR_OFF)
    }
    /// 翻转寄存器 POTR (写 1 翻转电平, 原子操作)
    fn potr(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + POTR_OFF)
    }
    /// 引脚控制寄存器 PCRn
    fn pcr(&self) -> Reg<u16> {
        Reg::new(P::PCR_OFFSET + N as usize * 4)
    }
    /// 引脚功能选择寄存器 PFSRn
    fn pfsr(&self) -> Reg<u16> {
        Reg::new(P::PCR_OFFSET + N as usize * 4 + 2)
    }

    /// 选择周边复用功能 (PFSR.FSEL)
    ///
    /// FSEL=0 为通用输出 GPO (默认)。具体功能号见数据手册"引脚功能表":
    /// - Func0~15 各引脚独立;
    /// - Func32~63 按引脚的 Func_Grp1/Grp2 分组映射 (表 2-2), 组由硬件固定。
    ///
    /// 复用功能引脚的输出/输入由外设驱动, 无需配置 PCR (POUTE/DDIS 等保持默认)。
    pub fn set_func(&self, fsel: u8) {
        with_unlocked(|| self.pfsr().write(fsel as u16));
    }

    /// 按配置初始化引脚: 选择 GPIO 功能 (PFSR.FSEL=0) 并设置模式/上拉/驱动/
    /// 初始电平/反相。
    ///
    /// 寄存器值对齐 DDL 的 GPIO_Init (PCR 的 POUT/POUTE/NOD/DRV/PUU/DDIS/INVE,
    /// 不设置 INTE/LTE —— 它们属于外部中断/输出锁存功能)。
    ///
    /// 注意: 本方法**重置**该引脚的 PCR 全部可写位 (INTE/LTE 等会被清零),
    /// 适用于引脚生命周期内的一次性初始化。
    pub fn configure(&self, config: Config) {
        let bit = 1u16 << N;
        let output = matches!(config.mode, Mode::Output | Mode::OpenDrain);

        with_unlocked(|| {
            // PFSR.FSEL = 0: 选择 GPIO 功能
            self.pfsr().write(0);

            // POUT(输出数据) 与 POUTE(输出使能) 同一次写入, 使能瞬间即为目标电平
            self.pcr().write(build_pcr_value(config));

            // 输出使能位同步到 POER
            if output {
                self.poer().modify(|v| v | bit);
            } else {
                self.poer().modify(|v| v & !bit);
            }
        });
    }

    /// 输出高电平 (POSR 写 1, 原子操作)
    ///
    /// POSR/PORR/POTR 是"写 1 生效"的数据寄存器, **不受 PWPR 保护**,
    /// 单次 volatile 写天然原子, 无需临界区/解锁 (对齐 DDL `GPIO_SetPins`)。
    pub fn set_high(&self) {
        self.posr().write(1u16 << N);
    }

    /// 输出低电平 (PORR 写 1, 原子操作)
    pub fn set_low(&self) {
        self.porr().write(1u16 << N);
    }

    /// 翻转输出电平 (POTR 写 1, 原子操作)
    pub fn toggle(&self) {
        self.potr().write(1u16 << N);
    }

    /// 读取引脚实时输入电平 (PIDR)
    pub fn is_high(&self) -> bool {
        self.pidr().read() & (1u16 << N) != 0
    }

    /// 引脚是否为低电平
    pub fn is_low(&self) -> bool {
        !self.is_high()
    }

    /// 读取引脚电平
    pub fn level(&self) -> Level {
        if self.is_high() {
            Level::High
        } else {
            Level::Low
        }
    }

    /// 设置输出电平
    pub fn set_level(&self, level: Level) {
        match level {
            Level::High => self.set_high(),
            Level::Low => self.set_low(),
        }
    }

    /// 使能/失能输出 (POER, 对齐 DDL `GPIO_OutputCmd`)。
    /// 失能后引脚呈高阻, 可用于总线分时复用 (如共享数据线)。
    pub fn set_output_enable(&self, enable: bool) {
        set_output_enable_port::<P>(1u16 << N, enable);
    }

    /// 读取引脚输出状态 (PODR, 对齐 DDL `GPIO_ReadOutputPins`)。
    /// 注意与 [`Pin::is_high`] (实时输入 PIDR) 的差别: 输入被外设占用/
    /// 引脚外部钳位时, 两者可能不同。
    pub fn output_is_high(&self) -> bool {
        Reg::<u16>::new(P::DATA_OFFSET + PODR_OFF).read() & (1u16 << N) != 0
    }

    /// 读取引脚输出电平 (PODR)
    pub fn output_level(&self) -> Level {
        if self.output_is_high() {
            Level::High
        } else {
            Level::Low
        }
    }
}

// ================================ 端口级操作 (DDL 对齐) ================================

/// 读取整个端口的实时输入状态 (PIDR, 对齐 DDL `GPIO_ReadInputPort`)
pub fn read_input_port<P: Port>() -> u16 {
    Reg::<u16>::new(P::DATA_OFFSET + PIDR_OFF).read()
}

/// 读取整个端口的输出状态 (PODR, 对齐 DDL `GPIO_ReadOutputPort`)
pub fn read_output_port<P: Port>() -> u16 {
    Reg::<u16>::new(P::DATA_OFFSET + PODR_OFF).read()
}

/// 写入整个端口的输出数据 (PODR, 对齐 DDL `GPIO_WritePort`)
pub fn write_output_port<P: Port>(value: u16) {
    with_unlocked(|| Reg::<u16>::new(P::DATA_OFFSET + PODR_OFF).write(value));
}

/// 端口输出使能/失能 (POER, 对齐 DDL `GPIO_OutputCmd`)
///
/// `mask` 为引脚位掩码 (bit N = 引脚 N), 仅影响掩码位。
pub fn set_output_enable_port<P: Port>(mask: u16, enable: bool) {
    with_unlocked(|| {
        let poer = Reg::<u16>::new(P::DATA_OFFSET + POER_OFF);
        if enable {
            poer.modify(|v| v | mask);
        } else {
            poer.modify(|v| v & !mask);
        }
    });
}

/// 由配置构造 PCR 寄存器值。
///
/// 输出数据位 (POUT) 与输出使能位 (POUTE) 在同一寄存器中一次性写入,
/// 保证输出使能生效瞬间引脚即为目标电平。
fn build_pcr_value(config: Config) -> u16 {
    let output = matches!(config.mode, Mode::Output | Mode::OpenDrain);

    let mut pcr = 0u16;
    if config.initial_level == Level::High {
        pcr |= PCR_POUT; // 输出数据
    }
    if output {
        pcr |= PCR_POUTE; // 输出使能
    }
    if config.pull_up {
        pcr |= PCR_PUU; // 内部上拉
    }
    debug_assert!(
        (config.drive as u16) & !(PCR_DRV_MASK >> PCR_DRV_SHIFT) == 0,
        "drive 编码越界"
    );
    pcr |= (config.drive as u16) << PCR_DRV_SHIFT; // 驱动能力
    if config.mode == Mode::OpenDrain {
        pcr |= PCR_NOD; // 开漏
    }
    if config.mode == Mode::Analog {
        pcr |= PCR_DDIS; // 关闭数字输入
    }
    if config.invert {
        pcr |= PCR_INVE; // 输入反相
    }
    pcr
}

// ================================ 接口层 ================================/// 输出引脚操作接口, 便于应用代码针对接口编程
pub trait OutputPin {
    /// 输出高电平
    fn set_high(&self);
    /// 输出低电平
    fn set_low(&self);
    /// 翻转输出电平
    fn toggle(&self);
}

/// 输入引脚操作接口
pub trait InputPin {
    /// 引脚是否为高电平
    fn is_high(&self) -> bool;
    /// 引脚是否为低电平
    fn is_low(&self) -> bool {
        !self.is_high()
    }
}

impl<P: Port, const N: u8> OutputPin for Pin<P, N> {
    fn set_high(&self) {
        self.set_high();
    }
    fn set_low(&self) {
        self.set_low();
    }
    fn toggle(&self) {
        self.toggle();
    }
}

impl<P: Port, const N: u8> InputPin for Pin<P, N> {
    fn is_high(&self) -> bool {
        self.is_high()
    }
    fn is_low(&self) -> bool {
        self.is_low()
    }
}

// ================================ 所有权层 ================================

/// GPIO 外设句柄。
///
/// 私有字段 `_private` 保证 `Gpio` 只能通过 [`Gpio::take`] 构造。
///
/// 注意: 单线程裸机环境不存在并发竞争, 句柄唯一性由类型系统保证,
/// 因此不做运行时占用检查。早期版本用 `AtomicBool` 作为全局占用标记,
/// 但那引入了对 `.bss` 段清零的依赖 —— 本工程的启动代码此前从未验证过
/// `.bss` 初始化, 该标记未清零时 `take()` 会失败, 加上 `expect` 在
/// 无输出的裸机上表现为静默死循环, 导致 LED 无法点亮。
pub struct Gpio {
    _private: (),
}

impl Gpio {
    /// 获取 GPIO 句柄
    pub fn take() -> Self {
        Self { _private: () }
    }

    /// 创建一个引脚
    pub fn pin<P: Port, const N: u8>(&self) -> Pin<P, N> {
        Pin::new()
    }
}
