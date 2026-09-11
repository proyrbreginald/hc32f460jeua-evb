//! HC32F460JEUA 板级支持 (新板: 12MHz XTAL / USART3 控制台 / 三路 LED)
//!
//! 本模块集中描述开发板资源及其初始化顺序。SoC 驱动只提供硬件能力，
//! 应用通过 [`Board`] / [`BoardResources`] 使用板载 LED 和控制台，避免
//! 在应用入口中直接绑定端口、引脚及外设实例。

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::gpio::{Config, Drive, Level, Mode, Pin, Port, PortB, PortC, PortH};
use crate::uart::UartConfig;

/// 板载 LED (均为 PortB): WORK=运行心跳, SUCCESS=启动完成, ERROR=故障。
/// 点亮极性由 `CFG_LED_ACTIVE_LEVEL` 决定 (本板高电平点亮)。
type LedWork = Pin<PortB, { crate::config::LED_WORK_PIN }>;
type LedSuccess = Pin<PortB, { crate::config::LED_SUCCESS_PIN }>;
type LedError = Pin<PortB, { crate::config::LED_ERROR_PIN }>;

/// 板载**外部**硬件看门狗引脚 (均为 PortB, 见 `CFG_HWDT_*`):
/// 使能脚高电平禁用/低电平使能, 喂狗脚每次喂狗翻转一次电平。
type HwdtEnablePin = Pin<PortB, { crate::config::HWDT_ENABLE_PIN }>;
type HwdtFeedPin = Pin<PortB, { crate::config::HWDT_FEED_PIN }>;

/// 板载 LED 标识 (shell `led` 命令与板级指示 API 共用)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Led {
    /// 运行心跳 (由 LED 线程周期翻转)
    Work,
    /// 启动完成 (系统初始化成功后常亮)
    Success,
    /// 故障 (panic/fault 或自检/soak 失败时常亮)
    Error,
}

impl Led {
    /// 名称 (shell 命令与日志显示)
    pub const fn name(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Success => "success",
            Self::Error => "error",
        }
    }
}

/// 板载 LED 点亮时的输出电平 (极性来自 `CFG_LED_ACTIVE_LEVEL`)
const fn led_on_level() -> Level {
    if crate::config::LED_ACTIVE_HIGH {
        Level::High
    } else {
        Level::Low
    }
}

/// 板载 LED 熄灭时的输出电平
const fn led_off_level() -> Level {
    if crate::config::LED_ACTIVE_HIGH {
        Level::Low
    } else {
        Level::High
    }
}

/// 按点亮极性写 LED 电平 (泛型以覆盖三路 LED 各自的 const 泛型引脚类型)
fn set_led_pin<P: Port, const N: u8>(pin: Pin<P, N>, on: bool) {
    pin.set_level(if on { led_on_level() } else { led_off_level() });
}

/// 读 LED 是否点亮 (PODR 输出电平 + 点亮极性)
fn led_pin_is_on<P: Port, const N: u8>(pin: Pin<P, N>) -> bool {
    (pin.output_level() == Level::High) == crate::config::LED_ACTIVE_HIGH
}

/// 上电初始化一路 LED: 推挽输出、初始熄灭 (泛型以覆盖各自的引脚类型)
fn configure_led<P: Port, const N: u8>(pin: Pin<P, N>) {
    pin.configure(Config {
        mode: Mode::Output,
        pull_up: false,
        drive: Drive::Low,
        initial_level: led_off_level(),
        invert: false,
    });
}

/// panic/fault 早期故障指示: 点亮 ERROR LED, 不依赖 [`BoardResources`] 是否已初始化。
///
/// 直接按编译期配置构造引脚并配置为输出 (Board::init 之前发生的故障也能点亮),
/// 只做 GPIO 寄存器写入与 PWPR 解锁, 不分配、不加锁、不依赖调度器, 因此可在
/// 异常上下文调用。注意 `CFG_PANIC_STRATEGY=reset` 时软复位会重新初始化 GPIO,
/// ERROR 常亮仅在 `halt` 策略下保持。
pub fn indicate_fault_early() {
    let pin = Pin::<PortB, { crate::config::LED_ERROR_PIN }>::new();
    pin.configure(Config {
        mode: Mode::Output,
        pull_up: false,
        drive: Drive::Low,
        initial_level: led_off_level(),
        invert: false,
    });
    pin.set_level(led_on_level());
}

