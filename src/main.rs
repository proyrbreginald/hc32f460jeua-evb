// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]

mod gpio;

use gpio::{Config, Drive, Gpio, Level, Mode, OutputPin, PortC};

// 设置 panic 处理函数
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

// 声明我们在链接脚本中定义的地址符号
unsafe extern "C" {
    unsafe static _data_load_addr: u32;
    unsafe static mut _data_ram_start: u32;
    unsafe static mut _data_ram_end: u32;
    unsafe static mut _bss_ram_start: u32;
    unsafe static mut _bss_ram_end: u32;
}

// 设置为复位处理函数
#[unsafe(no_mangle)]
unsafe extern "C" fn reset_handler() -> ! {
    // ---- SRAMC 初始化 (对齐 DDL startup_hc32f460.S 的 ClrSramSR + SetSRAM3Wait) ----
    //
    // HC32F460 的 SRAM 分块管理 (SRAMH/SRAM12/SRAM3/SRAMR), 其中 SRAM3
    // (0x20020000~0x20026FFF, 本工程的栈区) 是慢速块, 需要 1 个读/写等待周期,
    // 否则访问数据会损坏。复位后 WTCR=0 (0 等待), 必须显式配置。
    //
    // SRAMC 基址 0x4005_0800, 寄存器: WTCR(+0x0) WTPR(+0x4) CKCR(+0x8) CKPR(+0xC) CKSR(+0x10)
    // WTPR/CKPR 写保护键值: 0x77 解锁, 0x76 锁定 (SRAM_REG_UNLOCK_KEY/LOCK_KEY)
    unsafe {
        const SRAMC: usize = 0x4005_0800;

        // 清除 SRAM 校验错误标志 (CKSR: 1ERR/2ERR/PYERR)
        core::ptr::write_volatile((SRAMC + 0x10) as *mut u32, 0x1F);
        // 解锁 SRAMC 寄存器写保护
        core::ptr::write_volatile((SRAMC + 0x04) as *mut u32, 0x77);
        core::ptr::write_volatile((SRAMC + 0x0C) as *mut u32, 0x77);
        // SRAM3 读等待 1 周期 + 写等待 1 周期 (WTCR = 0x1100)
        core::ptr::write_volatile((SRAMC + 0x00) as *mut u32, 0x1100);
        // 恢复 SRAMC 寄存器写保护
        core::ptr::write_volatile((SRAMC + 0x04) as *mut u32, 0x76);
        core::ptr::write_volatile((SRAMC + 0x0C) as *mut u32, 0x76);
    }

    // 开启 FPU (CPACR: 使能 CP10 和 CP11 的完全访问权限)
    unsafe {
        let cpacr = 0xE000ED88 as *mut u32;
        let value = core::ptr::read_volatile(cpacr);
        core::ptr::write_volatile(cpacr, value | (0b1111 << 20));
    }

    // 初始化 .data 段
    unsafe {
        let mut src = core::ptr::addr_of!(_data_load_addr);
        let mut dest = core::ptr::addr_of_mut!(_data_ram_start);
        let end = core::ptr::addr_of_mut!(_data_ram_end);

        while dest < end {
            core::ptr::write_volatile(dest, core::ptr::read_volatile(src));
            src = src.add(1);
            dest = dest.add(1);
        }
    }

    // 初始化 .bss 段
    unsafe {
        let mut dest = core::ptr::addr_of_mut!(_bss_ram_start);
        let end = core::ptr::addr_of_mut!(_bss_ram_end);

        while dest < end {
            core::ptr::write_volatile(dest, 0);
            dest = dest.add(1);
        }
    }

    // 跳转到主循环：翻转 PC13 引脚
    blink_loop();
}

fn delay(iterations: u32) {
    for _ in 0..iterations {
        unsafe {
            core::arch::asm!("nop");
        }
    }
}

fn blink_loop() -> ! {
    // 获取 GPIO 句柄
    let gpio = Gpio::take();
    // 开发板上 LED 连接在 PC13
    let led = gpio.pin::<PortC, 13>();

    // 配置 PC13: GPIO 功能, 推挽输出, 低速驱动
    led.configure(Config {
        mode: Mode::Output,
        pull_up: false,
        drive: Drive::Low,
    });

    // 先点亮再交给通用闪烁逻辑
    led.set_level(Level::High);
    // led.set_level(Level::Low);
    blink(&led);
    loop {}
}

/// 针对 [`OutputPin`] 接口编程, 与具体端口/引脚解耦,
/// 同一份代码可以驱动任意输出引脚
fn blink(pin: &impl OutputPin) -> ! {
    loop {
        // 翻转引脚电平 (POTRC 写 1, 原子操作)
        pin.toggle();
        delay(40_000);
    }
}

// 分配复位处理函数到复位向量表
#[unsafe(link_section = ".vector_table.reset_vector")]
#[unsafe(no_mangle)]
pub static RESET_VECTOR: unsafe extern "C" fn() -> ! = reset_handler;

// 定义一个联合体，兼容函数指针和预留整型值
#[repr(C)]
#[derive(Clone, Copy)]
pub union Vector {
    pub handler: unsafe extern "C" fn(),
    pub reserved: usize,
}

// 编写一个默认的中断处理函数
// 如果某个中断触发了，但你没有编写具体逻辑，硬件将跳转到这里死循环，防止跑飞。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn default_handler() {
    loop {}
}

// 定义系统异常表
#[unsafe(link_section = ".vector_table.exceptions")]
#[unsafe(no_mangle)]
pub static EXCEPTIONS: [Vector; 14] = [
    Vector {
        handler: default_handler,
    }, // 2: NMI
    Vector {
        handler: default_handler,
    }, // 3: HardFault
    Vector {
        handler: default_handler,
    }, // 4: MemManage
    Vector {
        handler: default_handler,
    }, // 5: BusFault
    Vector {
        handler: default_handler,
    }, // 6: UsageFault
    Vector { reserved: 0 }, // 7: 预留
    Vector { reserved: 0 }, // 8: 预留
    Vector { reserved: 0 }, // 9: 预留
    Vector {
        handler: default_handler,
    }, // 10: 预留
    Vector {
        handler: default_handler,
    }, // 11: SVCall
    Vector {
        handler: default_handler,
    }, // 12: DebugMonitor
    Vector { reserved: 0 }, // 13: 预留
    Vector {
        handler: default_handler,
    }, // 14: PendSV
    Vector {
        handler: default_handler,
    }, // 15: SysTick
];

// 定义外设中断表
// HC32F460 共有 144 个外设中断 (INT000~INT143, 见 DDL hc32f460.h IRQn 定义),
// 向量表必须覆盖全部, 否则任何中断触发都会取到垃圾向量导致程序跑飞。
#[unsafe(link_section = ".vector_table.interrupts")]
#[unsafe(no_mangle)]
pub static INTERRUPTS: [Vector; 144] = [Vector {
    handler: default_handler,
}; 144];

// ICG 配置数据
#[unsafe(link_section = ".icgs")]
#[unsafe(no_mangle)]
pub static ICGS: [u32; 8] = [
    0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF,
];
