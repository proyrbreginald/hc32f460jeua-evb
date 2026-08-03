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
#[allow(clippy::empty_loop)]
pub unsafe extern "C" fn default_handler() {
    loop {}
}

/// 运行时注册的外设中断回调 (INT000~INT007, 供 [`register_irq`] 使用)
type IrqHandler = unsafe extern "C" fn();

/// 回调表容器: 访问路径由分发表入口与 [`register_irq`] 显式同步
struct IrqHandlerCell(core::cell::UnsafeCell<[Option<IrqHandler>; 8]>);

// 只有分发表入口 (中断上下文) 与 register_irq (初始化期, 中断未使能) 访问,
// 不经过该类型共享引用读取内部, 因此 Send/Sync 是安全的。
unsafe impl Sync for IrqHandlerCell {}

static IRQ_HANDLERS: IrqHandlerCell = IrqHandlerCell(core::cell::UnsafeCell::new([None; 8]));

/// 注册外设中断回调 (仅支持 INT000~INT007 槽位)
///
/// 向量表位于 FLASH 无法运行时改写, 因此这几个槽位预置了分发入口
/// ([`irq0_dispatch`] ~ [`irq7_dispatch`]), 由分发入口查表调用回调。
pub fn register_irq(n: usize, handler: IrqHandler) {
    assert!(n < 8, "register_irq: 仅支持 INT000~INT007");
    unsafe {
        (*IRQ_HANDLERS.0.get())[n] = Some(handler);
    }
}

/// 生成分发入口: 查表调用对应槽位的回调
macro_rules! irq_dispatch {
    ($name:ident, $n:literal) => {
        #[unsafe(no_mangle)]
        extern "C" fn $name() {
            unsafe {
                let h = (*IRQ_HANDLERS.0.get())[$n];
                if let Some(f) = h {
                    f();
                }
            }
        }
    };
}

irq_dispatch!(irq0_dispatch, 0);
irq_dispatch!(irq1_dispatch, 1);
irq_dispatch!(irq2_dispatch, 2);
irq_dispatch!(irq3_dispatch, 3);
irq_dispatch!(irq4_dispatch, 4);
irq_dispatch!(irq5_dispatch, 5);
irq_dispatch!(irq6_dispatch, 6);
irq_dispatch!(irq7_dispatch, 7);

/// 复位向量, 指向 [`crate::startup::reset_handler`]
#[unsafe(link_section = ".vector_table.reset_vector")]
#[unsafe(no_mangle)]
pub static RESET_VECTOR: unsafe extern "C" fn() -> ! = crate::startup::reset_handler;

/// 系统异常向量表 (异常 2~15)
///
/// 硬件 fault (MemManage/BusFault/UsageFault/HardFault) 指向
/// [`crate::panic::fault_handler`], 输出 SCB 诊断信息后按策略停机/复位。
#[unsafe(link_section = ".vector_table.exceptions")]
#[unsafe(no_mangle)]
pub static EXCEPTIONS: [Vector; 14] = [
    Vector {
        handler: default_handler,
    }, // 2: NMI
    Vector {
        handler: crate::panic::fault_handler,
    }, // 3: HardFault
    Vector {
        handler: crate::panic::fault_handler,
    }, // 4: MemManage
    Vector {
        handler: crate::panic::fault_handler,
    }, // 5: BusFault
    Vector {
        handler: crate::panic::fault_handler,
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
        handler: crate::rtos::context::pendsv_handler,
    }, // 14: PendSV (RTOS 上下文切换)
    Vector {
        handler: crate::sys_tick_handler,
    }, // 15: SysTick
];

/// 外设中断向量表
///
/// HC32F460 共有 144 个外设中断 (INT000~INT143, 见 DDL hc32f460.h IRQn 定义),
/// 向量表必须覆盖全部, 否则任何中断触发都会取到垃圾向量导致程序跑飞。
/// INT000~INT007 预置为分发入口 (见 [`register_irq`]), 其余指向默认处理器。
#[unsafe(link_section = ".vector_table.interrupts")]
#[unsafe(no_mangle)]
pub static INTERRUPTS: [Vector; 144] = {
    const fn build() -> [Vector; 144] {
        let mut t = [Vector {
            handler: default_handler,
        }; 144];
        t[0] = Vector {
            handler: irq0_dispatch,
        };
        t[1] = Vector {
            handler: irq1_dispatch,
        };
        t[2] = Vector {
            handler: irq2_dispatch,
        };
        t[3] = Vector {
            handler: irq3_dispatch,
        };
        t[4] = Vector {
            handler: irq4_dispatch,
        };
        t[5] = Vector {
            handler: irq5_dispatch,
        };
        t[6] = Vector {
            handler: irq6_dispatch,
        };
        t[7] = Vector {
            handler: irq7_dispatch,
        };
        t
    }
    build()
};
