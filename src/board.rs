//! HC32F460JEUA-EVB 板级支持
//!
//! 本模块集中描述开发板资源及其初始化顺序。SoC 驱动只提供硬件能力，
//! 应用通过 [`Board`] / [`BoardResources`] 使用板载 LED 和控制台，避免
//! 在应用入口中直接绑定端口、引脚及外设实例。

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::gpio::{Config, Drive, Mode, Pin, PortA, PortB, PortC};
use crate::uart::UartConfig;

/// 板载 LED: PC13，引脚号保留现有编译期配置校验。
type BoardLed = Pin<PortC, { crate::config::LED_PIN }>;

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
    led: BoardLed,
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
        let clocks = clocks.freeze();

        // DMA/CRC 句柄为 ZST 能力 token: 独占获取已在 `Peripherals::take`
        // 固化 (DMA 通道位图 / 构造入口收紧), 运行路径 (dma.rs::uart_tx_try
        // / copy_try) 按编译期配置重建等价句柄。此处只确认核心通道未被
        // 重复获取, 句柄随即丢弃。
        let _ = dma_tx.expect("DMA TX 通道已被重复获取");
        let _ = dma_copy.expect("DMA COPY 通道已被重复获取");
        let _ = crc;

        let resources = BoardResources {
            led: gpio.pin::<PortC, { crate::config::LED_PIN }>(),
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

        resources.led.configure(Config {
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

/// 实测最大喂狗间隔 (毫秒; supervisor 运行后累计的最大值)
#[cfg(shell_soak)]
pub fn wdt_feed_max_gap_ms() -> u32 {
    WDT_FEED_MAX_GAP.load(Ordering::Relaxed)
}
