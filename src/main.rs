// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]

mod vector_table;
mod icg;
mod startup;
mod gpio;

// 设置 panic 处理函数
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// 应用入口: 由 [`startup::reset_handler`] 在完成硬件与内存初始化后调用
pub(crate) fn main() -> ! {
    // 获取 GPIO 句柄
    let gpio = gpio::Gpio::take();
    // 开发板上 LED 连接在 PC13
    let led = gpio.pin::<gpio::PortC, 13>();

    // 配置 PC13: GPIO 功能, 推挽输出, 低速驱动, 初始电平高
    led.configure(gpio::Config {
        mode: gpio::Mode::Output,
        pull_up: false,
        drive: gpio::Drive::Low,
        initial_level: gpio::Level::High,
    });

    // 交给通用闪烁逻辑 (configure 已置位初始电平, 无需手动点亮)
    blink(&led)
}

fn delay(iterations: u32) {
    for _ in 0..iterations {
        unsafe {
            core::arch::asm!("nop");
        }
    }
}

/// 针对 [`OutputPin`] 接口编程, 与具体端口/引脚解耦,
/// 同一份代码可以驱动任意输出引脚
fn blink(pin: &impl gpio::OutputPin) -> ! {
    loop {
        // 翻转引脚电平 (POTRC 写 1, 原子操作)
        pin.toggle();
        delay(40_000);
    }
}