/// HC32F460JEUA-EVB 板级入口。
pub struct Board {
    peripherals: crate::peripherals::Peripherals,
}

/// 初始化后可交给应用使用的板级资源。
///
/// 只持有应用实际消费的能力 (LED/控制台/CAN/时钟快照); DMA/CRC 句柄
/// 是零大小能力 token, 一次性获取 (`Peripherals::take` 的位图/CAS) 后
/// 运行路径按编译期配置重建等价句柄, 无需在此常驻。
pub struct BoardResources {
    led_work: LedWork,
    led_success: LedSuccess,
    led_error: LedError,
    hwdt_enable_pin: HwdtEnablePin,
    hwdt_feed_pin: HwdtFeedPin,
    console: crate::config::ConsoleUart,
    can: crate::can::Can,
    clocks: crate::clk::Clocks,
}

static WDT_FEED_MAX_GAP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

struct ResourceSlot(UnsafeCell<MaybeUninit<BoardResources>>);

// The slot is written exactly once before BOARD_READY is published.
unsafe impl Sync for ResourceSlot {}

static RESOURCES: ResourceSlot = ResourceSlot(UnsafeCell::new(MaybeUninit::uninit()));
static BOARD_READY: AtomicBool = AtomicBool::new(false);

impl Board {
    /// 获取板级入口。全系统仅第一次调用成功 (由
    /// [`crate::peripherals::Peripherals::take`] 的一次性 CAS 保证)。
    pub fn take() -> Option<Self> {
        crate::peripherals::Peripherals::take().map(|peripherals| Self { peripherals })
    }

    /// 初始化开发板并返回静态板级资源。
    ///
    /// 顺序保持为：时钟 -> MPU -> GPIO -> SysTick -> UART -> CAN -> RTC。
    pub fn init(self) -> &'static BoardResources {
        let crate::peripherals::Peripherals {
            gpio,
            console,
            can,
            dma_tx,
            dma_copy,
            crc,
            clocks,
        } = self.peripherals;
        // 板载外部看门狗: 复位后使能脚为输入态, 板上看门狗默认处于使能, 因此
        // 在**时钟初始化之前**就把使能脚拉高禁用 (PB4 高=禁用), 避免 XTAL 起振
        // + PLL 锁定期间被外部看门狗复位 (GPIO 不受 FCG 门控, 此时可安全配置)。
        let hwdt_enable = gpio.pin::<PortB, { crate::config::HWDT_ENABLE_PIN }>();
        hwdt_enable.configure(Config {
            mode: Mode::Output,
            pull_up: false,
            drive: Drive::Low,
            initial_level: Level::High, // 高 = 禁用 (安全默认)
            invert: false,
        });
        let hwdt_feed = gpio.pin::<PortB, { crate::config::HWDT_FEED_PIN }>();
        hwdt_feed.configure(Config {
            mode: Mode::Output,
            pull_up: false,
            drive: Drive::Low,
            initial_level: Level::Low,
            invert: false,
        });

        let clocks = clocks.freeze();

        // DMA/CRC 句柄为 ZST 能力 token: 独占获取已在 `Peripherals::take`
        // 固化 (DMA 通道位图 / 构造入口收紧), 运行路径 (dma.rs::uart_tx_try
        // / copy_try) 按编译期配置重建等价句柄。此处只确认核心通道未被
        // 重复获取, 句柄随即丢弃。
        let _ = dma_tx.expect("DMA TX 通道已被重复获取");
        let _ = dma_copy.expect("DMA COPY 通道已被重复获取");
        let _ = crc;

        let resources = BoardResources {
            led_work: gpio.pin::<PortB, { crate::config::LED_WORK_PIN }>(),
            led_success: gpio.pin::<PortB, { crate::config::LED_SUCCESS_PIN }>(),
            led_error: gpio.pin::<PortB, { crate::config::LED_ERROR_PIN }>(),
            hwdt_enable_pin: hwdt_enable,
            hwdt_feed_pin: hwdt_feed,
            console,
            can,
            clocks,
        };

