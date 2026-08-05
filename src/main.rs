// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]
// unsafe 卫生: 所有 unsafe 操作必须显式包裹 unsafe 块
// (rtos/heap 等以"模块级契约 + 整模块 allow"设计的模块单独豁免)
#![deny(unsafe_op_in_unsafe_fn)]

// 使用 Rust 堆数据结构 (Vec/Box/String 等), 分配器见 heap 模块
extern crate alloc;

// ---- 编译期配置 (.cargo/config.toml [env] → env!) ----
mod config;

// ---- 内核基础设施 ----
mod critical_section; // PRIMASK 临界区 + 中断上下文检测 (ISR 误用防护)
mod heap; // 全局堆分配器 (边界标记 + 首次适配)
mod panic; // panic/fault 诊断: 寄存器解码 + 栈回溯 + 停机/复位策略
mod startup; // 复位入口: SRAM/FPU/时钟等待周期 + .data/.bss
mod vector_table; // 复位/异常/144 外设中断向量表 (原子回调槽)

// ---- 片内资源驱动 (寄存器级, 零依赖) ----
mod crc; // CRC 硬件加速器: CRC16/32 (X25/CCITT/IEEE), 累加模式
mod efm; // 片内 Flash (EFM): 擦除/编程/读等待/缓存/引导交换
mod icg; // ICG 硬件配置段 (flash 0x400, 复位时硬件载入)
mod intc; // 中断控制器: 事件源→SEL→NVIC 路由 + 注册 API
mod mpu; // 内存保护单元: FLASH 只读 + SRAM/外设 XN + 线程栈守卫
mod rtc; // 实时时钟 (RTC): LRC 源/时间日期/闹钟, 日志时间戳
mod sram; // 片内 SRAM (SRAMC): 等待周期/奇偶·ECC 错误检测
mod wdt; // 硬件看门狗 (WDT): 空闲线程喂狗, 溢出复位

// ---- 外设驱动 ----
mod can; // CAN 控制器: CAN2.0B, 位时间计算, 回环自测支持
mod clk; // 时钟链: XTAL + MPLL → 200MHz, 失败自动回退
mod gpio; // GPIO: 寄存器/端口/引脚分层, const 泛型封装
mod systick; // SysTick 节拍 (1kHz, RTOS 的时钟源)
mod uart; // USART1~4: 波特率/过采样/小数分频 + 无锁原子接收环

// ---- 输出通道 ----
mod console; // 控制台: 打印锁 (优先级继承) + 原子整行输出
mod log; // 应用日志: 可开关+分级+彩色, 与内核打印分离

// ---- RTOS 内核 (RT-Thread 架构移植) ----
mod rtos;

// ---- 应用 ----
mod banner; // 启动横幅 (依赖 clk/heap/rtos 公共状态)
mod selftest; // 内核自检 (shell `selftest` 命令同步执行)
mod shell; // 仿 Ubuntu 终端: 登录 + 命令提示符 + 系统信息命令

use core::sync::atomic::{AtomicU32, Ordering};
use gpio::{Config, Drive, Gpio, Mode, Pin, PortA, PortC};
use uart::UartConfig;

/// 全局堆分配器 (边界标记 + 首次适配, 见 heap 模块)
#[global_allocator]
static ALLOCATOR: heap::HeapAllocator = heap::HeapAllocator;

/// SysTick 中断频率 (Hz), 同时是 RTOS 的节拍频率 (.cargo/config.toml)
const SYSTICK_FREQ_HZ: u32 = config::SYSTICK_FREQ_HZ;

/// 板载 LED (PC13): 端口由类型系统固定, 引脚号来自配置, 存在性编译期校验
const LED: Pin<PortC, { config::LED_PIN }> = Pin::new();

/// 周期定时器触发计数
static TIMER_COUNT: AtomicU32 = AtomicU32::new(0);

