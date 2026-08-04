//! 内核定时器 — RT-Thread `timer.c` 的移植 (仅硬定时器)
//!
//! 定时器按绝对超时时刻 (`timeout_tick`) 升序插入有序链表, 由时钟
//! 中断调用 [`check`] 检查到期项并执行回调。回调运行在**中断上下文**,
//! 期间临界区已退出, 允许高优先级中断抢占与内核 API 调用。
//!
//! RT-Thread 以跳表组织定时链表; 本移植使用单层有序链表
//! (跳表层数 `RT_TIMER_SKIP_LIST_LEVEL=1` 时二者等价)。
//!
//! # 可变性设计
//!
//! 内部状态以 [`UnsafeCell`] 包裹: 内核对象可安全地作为 `static`
//! 使用 (如 `static TIMER: Timer = Timer::new()`), 同时避免编译器
//! 将 const 初始化的对象提升到只读存储 (FLASH), 导致内核原地修改
//! 被静默丢弃、链表被注入非法节点。

// 内核模块: unsafe 契约由临界区与模块文档统一说明, 函数体内不再逐段包裹
#![allow(unsafe_op_in_unsafe_fn)]
// 定时器 API 全集, 部分供应用选用 (演示工程仅使用 start + 回调)
#![allow(dead_code)]

use core::cell::UnsafeCell;

use crate::critical_section;
use crate::critical_section::CriticalSection;
use crate::rtos::klist::{KCell, ListHead};
use crate::rtos::tick;

/// 定时器链表头 (按 timeout_tick 升序)
static TIMER_LIST: KCell<ListHead> = KCell::new(ListHead::const_new());

/// 空回调 (const 初始化占位)
extern "C" fn nop_callback(_param: usize) {}

/// 定时器内部状态 (以 [`UnsafeCell`] 包裹, 内核原地修改)
struct TimerInner {
    node: ListHead,
    timeout_tick: u32,
    period_ticks: u32,
    started: bool,
    callback: extern "C" fn(usize),
    param: usize,
}

/// 定时器对象
pub struct Timer {
    inner: UnsafeCell<TimerInner>,
}

unsafe impl Send for Timer {}
unsafe impl Sync for Timer {}

impl Timer {
    /// 创建定时器 (未启动)
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(TimerInner {
                node: ListHead::const_new(),
                timeout_tick: 0,
                period_ticks: 0,
                started: false,
                callback: nop_callback,
                param: 0,
            }),
        }
    }

    /// 启动定时器: `delay_ticks` 后触发回调;
    /// `period_ticks != 0` 时周期触发 (每 `period_ticks` 一拍)。
    ///
    /// # 注意事项
    /// - 回调运行在中断上下文, 应保持简短, 避免阻塞式调用;
    /// - 启动后定时器对象必须保持原位 (静态或堆分配), 不可移动。
    pub fn start(
        &self,
        delay_ticks: u32,
        period_ticks: u32,
        callback: extern "C" fn(usize),
        param: usize,
    ) {
        critical_section::with(|cs| unsafe {
            self.start_internal(delay_ticks, period_ticks, callback, param, cs);
        });
    }

    /// 停止定时器 (回调将不再触发)
    pub fn stop(&self) {
        critical_section::with(|_| unsafe {
            self.stop_internal();
        });
    }

    /// 定时器是否处于启动状态
    pub fn is_active(&self) -> bool {
        critical_section::with(|_| unsafe { (*self.ptr()).started })
    }

    #[inline]
    fn ptr(&self) -> *mut TimerInner {
        self.inner.get()
    }

    /// 临界区内: 启动 (重启时先摘除旧节点)
    pub(crate) unsafe fn start_internal(
        &self,
        delay_ticks: u32,
        period_ticks: u32,
        callback: extern "C" fn(usize),
        param: usize,
        cs: CriticalSection<'_>,
    ) {
        let t = unsafe { &mut *self.ptr() };
        t.callback = callback;
        t.param = param;
        t.period_ticks = period_ticks;
        if t.node.is_linked() {
            t.node.remove();
        }
        t.timeout_tick = tick().wrapping_add(delay_ticks);
        t.started = true;
        insert_sorted(t, cs);
    }

    /// 临界区内: 停止
    pub(crate) unsafe fn stop_internal(&self) {
        let t = unsafe { &mut *self.ptr() };
        if t.node.is_linked() {
            t.node.remove();
        }
        t.started = false;
    }
}

/// 定时器节点 → 定时器对象
unsafe fn timer_from_node(node: *mut ListHead) -> *mut TimerInner {
    crate::rtos::klist::container_of!(node, TimerInner, node)
}

/// 临界区内: 按超时时刻升序插入
///
/// 相同时刻的定时器后到者排后 (先注册先回调)。
/// tick 回绕安全: 差值 < 2^31 视为"晚于"。
unsafe fn insert_sorted(t: &mut TimerInner, cs: CriticalSection<'_>) {
    let head = TIMER_LIST.get(cs) as *mut ListHead;
    let mut cur = unsafe { (*head).next_node() };
    while !cur.is_null() && cur != head {
        let e = timer_from_node(cur);
        if (t.timeout_tick.wrapping_sub(unsafe { (*e).timeout_tick }) as i32) < 0 {
            break;
        }
        cur = unsafe { (*cur).next_node() };
    }
    if cur.is_null() || cur == head {
        unsafe { (*head).push_back(&mut t.node) };
    } else {
        unsafe { (*cur).insert_before(&mut t.node) };
    }
}

/// 检查并触发到期定时器 (由时钟中断调用)
///
/// "摘除 + 回调"在**同一个临界区内**原子完成: 若回调在临界区外执行,
/// 线程删除路径 (stop_internal + 僵尸回收) 可能在其间释放回调参数
/// 引用的对象 (如线程 TCB), 造成 use-after-free。回调须保持简短,
/// 且不得在回调内阻塞 (临界区内不可挂起)。
pub(crate) fn check() {
    loop {
        let done = critical_section::with(|cs| unsafe {
            // 立即转裸指针: 之后 insert_sorted 需再次借用 TIMER_LIST,
            // &mut 引用不可重叠存活
            let head = TIMER_LIST.get(cs) as *mut ListHead;
            let Some(node) = (*head).first() else {
                return false;
            };
            let t = timer_from_node(node);
            // tick 回绕安全判定: (now - timeout) 视为 i32 时非负即到期
            if (tick().wrapping_sub((*t).timeout_tick) as i32) >= 0 {
                (*node).remove();
                // 回调在临界区内执行: 与 delete/stop 互斥, 参数对象安全
                // (回调须简短, 不得在回调内阻塞)
                let cb = (*t).callback;
                let param = (*t).param;
                cb(param);
                // 周期定时器重新入队 (回调内已 stop/重新 start 时跳过)
                let t = &mut *t;
                if t.period_ticks != 0 && !t.node.is_linked() {
                    if t.started {
                        t.timeout_tick = tick().wrapping_add(t.period_ticks);
                        insert_sorted(t, cs);
                    }
                } else {
                    // 一次性定时器: 触发后清除 started (is_active 不再误报)
                    t.started = false;
                }
                true
            } else {
                false
            }
        });
        if !done {
            break;
        }
    }
}