        // 振荡器/PLL 失败时硬件仍保持或回退到可用源。错误随冻结后的
        // Clocks 保存，后续外设一律按该实际快照计算分频。
        let clock_error = resources.clocks.error();

        // 硬实时指标测量 (DWT): 时钟就绪后立即初始化, 之后的临界区与
        // 节拍中断即进入测量范围
        crate::latency::init(resources.clocks.hclk_hz());

        if crate::config::MPU_ENABLE {
            crate::mpu::init();
            crate::rtos::set_context_switch_hook(Some(apply_thread_memory_protection));
            crate::log_debug!("MPU: 已使能 (FLASH 只读, SRAM/外设 XN, 线程栈守卫)");
        }

        // 板载外部看门狗: 使能脚已在时钟初始化前配置为"禁用"; 这里只报告配置,
        // 真正的"使能 + 首次喂狗"放在调度器启动前最后一步
        // ([`Self::start_hardware_watchdog`]), 以免控制台/RTC 初始化与横幅输出
        // 占用掉喂狗窗口。
        crate::log_debug!(
            "HWDT: PB{} 已配置为使能控制 (当前{}), 喂狗脚 PB{}",
            crate::config::HWDT_ENABLE_PIN,
            if crate::config::HWDT_ENABLE {
                "禁用, 启动末尾使能"
            } else {
                "禁用 (CFG_HWDT_ENABLE=false)"
            },
            crate::config::HWDT_FEED_PIN
        );

        // 三路 LED 全灭起步 (点亮电平由 CFG_LED_ACTIVE_LEVEL 决定);
        // 控制台 = PC13(USART3_TX)/PH2(USART3_RX), CAN = PB9/PB8 (Func_Grp2)。
        configure_led(resources.led_work);
        configure_led(resources.led_success);
        configure_led(resources.led_error);
        crate::log_debug!(
            "LED: work=PB{} success=PB{} error=PB{} ({})",
            crate::config::LED_WORK_PIN,
            crate::config::LED_SUCCESS_PIN,
            crate::config::LED_ERROR_PIN,
            if crate::config::LED_ACTIVE_HIGH {
                "高电平点亮"
            } else {
                "低电平点亮"
            }
        );
        gpio.pin::<PortC, { crate::config::UART_TX_PIN }>()
            .set_func(crate::config::UART_TX_FSEL);
        gpio.pin::<PortH, { crate::config::UART_RX_PIN }>()
            .set_func(crate::config::UART_RX_FSEL);
        if crate::config::CAN_ENABLE {
            gpio.pin::<PortB, { crate::config::CAN_TX_PIN }>()
                .set_func(crate::config::CAN_TX_FSEL);
            gpio.pin::<PortB, { crate::config::CAN_RX_PIN }>()
                .set_func(crate::config::CAN_RX_FSEL);
        }

        crate::systick::init(&resources.clocks, crate::config::SYSTICK_FREQ_HZ)
            .expect("SysTick 配置失败!");
        crate::log_debug!("SysTick: {} Hz", crate::config::SYSTICK_FREQ_HZ);

        resources
            .console
            .init(
                &resources.clocks,
                UartConfig {
                    baudrate: crate::config::UART_BAUDRATE,
                    oversample: crate::config::UART_OVERSAMPLE,
                    clock_div: crate::config::UART_CLOCK_DIV,
                    data_bits: crate::config::UART_DATA_BITS,
                    parity: crate::config::UART_PARITY,
                    stop_bits: crate::config::UART_STOP_BITS,
                    first_bit: crate::config::UART_FIRST_BIT,
                    start_bit_polarity: crate::config::UART_START_POLARITY,
                    flow_control: crate::config::UART_FLOW_CTRL,
                    noise_filter: crate::config::UART_NOISE_FILTER,
                },
            )
            .expect("UART 初始化失败!");
        let active_clock = resources.clocks.source();
        let active_name = active_clock
            .map(crate::clk::ClockSource::name)
            .unwrap_or("unknown");
        if let Some(error) = clock_error {
            crate::log_warn!(
                "系统时钟初始化失败 ({:?})，继续使用实际源 {} @ {} Hz",
                error,
                active_name,
                resources.clocks.system_hz()
            );
        } else if active_clock != Some(crate::config::CLOCK_SOURCE) {
            crate::log_warn!(
                "系统时钟已从配置源 {} 回退到 {} @ {} Hz",
                crate::config::CLOCK_SOURCE.name(),
                active_name,
                resources.clocks.system_hz()
            );
        } else {
            crate::log_debug!(
                "时钟: {} Hz (实际源 {})",
                resources.clocks.system_hz(),
                active_name
            );
        }
        crate::log_debug!(
            "控制台 UART: USART{} {} bps (过采样 {:?}, 分频 {:?})",
            crate::config::UART_UNIT,
            crate::config::UART_BAUDRATE,
            crate::config::UART_OVERSAMPLE,
            crate::config::UART_CLOCK_DIV
        );

