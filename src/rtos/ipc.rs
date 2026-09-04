//! 进程间通信 (IPC) — RT-Thread `ipc.c` 的移植
//!
//! 信号量 / 互斥量 (优先级继承) / 事件 / 邮箱 / 消息队列。
//!
//! # 阻塞语义 (与 RT-Thread 一致)
//!
//! - 等待方挂入对象挂起队列 (FIFO 或按优先级排序), 同时启动
//!   线程定时器实现超时唤醒;
//! - 唤醒方停止等待者线程定时器, 将其移入就绪队列;
//! - 超时回调与显式唤醒的竞争由"状态检查 + 临界区"消解:
//!   两者都先检查线程是否仍处于挂起态, 重复唤醒被拒绝。
//!
//! # 中断安全
//!
//! 非阻塞调用 (超时 = [`Timeout::Ticks(0)`]) 与唤醒类调用
//! (release/send/unlock) 可在中断上下文使用; 阻塞类调用
//! (`Forever`/`Ticks(n>0)`) 只能在线程上下文使用。
//!
//! # 资源获取安全
//!
//! 临界区以 [`CriticalSection`](crate::critical_section::CriticalSection)
//! 令牌形式贯穿内核内部调用 (编译期强制"须在关中断区间内访问");
//! [`Mutex::lock`] 返回 RAII 守卫 [`MutexGuard`] (析构自动释放,
//! `!Send + !Sync`, 解锁必然发生在持有线程, 经 `DerefMut` 提供
//! 保护数据的独占访问); [`Mailbox<T>`] 消息类型编码在类型中且要求
//! `T: Send` 才能跨线程共享 —— 忘解锁、跨线程解锁、错类型收发、
//! 临界区外访问共享状态均在编译期被排除。

// 内核模块: unsafe 契约由临界区与模块文档统一说明, 函数体内不再逐段包裹
#![allow(unsafe_op_in_unsafe_fn)]
// IPC API 全集, 部分供应用选用 (演示工程仅使用其中一部分)
#![allow(dead_code)]

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr;

use alloc::alloc::{alloc, dealloc};

use crate::critical_section;
use crate::critical_section::CriticalSection;
use crate::rtos::klist::ListHead;
use crate::rtos::sched;
use crate::rtos::thread::{
    TS_SUSPEND, ThreadInner, blocked_wait, resched_needed, thread_from_suspend, thread_timer_cb,
    wakeup_thread,
};

/// 阻塞超时
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    /// 无限等待
    Forever,
    /// 等待指定 tick 数；超过回绕安全半周期的值会钳位到 `i32::MAX`。
    Ticks(u32),
}

impl Timeout {
    /// 返回有限等待的定时器延时；`None` 只表示真正的永久等待。
    ///
    /// 不使用 `u32::MAX` 哨兵，保证 `Ticks(u32::MAX)` 仍是有限等待。
    fn timer_delay(self) -> Option<u32> {
        match self {
            Timeout::Forever => None,
            Timeout::Ticks(ticks) => Some(ticks.min(i32::MAX as u32)),
        }
    }
}

/// 验证 IPC 等待调用的运行上下文。
///
/// IPC 获取/接收 API 依赖已启动的调度器。ISR 仅允许 `Ticks(0)` 的
/// 非阻塞尝试；任何会挂起当前执行流的调用在所有构建配置下都被拒绝。
#[inline]
fn ensure_ipc_context(timeout: Timeout) -> Result<(), Error> {
    if !crate::rtos::scheduler_started() {
        return Err(Error::KernelNotStarted);
    }
    if timeout != Timeout::Ticks(0) && crate::critical_section::in_isr() {
        return Err(Error::InterruptContext);
    }
    Ok(())
}

/// 内核调用错误
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// 超时
    TimedOut,
    /// 资源已满 (邮箱/消息队列满)
    Full,
    /// 非法操作 (如挂起自身)
    Invalid,
    /// 阻塞操作被错误地用于中断上下文
    InterruptContext,
    /// IPC 操作发生在调度器启动前
    KernelNotStarted,
}

/// IPC 基类: 等待者挂起队列
pub(crate) struct IpcBase {
    pub(crate) suspend_list: ListHead,
}

impl IpcBase {
    pub(crate) const fn const_new() -> Self {
        Self {
            suspend_list: ListHead::const_new(),
        }
    }
}

// ---------------------------------------------------------------------------
// 内部状态辅助: 内核对象以 UnsafeCell 包裹, 使 const 静态对象
// 也能被内核原地修改 (防止编译器将对象提升到只读存储/FLASH)。
// ---------------------------------------------------------------------------

/// 临界区内: 当前线程按 FIFO 挂入对象挂起队列, 启动线程定时器
///
/// `delay == None` (无限等待) 时不启动定时器, 仅能由显式唤醒解除。
unsafe fn suspend_current(list: *mut ListHead, delay: Option<u32>, cs: CriticalSection<'_>) {
    let cur = sched::current();
    (*cur).error = 0;
    (*cur).suspend_node.remove();
    (*list).push_back(&mut (*cur).suspend_node);
    if let Some(ticks) = delay {
        (*cur)
            .thread_timer
            .start_internal(ticks, 0, thread_timer_cb, cur as usize, cs);
    }
    (*cur).state = TS_SUSPEND;
    sched::ready_remove(cur, cs);
}

/// 临界区内: 当前线程按优先级 (数字小者靠前) 挂入对象挂起队列
///
/// `delay == None` (无限等待) 时不启动定时器。
unsafe fn suspend_prio(list: *mut ListHead, delay: Option<u32>, cs: CriticalSection<'_>) {
    let cur = sched::current();
    (*cur).error = 0;
    (*cur).suspend_node.remove();
    insert_prio_waiter(list, cur);
    if let Some(ticks) = delay {
        (*cur)
            .thread_timer
            .start_internal(ticks, 0, thread_timer_cb, cur as usize, cs);
    }
    (*cur).state = TS_SUSPEND;
    sched::ready_remove(cur, cs);
}

