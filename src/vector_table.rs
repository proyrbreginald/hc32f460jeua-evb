//! 复位向量与异常/中断向量表
//!
//! 链接脚本 `link.ld` 按以下顺序布局:
//! - `.vector_table.reset_vector` (复位向量, 偏移 0x04);
//! - `.vector_table.exceptions` (系统异常 2~15);
//! - `.vector_table.interrupts` (外设中断 INT000~INT143);
//!
//! 初始堆栈指针 (偏移 0x00) 由链接脚本的
//! `LONG(ORIGIN(RAM) + LENGTH(RAM))` 生成。
//!
//! # 外设中断分发
//!
//! 向量表位于 FLASH 无法运行时改写, 因此全部 144 条中断线预置为分发
//! 入口 ([`irq0_dispatch`]~[`irq143_dispatch`]), 由分发入口查 RAM 回调
//! 表 ([`IRQ_HANDLERS`]) 调用回调。注册 API 见 [`crate::intc::register`]。

/// 向量表项: 兼容函数指针和预留整型值
#[repr(C)]
#[derive(Clone, Copy)]
pub union Vector {
    pub handler: unsafe extern "C" fn(),
    pub reserved: usize,
}

/// 未处理中断/异常的默认处理器
///
/// 如果某个**异常**触发了, 但没有编写具体处理逻辑,
/// 硬件将跳转到这里死循环, 防止程序跑飞。
#[unsafe(no_mangle)]
#[allow(clippy::empty_loop)]
pub unsafe extern "C" fn default_handler() {
    loop {}
}

/// 运行时注册的外设中断回调 (供 [`register_irq`] / [`crate::intc`] 使用)
type IrqHandler = unsafe extern "C" fn();

/// 回调表容器 (144 槽位, 对应 INT000~INT143)
///
/// 访问路径由分发入口 (中断上下文) 与 register_irq (初始化期, 中断未
/// 使能) 显式同步, 不经过该类型共享引用读取内部, 因此 Send/Sync 安全。
struct IrqHandlerCell(core::cell::UnsafeCell<[Option<IrqHandler>; 144]>);

unsafe impl Sync for IrqHandlerCell {}

static IRQ_HANDLERS: IrqHandlerCell = IrqHandlerCell(core::cell::UnsafeCell::new([None; 144]));

/// 注册外设中断回调 (INT000~INT143)
///
/// 向量表位于 FLASH 无法运行时改写, 因此全部槽位预置了分发入口,
/// 由分发入口查表调用回调。一般无需直接调用, 使用
/// [`crate::intc::register`] 一步完成路由+注册。
pub fn register_irq(n: usize, handler: IrqHandler) {
    assert!(n < 144, "register_irq: 仅支持 INT000~INT143");
    unsafe {
        (*IRQ_HANDLERS.0.get())[n] = Some(handler);
    }
}

/// 移除外设中断回调 (置 None; 未注册的槽位触发时静默返回)
pub fn unregister_irq(n: usize) {
    assert!(n < 144, "unregister_irq: 仅支持 INT000~INT143");
    unsafe {
        (*IRQ_HANDLERS.0.get())[n] = None;
    }
}