        // DMA: 外设时钟使能 + 控制台 UART 发送卸载 (USART_TI 事件触发)。
        // 输出达标时 `Uart::write` 自动改用 DMA 整块发送, 长输出不再
        // 逐字节轮询 TXE。
        if crate::config::DMA_ENABLE {
            crate::dma::init();
            crate::dma::uart_tx_init();
            crate::log_debug!(
                "DMA: 控制台 TX 卸载到 DMA{} CH{} (USART{} TI 事件, 阈值 {}B)",
                crate::config::DMA_TX_UNIT,
                crate::config::DMA_TX_CHANNEL,
                crate::config::UART_UNIT,
                crate::config::DMA_TX_MIN
            );
        }

        if crate::config::CAN_ENABLE {
            let timing = resources
                .can
                .init(&resources.clocks, crate::config::CAN_CONFIG)
                .expect("CAN 初始化失败");
            assert_eq!(
                timing,
                crate::config::CAN_BIT_TIMING,
                "CAN 运行时与编译期位时序不一致"
            );
            crate::log_debug!(
                "CAN: {} bps (实际 {} bps, 误差 {}ppm, 采样点 {}‰, {}TQ, PRESC={}, SEG1={}, SEG2={}, SJW={})",
                crate::config::CAN_BITRATE,
                timing.actual_bitrate(resources.clocks.xtal_hz()),
                timing.error_ppm(resources.clocks.xtal_hz(), crate::config::CAN_BITRATE),
                timing.sample_point_permille(),
                timing.total_time_quanta(),
                timing.prescaler,
                timing.time_seg1,
                timing.time_seg2,
                timing.sjw
            );
        }

        if crate::config::RTC_ENABLE {
            crate::rtc::init(crate::rtc::Config {
                clock_src: crate::rtc::ClockSource::Lrc,
                hour_format: crate::rtc::HourFormat::H24,
                int_period: crate::rtc::IntPeriod::Sec,
            })
            .expect("RTC 初始化超时");
            crate::rtc::set_date(crate::rtc::Date {
                year: 0,
                month: 1,
                day: 1,
                weekday: 6,
            })
            .expect("RTC 日期写入超时");
            crate::rtc::set_time(crate::rtc::Time {
                hour: 0,
                minute: 0,
                second: 0,
                pm: false,
            })
            .expect("RTC 时间写入超时");
            crate::rtc::start().expect("RTC 启动超时");
            crate::log_info!("RTC 已启动 (LRC 源, 24H), 日志时间戳生效 [天:时:分:秒]");
        }

        unsafe { (*RESOURCES.0.get()).write(resources) };
        BOARD_READY.store(true, Ordering::Release);
        crate::console::mark_ready();
        BoardResources::resources()
    }
}