/// 临界区内：按线程当前优先级插入等待队列。
unsafe fn insert_prio_waiter(list: *mut ListHead, thread: *mut ThreadInner) {
    // 找到第一个"优先级更低"的等待者, 插入其前
    let mut before: *mut ListHead = ptr::null_mut();
    let mut n = (*list).first();
    while let Some(node) = n {
        let w = thread_from_suspend(node);
        if (*w).current_priority > (*thread).current_priority {
            before = node;
            break;
        }
        let nn = unsafe { (*node).next_node() };
        if nn == list {
            break; // 到达队尾 (尾节点的 next 指向头哨兵)
        }
        n = Some(nn);
    }
    if before.is_null() {
        (*list).push_back(&mut (*thread).suspend_node);
    } else {
        (*before).insert_before(&mut (*thread).suspend_node);
    }
}

/// 临界区内: 唤醒挂起队列头部等待者
///
/// 返回被唤醒线程 (由调用方决定是否需要重新调度)。
unsafe fn wake_head(list: *mut ListHead, cs: CriticalSection<'_>) -> Option<*mut ThreadInner> {
    let node = (*list).pop_first()?;
    let w = thread_from_suspend(node);
    wakeup_thread(w, cs);
    Some(w)
}

// ---------------------------------------------------------------------------
// 信号量
// ---------------------------------------------------------------------------

/// 信号量
pub struct Semaphore {
    inner: UnsafeCell<SemaphoreInner>,
}

struct SemaphoreInner {
    base: IpcBase,
    value: u16,
    max_value: u16,
}

unsafe impl Send for Semaphore {}
unsafe impl Sync for Semaphore {}

impl Semaphore {
    /// 创建信号量: 初始计数值 / 最大值
    pub const fn new(init_value: u16, max_value: u16) -> Self {
        Self {
            inner: UnsafeCell::new(SemaphoreInner {
                base: IpcBase::const_new(),
                value: init_value,
                max_value,
            }),
        }
    }

    /// 获取信号量 (可超时)
    ///
    /// **唤醒即获得资源**: `release` 对等待者是"唤醒或计数+1"二选一,
    /// 被唤醒即代表资源已移交, 不得重查计数 (重查必为 0 导致再挂起死锁)。
    pub fn take(&self, timeout: Timeout) -> Result<(), Error> {
        ensure_ipc_context(timeout)?;
        let mut outcome = Ok(());
        let mut blocked = false;
        critical_section::with(|cs| unsafe {
            let s = &mut *self.ptr();
            if s.value > 0 {
                s.value -= 1;
            } else if timeout == Timeout::Ticks(0) {
                outcome = Err(Error::TimedOut);
            } else {
                suspend_current(&mut s.base.suspend_list, timeout.timer_delay(), cs);
                blocked = true;
            }
        });
        if blocked && blocked_wait() {
            return Err(Error::TimedOut);
        }
        outcome
    }

    /// 释放信号量: 有等待者时唤醒队首, 否则计数值 +1 (上限截断)
    pub fn release(&self) {
        let need = critical_section::with(|cs| unsafe {
            let s = &mut *self.ptr();
            if let Some(w) = wake_head(&mut s.base.suspend_list, cs) {
                resched_needed(w)
            } else {
                if s.value < s.max_value {
                    s.value += 1;
                }
                false
            }
        });
        if need {
            sched::schedule();
        }
    }

    #[inline]
    fn ptr(&self) -> *mut SemaphoreInner {
        self.inner.get()
    }
}

// ---------------------------------------------------------------------------
// 互斥量 (优先级继承)
// ---------------------------------------------------------------------------

/// 互斥量 `Mutex<T>`: 保护 `T` 数据 + 优先级继承 (非递归)
///
/// 等待队列按优先级排序, 释放时所有权转移给最高优先级等待者;
/// 阻塞等待者会提升持有者 (及持有者的等待链) 的当前优先级,
/// 缓解无界优先级反转 (与 RT-Thread 一致)。
///
/// # 数据安全 ([`MutexGuard`])
///
/// [`Mutex::lock`] 成功返回 RAII 守卫 [`MutexGuard`], 经 `Deref`/
/// `DerefMut` 提供对保护数据的**独占访问** (`&mut T`): 内核协议保证
/// 任意时刻至多一个守卫处于活动状态 (非递归 + 唤醒即转移所有权;
/// 持守线程被删除时内核将所有权转移给等待者, 其守卫随栈终止不再
/// 使用), 因此 `&mut T` 别名在类型层面不可能。守卫析构时自动释放
/// 互斥量, **忘解锁/重复解锁/跨线程解锁在编译期被排除**:
///
/// - 守卫为 `!Send + !Sync` (`PhantomData<*mut ()>`), 不能被移动
///   到其他线程, 解锁必然发生在持有线程 (优先级继承链不因跨线程
///   析构而错乱);
/// - **非递归语义**: 持有守卫期间再次 [`Mutex::lock`] 同一互斥量
///   立即返回 [`Error::Invalid`] (而非死锁); 递归获取是编程错误,
///   正确代码 (守卫所有权) 不可能触发;
/// - 守卫持有对互斥量的借用 (`&'a Mutex<T>`), 互斥量在其存续期间
///   不能移动。
///
/// `T: Send` 时 `Mutex<T>` 为 `Send + Sync`, 可放入 `static` 供
/// 多线程共享。
pub struct Mutex<T: ?Sized> {
    inner: UnsafeCell<MutexInner>,
    value: UnsafeCell<T>,
}

