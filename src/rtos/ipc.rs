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

// 内核模块: unsafe 契约由临界区与模块文档统一说明, 函数体内不再逐段包裹
#![allow(unsafe_op_in_unsafe_fn)]
// IPC API 全集, 部分供应用选用 (演示工程仅使用其中一部分)
#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::ptr;

use alloc::vec;

use crate::critical_section;
use crate::rtos::klist::ListHead;
use crate::rtos::sched;
use crate::rtos::thread::{
    TS_SUSPEND, Thread, blocked_wait, resched_needed, thread_from_suspend, thread_timer_cb,
    wakeup_thread,
};

/// 阻塞超时
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    /// 无限等待
    Forever,
    /// 等待指定 tick 数
    Ticks(u32),
}

impl Timeout {
    fn ticks(self) -> u32 {
        match self {
            Timeout::Forever => u32::MAX,
            Timeout::Ticks(t) => t,
        }
    }
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
/// `ticks == u32::MAX` (无限等待) 时不启动定时器, 仅能由显式唤醒解除。
unsafe fn suspend_current(list: *mut ListHead, ticks: u32) {
    let cur = sched::current();
    (*cur).error = 0;
    (*cur).suspend_node.remove();
    (*list).push_back(&mut (*cur).suspend_node);
    if ticks != u32::MAX {
        (*cur)
            .thread_timer
            .start_internal(ticks, 0, thread_timer_cb, cur as usize);
    }
    (*cur).state = TS_SUSPEND;
    sched::ready_remove(cur);
}

/// 临界区内: 当前线程按优先级 (数字小者靠前) 挂入对象挂起队列
///
/// `ticks == u32::MAX` (无限等待) 时不启动定时器。
unsafe fn suspend_prio(list: *mut ListHead, ticks: u32) {
    let cur = sched::current();
    (*cur).error = 0;
    (*cur).suspend_node.remove();
    // 找到第一个"优先级更低"的等待者, 插入其前
    let mut before: *mut ListHead = ptr::null_mut();
    let mut n = (*list).first();
    while let Some(node) = n {
        let w = thread_from_suspend(node);
        if (*w).current_priority > (*cur).current_priority {
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
        (*list).push_back(&mut (*cur).suspend_node);
    } else {
        (*before).insert_before(&mut (*cur).suspend_node);
    }
    if ticks != u32::MAX {
        (*cur)
            .thread_timer
            .start_internal(ticks, 0, thread_timer_cb, cur as usize);
    }
    (*cur).state = TS_SUSPEND;
    sched::ready_remove(cur);
}

/// 临界区内: 唤醒挂起队列头部等待者
///
/// 返回被唤醒线程 (由调用方决定是否需要重新调度)。
unsafe fn wake_head(list: *mut ListHead) -> Option<*mut Thread> {
    let node = (*list).pop_first()?;
    let w = thread_from_suspend(node);
    wakeup_thread(w);
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
    pub fn take(&self, timeout: Timeout) -> Result<(), Error> {
        let mut outcome = Ok(());
        let mut blocked = false;
        critical_section::with(|| unsafe {
            let s = &mut *self.ptr();
            if s.value > 0 {
                s.value -= 1;
            } else if timeout == Timeout::Ticks(0) {
                outcome = Err(Error::TimedOut);
            } else {
                suspend_current(&mut s.base.suspend_list, timeout.ticks());
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
        let need = critical_section::with(|| unsafe {
            let s = &mut *self.ptr();
            if let Some(w) = wake_head(&mut s.base.suspend_list) {
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

/// 互斥量: 递归持有 + 优先级继承
///
/// 等待队列按优先级排序, 释放时所有权转移给最高优先级等待者;
/// 阻塞等待者会提升持有者 (及持有者的等待链) 的当前优先级,
/// 缓解无界优先级反转 (与 RT-Thread 一致)。
pub struct Mutex {
    inner: UnsafeCell<MutexInner>,
}

/// 互斥量内部状态 (以 [`UnsafeCell`] 包裹)
pub(crate) struct MutexInner {
    pub(crate) base: IpcBase,
    pub(crate) owner: *mut Thread,
    pub(crate) hold: u8,
    pub(crate) taken_node: ListHead,
}

unsafe impl Send for Mutex {}
unsafe impl Sync for Mutex {}

impl Mutex {
    /// 创建互斥量 (未持有)
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(MutexInner {
                base: IpcBase::const_new(),
                owner: ptr::null_mut(),
                hold: 0,
                taken_node: ListHead::const_new(),
            }),
        }
    }

    /// 获取互斥量 (递归持有合法; 可超时)
    pub fn lock(&self, timeout: Timeout) -> Result<(), Error> {
        let mut outcome = Ok(());
        let mut blocked = false;
        critical_section::with(|| unsafe {
            let m = &mut *self.ptr();
            let cur = sched::current();
            if m.owner.is_null() {
                m.owner = cur;
                m.hold = 1;
                (*cur).taken_list.insert_after(&mut m.taken_node);
            } else if m.owner == cur {
                m.hold = m.hold.saturating_add(1);
            } else if timeout == Timeout::Ticks(0) {
                outcome = Err(Error::TimedOut);
            } else {
                // 优先级继承: 提升持有者 (及继承链) 到当前线程优先级
                let own = m.owner;
                if (*own).current_priority > (*cur).current_priority {
                    priority_inherit(own, (*cur).current_priority);
                }
                (*cur).pending_mutex = self.ptr();
                suspend_prio(&mut m.base.suspend_list, timeout.ticks());
                blocked = true;
            }
        });
        if blocked && blocked_wait() {
            return Err(Error::TimedOut);
        }
        outcome
    }

    /// 释放互斥量 (仅持有者有效; 非持有者调用被忽略)
    ///
    /// 释放后取消优先级继承 (恢复初始优先级), 所有权转移给
    /// 最高优先级等待者。
    pub fn unlock(&self) {
        let need = critical_section::with(|| unsafe {
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
            // 取消优先级继承
            sched::change_priority(cur, (*cur).init_priority);
            // 所有权转移给最高优先级等待者
            if let Some(w) = wake_head(&mut m.base.suspend_list) {
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
    pub(crate) fn owner(&self) -> *mut Thread {
        critical_section::with(|| unsafe { (*self.ptr()).owner })
    }
}

/// 优先级继承: 沿"等待互斥量 → 持有者"链提升优先级
unsafe fn priority_inherit(mut chain: *mut Thread, prio: u8) {
    loop {
        if unsafe { (*chain).current_priority } <= prio {
            return;
        }
        sched::change_priority(chain, prio);
        let pm = unsafe { (*chain).pending_mutex };
        if pm.is_null() {
            return;
        }
        let owner = unsafe { (*pm).owner };
        if owner.is_null() || owner == chain {
            return;
        }
        chain = owner;
    }
}

/// 临界区内: 线程退出时释放其持有的全部互斥量 (所有权转移给等待者)
pub(crate) unsafe fn mutex_release_all_held(t: *mut Thread) {
    while let Some(node) = unsafe { (*t).taken_list.first() } {
        let m = mutex_from_taken(node);
        if let Some(w) = wake_head(&mut unsafe { (*m).base.suspend_list }) {
            (*w).pending_mutex = ptr::null_mut();
            (*m).owner = w;
            (*m).hold = 1;
            (*w).taken_list.insert_after(&mut (*m).taken_node);
        } else {
            (*m).owner = ptr::null_mut();
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
        critical_section::with(|| unsafe { (*self.ptr()).set })
    }

    /// 发送事件: 置位并唤醒所有满足条件的等待者
    pub fn send(&self, set: u32) {
        let need = critical_section::with(|| unsafe {
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
                    wakeup_thread(w);
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
        let mut outcome = Err(Error::TimedOut);
        let mut blocked = false;
        critical_section::with(|| unsafe {
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
                suspend_current(&mut e.base.suspend_list, timeout.ticks());
                blocked = true;
            }
        });
        if blocked {
            if blocked_wait() {
                return Err(Error::TimedOut);
            }
            // 返回唤醒时匹配的事件位 (由 Event::send 记录)
            outcome = Ok(critical_section::with(|| unsafe {
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
// 邮箱 (环形缓冲, 存机器字)
// ---------------------------------------------------------------------------

/// 邮箱: 环形缓冲存放机器字消息 (RT-Thread `rt_mailbox`)
///
/// 接收等待者挂于 `base.suspend_list`, 发送等待者挂于 `sender_list`。
pub struct Mailbox {
    inner: UnsafeCell<MailboxInner>,
}

struct MailboxInner {
    base: IpcBase,
    sender_list: ListHead,
    pool: *mut usize,
    size: u32,
    count: u32,
    in_idx: u32,
    out_idx: u32,
}

unsafe impl Send for Mailbox {}
unsafe impl Sync for Mailbox {}

impl Mailbox {
    /// 创建容量为 `capacity` 的邮箱
    ///
    /// 消息池在**首次使用**时分配 (惰性初始化, 使 [`Mailbox`] 可作
    /// `static` 使用); 常量求值时仅检查容量合法性。
    pub const fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Mailbox::new: 容量必须大于 0");
        Self {
            inner: UnsafeCell::new(MailboxInner {
                base: IpcBase::const_new(),
                sender_list: ListHead::const_new(),
                pool: ptr::null_mut(),
                size: capacity as u32,
                count: 0,
                in_idx: 0,
                out_idx: 0,
            }),
        }
    }

    /// 发送消息 (满时按超时阻塞)
    pub fn send(&self, msg: usize, timeout: Timeout) -> Result<(), Error> {
        self.send_impl(msg, timeout, false)
    }

    /// 紧急发送: 插入队首
    pub fn urgent(&self, msg: usize, timeout: Timeout) -> Result<(), Error> {
        self.send_impl(msg, timeout, true)
    }

    fn send_impl(&self, msg: usize, timeout: Timeout, urgent: bool) -> Result<(), Error> {
        let mut outcome = Err(Error::Full);
        let mut need = false;
        let mut blocked = false;
        critical_section::with(|| unsafe {
            let mb = &mut *self.ptr();
            mb.ensure_pool();
            if mb.count < mb.size {
                if urgent {
                    mb.out_idx = (mb.out_idx + mb.size - 1) % mb.size;
                    *mb.pool.add(mb.out_idx as usize) = msg;
                } else {
                    *mb.pool.add(mb.in_idx as usize) = msg;
                    mb.in_idx = (mb.in_idx + 1) % mb.size;
                }
                mb.count += 1;
                outcome = Ok(());
                if let Some(w) = wake_head(&mut mb.base.suspend_list) {
                    need = resched_needed(w);
                }
            } else if timeout == Timeout::Ticks(0) {
                outcome = Err(Error::Full);
            } else {
                suspend_current(&mut mb.sender_list, timeout.ticks());
                blocked = true;
            }
        });
        if need {
            sched::schedule();
        }
        if blocked && blocked_wait() {
            return Err(Error::TimedOut);
        }
        outcome
    }

    /// 接收消息 (空时按超时阻塞)
    pub fn recv(&self, timeout: Timeout) -> Result<usize, Error> {
        let mut outcome = Err(Error::TimedOut);
        let mut need = false;
        let mut blocked = false;
        critical_section::with(|| unsafe {
            let mb = &mut *self.ptr();
            mb.ensure_pool();
            if mb.count > 0 {
                let msg = *mb.pool.add(mb.out_idx as usize);
                mb.out_idx = (mb.out_idx + 1) % mb.size;
                mb.count -= 1;
                outcome = Ok(msg);
                if let Some(w) = wake_head(&mut mb.sender_list) {
                    need = resched_needed(w);
                }
            } else if timeout == Timeout::Ticks(0) {
                outcome = Err(Error::TimedOut);
            } else {
                suspend_current(&mut mb.base.suspend_list, timeout.ticks());
                blocked = true;
            }
        });
        if need {
            sched::schedule();
        }
        if blocked && blocked_wait() {
            return Err(Error::TimedOut);
        }
        outcome
    }

    #[inline]
    fn ptr(&self) -> *mut MailboxInner {
        self.inner.get()
    }
}

/// 邮箱内部状态: 消息池惰性分配
impl MailboxInner {
    /// 临界区内: 惰性分配消息池
    ///
    /// 注意: 必须以 `&mut self` 直接写字段 (通过共享引用派生裸指针
    /// 写入会被编译器按死代码消除)。
    unsafe fn ensure_pool(&mut self) {
        if !self.pool.is_null() {
            return;
        }
        let v = vec![0usize; self.size as usize];
        let (pool, _, _) = v.into_raw_parts();
        self.pool = pool;
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
    pool: *mut u8,
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
        Self {
            inner: UnsafeCell::new(MessageQueueInner {
                base: IpcBase::const_new(),
                sender_list: ListHead::const_new(),
                pool: ptr::null_mut(),
                // 块大小 = 8 (next+len 头) + 消息大小, 4 字节对齐 (≥ 12)
                block_size: ((8 + msg_size + 3) & !3) as u32,
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
        let mut outcome = Err(Error::Full);
        let mut need = false;
        let mut blocked = false;
        critical_section::with(|| unsafe {
            let q = &mut *self.ptr();
            q.ensure_pool();
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
                if let Some(w) = wake_head(&mut q.base.suspend_list) {
                    need = resched_needed(w);
                }
            } else if timeout == Timeout::Ticks(0) {
                outcome = Err(Error::Full);
            } else {
                suspend_current(&mut q.sender_list, timeout.ticks());
                blocked = true;
            }
        });
        if need {
            sched::schedule();
        }
        if blocked && blocked_wait() {
            return Err(Error::TimedOut);
        }
        outcome
    }

    /// 接收消息: 拷贝到 `buf`, 返回实际字节数 (空时按超时阻塞)
    pub fn recv(&self, buf: &mut [u8], timeout: Timeout) -> Result<usize, Error> {
        let mut outcome = Err(Error::TimedOut);
        let mut need = false;
        let mut blocked = false;
        critical_section::with(|| unsafe {
            let q = &mut *self.ptr();
            q.ensure_pool();
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
                if let Some(w) = wake_head(&mut q.sender_list) {
                    need = resched_needed(w);
                }
            } else if timeout == Timeout::Ticks(0) {
                outcome = Err(Error::TimedOut);
            } else {
                suspend_current(&mut q.base.suspend_list, timeout.ticks());
                blocked = true;
            }
        });
        if need {
            sched::schedule();
        }
        if blocked && blocked_wait() {
            return Err(Error::TimedOut);
        }
        outcome
    }

    #[inline]
    fn ptr(&self) -> *mut MessageQueueInner {
        self.inner.get()
    }
}

/// 消息队列内部状态: 块访问辅助
impl MessageQueueInner {
    /// 临界区内: 惰性分配消息池并初始化空闲链表
    ///
    /// 注意: 必须以 `&mut self` 直接写字段 (通过共享引用派生裸指针
    /// 写入会被编译器按死代码消除)。
    unsafe fn ensure_pool(&mut self) {
        if !self.pool.is_null() {
            return;
        }
        let block_size = self.block_size as usize;
        let max_msgs = self.max_msgs as usize;
        let v = vec![0u8; block_size * max_msgs];
        let (pool, _, _) = v.into_raw_parts();
        self.pool = pool;
        for i in 0..max_msgs {
            let next = if i + 1 < max_msgs {
                (i + 1) as u32
            } else {
                BLOCK_END
            };
            self.set_block_next(i as u32, next);
        }
    }

    #[inline]
    fn block_ptr(&self, idx: u32) -> *mut u8 {
        unsafe { self.pool.add(idx as usize * self.block_size as usize) }
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
