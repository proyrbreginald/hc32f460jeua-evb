// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]

mod gpio;
mod icg;
mod startup;
mod systick;
mod vector_table;

use gpio::{Config, Drive, Gpio, Level, Mode, Pin, PortC};

// 设置 panic 处理函数
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// SysTick 中断频率 (Hz)。每次中断翻转一次 LED,
/// 因此 LED 闪烁频率 = 此值的一半。
const SYSTICK_FREQ_HZ: u32 = 1000u32;

/// PC13 LED, const 构造 (引脚号在编译期校验),
/// 主循环与中断共享 (Copy + Sync, 见 gpio 模块的并发安全说明)。
const LED: Pin<PortC, 13> = Pin::new();

/// SysTick 中断服务函数: 翻转 PC13 LED 并累加节拍计数
///
/// 由向量表 [`vector_table::EXCEPTIONS`] 的 SysTick 槽位 (异常 15) 指向。
/// LED 翻转走 POTR 硬件原子操作, 且在 with_unlocked 临界区内, 中断安全。
#[unsafe(no_mangle)]
pub extern "C" fn sys_tick_handler() {
    // LED.toggle();
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

    // 开启 SysTick 中断
    systick::init(SYSTICK_FREQ_HZ).expect("SysTick 配置失败");

    // 主循环: 由中断驱动 LED 翻转
    loop {
        systick::delay_ms(500u32);
        LED.toggle();
    }
}