/// 互斥量守卫 (RAII): 析构时自动释放互斥量, 提供受保护数据的独占访问
///
/// 由 [`Mutex::lock`] 在成功路径返回; `!Send + !Sync`, 析构必然
/// 发生在获取它的线程上下文 (与互斥量的优先级继承语义一致)。
pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
    value: &'a mut T,
    _not_send_or_sync: PhantomData<*mut ()>,
}

impl<'a, T: ?Sized> MutexGuard<'a, T> {
    #[inline]
    pub(crate) fn new(mutex: &'a Mutex<T>, value: &'a mut T) -> Self {
        Self {
            mutex,
            value,
            _not_send_or_sync: PhantomData,
        }
    }
}

impl<T: ?Sized> core::ops::Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T: ?Sized> core::ops::DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    /// 释放互斥量: 恢复持有者优先级, 所有权转移给最高优先级等待者。
    /// 由编译器保证必然执行 (忘解锁/重复解锁不可能)。
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<T: ?Sized> core::fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MutexGuard")
    }
}

/// 互斥量内部状态 (以 [`UnsafeCell`] 包裹)
pub(crate) struct MutexInner {
    pub(crate) base: IpcBase,
    pub(crate) owner: *mut ThreadInner,
    pub(crate) hold: u8,
    pub(crate) taken_node: ListHead,
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// 创建互斥量并保护 `value` (未持有)
    pub const fn new(value: T) -> Self {
        Self {
            inner: UnsafeCell::new(MutexInner {
                base: IpcBase::const_new(),
                owner: ptr::null_mut(),
                hold: 0,
                taken_node: ListHead::const_new(),
            }),
            value: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> Mutex<T> {
    /// 获取互斥量 (可超时), 成功返回 RAII 守卫
    ///
    /// **非递归**: 持有守卫期间再次获取同一互斥量返回
    /// [`Error::Invalid`]。**唤醒即获得持有权**: [`MutexGuard`] 析构
    /// 时执行释放, 并把所有权转移给被唤醒的等待者, 唤醒后不得重查
    /// 持有权。
    pub fn lock(&self, timeout: Timeout) -> Result<MutexGuard<'_, T>, Error> {
        ensure_ipc_context(timeout)?;
        // 初始为 Ok: 阻塞唤醒 (所有权已由 release 转移) 亦属成功路径;
        // 仅立即超时/重复获取置 Err
        let mut outcome = Ok(());
        let mut blocked = false;
        critical_section::with(|cs| unsafe {
            let m = &mut *self.ptr();
            let cur = sched::current();
            if m.owner.is_null() {
                m.owner = cur;
                m.hold = 1;
                (*cur).taken_list.insert_after(&mut m.taken_node);
            } else if m.owner == cur {
                // 非递归: 同一线程重复获取是编程错误, 立即报错而非死锁
                outcome = Err(Error::Invalid);
            } else if timeout == Timeout::Ticks(0) {
                outcome = Err(Error::TimedOut);
            } else {
                // 优先级继承: 提升持有者 (及继承链) 到当前线程优先级
                let own = m.owner;
                if (*own).current_priority > (*cur).current_priority {
                    priority_inherit(own, (*cur).current_priority, cs);
                }
                (*cur).pending_mutex = self.ptr();
                suspend_prio(&mut m.base.suspend_list, timeout.timer_delay(), cs);
                blocked = true;
            }
        });
        if blocked && blocked_wait() {
            return Err(Error::TimedOut);
        }
        // 成功 (含阻塞唤醒): 包装守卫; 内核协议保证至多一个守卫存在,
        // 经 UnsafeCell 派生的 `&mut T` 即独占访问 (别名不可能)
        outcome.map(|()| MutexGuard::new(self, unsafe { &mut *self.value.get() }))
    }

    /// 释放互斥量 (仅持有者有效; 非持有者调用被忽略)
    ///
    /// 释放后取消优先级继承 (恢复初始优先级), 所有权转移给
    /// 最高优先级等待者。**底层原语: 仅由 [`MutexGuard`] 析构调用,
    /// 不可直接使用** (守卫是唯一合法的释放路径)。
    fn unlock(&self) {
        let need = critical_section::with(|cs| unsafe {
            let m = &mut *self.ptr();
            let cur = sched::current();
            if m.owner != cur {
                return false; // 非持有者: 忽略
            }
            m.hold -= 1;
            if m.hold != 0 {
                return false;
            }
            // 释放所有权
            m.owner = ptr::null_mut();
            m.taken_node.remove();
            // 仅释放当前 mutex；仍持有的其他 mutex 可能继续要求继承。
            recompute_priority_chain(cur, cs);
            // 所有权转移给最高优先级等待者
            if let Some(w) = wake_head(&mut m.base.suspend_list, cs) {
                (*w).pending_mutex = ptr::null_mut();
                m.owner = w;
                m.hold = 1;
                (*w).taken_list.insert_after(&mut m.taken_node);
                resched_needed(w)
            } else {
                false
            }
        });
        if need {
            sched::schedule();
        }
    }

    #[inline]
    fn ptr(&self) -> *mut MutexInner {
        self.inner.get()
    }

    /// 当前持有者 (诊断用)
    pub(crate) fn owner(&self) -> *mut ThreadInner {
        critical_section::with(|_| unsafe { (*self.ptr()).owner })
    }
}

/// 线程在互斥量依赖图中的后继；自等待按链尾处理。
unsafe fn inheritance_successor(thread: *mut ThreadInner) -> *mut ThreadInner {
    if thread.is_null() {
        return ptr::null_mut();
    }
    let pending = (*thread).pending_mutex;
    if pending.is_null() {
        return ptr::null_mut();
    }
    let owner = (*pending).owner;
    if owner == thread {
        ptr::null_mut()
    } else {
        owner
    }
}

/// 用 Floyd 算法查找依赖链的环入口；不在临界区内分配辅助存储。
unsafe fn inheritance_cycle_entry(start: *mut ThreadInner) -> *mut ThreadInner {
    let mut slow = inheritance_successor(start);
    let mut fast = inheritance_successor(inheritance_successor(start));

    while !slow.is_null() && !fast.is_null() && slow != fast {
        slow = inheritance_successor(slow);
        fast = inheritance_successor(inheritance_successor(fast));
    }
    if slow.is_null() || fast.is_null() {
        return ptr::null_mut();
    }

    slow = start;
    while slow != fast {
        slow = inheritance_successor(slow);
        fast = inheritance_successor(fast);
    }
    slow
}

/// 优先级继承: 沿"等待互斥量 → 持有者"链提升优先级。
///
/// 链长度由实际线程关系决定，与优先级数量无关；若应用形成互斥量
/// 死锁环，每个不同线程最多访问一次。
unsafe fn priority_inherit(mut chain: *mut ThreadInner, prio: u8, cs: CriticalSection<'_>) {
    let cycle_entry = inheritance_cycle_entry(chain);
    let mut entered_cycle = chain == cycle_entry;
    loop {
        if unsafe { (*chain).current_priority } <= prio {
            return;
        }
        sched::change_priority(chain, prio, cs);
        let pm = unsafe { (*chain).pending_mutex };
        if pm.is_null() {
            return;
        }
        // 被提升的线程本身可能正在等待另一把 mutex；重新按有效优先级
        // 排队，保证 unlock 仍会唤醒真正最高优先级的等待者。
        unsafe { (*chain).suspend_node.remove() };
        let waiters = core::ptr::addr_of_mut!((*pm).base.suspend_list);
        insert_prio_waiter(waiters, chain);
        let owner = inheritance_successor(chain);
        if owner.is_null() {
            return;
        }
        if owner == cycle_entry {
            if entered_cycle {
                return;
            }
            entered_cycle = true;
        }
        chain = owner;
    }
}

/// 根据线程仍持有的全部 mutex 计算有效优先级。
unsafe fn effective_priority(t: *mut ThreadInner) -> u8 {
    let mut priority = (*t).init_priority;
    let head = &mut (*t).taken_list as *mut ListHead;
    let mut node = (*head).next_node();
    while !node.is_null() && node != head {
        let mutex = mutex_from_taken(node);
        if let Some(waiter_node) = (*mutex).base.suspend_list.first() {
            let waiter = thread_from_suspend(waiter_node);
            priority = priority.min((*waiter).current_priority);
        }
        node = (*node).next_node();
    }
    priority
}

/// 重算有效优先级，并沿“本线程等待的 mutex -> owner”链传播。
unsafe fn recompute_priority_chain(mut thread: *mut ThreadInner, cs: CriticalSection<'_>) {
    let cycle_entry = inheritance_cycle_entry(thread);
    let mut entered_cycle = thread == cycle_entry;
    loop {
        let priority = effective_priority(thread);
        if priority == (*thread).current_priority {
            return;
        }
        sched::change_priority(thread, priority, cs);
        let pending = (*thread).pending_mutex;
        if pending.is_null() {
            return;
        }
        (*thread).suspend_node.remove();
        insert_prio_waiter(&mut (*pending).base.suspend_list, thread);
        let owner = inheritance_successor(thread);
        if owner.is_null() {
            return;
        }
        if owner == cycle_entry {
            if entered_cycle {
                return;
            }
            entered_cycle = true;
        }
        thread = owner;
    }
}

/// 在线程已从 mutex 等待队列摘除后，清理继承关系并回算 owner。
pub(crate) unsafe fn mutex_waiter_removed(t: *mut ThreadInner, cs: CriticalSection<'_>) {
    let pending = (*t).pending_mutex;
    (*t).pending_mutex = ptr::null_mut();
    if pending.is_null() {
        return;
    }
    let owner = (*pending).owner;
    if !owner.is_null() && owner != t {
        recompute_priority_chain(owner, cs);
    }
}

/// 临界区内: 线程退出时释放其持有的全部互斥量 (所有权转移给等待者)
pub(crate) unsafe fn mutex_release_all_held(t: *mut ThreadInner, cs: CriticalSection<'_>) {
    while let Some(node) = unsafe { (*t).taken_list.first() } {
        let m = mutex_from_taken(node);
        // 必须先从旧 owner 的 taken_list 摘除；否则队首永远不变，线程
        // 退出会在关中断状态下死循环。节点摘除后才能转移给新 owner。
        unsafe { (*m).taken_node.remove() };
        (*m).owner = ptr::null_mut();
        (*m).hold = 0;
        let waiters = core::ptr::addr_of_mut!((*m).base.suspend_list);
        if let Some(w) = wake_head(waiters, cs) {
            (*w).pending_mutex = ptr::null_mut();
            (*m).owner = w;
            (*m).hold = 1;
            (*w).taken_list.insert_after(&mut (*m).taken_node);
        }
    }
}

/// taken 节点 → 互斥量
unsafe fn mutex_from_taken(node: *mut ListHead) -> *mut MutexInner {
    crate::rtos::klist::container_of!(node, MutexInner, taken_node)
}

// ---------------------------------------------------------------------------
// 事件 (32 位事件标志组)
// ---------------------------------------------------------------------------

/// 事件等待模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOpt {
    /// AND: 全部置位才满足
    And,
    /// AND + 唤醒时清除
    AndClear,
    /// OR: 任一置位即满足
    Or,
    /// OR + 唤醒时清除
    OrClear,
}

impl EventOpt {
    fn is_and(&self) -> bool {
        matches!(self, EventOpt::And | EventOpt::AndClear)
    }

