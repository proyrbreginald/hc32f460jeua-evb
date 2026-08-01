// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]

mod console;
mod gpio;
mod icg;
mod startup;
mod systick;
mod uart;
mod vector_table;

// print!/println! 由 console 模块通过 #[macro_export] 导出, 直接可用

use gpio::{Config, Drive, Gpio, Level, Mode, Pin, PortA, PortC};
use uart::{Uart1, UartConfig};

// 设置 panic 处理函数
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// SysTick 中断频率 (Hz)
const SYSTICK_FREQ_HZ: u32 = 1;

/// PC13 LED, const 构造 (引脚号在编译期校验),
/// 主循环与中断共享 (Copy + Sync, 见 gpio 模块的并发安全说明)。
const LED: Pin<PortC, 13> = Pin::new();

/// SysTick 中断服务函数: 累加节拍计数
///
/// 由向量表 [`vector_table::EXCEPTIONS`] 的 SysTick 槽位 (异常 15) 指向。
/// 注意: 中断内不要调用打印宏 (与主循环发送可能交错, 见 console 模块说明)。
#[unsafe(no_mangle)]
pub extern "C" fn sys_tick_handler() {
    systick::on_tick();
    // Arm Errata 838869: ISR 末尾加 DSB, 确保中断唤醒低功耗模式的行为可靠
    unsafe {
        core::arch::asm!("dsb sy");
    }
}

/// 应用入口: 由 [`startup::reset_handler`] 在完成硬件与内存初始化后调用
pub(crate) fn main() -> ! {
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
    systick::init(SYSTICK_FREQ_HZ).expect("SysTick 配置失败");

    // 初始化 USART1 (console 的绑定目标): 115200, 8N1
    let uart = Uart1::take();
    uart.init(UartConfig::default()).expect("UART 初始化失败");

    // 控制台输出验证
    println!("HC32F460 console ready!");
    println!("console uart = {}", stringify!(Uart1));

    // 主循环: 周期性输出计数
    let mut count: u32 = 0;
    loop {
        systick::delay_ms(500u32);
        LED.toggle();
        println!("count = {}", count);
        count = count.wrapping_add(1);
    }
}
