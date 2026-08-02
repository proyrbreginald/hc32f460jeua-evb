// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]

// 使用 Rust 堆数据结构 (Vec/Box/String 等), 分配器见 heap 模块
extern crate alloc;

// -- 启动与内核基础设施 --
mod critical_section; // PRIMASK 临界区 (中断安全的基础)
mod heap;             // 全局堆分配器 (边界标记 + 首次适配)
mod icg;              // ICG 硬件配置段
mod startup;          // 复位入口: SRAM/FPU/时钟等待周期 + .data/.bss
mod vector_table;     // 复位/异常/144 外设中断向量表
mod panic;            // panic 与硬件 fault 诊断

// -- 外设驱动 (寄存器级, 零依赖) --
mod clk;              // 时钟链: XTAL + MPLL → 200MHz, 失败自动回退
mod gpio;             // GPIO: 寄存器/端口/引脚分层, const 泛型封装
mod systick;          // SysTick 节拍 (1kHz, RTOS 的时钟源)
mod uart;             // USART1~4: 波特率/过采样/小数分频
mod console;          // 控制台: 打印锁 (优先级继承) + 原子整行输出

// -- RTOS 内核 (RT-Thread 架构移植) --
mod rtos;

use core::sync::atomic::{AtomicU32, Ordering};
use gpio::{Config, Drive, Gpio, Level, Mode, Pin, PortA, PortC};
use uart::{Uart1, UartConfig};

/// 全局堆分配器 (边界标记 + 首次适配, 见 heap 模块)
#[global_allocator]
static ALLOCATOR: heap::HeapAllocator = heap::HeapAllocator;

/// SysTick 中断频率 (Hz), 同时是 RTOS 的节拍频率
const SYSTICK_FREQ_HZ: u32 = 1000;

/// PC13 LED, const 构造 (引脚号在编译期校验)
const LED: Pin<PortC, 13> = Pin::new();

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

    // 创建演示线程
    rtos::thread_create("led", 2048, 2, 10, led_thread, 0);
    rtos::thread_create("rx", 2048, 18, 10, rx_thread, 0);

    // 周期定时器 (回调在中断上下文执行)
    static TIMER: rtos::Timer = rtos::Timer::new();
    TIMER.start(2000, 2000, timer_cb, 0);

    // 使能 UART1 接收中断 (INTC 通道 INT001, NVIC 优先级 8)
    uart.enable_rx_interrupt(1, 8);

    // 内核启动横幅 (创建线程后、启动前, 就绪统计包含所有线程)
    rtos::banner::show();

    // 启动调度器, 永不返回
    rtos::start();
}

// ---- 演示线程 ----

/// LED 线程: 每 500ms 翻转一次 (由线程调度而非中断分频)
extern "C" fn led_thread(_param: usize) {
    loop {
        LED.toggle();
        rtos::thread_delay_ms(500);
    }
}

/// RX 演示线程: 轮询读取中断接收的环形缓冲, 回显收到的数据
extern "C" fn rx_thread(_param: usize) {
    let uart = Uart1::take();
    let mut buf = [0u8; 128];
    loop {
        rtos::thread_delay_ms(25);
        let n = uart.drain_rx(&mut buf);
        if n > 0 {
            println!("[RX] {} 字节: {:02X?} \"{}\"", n, &buf[..n], printable(&buf[..n]));
        }
    }
}

/// 转成可打印字符串 (非 ASCII 字节显示为 '.')
fn printable(bytes: &[u8]) -> alloc::string::String {
    bytes
        .iter()
        .map(|&b| {
            if (0x20..=0x7E).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// 周期定时器回调 (中断上下文): 仅做计数, 不调用阻塞 API
extern "C" fn timer_cb(_param: usize) {
    TIMER_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// 硬件初始化: 时钟 → GPIO (LED + UART 引脚) → SysTick → USART1
fn hardware_init() -> Uart1 {
    // 时钟初始化: 外部晶振 + MPLL → 200MHz (失败自动回退 MRC)
    let _ = clk::init(clk::ClockSource::Pll200);

    // GPIO
    let gpio = Gpio::take();
    gpio.pin::<PortC, 13>().configure(Config {
        mode: Mode::Output,
        pull_up: false,
        drive: Drive::Low,
        initial_level: Level::High,
    });
    // UART 引脚复用: PA9=USART1_TX (FSEL 32), PA10=USART1_RX (FSEL 33)
    gpio.pin::<PortA, 9>().set_func(32);
    gpio.pin::<PortA, 10>().set_func(33);

    // SysTick (RTOS 节拍源)
    systick::init(SYSTICK_FREQ_HZ).expect("SysTick 配置失败!");

    // USART1 (console 绑定目标): 115200, 8N1
    let uart = Uart1::take();
    uart.init(UartConfig::default()).expect("UART 初始化失败!");
    uart
}
