//! 空闲线程与僵尸线程回收 — RT-Thread `idle.c` / `defunct.c` 的移植
//!
//! 空闲线程为最低优先级线程, 永远就绪 (调度器无需处理"无线程可运行")。
//! 它回收僵尸队列中的已退出/被删除线程: 先由 PendSV 完成切换、
//! 原线程不再执行, 再由空闲线程释放其栈与 TCB, 与 RT-Thread
//! `rt_defunct_execute` 的行为一致。

// 内核模块: unsafe 契约由临界区与模块文档统一说明, 函数体内不再逐段包裹
#![allow(unsafe_op_in_unsafe_fn)]

use crate::critical_section;
use crate::critical_section::CriticalSection;
use crate::rtos::klist::{KCell, ListHead};
use crate::rtos::sched;
use crate::rtos::thread::{Thread, free_thread, thread_create};

/// 僵尸队列: 已退出/被删除的线程等待空闲线程回收
static DEFUNCT: KCell<ListHead> = KCell::new(ListHead::const_new());

/// 临界区内: 线程进入僵尸队列 (须持有临界区令牌)
pub(crate) unsafe fn defunct_push(t: *mut Thread, cs: CriticalSection<'_>) {
    unsafe { (*DEFUNCT.get(cs)).push_back(&mut (*t).defunct_node) };
}

/// 创建空闲线程 (由 [`crate::rtos::init`] 调用)
pub(crate) fn create_idle() {
    let _ = thread_create(
        "idle",
        crate::config::IDLE_STACK_SIZE,
        crate::rtos::IDLE_PRIORITY,
        1,
        idle_entry,
        0,
    );
}

/// 空闲线程主循环: 栈溢出巡检 → 喂狗 → 回收僵尸线程 → 让出 CPU
extern "C" fn idle_entry(_param: usize) {
    loop {
        // 栈溢出巡检: 任一线程栈底 canary 被破坏即 panic
        // (panic 处理器按 STRATEGY 停机或复位, 见 panic 模块)
        if let Some(name) = crate::rtos::thread::check_stack_canaries() {
            panic!("线程栈溢出: {}", name);
        }
        // 主栈 (MSP: 启动/中断栈) canary 巡检 (堆/主栈边界字)
        if !crate::rtos::thread::check_main_stack_canary() {
            panic!("主栈 (MSP) 溢出: 中断/启动栈 canary 被破坏");
        }
        // 看门狗喂狗 (CFG_WDT_ENABLE 编译期开关, false 时整段消除)
        if crate::config::WDT_ENABLE {
            crate::wdt::feed();
        }
        defunct_execute();
        sched::schedule();
        // 无更紧急线程时进入低功耗等待 (SysTick 等中断唤醒)
        unsafe { core::arch::asm!("wfi") };
    }
}

/// 回收僵尸队列中的线程 (释放栈与 TCB)
fn defunct_execute() {
    loop {
        let t = critical_section::with(|cs| unsafe {
            let node = (*DEFUNCT.get(cs)).pop_first()?;
            Some(thread_from_defunct(node))
        });
        let Some(t) = t else { break };
        unsafe { free_thread(t) };
    }
}

/// 僵尸节点 → 线程
unsafe fn thread_from_defunct(node: *mut ListHead) -> *mut Thread {
    crate::rtos::klist::container_of!(node, Thread, defunct_node)
}