    fn clear(&self) -> bool {
        matches!(self, EventOpt::AndClear | EventOpt::OrClear)
    }
}

/// 事件标志组 (32 位, RT-Thread `rt_event`)
pub struct Event {
    inner: UnsafeCell<EventInner>,
}

struct EventInner {
    base: IpcBase,
    set: u32,
}

unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    /// 创建事件 (全部标志清零)
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(EventInner {
                base: IpcBase::const_new(),
                set: 0,
            }),
        }
    }

    /// 当前事件位
    pub fn flags(&self) -> u32 {
        critical_section::with(|_| unsafe { (*self.ptr()).set })
    }

    /// 发送事件: 置位并唤醒所有满足条件的等待者
    pub fn send(&self, set: u32) {
        let need = critical_section::with(|cs| unsafe {
            let e = &mut *self.ptr();
            e.set |= set;
            let mut need = false;
            // 遍历挂起队列 (先保存后继, 允许移除)
            let mut n = e.base.suspend_list.first();
            let head = &mut e.base.suspend_list as *mut ListHead;
            while let Some(node) = n {
                let nn = (*node).next_node();
                let next = if nn.is_null() || nn == head {
                    None
                } else {
                    Some(nn)
                };
                let w = thread_from_suspend(node);
                let wanted = (*w).event_wanted;
                let opt = (*w).event_opt;
                let matched = if opt.is_and() {
                    (e.set & wanted) == wanted
                } else {
                    (e.set & wanted) != 0
                };
                if matched {
                    // 记录唤醒时匹配的事件位 (供 recv 返回)
                    let bits = if opt.is_and() { wanted } else { e.set & wanted };
                    // 按等待模式清除事件位
                    if opt.clear() {
                        if opt.is_and() {
                            e.set &= !wanted;
                        } else {
                            e.set &= !bits;
                        }
                    }
                    // 唤醒等待者
                    (*node).remove();
                    (*w).event_recv_bits = bits;
                    wakeup_thread(w, cs);
                    need |= resched_needed(w);
                }
                n = next;
            }
            need
        });
        if need {
            sched::schedule();
        }
    }

    /// 等待事件 (可超时); 成功返回唤醒时的事件位
    pub fn recv(&self, wanted: u32, opt: EventOpt, timeout: Timeout) -> Result<u32, Error> {
        ensure_ipc_context(timeout)?;
        let mut outcome = Err(Error::TimedOut);
        let mut blocked = false;
        critical_section::with(|cs| unsafe {
            let e = &mut *self.ptr();
            let matched = if opt.is_and() {
                (e.set & wanted) == wanted
            } else {
                (e.set & wanted) != 0
            };
            if matched {
                // 返回的匹配位: AND = 等待的全集合, OR = 实际置位位
                // (与 Event::send 唤醒路径的 event_recv_bits 语义一致)
                let bits = if opt.is_and() { wanted } else { e.set & wanted };
                if opt.clear() {
                    if opt.is_and() {
                        e.set &= !wanted;
                    } else {
                        e.set &= !bits;
                    }
                }
                outcome = Ok(bits);
            } else if timeout == Timeout::Ticks(0) {
                outcome = Err(Error::TimedOut);
            } else {
                let cur = sched::current();
                (*cur).event_wanted = wanted;
                (*cur).event_opt = opt;
                suspend_current(&mut e.base.suspend_list, timeout.timer_delay(), cs);
                blocked = true;
            }
        });
        if blocked {
            if blocked_wait() {
                return Err(Error::TimedOut);
            }
            // 返回唤醒时匹配的事件位 (由 Event::send 记录)
            outcome = Ok(critical_section::with(|_| unsafe {
                (*sched::current()).event_recv_bits
            }));
        }
        outcome
    }

    #[inline]
    fn ptr(&self) -> *mut EventInner {
        self.inner.get()
    }
}