impl BoardResources {
    /// 已初始化的全局板级资源，供无捕获线程入口访问。
    ///
    /// 在 [`Board::init`] 完成前调用属于启动顺序错误，会立即 panic。
    pub fn get() -> &'static Self {
        assert!(
            BOARD_READY.load(Ordering::Acquire),
            "BoardResources 尚未初始化"
        );
        Self::resources()
    }

    fn resources() -> &'static Self {
        // BOARD_READY is the acquire fence for this one-time publication.
        unsafe { (*RESOURCES.0.get()).assume_init_ref() }
    }

    /// 设置指定 LED 点亮/熄灭 (点亮极性由 `CFG_LED_ACTIVE_LEVEL` 决定)。
    pub fn set_led(&self, led: Led, on: bool) {
        match led {
            Led::Work => set_led_pin(self.led_work, on),
            Led::Success => set_led_pin(self.led_success, on),
            Led::Error => set_led_pin(self.led_error, on),
        }
    }

    /// 翻转指定 LED (心跳等周期指示用; 单次原子写, 无需临界区)。
    pub fn toggle_led(&self, led: Led) {
        match led {
            Led::Work => self.led_work.toggle(),
            Led::Success => self.led_success.toggle(),
            Led::Error => self.led_error.toggle(),
        }
    }

    /// 指定 LED 当前是否点亮 (读 PODR 输出电平 + 点亮极性)。
    pub fn led_is_on(&self, led: Led) -> bool {
        match led {
            Led::Work => led_pin_is_on(self.led_work),
            Led::Success => led_pin_is_on(self.led_success),
            Led::Error => led_pin_is_on(self.led_error),
        }
    }

    /// 启动完成指示: SUCCESS 常亮, ERROR 熄灭 (`main` 在系统就绪后调用)。
    pub fn indicate_boot_ok(&self) {
        self.set_led(Led::Error, false);
        self.set_led(Led::Success, true);
    }

    /// 故障指示: ERROR 常亮 (panic/fault 与自检/soak 失败路径调用)。
    ///
    /// 默认 release 配置不编译 selftest/soak, 因此本方法对应用代码保留,
    /// 未引用时不视为缺陷。
    #[allow(dead_code)]
    pub fn indicate_fault(&self) {
        self.set_led(Led::Success, false);
        self.set_led(Led::Error, true);
    }

    /// 板载外部看门狗喂狗: 翻转喂狗脚 (每次调用产生一次电平跳变)。
    ///
    /// 使能状态下必须保证相邻两次调用的间隔不超过
    /// `CFG_HWDT_FEED_MS`(硬件要求 1s 周期), 否则外部看门狗复位主控。
    pub fn hwdt_feed(&self) {
        self.hwdt_feed_pin.toggle();
    }

    /// 使能/禁用板载外部看门狗 (使能脚: 低电平使能, 高电平禁用)。
    ///
    /// 重新使能后必须在一个喂狗周期内调用 [`Self::hwdt_feed`]; 禁用期间
    /// 无需喂狗 (烧录/调试时应保持禁用)。
    pub fn set_hwdt_enabled(&self, enable: bool) {
        self.hwdt_enable_pin
            .set_level(if enable { Level::Low } else { Level::High });
    }

    /// 板载外部看门狗当前是否使能 (读使能脚输出电平: 低 = 使能)。
    pub fn hwdt_enabled(&self) -> bool {
        self.hwdt_enable_pin.output_level() == Level::Low
    }

    /// 按编译期配置使能板载外部看门狗并创建喂狗线程 (`CFG_HWDT_ENABLE=true` 时)。
    ///
    /// 必须在调度器启动前、且尽量靠近 `rtos::start()` 时调用: 使能之后到喂狗
    /// 线程首次运行之间的窗口必须远小于 `CFG_HWDT_FEED_MS`, 否则外部看门狗会
    /// 在启动阶段复位主控。禁用配置下保持 PB4 高电平且不创建线程。
    pub fn start_hardware_watchdog(&self) {
        if !crate::config::HWDT_ENABLE {
            return;
        }
        assert!(
            !crate::rtos::scheduler_started(),
            "板载看门狗喂狗线程必须在调度器启动前创建"
        );
        // 先建线程再使能: 线程已就绪, 使能后立即喂一次, 之后由线程周期喂狗
        let _ = crate::rtos::thread_create(
            "hwdt",
            crate::config::HWDT_STACK_SIZE,
            crate::config::HWDT_PRIORITY,
            0,
            hardware_watchdog_supervisor,
            0,
        );
        self.set_hwdt_enabled(true);
        self.hwdt_feed();
        crate::log_debug!(
            "HWDT: 已使能 (PB{} 低=使能, PB{} 每 {}ms 喂狗, 线程 P{})",
            crate::config::HWDT_ENABLE_PIN,
            crate::config::HWDT_FEED_PIN,
            crate::config::HWDT_FEED_INTERVAL_MS,
            crate::config::HWDT_PRIORITY
        );
    }

    /// 当前系统时钟频率。
    pub fn system_clock_hz(&self) -> u32 {
        self.clocks.system_hz()
    }

    /// 冻结后的实际时钟快照 (驱动/测试恢复路径按实测频率计算;
    /// selftest/soak/`can` 命令的 CAN 初始化等路径消费)。
    #[cfg(any(shell_selftest, shell_soak, can_enabled))]
    pub(crate) fn clocks(&self) -> &crate::clk::Clocks {
        &self.clocks
    }

    /// 已初始化的控制台 UART。
    pub(crate) fn console(&self) -> &crate::config::ConsoleUart {
        &self.console
    }

    /// 检测用户是否按下 ESC (0x1B): 轮询并清空接收缓冲。
    ///
    /// 长时间运行的测试 (自检/soak) 期间终端输入一律丢弃 (ESC 除外);
    /// 返回 true 表示请求中断。放在本模块以便 selftest 与 soak 共享。
    #[cfg(any(shell_selftest, shell_soak))]
    pub(crate) fn abort_requested(&self) -> bool {
        let mut esc = false;
        while let Some(b) = self.console.read_rx() {
            if b == 0x1B {
                esc = true;
            }
        }
        esc
    }

    /// 板级唯一 CAN 控制器句柄 (selftest/soak 回环与 shell `can` 命令)。
    #[cfg(any(shell_selftest, shell_soak, can_enabled))]
    pub(crate) fn can(&self) -> &crate::can::Can {
        &self.can
    }

    /// 临时接管 CAN 控制器 (内部回环自检 / soak CAN 压力项共用)。
    ///
    /// 应用 CAN (`CFG_CAN_ENABLE`) 或 shell `can init` 已占用控制器时, 内部
    /// 回环测试必须以 RESET 清空收发队列为前提才能独占控制器。返回接管前的
    /// 工作模式 (`None` = 接管前未初始化), 测试结束必须用
    /// [`Self::can_release`] 恢复。调用方必须先确认
    /// [`crate::can::Can::irq_registered`] 为 `false` —— 接管会 RESET 控制器,
    /// 已注册的 IRQ 消费者会被架空。
    #[cfg(any(shell_selftest, shell_soak))]
    pub(crate) fn can_takeover(&self) -> Option<crate::can::WorkMode> {
        let previous = self.can.work_mode();
        if previous.is_some() {
            self.can.deinit();
        }
        previous
    }

    /// 结束 CAN 接管: 恢复接管前状态。
    ///
    /// 控制器原本已初始化时按原工作模式重新应用应用配置 (`CAN_CONFIG`),
    /// 原本未初始化时保持未初始化 (调用方负责按需关闭 XTAL)。
    #[cfg(any(shell_selftest, shell_soak))]
    pub(crate) fn can_release(
        &self,
        previous: Option<crate::can::WorkMode>,
    ) -> Result<(), crate::can::CanError> {
        self.can.deinit();
        let Some(mode) = previous else {
            return Ok(());
        };
        self.can
            .init(&self.clocks, can_config_for_mode(mode))
            .map(|_| ())
    }

    /// 注册并使能控制台 UART 接收中断。
    pub fn enable_console_rx_interrupt(&self) {
        self.console.enable_rx_interrupt(
            crate::intc::Line::new(crate::config::UART_RX_IRQ_CHANNEL as u8),
            crate::config::UART_RX_IRQ_PRIORITY,
        );
    }

    /// 按编译期配置启动硬件看门狗。
    pub fn start_watchdog(&self) {
        if crate::config::WDT_ENABLE {
            assert!(
                !crate::rtos::scheduler_started(),
                "WDT supervisor 必须在调度器启动前创建"
            );

            let pclk3_hz = self.clocks.pclk3_hz();
            let timeout_us = crate::wdt::DEFAULT
                .timeout_us(pclk3_hz)
                .expect("WDT 配置非法或 PCLK3 未运行");
            let feed_us = u64::from(crate::config::WDT_FEED_INTERVAL_MS) * 1_000;
            assert!(
                feed_us * 4 <= timeout_us,
                "WDT 喂狗周期必须至少保留 4 倍超时余量"
            );

            // 先确保 supervisor 的全部内存资源可用。线程在 rtos::start
            // 前不会运行，因此随后配置硬件并首次喂狗不存在并发窗口。
            let _ = crate::rtos::thread_create(
                "watchdog",
                crate::config::WDT_STACK_SIZE,
                crate::config::WDT_PRIORITY,
                0,
                watchdog_supervisor,
                0,
            );
            crate::wdt::init(crate::wdt::DEFAULT);
            crate::wdt::feed();
            crate::log_debug!(
                "WDT: 已启动 (PCLK3 {} Hz, 超时 {}ms, supervisor 每 {}ms 喂狗)",
                pclk3_hz,
                timeout_us / 1_000,
                crate::config::WDT_FEED_INTERVAL_MS
            );
        }
    }
}

