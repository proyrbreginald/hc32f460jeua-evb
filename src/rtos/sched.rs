//! 调度器 — RT-Thread `scheduler_up.c` 的移植 (单核 UP)
//!
//! # 就绪队列组织 (与 RT-Thread 一致)
//!
//! - 32 个优先级就绪队列 (双向链表, 同优先级 FIFO 轮转);
//! - 就绪优先级位图 `READY_GROUP`: bit N = 优先级 N 存在就绪线程;
//! - 取最高优先级就绪线程 = 位图 `trailing_zeros`
//!   (RT-Thread 用 ARM RBIT+CLZ 硬件指令, `RT_USING_CPU_FFS`)。
//!
//! # 调度判定
//!
//! 运行中的线程**始终留在就绪队列中** (状态 READY), 调度只需比较
//! "最高就绪线程 == 当前线程": 相等则无需切换 (除非当前线程已让出);
//! 不等则切换到目标线程, 目标线程状态置 RUNNING。
//!
//! # 切换执行
//!
//! 调度器本身不保存/恢复寄存器, 只向 PendSV 编码切换请求
//! (见 [`crate::rtos::context`]), 由最低优先级异常在中断返回后执行,
//! 因此线程上下文与中断上下文共用同一切换路径。

// 内核模块: unsafe 契约由临界区与模块文档统一说明, 函数体内不再逐段包裹
#![allow(unsafe_op_in_unsafe_fn)]

use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use crate::critical_section;
use crate::rtos::context;
use crate::rtos::klist::{KCell, ListHead};
use crate::rtos::thread::{thread_from_ready, Thread, STACK_PATTERN};
use crate::rtos::PRIORITY_MAX;

/// 就绪表: 每个优先级一个就绪队列 (队尾插入)
static READY_TABLE: KCell<[ListHead; PRIORITY_MAX as usize]> =
    KCell::new([ListHead::const_new(); PRIORITY_MAX as usize]);
/// 就绪优先级位图
static READY_GROUP: AtomicU32 = AtomicU32::new(0);
/// 当前线程
static CURRENT: AtomicPtr<Thread> = AtomicPtr::new(core::ptr::null_mut());

/// 当前线程 (调度器启动前为 null)
#[inline]
pub(crate) fn current() -> *mut Thread {
    CURRENT.load(Ordering::Relaxed)
}/// 设置当前线程 (调度器启动时)
#[inline]
pub(crate) fn set_current(t: *mut Thread) {
    CURRENT.store(t, Ordering::Relaxed);
}

/// 临界区内: 最高优先级就绪线程
pub(crate) unsafe fn highest_ready_thread() -> Option<*mut Thread> {
    let group = READY_GROUP.load(Ordering::Relaxed);
    if group == 0 {
        return None;
    }
    let prio = group.trailing_zeros() as usize;
    let node = unsafe { (*READY_TABLE.get()).get_unchecked(prio) }.first()?;
    Some(unsafe { thread_from_ready(node) })
}

/// 临界区内: 线程入就绪队列 (队尾)
pub(crate) unsafe fn ready_insert(t: *mut Thread) {
    let prio = unsafe { (*t).current_priority } as usize;
    let queue = unsafe { (*READY_TABLE.get()).get_unchecked_mut(prio) };
    unsafe { queue.push_back(&mut (*t).ready_node) };
    READY_GROUP.fetch_or(1 << prio, Ordering::Relaxed);
}

/// 临界区内: 线程出就绪队列
pub(crate) unsafe fn ready_remove(t: *mut Thread) {
    unsafe { (*t).ready_node.remove() };
    let prio = unsafe { (*t).current_priority } as usize;
    if unsafe { (*READY_TABLE.get()).get_unchecked(prio) }.is_empty() {
        READY_GROUP.fetch_and(!(1 << prio), Ordering::Relaxed);
    }
}

/// 临界区内: 修改线程优先级 (就绪时重排就绪队列)
pub(crate) unsafe fn change_priority(t: *mut Thread, prio: u8) {
    if unsafe { (*t).current_priority } == prio {
        return;
    }
    let ready = unsafe { (*t).ready_node.is_linked() };
    if ready {
        unsafe { ready_remove(t) };
    }
    unsafe { (*t).current_priority = prio };
    if ready {
        unsafe { ready_insert(t) };
    }
}

/// 触发重新调度
///
/// 最高就绪线程不是当前线程 (或当前线程已让出) 时请求上下文切换;
/// 线程上下文与中断上下文均只置位 PendSV, 无需区分。
pub(crate) fn schedule() {
    critical_section::with(|| unsafe {
        let cur = current();
        if cur.is_null() {
            return;
        }
        let Some(to) = highest_ready_thread() else { return };
        if to == cur {
            // 当前线程仍是最高就绪: 让出标志在无竞争者时失效
            (*to).yielded = false;
            return;
        }
        // 栈溢出检查: 目标线程与当前线程的栈底魔数 (当前线程在切换
        // 前也可能已溢出并破坏相邻 TCB, 每次调度检查缩短发现窗口)
        stack_guard_check(cur);
        stack_guard_check(to);
        (*cur).yielded = false;
        (*to).yielded = false;
        set_current(to);
        context::request_switch(&mut (*cur).sp, &mut (*to).sp);
    });
}

/// 栈溢出检查: PSP 越界或栈底魔数被破坏
unsafe fn stack_guard_check(t: *mut Thread) {
    let base = (*t).stack_addr;
    let end = base + (*t).stack_size;
    let sp = (*t).sp;
    if sp < base || sp >= end {
        panic!("thread '{}' stack overflow (sp={:#x}, stack=[{:#x},{:#x}))",
            (*t).name, sp, base, end);
    }
    for i in 0..4 {
        if unsafe { (base as *const u32).add(i).read_volatile() } != STACK_PATTERN {
            panic!("thread '{}' stack overflow (stack bottom overwritten)", (*t).name);
        }
    }
}

/// 时间片轮转 (由时钟中断调用)
///
/// 当前线程时间片耗尽时重新装载, 并移至同优先级队尾、置让出标志;
/// 返回是否需要重新调度。
pub(crate) unsafe fn tick_increase() -> bool {
    let mut need = false;
    critical_section::with(|| unsafe {
        let cur = current();
        if cur.is_null() {
            return;
        }
        let t = &mut *cur;
        if t.init_tick == 0 {
            return; // 时间片为 0: 不参与轮转
        }
        if t.remaining_tick > 0 {
            t.remaining_tick -= 1;
        }
        if t.remaining_tick == 0 {
            t.remaining_tick = t.init_tick;
            if t.ready_node.is_linked() {
                t.ready_node.remove();
                ready_insert(cur);
                t.yielded = true;
                need = true;
            }
        }
    });
    need
}

/// 当前线程主动让出 CPU (同优先级轮转)
pub(crate) unsafe fn yield_thread() {
    let cur = current();
    let need = critical_section::with(|| unsafe {
        if !(*cur).ready_node.is_linked() {
            return false;
        }
        (*cur).ready_node.remove();
        ready_insert(cur);
        (*cur).yielded = true;
        true
    });
    if need {
        schedule();
    }
}