/// SysTick 中断服务函数: 驱动 RTOS 时钟节拍
///
/// 由向量表 [`vector_table::EXCEPTIONS`] 的 SysTick 槽位 (异常 15) 指向。
/// 节拍驱动: 节拍递增 → 时间片轮转 → 定时器检查 → 调度。
#[unsafe(no_mangle)]
pub extern "C" fn sys_tick_handler() {
    rtos::tick_increase();
    // Arm Errata 838869: ISR 末尾加 DSB, 确保中断唤醒低功耗模式的行为可靠
    unsafe {
        core::arch::asm!("dsb sy");
    }
}

/// 应用入口: 由 [`startup::reset_handler`] 在完成硬件与内存初始化后调用
pub(crate) fn main() -> ! {
    // 硬件初始化 (时钟 200MHz / LED / UART 引脚 / SysTick / USART1)
    let uart = hardware_init();

    // RTOS 初始化: 中断优先级 + 空闲线程
    rtos::init();

    // 创建演示线程 (栈/优先级/时间片来自 .cargo/config.toml)
    // 注: selftest 不在此运行, 由 shell 命令 `selftest` 同步执行
    // (见 selftest 模块, 受 CFG_APP_SELFTEST_ENABLE 控制)
    rtos::thread_create(
        "led",
        config::APP_LED_STACK,
        config::APP_LED_PRIORITY,
        config::APP_LED_TIMESLICE,
        led_thread,
        0,
    );
    rtos::thread_create(
        "shell",
        config::APP_SHELL_STACK,
        config::APP_SHELL_PRIORITY,
        config::APP_SHELL_TIMESLICE,
        shell::shell_entry,
        0,
    );

    // 周期定时器 (回调在中断上下文执行)
    static TIMER: rtos::Timer = rtos::Timer::new();
    TIMER.start(
        config::APP_TIMER_PERIOD_MS,
        config::APP_TIMER_PERIOD_MS,
        timer_cb,
        0,
    );

    // 使能控制台 UART 接收中断 (NVIC 线/优先级来自 .cargo/config.toml)
    uart.enable_rx_interrupt(
        intc::Line::new(config::UART_RX_IRQ_CHANNEL as u8),
        config::UART_RX_IRQ_PRIORITY,
    );

    // 硬件看门狗 (CFG_WDT_ENABLE): 立即启动计数, 由空闲线程每节拍喂狗;
    // 任意线程死循环/死锁 → ~2.7s 后硬件复位
    if config::WDT_ENABLE {
        wdt::init(wdt::DEFAULT);
        wdt::feed(); // 首次喂狗启动计数 (软件启动模式)
        log_debug!("WDT: 已启动 (溢出 ≈2.7s, 空闲线程喂狗)");
    }

    // 内核启动横幅 (创建线程后、启动前, 就绪统计包含所有线程;
    // 开头先清屏, 使每次启动与上次输出明确分隔)
    banner::show();

    // 应用日志 (与内核打印分离): 输出与否由 CFG_LOG_ENABLE / CFG_LOG_LEVEL
    // 决定, 运行时可经 shell `log` 命令切换
    log_info!(
        "系统启动: {} @ {} MHz",
        config::CORE,
        clk::system_clock_hz() / 1_000_000
    );

    // 启动调度器, 永不返回
    rtos::start();
}

// ---- 演示线程 ----

/// LED 线程: 每 500ms 翻转一次 (周期来自配置, 由线程调度而非中断分频)
extern "C" fn led_thread(_param: usize) {
    loop {
        LED.toggle();
        // debug 级: 默认阈值 (info) 不输出, 可经 `log level debug` 打开
        log_debug!("LED 翻转, uptime = {} ms", rtos::uptime_ms());
        rtos::thread_delay_ms(config::APP_LED_BLINK_MS);
    }
}