// ---------------------------------------------------------------------------
// 邮箱 (环形缓冲, 泛型消息)
// ---------------------------------------------------------------------------

/// 邮箱: 环形缓冲存放 `T` 消息 (RT-Thread `rt_mailbox`)
///
/// 接收等待者挂于 `base.suspend_list`, 发送等待者挂于 `sender_list`。
///
/// # 类型安全
///
/// `T` 编码在类型中: 同一邮箱只能收发同一类型的消息, 消息在收发间
/// 按值拷贝转移。`Mailbox<T>` 仅在 `T: Send` 时是 `Send + Sync`
/// (可放入 `static` 供多线程共享) —— 无法通过邮箱跨线程传递
/// 非 `Send` 类型 (`Rc` 等), 由编译器保证。
///
/// # 安全边界
///
/// `send`/`urgent`/`recv` 要求 `T: Copy` (消息为按值语义的平凡类型);
/// 池内槽位由计数保护, 未发送的槽位不会被读取。
pub struct Mailbox<T> {
    inner: UnsafeCell<MailboxInner<T>>,
}

struct MailboxInner<T> {
    base: IpcBase,
    sender_list: ListHead,
    /// 消息池 (惰性初始化; 原子指针: 分配在临界区**外**完成, 经 CAS
    /// 一次性发布 —— 临界区内绝不发生 malloc, 关中断时间有界)
    pool: core::sync::atomic::AtomicPtr<T>,
    size: u32,
    count: u32,
    in_idx: u32,
    out_idx: u32,
}

unsafe impl<T: Send> Send for Mailbox<T> {}
unsafe impl<T: Send> Sync for Mailbox<T> {}

