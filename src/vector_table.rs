//! 复位向量与异常/中断向量表
//!
//! 链接脚本 `link.ld` 按以下顺序布局:
//! - `.vector_table.reset_vector` (复位向量, 偏移 0x04);
//! - `.vector_table.exceptions` (系统异常 2~15);
//! - `.vector_table.interrupts` (外设中断 INT000~INT143);
//!
//! 初始堆栈指针 (偏移 0x00) 由链接脚本的
//! `LONG(ORIGIN(RAM) + LENGTH(RAM))` 生成。

/// 向量表项: 兼容函数指针和预留整型值
#[repr(C)]
#[derive(Clone, Copy)]
pub union Vector {
    pub handler: unsafe extern "C" fn(),
    pub reserved: usize,
}

/// 未处理中断/异常的默认处理器
///
/// 如果某个中断/异常触发了, 但没有编写具体处理逻辑,
/// 硬件将跳转到这里死循环, 防止程序跑飞。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn default_handler() {
    loop {}
}

/// 复位向量, 指向 [`crate::startup::reset_handler`]
#[unsafe(link_section = ".vector_table.reset_vector")]
#[unsafe(no_mangle)]
pub static RESET_VECTOR: unsafe extern "C" fn() -> ! = crate::startup::reset_handler;

/// 系统异常向量表 (异常 2~15)
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

/// 外设中断向量表
///
/// HC32F460 共有 144 个外设中断 (INT000~INT143, 见 DDL hc32f460.h IRQn 定义),
/// 向量表必须覆盖全部, 否则任何中断触发都会取到垃圾向量导致程序跑飞。
#[unsafe(link_section = ".vector_table.interrupts")]
#[unsafe(no_mangle)]
pub static INTERRUPTS: [Vector; 144] = [Vector {
    handler: default_handler,
}; 144];