/// 周期定时器回调 (中断上下文): 仅做计数, 不调用阻塞 API
extern "C" fn timer_cb(_param: usize) {
    TIMER_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// 硬件初始化: 时钟 → GPIO (LED + UART 引脚) → SysTick → 控制台 USART
///
/// 各参数 (时钟源/引脚/波特率/节拍频率等) 均来自 .cargo/config.toml。
fn hardware_init() -> config::ConsoleUart {
    // 时钟初始化: 按配置选择时钟源 (mrc/hrc/xtal/pll, PLL 源可配 XTAL 或
    // HRC), 失败自动回退 (见 clk::init)
    let _ = clk::init();
    log_debug!(
        "时钟: {} Hz (源 {:?})",
        clk::system_clock_hz(),
        config::CLOCK_SOURCE
    );

    // MPU: FLASH 只读 + SRAM/外设 XN + 线程栈守卫 (硬件捕获栈溢出与
    // 野指针破坏; CFG_MPU_ENABLE 开关, 须在调度器启动前初始化)
    if config::MPU_ENABLE {
        mpu::init();
        log_debug!("MPU: 已使能 (FLASH 只读, SRAM/外设 XN, 线程栈守卫)");
    }

    // GPIO
    let gpio = Gpio::take();
    gpio.pin::<PortC, { config::LED_PIN }>().configure(Config {
        mode: Mode::Output,
        pull_up: false,
        drive: Drive::Low,
        initial_level: config::LED_INITIAL_LEVEL,
        invert: false,
    });
    // UART 引脚复用 (PA9=TX / PA10=RX; 引脚号/功能号来自配置)
    gpio.pin::<PortA, { config::UART_TX_PIN }>()
        .set_func(config::UART_TX_FSEL);
    gpio.pin::<PortA, { config::UART_RX_PIN }>()
        .set_func(config::UART_RX_FSEL);

    // SysTick (RTOS 节拍源)
    systick::init(SYSTICK_FREQ_HZ).expect("SysTick 配置失败!");
    log_debug!("SysTick: {} Hz", SYSTICK_FREQ_HZ);

    // 控制台 USART (波特率/数据位/校验/停止位/流控等来自配置)
    let uart = config::ConsoleUart::take();
    uart.init(UartConfig {
        baudrate: config::UART_BAUDRATE,
        oversample: config::UART_OVERSAMPLE,
        clock_div: config::UART_CLOCK_DIV,
        data_bits: config::UART_DATA_BITS,
        parity: config::UART_PARITY,
        stop_bits: config::UART_STOP_BITS,
        first_bit: config::UART_FIRST_BIT,
        start_bit_polarity: config::UART_START_POLARITY,
        flow_control: config::UART_FLOW_CTRL,
        noise_filter: config::UART_NOISE_FILTER,
    })
    .expect("UART 初始化失败!");
    // 控制台就绪: 此后 println!/log! 才真正输出 (就绪前打印被静默丢弃,
    // 防止 UART 时钟未使能时 TXE 等待死循环)
    console::mark_ready();
    log_debug!(
        "控制台 UART: USART{} {} bps (过采样 {:?}, 分频 {:?})",
        config::UART_UNIT,
        config::UART_BAUDRATE,
        config::UART_OVERSAMPLE,
        config::UART_CLOCK_DIV
    );

    // RTC (LRC 源, 24H; 基准 2000-01-01 00:00:00) — 日志时间戳 [天:时:分:秒]
    //
    // 注意: 必须在 UART 就绪后初始化 —— 确认日志为 info 级会实际打印,
    // 若在 UART 使能前打印, TXE 等待将死循环 (时钟未开, SR 读回 0)。
    if config::RTC_ENABLE {
        rtc::init(rtc::Config {
            clock_src: rtc::ClockSource::Lrc,
            hour_format: rtc::HourFormat::H24,
            int_period: rtc::IntPeriod::Sec,
        });
        rtc::set_date(rtc::Date {
            year: 0,
            month: 1,
            day: 1,
            weekday: 6,
        });
        rtc::set_time(rtc::Time {
            hour: 0,
            minute: 0,
            second: 0,
            pm: false,
        });
        rtc::start();
        log_info!("RTC 已启动 (LRC 源, 24H), 日志时间戳生效 [天:时:分:秒]");
    }
    uart
}