impl<T: Copy> Mailbox<T> {
    /// 创建容量为 `capacity` 的邮箱
    ///
    /// 消息池在**首次使用前**按需分配 (惰性初始化, 使 [`Mailbox`] 可作
    /// `static` 使用); 分配发生在临界区**外** (见 [`MailboxInner::ensure_pool`]),
    /// 常量求值时仅检查容量合法性。
    pub const fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Mailbox::new: 容量必须大于 0");
        assert!(capacity <= u32::MAX as usize, "Mailbox::new: 容量超出 u32");
        assert!(
            core::mem::size_of::<T>() > 0,
            "Mailbox::new: 不支持零大小消息类型"
        );
        Self {
            inner: UnsafeCell::new(MailboxInner {
                base: IpcBase::const_new(),
                sender_list: ListHead::const_new(),
                pool: core::sync::atomic::AtomicPtr::new(ptr::null_mut()),
                size: capacity as u32,
                count: 0,
                in_idx: 0,
                out_idx: 0,
            }),
        }
    }

    /// 发送消息 (满时按超时阻塞)
    pub fn send(&self, msg: T, timeout: Timeout) -> Result<(), Error> {
        self.send_impl(msg, timeout, false)
    }

    /// 紧急发送: 插入队首
    pub fn urgent(&self, msg: T, timeout: Timeout) -> Result<(), Error> {
        self.send_impl(msg, timeout, true)
    }

    fn send_impl(&self, msg: T, timeout: Timeout, urgent: bool) -> Result<(), Error> {
        ensure_ipc_context(timeout)?;
        // 池惰性分配在临界区外完成 (无锁 CAS 发布), 保证关中断区间无 malloc
        let pool = self.ensure_pool();
        loop {
            let mut outcome = Err(Error::Full);
            let mut need = false;
            let mut blocked = false;
            critical_section::with(|cs| unsafe {
                let mb = &mut *self.ptr();
                if mb.count < mb.size {
                    if urgent {
                        mb.out_idx = if mb.out_idx == 0 {
                            mb.size - 1
                        } else {
                            mb.out_idx - 1
                        };
                        ptr::write(pool.add(mb.out_idx as usize), msg);
                    } else {
                        ptr::write(pool.add(mb.in_idx as usize), msg);
                        mb.in_idx = (mb.in_idx + 1) % mb.size;
                    }
                    mb.count += 1;
                    outcome = Ok(());
                    if let Some(w) = wake_head(&mut mb.base.suspend_list, cs) {
                        need = resched_needed(w);
                    }
                } else if timeout == Timeout::Ticks(0) {
                    outcome = Err(Error::Full);
                } else {
                    suspend_current(&mut mb.sender_list, timeout.timer_delay(), cs);
                    blocked = true;
                }
            });
            if need {
                sched::schedule();
            }
            if blocked {
                // 唤醒后: 超时返回, 否则重试发送 (消息不得丢失)
                if blocked_wait() {
                    return Err(Error::TimedOut);
                }
                continue;
            }
            return outcome;
        }
    }

    /// 接收消息 (空时按超时阻塞)
    ///
    /// 阻塞等待者被 [`Mailbox::send`] 唤醒后回到循环重新检查
    /// (对齐 RT-Thread 语义), 消息不会滞留。
    pub fn recv(&self, timeout: Timeout) -> Result<T, Error> {
        ensure_ipc_context(timeout)?;
        // 池惰性分配在临界区外完成 (无锁 CAS 发布)
        let pool = self.ensure_pool();
        loop {
            let mut outcome = Err(Error::TimedOut);
            let mut need = false;
            let mut blocked = false;
            critical_section::with(|cs| unsafe {
                let mb = &mut *self.ptr();
                if mb.count > 0 {
                    let msg = ptr::read(pool.add(mb.out_idx as usize));
                    mb.out_idx = (mb.out_idx + 1) % mb.size;
                    mb.count -= 1;
                    outcome = Ok(msg);
                    if let Some(w) = wake_head(&mut mb.sender_list, cs) {
                        need = resched_needed(w);
                    }
                } else if timeout == Timeout::Ticks(0) {
                    outcome = Err(Error::TimedOut);
                } else {
                    suspend_current(&mut mb.base.suspend_list, timeout.timer_delay(), cs);
                    blocked = true;
                }
            });
            if need {
                sched::schedule();
            }
            if blocked {
                // 唤醒后: 超时返回, 否则重试接收
                if blocked_wait() {
                    return Err(Error::TimedOut);
                }
                continue;
            }
            return outcome;
        }
    }

    #[inline]
    fn ptr(&self) -> *mut MailboxInner<T> {
        self.inner.get()
    }

    /// 临界区外确保消息池已分配 (见 [`MailboxInner::ensure_pool`])
    #[inline]
    fn ensure_pool(&self) -> *mut T {
        unsafe { (*self.ptr()).ensure_pool() }
    }
}

impl<T> Drop for Mailbox<T> {
    fn drop(&mut self) {
        let mb = self.inner.get_mut();
        let pool = mb.pool.load(core::sync::atomic::Ordering::Relaxed);
        if pool.is_null() {
            return;
        }
        // 池已由 ensure_pool 成功构造并发布 (布局与发布值一一对应)
        let layout = Layout::array::<T>(mb.size as usize).expect("邮箱消息池布局失效");
        unsafe { dealloc(pool.cast(), layout) };
        mb.pool.store(ptr::null_mut(), core::sync::atomic::Ordering::Relaxed);
    }
}

