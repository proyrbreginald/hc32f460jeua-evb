//! HC32F460 GPIO 硬件抽象层 (HAL)
//!
//! 寄存器布局依据 `doc/chip/HC32F460.svd` (GPIO 外设, 基地址 `0x4005_3800`),
//! 功能说明参考 HC32F460 系列参考手册 Rev1.71 GPIO 章节。
//!
//! # 设计分层
//!
//! | 层 | 类型 | 职责 |
//! |----|------|------|
//! | 寄存器层 | [`Reg`] | 偏移量编码在 const 泛型中的零开销易失性寄存器 |
//! | 端口层 | [`Port`] trait + `PortA`~`PortH` | 端口的寄存器布局元数据 |
//! | 引脚层 | [`Pin<P, N>`] | 端口/引脚号编码在类型中, 越界在编译期报错 |
//! | 接口层 | [`OutputPin`] / [`InputPin`] | 引脚操作的抽象 trait |
//! | 所有权层 | [`Gpio`] | 单例句柄, `take()` 保证全局独占 |
//! | 保护层 | [`with_unlocked`] | PWPR 写保护自动解锁/加锁, 应用层无感 |
//!
//! HAL 提供完整 API, 但应用往往只使用其中一部分,
//! 因此忽略未使用项 (输入/输出接口, 单个引脚方法等) 的死代码警告。
#![allow(dead_code)]

use core::marker::PhantomData;

/// GPIO 外设基地址 (SVD: GPIO.baseAddress)
const GPIO_BASE: usize = 0x4005_3800;

/// PWPR 解除写保护: WE=1, WP=0xA5
/// PWPR 解除写保护: WE=1, WP=0xA5 (与 DDL GPIO_REG_UNLOCK_KEY 一致)
const PWPR_UNLOCK: u16 = 0xA501;
/// PWPR 恢复写保护: WE=0, WP=0xA5 (与 DDL GPIO_REG_LOCK_KEY 一致)
const PWPR_LOCK: u16 = 0xA500;

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

/// PWPR: 端口写保护控制寄存器 (偏移 0x3FC)
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
/// 整个解锁-执行-加锁窗口处于临界区 (PRIMASK 关中断) 内:
/// - 若中断在此窗口内也操作 GPIO, 嵌套的解锁/加锁会使主循环的写入
///   被中断的加锁"提前锁死"而静默丢弃, 因此窗口内禁止中断;
/// - 进入时保存 PRIMASK, 退出时原样恢复 (嵌套中断环境安全)。
fn with_unlocked<T>(f: impl FnOnce() -> T) -> T {
    // 保存 PRIMASK 并关中断
    let primask: u32;
    unsafe {
        core::arch::asm!("mrs {}, primask", out(reg) primask);
        core::arch::asm!("cpsid i");
    }

    pwpr().write(PWPR_UNLOCK);
    let result = f();
    pwpr().write(PWPR_LOCK);

    // 恢复中断状态: 仅当进入前中断开启时才重新开启
    if primask & 1 == 0 {
        unsafe {
            core::arch::asm!("cpsie i");
        }
    }
    result
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
    /// 该端口实际可用的引脚数 (不同封装引脚数不同)
    const PIN_COUNT: u8;
}

macro_rules! port {
    ($name:ident, $data:expr, $pcr:expr, $count:expr) => {
        /// 端口标记类型
        pub struct $name;

        impl sealed::Sealed for $name {}

        impl Port for $name {
            const DATA_OFFSET: usize = $data;
            const PCR_OFFSET: usize = $pcr;
            const PIN_COUNT: u8 = $count;
        }
    };
}

port!(PortA, 0x000, 0x400, 16);
port!(PortB, 0x010, 0x440, 16);
port!(PortC, 0x020, 0x480, 16);
port!(PortD, 0x030, 0x4C0, 16);
port!(PortE, 0x040, 0x500, 16);
port!(PortH, 0x050, 0x540, 3); // JEUA 封装仅有 PH0~PH2

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
    /// 内部上拉使能 (PCR.PUU=1)
    pub pull_up: bool,
    /// 驱动能力 (PCR.DRV, 仅输出模式有效)
    pub drive: Drive,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Input,
            pull_up: false,
            drive: Drive::Medium,
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
    /// 时, 引脚号越界会在编译期报错。
    pub const fn new() -> Self {
        assert!(N < P::PIN_COUNT, "pin index out of range");
        Self { _port: PhantomData }
    }

    /// 输入数据寄存器 PIDR (只读, 实时引脚电平)
    fn pidr(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + 0x00)
    }
    /// 输出数据寄存器 PODR
    fn podr(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + 0x04)
    }
    /// 输出使能寄存器 POER
    fn poer(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + 0x06)
    }
    /// 置位寄存器 POSR (写 1 输出高电平, 原子操作)
    fn posr(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + 0x08)
    }
    /// 复位寄存器 PORR (写 1 输出低电平, 原子操作)
    fn porr(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + 0x0A)
    }
    /// 翻转寄存器 POTR (写 1 翻转电平, 原子操作)
    fn potr(&self) -> Reg<u16> {
        Reg::new(P::DATA_OFFSET + 0x0C)
    }
    /// 引脚控制寄存器 PCRn
    fn pcr(&self) -> Reg<u16> {
        Reg::new(P::PCR_OFFSET + N as usize * 4)
    }
    /// 引脚功能选择寄存器 PFSRn
    fn pfsr(&self) -> Reg<u16> {
        Reg::new(P::PCR_OFFSET + N as usize * 4 + 2)
    }

    /// 按配置初始化引脚: 选择 GPIO 功能 (PFSR.FSEL=0) 并设置模式/上拉/驱动。
    ///
    /// 寄存器值对齐 DDL 的 GPIO_Init (PCR 的 POUT/POUTE/NOD/DRV/PUU/DDIS,
    /// 不设置 INTE/LTE/INVE —— 它们属于外部中断/锁存/输入反相功能)。
    pub fn configure(&self, config: Config) {
        let bit = 1u16 << N;
        let output = matches!(config.mode, Mode::Output | Mode::OpenDrain);

        with_unlocked(|| {
            // PFSR.FSEL = 0: 选择 GPIO 功能
            self.pfsr().write(0);

            // 构造 PCR 值
            let mut pcr = 0u16;
            if output {
                pcr |= 1 << 1; // POUTE: 输出使能
            }
            if config.pull_up {
                pcr |= 1 << 6; // PUU: 内部上拉
            }
            pcr |= (config.drive as u16) << 4; // DRV: 驱动能力
            if config.mode == Mode::OpenDrain {
                pcr |= 1 << 2; // NOD: 开漏
            }
            if config.mode == Mode::Analog {
                pcr |= 1 << 15; // DDIS: 关闭数字输入
            }
            self.pcr().write(pcr);

            // 输出使能位同步到 POER
            if output {
                self.poer().modify(|v| v | bit);
            } else {
                self.poer().modify(|v| v & !bit);
            }
        });
    }

    /// 输出高电平 (POSR 写 1, 原子操作)
    pub fn set_high(&self) {
        with_unlocked(|| self.posr().write(1u16 << N));
    }

    /// 输出低电平 (PORR 写 1, 原子操作)
    pub fn set_low(&self) {
        with_unlocked(|| self.porr().write(1u16 << N));
    }

    /// 翻转输出电平 (POTR 写 1, 原子操作)
    pub fn toggle(&self) {
        with_unlocked(|| self.potr().write(1u16 << N));
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
}

// ================================ 接口层 ================================

/// 输出引脚操作接口, 便于应用代码针对接口编程
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