/// 应用 CAN 配置 + 指定工作模式 (shell `can init` 与接管恢复共用)。
///
/// 模式语义: 仅外部回环按 `CFG_CAN_SELF_ACK` 决定自应答 (内部回环由控制器
/// 自动 ACK), 其余字段取自 [`crate::config::CAN_CONFIG`]。
#[cfg(any(shell_selftest, shell_soak, can_enabled))]
pub(crate) fn can_config_for_mode(mode: crate::can::WorkMode) -> crate::can::Config {
    crate::can::Config {
        mode,
        self_ack: matches!(
            mode,
            crate::can::WorkMode::ExternalLoopback | crate::can::WorkMode::ExternalLoopbackSilent
        ) && crate::config::CAN_SELF_ACK,
        ..crate::config::CAN_CONFIG
    }
}

/// PendSV 上下文切换 hook：旧 PSP 保存后切换即将运行线程的 MPU 守卫。
fn apply_thread_memory_protection(next: crate::rtos::ContextSwitchInfo) {
    crate::mpu::set_thread_guard(next.guard_base);
}

/// 检测用户是否按下 ESC (0x1B): 轮询并清空接收缓冲。
///
/// 长时间运行的测试 (自检/soak) 期间终端输入一律丢弃 (ESC 除外);
/// 返回 true 表示请求中断。selftest 与 soak 均通过本函数共享。
#[cfg(any(shell_selftest, shell_soak))]
pub(crate) fn abort_requested() -> bool {
    BoardResources::get().abort_requested()
}

