// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]

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

    // 跳转到主循环：延时翻转 PC13 引脚
    blink_loop();
}

// 怠速延时函数：在 8MHz 系统时钟下，每次迭代约消耗 5 个周期
fn delay(iterations: u32) {
    let tracker: u32 = 0;
    let p = &tracker as *const u32;
    unsafe {
        for _ in 0..iterations {
            core::ptr::read_volatile(p);
        }
    }
}

fn blink_loop() -> ! {
    // 上电后延迟约 1 秒，给调试器留出 SWD 连接窗口
    // 防止因过早操作 GPIO 配置而抢占 SWD 引脚导致无法再次烧录
    delay(2_000_000);

    const GPIO: *mut u32 = 0x40053800 as *mut u32;

    unsafe {
        // 解除 GPIO 写保护 (PWPR.WP = 0x5A, PWPR.WE = 1)
        core::ptr::write_volatile(GPIO.byte_add(0x3FC) as *mut u16, 0x5A01u16);

        // 配置 PC13 为 GPIO 功能 (PFSRC13.FSEL = 0)
        core::ptr::write_volatile(GPIO.byte_add(0x4B6) as *mut u16, 0u16);

        // 配置 PC13 为输出 (PCRC13.POUTE = 1)
        core::ptr::write_volatile(GPIO.byte_add(0x4B4) as *mut u16, 1u16 << 1);

        // 解除 PWPR 写保护后不再锁定，否则后续 POSRC/PORRC 写入也将被忽略
    }

    loop {
        // 约 500ms 延时 (8MHz * 0.5 / 5 ≈ 800000)
        delay(800_000);

        unsafe {
            // 置位 PC13，输出高电平
            core::ptr::write_volatile(GPIO.byte_add(0x28) as *mut u16, 1u16 << 13);
        }

        delay(800_000);

        unsafe {
            // 复位 PC13，输出低电平
            core::ptr::write_volatile(GPIO.byte_add(0x2A) as *mut u16, 1u16 << 13);
        }
    }
}

// 分配复位处理函数到复位向量表
#[unsafe(link_section = ".vector_table.reset_vector")]
#[unsafe(no_mangle)]
pub static RESET_VECTOR: unsafe extern "C" fn() -> ! = reset_handler;

// 定义一个联合体，兼容函数指针和预留整型值
#[repr(C)]
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
    Vector { reserved: 0 }, // 10: 预留
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
#[unsafe(link_section = ".vector_table.interrupts")]
#[unsafe(no_mangle)]
pub static INTERRUPTS: [Vector; 1] = [Vector {
    handler: default_handler,
}];

// ICG 配置数据
#[unsafe(link_section = ".icgs")]
#[unsafe(no_mangle)]
pub static ICGS: [u32; 8] = [
    0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF,
];
