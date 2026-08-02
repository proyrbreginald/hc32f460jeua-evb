//! 空闲线程与僵尸线程回收 — RT-Thread `idle.c` / `defunct.c` 的移植
//!
//! 空闲线程为最低优先级线程, 永远就绪 (调度器无需处理"无线程可运行")。
//! 它回收僵尸队列中的已退出/被删除线程: 先由 PendSV 完成切换、
//! 原线程不再执行, 再由空闲线程释放其栈与 TCB, 与 RT-Thread
//! `rt_defunct_execute` 的行为一致。

// 内核模块: unsafe 契约由临界区与模块文档统一说明, 函数体内不再逐段包裹
#![allow(unsafe_op_in_unsafe_fn)]

use crate::critical_section;
use crate::rtos::klist::{KCell, ListHead};
use crate::rtos::sched;
use crate::rtos::thread::{free_thread, thread_create, Thread};

/// 僵尸队列: 已退出/被删除的线程等待空闲线程回收
static DEFUNCT: KCell<ListHead> = KCell::new(ListHead::const_new());

/// 临界区内: 线程进入僵尸队列
pub(crate) unsafe fn defunct_push(t: *mut Thread) {
    unsafe { (*DEFUNCT.get()).push_back(&mut (*t).defunct_node) };
}

/// 创建空闲线程 (由 [`crate::rtos::init`] 调用)
pub(crate) fn create_idle() {
    let _ = thread_create("idle", 1024, crate::rtos::IDLE_PRIORITY, 1, idle_entry, 0);
}

/// 空闲线程主循环: 回收僵尸线程 → 让出 CPU → 等待中断
extern "C" fn idle_entry(_param: usize) {
    loop {
        defunct_execute();
        sched::schedule();
        // 无更紧急线程时进入低功耗等待 (SysTick 等中断唤醒)
        unsafe { core::arch::asm!("wfi") };
    }
}

/// 回收僵尸队列中的线程 (释放栈与 TCB)
fn defunct_execute() {
    loop {
        let t = critical_section::with(|| unsafe {
            let node = (*DEFUNCT.get()).pop_first()?;
            Some(thread_from_defunct(node))
        });
        let Some(t) = t else { break };
        unsafe { free_thread(t) };
    }
}

/// 僵尸节点 → 线程
unsafe fn thread_from_defunct(node: *mut ListHead) -> *mut Thread {
    (node as *mut u8).sub(core::mem::offset_of!(Thread, defunct_node)) as *mut Thread
}