/// 最高优先级 WDT supervisor：周期休眠，避免合法的长时间轮询输出因
/// idle 无法运行而误触发复位，同时验证 SysTick/PendSV 仍可调度线程。
extern "C" fn watchdog_supervisor(_param: usize) {
    let mut last = crate::rtos::uptime_ms();
    loop {
        crate::wdt::feed();
        crate::rtos::thread_delay_ms(crate::config::WDT_FEED_INTERVAL_MS)
            .expect("WDT supervisor 必须在线程上下文运行");
        // 记录实测最大喂狗间隔 (供压力测试报告证明余量): supervisor
        // 为最高优先级, 实际间隔 ≈ 周期 + 节拍抖动
        let now = crate::rtos::uptime_ms();
        let gap = now.wrapping_sub(last);
        last = now;
        WDT_FEED_MAX_GAP.fetch_max(
            gap.max(crate::config::WDT_FEED_INTERVAL_MS),
            Ordering::Relaxed,
        );
    }
}

/// 板载外部看门狗喂狗线程: 周期翻转喂狗脚。
///
/// 与内部 WDT supervisor 同策略 —— 高优先级 + 主动休眠: 合法的长时间轮询
/// 输出不会饿死它; 只有 SysTick/PendSV/调度长期停滞才会漏喂, 此时由外部
/// 看门狗复位整个主控 (这正是它存在的意义)。
extern "C" fn hardware_watchdog_supervisor(_param: usize) {
    let board = BoardResources::get();
    loop {
        board.hwdt_feed();
        crate::rtos::thread_delay_ms(crate::config::HWDT_FEED_INTERVAL_MS)
            .expect("板载看门狗喂狗必须在线程上下文运行");
    }
}

/// 实测最大喂狗间隔 (毫秒; supervisor 运行后累计的最大值)
#[cfg(shell_soak)]
pub fn wdt_feed_max_gap_ms() -> u32 {
    WDT_FEED_MAX_GAP.load(Ordering::Relaxed)
}