/// 生成分发入口: 查表调用对应槽位的回调 (未注册时静默返回)
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
irq_dispatch!(irq8_dispatch, 8);
irq_dispatch!(irq9_dispatch, 9);
irq_dispatch!(irq10_dispatch, 10);
irq_dispatch!(irq11_dispatch, 11);
irq_dispatch!(irq12_dispatch, 12);
irq_dispatch!(irq13_dispatch, 13);
irq_dispatch!(irq14_dispatch, 14);
irq_dispatch!(irq15_dispatch, 15);
irq_dispatch!(irq16_dispatch, 16);
irq_dispatch!(irq17_dispatch, 17);
irq_dispatch!(irq18_dispatch, 18);
irq_dispatch!(irq19_dispatch, 19);
irq_dispatch!(irq20_dispatch, 20);
irq_dispatch!(irq21_dispatch, 21);
irq_dispatch!(irq22_dispatch, 22);
irq_dispatch!(irq23_dispatch, 23);
irq_dispatch!(irq24_dispatch, 24);
irq_dispatch!(irq25_dispatch, 25);
irq_dispatch!(irq26_dispatch, 26);
irq_dispatch!(irq27_dispatch, 27);
irq_dispatch!(irq28_dispatch, 28);
irq_dispatch!(irq29_dispatch, 29);
irq_dispatch!(irq30_dispatch, 30);
irq_dispatch!(irq31_dispatch, 31);
irq_dispatch!(irq32_dispatch, 32);
irq_dispatch!(irq33_dispatch, 33);
irq_dispatch!(irq34_dispatch, 34);
irq_dispatch!(irq35_dispatch, 35);
irq_dispatch!(irq36_dispatch, 36);
irq_dispatch!(irq37_dispatch, 37);
irq_dispatch!(irq38_dispatch, 38);
irq_dispatch!(irq39_dispatch, 39);
irq_dispatch!(irq40_dispatch, 40);
irq_dispatch!(irq41_dispatch, 41);
irq_dispatch!(irq42_dispatch, 42);
irq_dispatch!(irq43_dispatch, 43);
irq_dispatch!(irq44_dispatch, 44);
irq_dispatch!(irq45_dispatch, 45);
irq_dispatch!(irq46_dispatch, 46);
irq_dispatch!(irq47_dispatch, 47);
irq_dispatch!(irq48_dispatch, 48);
irq_dispatch!(irq49_dispatch, 49);
irq_dispatch!(irq50_dispatch, 50);
irq_dispatch!(irq51_dispatch, 51);
irq_dispatch!(irq52_dispatch, 52);
irq_dispatch!(irq53_dispatch, 53);
irq_dispatch!(irq54_dispatch, 54);
irq_dispatch!(irq55_dispatch, 55);
irq_dispatch!(irq56_dispatch, 56);
irq_dispatch!(irq57_dispatch, 57);
irq_dispatch!(irq58_dispatch, 58);
irq_dispatch!(irq59_dispatch, 59);
irq_dispatch!(irq60_dispatch, 60);
irq_dispatch!(irq61_dispatch, 61);
irq_dispatch!(irq62_dispatch, 62);
irq_dispatch!(irq63_dispatch, 63);
irq_dispatch!(irq64_dispatch, 64);
irq_dispatch!(irq65_dispatch, 65);
irq_dispatch!(irq66_dispatch, 66);
irq_dispatch!(irq67_dispatch, 67);
irq_dispatch!(irq68_dispatch, 68);
irq_dispatch!(irq69_dispatch, 69);
irq_dispatch!(irq70_dispatch, 70);
irq_dispatch!(irq71_dispatch, 71);
irq_dispatch!(irq72_dispatch, 72);
irq_dispatch!(irq73_dispatch, 73);
irq_dispatch!(irq74_dispatch, 74);
irq_dispatch!(irq75_dispatch, 75);
irq_dispatch!(irq76_dispatch, 76);
irq_dispatch!(irq77_dispatch, 77);
irq_dispatch!(irq78_dispatch, 78);
irq_dispatch!(irq79_dispatch, 79);
irq_dispatch!(irq80_dispatch, 80);
irq_dispatch!(irq81_dispatch, 81);
irq_dispatch!(irq82_dispatch, 82);
irq_dispatch!(irq83_dispatch, 83);
irq_dispatch!(irq84_dispatch, 84);
irq_dispatch!(irq85_dispatch, 85);
irq_dispatch!(irq86_dispatch, 86);
irq_dispatch!(irq87_dispatch, 87);
irq_dispatch!(irq88_dispatch, 88);
irq_dispatch!(irq89_dispatch, 89);
irq_dispatch!(irq90_dispatch, 90);
irq_dispatch!(irq91_dispatch, 91);
irq_dispatch!(irq92_dispatch, 92);
irq_dispatch!(irq93_dispatch, 93);
irq_dispatch!(irq94_dispatch, 94);
irq_dispatch!(irq95_dispatch, 95);
irq_dispatch!(irq96_dispatch, 96);
irq_dispatch!(irq97_dispatch, 97);
irq_dispatch!(irq98_dispatch, 98);
irq_dispatch!(irq99_dispatch, 99);
irq_dispatch!(irq100_dispatch, 100);
irq_dispatch!(irq101_dispatch, 101);
irq_dispatch!(irq102_dispatch, 102);
irq_dispatch!(irq103_dispatch, 103);
irq_dispatch!(irq104_dispatch, 104);
irq_dispatch!(irq105_dispatch, 105);
irq_dispatch!(irq106_dispatch, 106);
irq_dispatch!(irq107_dispatch, 107);
irq_dispatch!(irq108_dispatch, 108);
irq_dispatch!(irq109_dispatch, 109);
irq_dispatch!(irq110_dispatch, 110);
irq_dispatch!(irq111_dispatch, 111);
irq_dispatch!(irq112_dispatch, 112);
irq_dispatch!(irq113_dispatch, 113);
irq_dispatch!(irq114_dispatch, 114);
irq_dispatch!(irq115_dispatch, 115);
irq_dispatch!(irq116_dispatch, 116);
irq_dispatch!(irq117_dispatch, 117);
irq_dispatch!(irq118_dispatch, 118);
irq_dispatch!(irq119_dispatch, 119);
irq_dispatch!(irq120_dispatch, 120);
irq_dispatch!(irq121_dispatch, 121);
irq_dispatch!(irq122_dispatch, 122);
irq_dispatch!(irq123_dispatch, 123);
irq_dispatch!(irq124_dispatch, 124);
irq_dispatch!(irq125_dispatch, 125);
irq_dispatch!(irq126_dispatch, 126);
irq_dispatch!(irq127_dispatch, 127);
irq_dispatch!(irq128_dispatch, 128);
irq_dispatch!(irq129_dispatch, 129);
irq_dispatch!(irq130_dispatch, 130);
irq_dispatch!(irq131_dispatch, 131);
irq_dispatch!(irq132_dispatch, 132);
irq_dispatch!(irq133_dispatch, 133);
irq_dispatch!(irq134_dispatch, 134);
irq_dispatch!(irq135_dispatch, 135);
irq_dispatch!(irq136_dispatch, 136);
irq_dispatch!(irq137_dispatch, 137);
irq_dispatch!(irq138_dispatch, 138);
irq_dispatch!(irq139_dispatch, 139);
irq_dispatch!(irq140_dispatch, 140);
irq_dispatch!(irq141_dispatch, 141);
irq_dispatch!(irq142_dispatch, 142);
irq_dispatch!(irq143_dispatch, 143);

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
        handler: crate::panic::nmi_handler,
    }, // 2: NMI (SRAM 奇偶/ECC 等硬件事件, 输出诊断)
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

