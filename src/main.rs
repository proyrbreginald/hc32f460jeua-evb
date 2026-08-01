// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]

mod clk;
mod console;
mod gpio;
mod icg;
mod panic;
mod startup;
mod systick;
mod uart;
mod vector_table;

use gpio::{Config, Drive, Gpio, Level, Mode, Pin, PortA, PortC};
use uart::{Uart1, UartConfig};

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
    // 时钟初始化: 外部晶振 + MPLL → 200MHz (失败自动回退 MRC,
    // 状态由 clk 模块记录, UART 就绪后统一报告)
    let _ = clk::init(clk::ClockSource::Pll200);

    // 获取 GPIO 句柄
    let gpio = Gpio::take();

    // 配置 PC13: GPIO 功能, 推挽输出, 低速驱动, 初始电平高
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

    // 开启 SysTick 中断
    systick::init(SYSTICK_FREQ_HZ).expect("SysTick config failed!");

    // 初始化 USART1 (console 的绑定目标): 115200, 8N1
    let uart = Uart1::take();
    uart.init(UartConfig::default()).expect("UART init failed!");

    // 输出通道就绪: 报告时钟配置结果 (含外部晶振失败记录)
    println!("HC32F460 console ready!");
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
    println!("console uart = {}", stringify!(Uart1));

    // 主循环: 周期性输出计数 (LED 由 SysTick 中断分频翻转)
    let mut count: u32 = 0;
    loop {
        systick::delay_ms(500u32);
        println!("count = {}", count);
        count = count.wrapping_add(1);
    }
}
