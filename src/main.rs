// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]

// 使用 Rust 堆数据结构 (Vec/Box/String 等), 分配器见 heap 模块
extern crate alloc;

mod clk;
mod console;
mod critical_section;
mod gpio;
mod heap;
mod icg;
mod panic;
mod startup;
mod systick;
mod uart;
mod vector_table;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use gpio::{Config, Drive, Gpio, Level, Mode, Pin, PortA, PortC};
use uart::{Uart1, UartConfig};

/// 全局堆分配器 (边界标记 + 首次适配, 见 heap 模块)
#[global_allocator]
static ALLOCATOR: heap::HeapAllocator = heap::HeapAllocator;

/// SysTick 中断频率 (Hz)
///
/// 200MHz 系统时钟下 24 位重装载值 (reload = HCLK/freq - 1 ≤ 0xFFFFFF)
/// 限制最低频率 ≈ 12Hz, 故采用 1000Hz (1ms 节拍), LED 闪烁在中断内分频。
const SYSTICK_FREQ_HZ: u32 = 1000;

/// LED 翻转周期 (ms): 每 500ms 翻转一次 → 1Hz 完整闪烁周期
const LED_BLINK_PERIOD_MS: u32 = 500;

/// PC13 LED, const 构造 (引脚号在编译期校验),
/// 主循环与中断共享 (Copy + Sync, 见 gpio 模块的并发安全说明)。
const LED: Pin<PortC, 13> = Pin::new();

/// SysTick 中断服务函数: 累加节拍计数并分频翻转 LED
///
/// 由向量表 [`vector_table::EXCEPTIONS`] 的 SysTick 槽位 (异常 15) 指向。
/// 注意: 中断内不要调用打印宏 (与主循环发送可能交错, 见 console 模块说明)。
#[unsafe(no_mangle)]
pub extern "C" fn sys_tick_handler() {
    systick::on_tick();
    // 每 500ms 翻转一次 (1000Hz 节拍下 500 次中断一个周期)
    if systick::get_tick_ms() % LED_BLINK_PERIOD_MS == 0 {
        LED.toggle();
    }
    // Arm Errata 838869: ISR 末尾加 DSB, 确保中断唤醒低功耗模式的行为可靠
    unsafe {
        core::arch::asm!("dsb sy");
    }
}

/// 应用入口: 由 [`startup::reset_handler`] 在完成硬件与内存初始化后调用
pub(crate) fn main() -> ! {
    // 硬件初始化 (时钟 200MHz / LED / UART 引脚 / SysTick / USART1)
    let _uart = hardware_init();

    // 输出通道就绪: 报告启动状态
    report_startup();

    // 堆数据结构验证 (临时演示, 可移除)
    heap_demo();

    // 主循环: 周期性堆压力测试 (分配/释放) + 计数输出
    let mut count: u32 = 0;
    loop {
        systick::delay_ms(10u32);
        println!("count = {}", count);

        // 每轮分配/丢弃一个 Vec, 验证 dealloc 与碎片合并
        let tmp: Vec<u32> = (0..128).map(|i| i + count).collect();
        println!("  heap vec[128] sum = {}", tmp.iter().sum::<u32>());

        count = count.wrapping_add(1);
    }
}

/// 硬件初始化: 时钟 → GPIO (LED + UART 引脚) → SysTick → USART1
///
/// 返回 console 绑定的 UART 句柄。
fn hardware_init() -> Uart1 {
    // 时钟初始化: 外部晶振 + MPLL → 200MHz (失败自动回退 MRC,
    // 状态由 clk 模块记录, UART 就绪后统一报告)
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
    // Func_Grp1 组, 由引脚硬件固定 (数据手册表 2-1/2-2)
    gpio.pin::<PortA, 9>().set_func(32);
    gpio.pin::<PortA, 10>().set_func(33);

    // SysTick (LED 闪烁 + 节拍)
    systick::init(SYSTICK_FREQ_HZ).expect("SysTick config failed!");

    // USART1 (console 绑定目标): 115200, 8N1
    let uart = Uart1::take();
    uart.init(UartConfig::default()).expect("UART init failed!");
    uart
}

/// 启动报告: 输出通道就绪后打印时钟配置结果 (含外部晶振失败记录)
fn report_startup() {
    println!("HC32F460 console ready!");
    println!("console uart = {}", stringify!(Uart1));
    match clk::xtal_status() {
        clk::XtalStatus::Active => {
            println!("XTAL active: system clock = {} Hz", clk::system_clock_hz());
        }
        clk::XtalStatus::Failed => {
            println!(
                "WARNING: XTAL init failed! fallback MRC, system clock = {} Hz",
                clk::system_clock_hz()
            );
        }
        clk::XtalStatus::NotAttempted => {
            println!(
                "XTAL not attempted: system clock = {} Hz",
                clk::system_clock_hz()
            );
        }
    }
}

/// 堆数据结构演示: Vec / Box / String
fn heap_demo() {
    // Vec (可增长数组)
    let mut v: Vec<u32> = alloc::vec![1, 2, 3];
    v.push(4);
    v.push(5);
    println!("vec: len={}, sum={}", v.len(), v.iter().sum::<u32>());

    // Box (堆上单值)
    let b = Box::new(42u32);
    println!("box: {}", *b);

    // String (UTF-8)
    let mut s = String::from("heap");
    s.push_str("-string");
    println!("string: \"{}\", len={}", s, s.len());

    // 大块分配 (验证分裂/合并路径)
    let mut big: Vec<u8> = Vec::with_capacity(4096);
    big.resize(4096, 0xAB);
    println!(
        "big vec: len={}, first=0x{:02x}, last=0x{:02x}",
        big.len(),
        big[0],
        big[4095]
    );
}