/// 邮箱内部状态: 消息池惰性分配 (临界区外)
impl<T: Copy> MailboxInner<T> {
    /// 确保消息池已分配并返回池指针。
    ///
    /// **绝不进入临界区**: 分配完成后经 CAS 一次性发布, 并发首用者
    /// 各分配一块、CAS 失败者释放自己的备份 —— 临界区内只读原子指针,
    /// 关中断时间不包含 malloc (硬实时要求)。
    fn ensure_pool(&self) -> *mut T {
        let pool = self.pool.load(core::sync::atomic::Ordering::Acquire);
        if !pool.is_null() {
            return pool;
        }
        let layout = Layout::array::<T>(self.size as usize).expect("邮箱消息池布局无效");
        let allocated = unsafe { alloc(layout) };
        assert!(!allocated.is_null(), "邮箱消息池分配失败");
        match self.pool.compare_exchange(
            ptr::null_mut(),
            allocated as *mut T,
            core::sync::atomic::Ordering::AcqRel,
            core::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => allocated as *mut T,
            Err(published) => {
                unsafe { dealloc(allocated, layout) };
                published
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 消息队列 (定长消息的链式缓冲)
// ---------------------------------------------------------------------------

/// 消息块链尾/空闲链表空标记
const BLOCK_END: u32 = u32::MAX;

/// 消息队列 (RT-Thread `rt_messagequeue`)
///
/// 消息池划分为定长块, 块布局: `[next: u32][len: u32][data: msg_size]`,
/// 已用块构成队列 (head→tail), 空闲块构成空闲链表。
pub struct MessageQueue {
    inner: UnsafeCell<MessageQueueInner>,
}

struct MessageQueueInner {
    base: IpcBase,
    sender_list: ListHead,
    /// 消息池 (惰性初始化; 原子指针: 分配与空闲链初始化在临界区**外**
    /// 完成, 经 CAS 一次性发布 —— 临界区内绝不发生 malloc)
    pool: core::sync::atomic::AtomicPtr<u8>,
    block_size: u32,
    msg_size: u32,
    max_msgs: u32,
    head: u32,
    tail: u32,
    free: u32,
    count: u32,
}

unsafe impl Send for MessageQueue {}
unsafe impl Sync for MessageQueue {}

impl MessageQueue {
    /// 创建消息队列: 消息大小 (字节) / 最大消息数
    ///
    /// 消息池在**首次使用**时分配 (惰性初始化, 使 [`MessageQueue`]
    /// 可作 `static` 使用); 常量求值时仅检查参数合法性。
    pub const fn new(msg_size: usize, max_msgs: usize) -> Self {
        assert!(msg_size > 0 && max_msgs > 0, "MessageQueue::new: 参数无效");
        let with_header = match msg_size.checked_add(8) {
            Some(value) => value,
            None => panic!("MessageQueue::new: 块大小溢出"),
        };
        let with_padding = match with_header.checked_add(3) {
            Some(value) => value,
            None => panic!("MessageQueue::new: 块对齐溢出"),
        };
        let block_size = with_padding & !3;
        assert!(
            block_size <= u32::MAX as usize && max_msgs <= u32::MAX as usize,
            "MessageQueue::new: 参数超出 u32"
        );
        let pool_size = match block_size.checked_mul(max_msgs) {
            Some(value) => value,
            None => panic!("MessageQueue::new: 消息池大小溢出"),
        };
        assert!(
            pool_size <= isize::MAX as usize,
            "MessageQueue::new: 消息池超出 Layout 上限"
        );
        Self {
            inner: UnsafeCell::new(MessageQueueInner {
                base: IpcBase::const_new(),
                sender_list: ListHead::const_new(),
                pool: core::sync::atomic::AtomicPtr::new(ptr::null_mut()),
                // 块大小 = 8 (next+len 头) + 消息大小, 4 字节对齐 (≥ 12)
                block_size: block_size as u32,
                msg_size: msg_size as u32,
                max_msgs: max_msgs as u32,
                head: BLOCK_END,
                tail: BLOCK_END,
                free: 0,
                count: 0,
            }),
        }
    }

    /// 发送消息: 拷贝到消息池 (满时按超时阻塞)
    pub fn send(&self, buf: &[u8], timeout: Timeout) -> Result<(), Error> {
        self.send_impl(buf, timeout, false)
    }

    /// 紧急发送: 插入队首
    pub fn urgent(&self, buf: &[u8], timeout: Timeout) -> Result<(), Error> {
        self.send_impl(buf, timeout, true)
    }

    fn send_impl(&self, buf: &[u8], timeout: Timeout, urgent: bool) -> Result<(), Error> {
        ensure_ipc_context(timeout)?;
        // 池惰性分配在临界区外完成 (无锁 CAS 发布), 保证关中断区间无 malloc
        self.ensure_pool();
        loop {
            let mut outcome = Err(Error::Full);
            let mut need = false;
            let mut blocked = false;
            critical_section::with(|cs| unsafe {
                let q = &mut *self.ptr();
                if q.free != BLOCK_END {
                    let b = q.free;
                    q.free = q.block_next(b);
                    let len = buf.len().min(q.msg_size as usize);
                    q.set_block_len(b, len);
                    core::ptr::copy_nonoverlapping(buf.as_ptr(), q.block_data(b), len);
                    // 入队 (队首或队尾)
                    if urgent {
                        q.set_block_next(b, q.head);
                        q.head = b;
                        if q.count == 0 {
                            q.tail = b;
                        }
                    } else if q.count == 0 {
                        q.head = b;
                        q.tail = b;
                        q.set_block_next(b, BLOCK_END);
                    } else {
                        q.set_block_next(q.tail, b);
                        q.tail = b;
                        q.set_block_next(b, BLOCK_END);
                    }
                    q.count += 1;
                    outcome = Ok(());
                    if let Some(w) = wake_head(&mut q.base.suspend_list, cs) {
                        need = resched_needed(w);
                    }
                } else if timeout == Timeout::Ticks(0) {
                    outcome = Err(Error::Full);
                } else {
                    suspend_current(&mut q.sender_list, timeout.timer_delay(), cs);
                    blocked = true;
                }
            });
            if need {
                sched::schedule();
            }
            if blocked {
                // 唤醒后: 超时返回, 否则重试发送 (消息不得丢失)
                if blocked_wait() {
                    return Err(Error::TimedOut);
                }
                continue;
            }
            return outcome;
        }
    }

    /// 接收消息: 拷贝到 `buf`, 返回实际字节数 (空时按超时阻塞)
    pub fn recv(&self, buf: &mut [u8], timeout: Timeout) -> Result<usize, Error> {
        ensure_ipc_context(timeout)?;
        // 池惰性分配在临界区外完成 (无锁 CAS 发布)
        self.ensure_pool();
        loop {
            let mut outcome = Err(Error::TimedOut);
            let mut need = false;
            let mut blocked = false;
            critical_section::with(|cs| unsafe {
                let q = &mut *self.ptr();
                if q.count > 0 {
                    let b = q.head;
                    q.head = q.block_next(b);
                    q.count -= 1;
                    let len = q.block_len(b).min(buf.len());
                    core::ptr::copy_nonoverlapping(q.block_data(b), buf.as_mut_ptr(), len);
                    // 归还空闲链表
                    q.set_block_next(b, q.free);
                    q.free = b;
                    outcome = Ok(len);
                    if let Some(w) = wake_head(&mut q.sender_list, cs) {
                        need = resched_needed(w);
                    }
                } else if timeout == Timeout::Ticks(0) {
                    outcome = Err(Error::TimedOut);
                } else {
                    suspend_current(&mut q.base.suspend_list, timeout.timer_delay(), cs);
                    blocked = true;
                }
            });
            if need {
                sched::schedule();
            }
            if blocked {
                // 唤醒后: 超时返回, 否则重试接收
                if blocked_wait() {
                    return Err(Error::TimedOut);
                }
                continue;
            }
            return outcome;
        }
    }

    #[inline]
    fn ptr(&self) -> *mut MessageQueueInner {
        self.inner.get()
    }

    /// 临界区外确保消息池已分配 (见 [`MessageQueueInner::ensure_pool`])
    #[inline]
    fn ensure_pool(&self) -> *mut u8 {
        unsafe { (*self.ptr()).ensure_pool() }
    }
}

impl Drop for MessageQueue {
    fn drop(&mut self) {
        let q = self.inner.get_mut();
        let pool = q.pool.load(core::sync::atomic::Ordering::Relaxed);
        if pool.is_null() {
            return;
        }
        let layout = q.pool_layout();
        unsafe { dealloc(pool, layout) };
        q.pool.store(ptr::null_mut(), core::sync::atomic::Ordering::Relaxed);
    }
}

/// 消息队列内部状态: 块访问辅助
impl MessageQueueInner {
    /// 确保消息池已分配并返回池指针。
    ///
    /// **绝不进入临界区**: 分配 + 空闲链初始化完成后经 CAS 一次性发布,
    /// 并发首用者各构造一份、CAS 失败者释放自己的备份 —— 临界区内
    /// 只读原子指针, 关中断时间不包含 malloc (硬实时要求)。
    fn ensure_pool(&self) -> *mut u8 {
        let pool = self.pool.load(core::sync::atomic::Ordering::Acquire);
        if !pool.is_null() {
            return pool;
        }
        let max_msgs = self.max_msgs as usize;
        let layout = self.pool_layout();
        let allocated = unsafe { alloc(layout) };
        assert!(!allocated.is_null(), "消息队列池分配失败");
        // 初始化空闲链表 (必须在 CAS 发布前完成, 保证发布值完全可用)
        unsafe {
            core::ptr::write_bytes(allocated, 0, layout.size());
            let init_pool = allocated;
            for i in 0..max_msgs {
                let next = if i + 1 < max_msgs {
                    (i + 1) as u32
                } else {
                    BLOCK_END
                };
                *((init_pool.add(i * self.block_size as usize)) as *mut u32) = next;
            }
        }
        match self.pool.compare_exchange(
            ptr::null_mut(),
            allocated,
            core::sync::atomic::Ordering::AcqRel,
            core::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => allocated,
            Err(published) => {
                unsafe { dealloc(allocated, layout) };
                published
            }
        }
    }

    fn pool_layout(&self) -> Layout {
        let bytes = (self.block_size as usize)
            .checked_mul(self.max_msgs as usize)
            .expect("消息队列池大小失效");
        Layout::from_size_align(bytes, core::mem::align_of::<u32>()).expect("消息队列池布局失效")
    }

    #[inline]
    fn block_ptr(&self, idx: u32) -> *mut u8 {
        unsafe { self.pool_ptr().add(idx as usize * self.block_size as usize) }
    }

    /// 池指针 (调用方保证已发布)
    #[inline]
    fn pool_ptr(&self) -> *mut u8 {
        let pool = self.pool.load(core::sync::atomic::Ordering::Relaxed);
        debug_assert!(!pool.is_null(), "消息池未分配");
        pool
    }

    #[inline]
    unsafe fn block_next(&self, idx: u32) -> u32 {
        unsafe { *(self.block_ptr(idx) as *const u32) }
    }

    #[inline]
    unsafe fn set_block_next(&self, idx: u32, next: u32) {
        unsafe { *(self.block_ptr(idx) as *mut u32) = next };
    }

    #[inline]
    unsafe fn block_len(&self, idx: u32) -> usize {
        unsafe { *(self.block_ptr(idx).add(4) as *const u32) as usize }
    }

    #[inline]
    unsafe fn set_block_len(&self, idx: u32, len: usize) {
        unsafe { *(self.block_ptr(idx).add(4) as *mut u32) = len as u32 };
    }

    #[inline]
    fn block_data(&self, idx: u32) -> *mut u8 {
        unsafe { self.block_ptr(idx).add(8) }
    }
}