/// 外设中断向量表 (INT000~INT143)
///
/// HC32F460 共有 144 个外设中断 (INT000~INT143, 见 DDL hc32f460.h IRQn
/// 定义), 向量表必须覆盖全部。全部槽位预置为分发入口
/// ([`irq0_dispatch`]~[`irq143_dispatch`]), 由 [`crate::intc::register`]
/// 运行时注册回调; 未注册的槽位触发时静默返回。
#[unsafe(link_section = ".vector_table.interrupts")]
#[unsafe(no_mangle)]
pub static INTERRUPTS: [Vector; 144] = [
    Vector {
        handler: irq0_dispatch,
    },
    Vector {
        handler: irq1_dispatch,
    },
    Vector {
        handler: irq2_dispatch,
    },
    Vector {
        handler: irq3_dispatch,
    },
    Vector {
        handler: irq4_dispatch,
    },
    Vector {
        handler: irq5_dispatch,
    },
    Vector {
        handler: irq6_dispatch,
    },
    Vector {
        handler: irq7_dispatch,
    },
    Vector {
        handler: irq8_dispatch,
    },
    Vector {
        handler: irq9_dispatch,
    },
    Vector {
        handler: irq10_dispatch,
    },
    Vector {
        handler: irq11_dispatch,
    },
    Vector {
        handler: irq12_dispatch,
    },
    Vector {
        handler: irq13_dispatch,
    },
    Vector {
        handler: irq14_dispatch,
    },
    Vector {
        handler: irq15_dispatch,
    },
    Vector {
        handler: irq16_dispatch,
    },
    Vector {
        handler: irq17_dispatch,
    },
    Vector {
        handler: irq18_dispatch,
    },
    Vector {
        handler: irq19_dispatch,
    },
    Vector {
        handler: irq20_dispatch,
    },
    Vector {
        handler: irq21_dispatch,
    },
    Vector {
        handler: irq22_dispatch,
    },
    Vector {
        handler: irq23_dispatch,
    },
    Vector {
        handler: irq24_dispatch,
    },
    Vector {
        handler: irq25_dispatch,
    },
    Vector {
        handler: irq26_dispatch,
    },
    Vector {
        handler: irq27_dispatch,
    },
    Vector {
        handler: irq28_dispatch,
    },
    Vector {
        handler: irq29_dispatch,
    },
    Vector {
        handler: irq30_dispatch,
    },
    Vector {
        handler: irq31_dispatch,
    },
    Vector {
        handler: irq32_dispatch,
    },
    Vector {
        handler: irq33_dispatch,
    },
    Vector {
        handler: irq34_dispatch,
    },
    Vector {
        handler: irq35_dispatch,
    },
    Vector {
        handler: irq36_dispatch,
    },
    Vector {
        handler: irq37_dispatch,
    },
    Vector {
        handler: irq38_dispatch,
    },
    Vector {
        handler: irq39_dispatch,
    },
    Vector {
        handler: irq40_dispatch,
    },
    Vector {
        handler: irq41_dispatch,
    },
    Vector {
        handler: irq42_dispatch,
    },
    Vector {
        handler: irq43_dispatch,
    },
    Vector {
        handler: irq44_dispatch,
    },
    Vector {
        handler: irq45_dispatch,
    },
    Vector {
        handler: irq46_dispatch,
    },
    Vector {
        handler: irq47_dispatch,
    },
    Vector {
        handler: irq48_dispatch,
    },
    Vector {
        handler: irq49_dispatch,
    },
    Vector {
        handler: irq50_dispatch,
    },
    Vector {
        handler: irq51_dispatch,
    },
    Vector {
        handler: irq52_dispatch,
    },
    Vector {
        handler: irq53_dispatch,
    },
    Vector {
        handler: irq54_dispatch,
    },
    Vector {
        handler: irq55_dispatch,
    },
    Vector {
        handler: irq56_dispatch,
    },
    Vector {
        handler: irq57_dispatch,
    },
    Vector {
        handler: irq58_dispatch,
    },
    Vector {
        handler: irq59_dispatch,
    },
    Vector {
        handler: irq60_dispatch,
    },
    Vector {
        handler: irq61_dispatch,
    },
    Vector {
        handler: irq62_dispatch,
    },
    Vector {
        handler: irq63_dispatch,
    },
    Vector {
        handler: irq64_dispatch,
    },
    Vector {
        handler: irq65_dispatch,
    },
    Vector {
        handler: irq66_dispatch,
    },
    Vector {
        handler: irq67_dispatch,
    },
    Vector {
        handler: irq68_dispatch,
    },
    Vector {
        handler: irq69_dispatch,
    },
    Vector {
        handler: irq70_dispatch,
    },
    Vector {
        handler: irq71_dispatch,
    },
    Vector {
        handler: irq72_dispatch,
    },
    Vector {
        handler: irq73_dispatch,
    },
    Vector {
        handler: irq74_dispatch,
    },
    Vector {
        handler: irq75_dispatch,
    },
    Vector {
        handler: irq76_dispatch,
    },
    Vector {
        handler: irq77_dispatch,
    },
    Vector {
        handler: irq78_dispatch,
    },
    Vector {
        handler: irq79_dispatch,
    },
    Vector {
        handler: irq80_dispatch,
    },
    Vector {
        handler: irq81_dispatch,
    },
    Vector {
        handler: irq82_dispatch,
    },
    Vector {
        handler: irq83_dispatch,
    },
    Vector {
        handler: irq84_dispatch,
    },
    Vector {
        handler: irq85_dispatch,
    },
    Vector {
        handler: irq86_dispatch,
    },
    Vector {
        handler: irq87_dispatch,
    },
    Vector {
        handler: irq88_dispatch,
    },
    Vector {
        handler: irq89_dispatch,
    },
    Vector {
        handler: irq90_dispatch,
    },
    Vector {
        handler: irq91_dispatch,
    },
    Vector {
        handler: irq92_dispatch,
    },
    Vector {
        handler: irq93_dispatch,
    },
    Vector {
        handler: irq94_dispatch,
    },
    Vector {
        handler: irq95_dispatch,
    },
    Vector {
        handler: irq96_dispatch,
    },
    Vector {
        handler: irq97_dispatch,
    },
    Vector {
        handler: irq98_dispatch,
    },
    Vector {
        handler: irq99_dispatch,
    },
    Vector {
        handler: irq100_dispatch,
    },
    Vector {
        handler: irq101_dispatch,
    },
    Vector {
        handler: irq102_dispatch,
    },
    Vector {
        handler: irq103_dispatch,
    },
    Vector {
        handler: irq104_dispatch,
    },
    Vector {
        handler: irq105_dispatch,
    },
    Vector {
        handler: irq106_dispatch,
    },
    Vector {
        handler: irq107_dispatch,
    },
    Vector {
        handler: irq108_dispatch,
    },
    Vector {
        handler: irq109_dispatch,
    },
    Vector {
        handler: irq110_dispatch,
    },
    Vector {
        handler: irq111_dispatch,
    },
    Vector {
        handler: irq112_dispatch,
    },
    Vector {
        handler: irq113_dispatch,
    },
    Vector {
        handler: irq114_dispatch,
    },
    Vector {
        handler: irq115_dispatch,
    },
    Vector {
        handler: irq116_dispatch,
    },
    Vector {
        handler: irq117_dispatch,
    },
    Vector {
        handler: irq118_dispatch,
    },
    Vector {
        handler: irq119_dispatch,
    },
    Vector {
        handler: irq120_dispatch,
    },
    Vector {
        handler: irq121_dispatch,
    },
    Vector {
        handler: irq122_dispatch,
    },
    Vector {
        handler: irq123_dispatch,
    },
    Vector {
        handler: irq124_dispatch,
    },
    Vector {
        handler: irq125_dispatch,
    },
    Vector {
        handler: irq126_dispatch,
    },
    Vector {
        handler: irq127_dispatch,
    },
    Vector {
        handler: irq128_dispatch,
    },
    Vector {
        handler: irq129_dispatch,
    },
    Vector {
        handler: irq130_dispatch,
    },
    Vector {
        handler: irq131_dispatch,
    },
    Vector {
        handler: irq132_dispatch,
    },
    Vector {
        handler: irq133_dispatch,
    },
    Vector {
        handler: irq134_dispatch,
    },
    Vector {
        handler: irq135_dispatch,
    },
    Vector {
        handler: irq136_dispatch,
    },
    Vector {
        handler: irq137_dispatch,
    },
    Vector {
        handler: irq138_dispatch,
    },
    Vector {
        handler: irq139_dispatch,
    },
    Vector {
        handler: irq140_dispatch,
    },
    Vector {
        handler: irq141_dispatch,
    },
    Vector {
        handler: irq142_dispatch,
    },
    Vector {
        handler: irq143_dispatch,
    },
];
