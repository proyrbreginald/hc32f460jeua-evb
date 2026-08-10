//! HC32F460JEUA-EVB 板级支持
//!
//! 本模块集中描述开发板资源及其初始化顺序。SoC 驱动只提供硬件能力，
//! 应用通过 [`Board`] / [`BoardResources`] 使用板载 LED 和控制台，避免
//! 在应用入口中直接绑定端口、引脚及外设实例。

use core::sync::atomic::{AtomicBool, Ordering};

use crate::gpio::{Config, Drive, Gpio, Mode, Pin, PortA, PortC};
use crate::uart::UartConfig;

/// 板载 LED: PC13，引脚号保留现有编译期配置校验。
type BoardLed = Pin<PortC, { crate::config::LED_PIN }>;

/// HC32F460JEUA-EVB 板级入口。
pub struct Board {
    _private: (),
}

/// 初始化后可交给应用使用的板级资源。
pub struct BoardResources {
    led: BoardLed,
    console: crate::config::ConsoleUart,
}

static RESOURCES: BoardResources = BoardResources {
    led: BoardLed::new(),
    console: crate::config::ConsoleUart::take(),
};
static BOARD_TAKEN: AtomicBool = AtomicBool::new(false);
static BOARD_READY: AtomicBool = AtomicBool::new(false);

impl Board {
    /// 获取板级入口。全系统仅第一次调用成功。
    pub fn take() -> Option<Self> {
        BOARD_TAKEN
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self { _private: () })
    }

    /// 初始化开发板并返回静态板级资源。
    ///
    /// 顺序保持为：时钟 -> MPU -> GPIO -> SysTick -> UART -> RTC。
    pub fn init(self) -> &'static BoardResources {
        let _ = crate::clk::init();
        crate::log_debug!(
            "时钟: {} Hz (源 {:?})",
            crate::clk::system_clock_hz(),
            crate::config::CLOCK_SOURCE
        );

        if crate::config::MPU_ENABLE {
            crate::mpu::init();
            crate::rtos::set_context_switch_hook(Some(apply_thread_memory_protection));
            crate::log_debug!("MPU: 已使能 (FLASH 只读, SRAM/外设 XN, 线程栈守卫)");
        }

        let gpio = Gpio::take();
        RESOURCES.led.configure(Config {
            mode: Mode::Output,
            pull_up: false,
            drive: Drive::Low,
            initial_level: crate::config::LED_INITIAL_LEVEL,
            invert: false,
        });
        gpio.pin::<PortA, { crate::config::UART_TX_PIN }>()
            .set_func(crate::config::UART_TX_FSEL);
        gpio.pin::<PortA, { crate::config::UART_RX_PIN }>()
            .set_func(crate::config::UART_RX_FSEL);

        crate::systick::init(crate::config::SYSTICK_FREQ_HZ).expect("SysTick 配置失败!");
        crate::log_debug!("SysTick: {} Hz", crate::config::SYSTICK_FREQ_HZ);

        RESOURCES
            .console
            .init(UartConfig {
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
            })
            .expect("UART 初始化失败!");
        crate::console::mark_ready();
        crate::log_debug!(
            "控制台 UART: USART{} {} bps (过采样 {:?}, 分频 {:?})",
            crate::config::UART_UNIT,
            crate::config::UART_BAUDRATE,
            crate::config::UART_OVERSAMPLE,
            crate::config::UART_CLOCK_DIV
        );

        if crate::config::RTC_ENABLE {
            crate::rtc::init(crate::rtc::Config {
                clock_src: crate::rtc::ClockSource::Lrc,
                hour_format: crate::rtc::HourFormat::H24,
                int_period: crate::rtc::IntPeriod::Sec,
            });
            crate::rtc::set_date(crate::rtc::Date {
                year: 0,
                month: 1,
                day: 1,
                weekday: 6,
            });
            crate::rtc::set_time(crate::rtc::Time {
                hour: 0,
                minute: 0,
                second: 0,
                pm: false,
            });
            crate::rtc::start();
            crate::log_info!("RTC 已启动 (LRC 源, 24H), 日志时间戳生效 [天:时:分:秒]");
        }

        BOARD_READY.store(true, Ordering::Release);
        &RESOURCES
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
        &RESOURCES
    }

    /// 翻转板载 LED。
    pub fn toggle_led(&self) {
        self.led.toggle();
    }

    /// 设置板载 LED 输出电平。
    pub fn set_led(&self, on: bool) {
        if on {
            self.led.set_high();
        } else {
            self.led.set_low();
        }
    }

    /// 当前系统时钟频率。
    pub fn system_clock_hz(&self) -> u32 {
        crate::clk::system_clock_hz()
    }

    /// 已初始化的控制台 UART。
    pub(crate) fn console(&self) -> &crate::config::ConsoleUart {
        &self.console
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
            crate::wdt::init(crate::wdt::DEFAULT);
            crate::wdt::feed();
            crate::rtos::set_idle_hook(Some(feed_watchdog));
            crate::log_debug!("WDT: 已启动 (溢出 ≈2.7s, 空闲线程喂狗)");
        }
    }
}

/// RTOS 上下文切换 hook：将 MPU 栈守卫切换到即将运行的线程。
fn apply_thread_memory_protection(next: crate::rtos::ContextSwitchInfo) {
    crate::mpu::set_thread_guard(next.guard_base);
}

/// RTOS idle hook：证明调度器仍可运行后喂硬件看门狗。
fn feed_watchdog() {
    crate::wdt::feed();
}
