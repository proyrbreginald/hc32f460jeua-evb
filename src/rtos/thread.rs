//! 线程管理 — RT-Thread `thread.c` 的移植
//!
//! - 线程创建: 分配 TCB + 栈, 构造初始栈帧后自动启动 (就绪);
//! - 线程退出: 入口返回后硬件跳入 [`thread_exit`], 清理后进入
//!   僵尸队列, 由空闲线程回收 (defunct 机制);
//! - 延时/挂起: 线程定时器 + 睡眠队列, 超时回调唤醒。

// 内核模块: unsafe 契约由临界区与模块文档统一说明, 函数体内不再逐段包裹
#![allow(unsafe_op_in_unsafe_fn)]

use core::ptr;

use alloc::alloc::{Layout, alloc, dealloc};
use alloc::sync::Arc;

use crate::critical_section;
use crate::critical_section::CriticalSection;
use crate::rtos::context;
use crate::rtos::idle::defunct_push;
use crate::rtos::ipc::{Error, EventOpt, MutexInner, mutex_release_all_held};
use crate::rtos::klist::{KCell, ListHead};
use crate::rtos::sched;
use crate::rtos::timer::Timer;
use crate::rtos::{PRIORITY_MAX, TICKS_PER_SEC};

/// 线程状态
pub(crate) const TS_INIT: u8 = 0;
pub(crate) const TS_READY: u8 = 1;
pub(crate) const TS_RUNNING: u8 = 2;
pub(crate) const TS_SUSPEND: u8 = 3;
pub(crate) const TS_CLOSE: u8 = 4;

/// 线程栈填充魔数 (溢出检测)
pub(crate) const STACK_PATTERN: u32 = 0xA5A5_A5A5;

/// 睡眠队列 (线程延时挂起; 与 IPC 挂起队列共用 suspend_node)
static SLEEP_LIST: KCell<ListHead> = KCell::new(ListHead::const_new());

/// 全部已创建线程链表 (供 `ps` 等诊断遍历)
static ALL_THREADS: KCell<ListHead> = KCell::new(ListHead::const_new());

/// 线程控制块 (TCB) — RT-Thread `struct rt_thread` 的 Rust 移植
///
/// 由内核以 `Arc` 管理: [`thread_create`] 返回 [`Arc<Thread>`] 句柄,
/// **句柄在 TCB 被回收后仍可使用 (不悬垂)** —— TCB 的存活由
/// 用户句柄与内核侧强引用 ([`Thread::kernel_self`]) 共同维持:
///
/// - 线程退出/删除后, 空闲线程回收**栈**并释放内核侧强引用;
///   只要仍有用户句柄, TCB 本身保持存活;
/// - 全部用户句柄释放后, TCB 才被释放。
///
/// 除 [`Thread::name`] 外, 字段仅在临界区 (关中断) 内访问。
pub struct Thread {
    /// 内核侧强引用 (与用户句柄构成 TCB 的存活集合)
    ///
    /// 线程创建时写入; 回收 (空闲线程) 时取出并释放 —— 若用户仍持有
    /// [`Arc<Thread>`] 句柄, TCB 由句柄维持, 杜绝 use-after-free。
    pub(crate) kernel_self: Option<Arc<Thread>>,
    /// 保存的线程栈指针 (PSP, 由 PendSV 汇编读写)
    pub(crate) sp: usize,
    /// 栈基址 / 大小
    pub(crate) stack_addr: usize,
    pub(crate) stack_size: usize,
    /// 入口参数 (诊断用, 当前版本仅用于初始栈帧)
    #[allow(dead_code)]
    pub(crate) parameter: usize,
    /// 线程名 (诊断用)
    pub(crate) name: &'static str,
    /// 初始优先级 / 当前优先级 (互斥量继承时被提升)
    pub(crate) init_priority: u8,
    pub(crate) current_priority: u8,
    /// 时间片 (tick), 0 = 不参与轮转
    pub(crate) init_tick: u32,
    pub(crate) remaining_tick: u32,
    /// 状态 (TS_*)
    pub(crate) state: u8,
    /// 让出标志 (时间片耗尽/主动 yield, 同优先级轮转判定)
    pub(crate) yielded: bool,
    /// 错误码 (IPC 超时由定时器回调写入)
    pub(crate) error: i32,
    /// 就绪队列节点
    pub(crate) ready_node: ListHead,
    /// 挂起队列节点 (IPC 等待队列 / 睡眠队列)
    pub(crate) suspend_node: ListHead,
    /// 僵尸队列节点
    pub(crate) defunct_node: ListHead,
    /// 全局线程链表节点 (供诊断遍历)
    pub(crate) list_node: ListHead,
    /// 线程内建定时器 (超时回调 = [`thread_timer_cb`])
    pub(crate) thread_timer: Timer,
    /// 已持有互斥量链表头
    pub(crate) taken_list: ListHead,
    /// 当前等待的互斥量 (优先级继承链)
    pub(crate) pending_mutex: *mut MutexInner,
    /// 事件等待信息
    pub(crate) event_wanted: u32,
    pub(crate) event_opt: EventOpt,
    /// 事件唤醒时匹配的事件位 (由 Event::send 写入, recv 返回)
    pub(crate) event_recv_bits: u32,
}

unsafe impl Send for Thread {}
unsafe impl Sync for Thread {}

impl Thread {
    /// 线程名
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// 当前优先级
    pub fn priority(&self) -> u8 {
        critical_section::with(|_| self.current_priority)
    }

    /// 删除线程: 进入僵尸队列, 由空闲线程回收资源
    ///
    /// 对当前线程调用时**永不返回** (删除自身)。
    pub fn delete(&self) {
        let t = self as *const Thread as *mut Thread;
        if t == sched::current() {
            unsafe { exit_and_schedule(t) };
        }
        critical_section::with(|cs| unsafe { delete_bookkeeping(t, cs) });
        sched::schedule();
    }

    /// 挂起线程 (仅可就绪线程挂起, 不可挂起自身)
    ///
    /// 挂起的线程只能通过 [`Thread::resume`] 恢复。
    pub fn suspend(&self) -> Result<(), Error> {
        let t = self as *const Thread as *mut Thread;
        if t == sched::current() {
            return Err(Error::Invalid);
        }
        let mut ok = false;
        let mut need = false;
        critical_section::with(|cs| unsafe {
            if (*t).state == TS_READY || (*t).state == TS_RUNNING {
                sched::ready_remove(t, cs);
                (*t).state = TS_SUSPEND;
                ok = true;
                need = (*t).current_priority < (*sched::current()).current_priority;
            }
        });
        if !ok {
            return Err(Error::Invalid);
        }
        if need {
            sched::schedule();
        }
        Ok(())
    }

    /// 恢复挂起的线程 (含延时/IPC 等待中的线程)
    ///
    /// 注意: 对挂在**信号量/互斥量**等待队列上的线程恢复属于未定义语义
    /// (唤醒即"获得"是 release/unlock 的职责), 本 API 仅应在延时/显式
    /// 挂起场景使用; 邮箱/消息队列等待者被恢复后经重查条件自洽。
    pub fn resume(&self) -> Result<(), Error> {
        let t = self as *const Thread as *mut Thread;
        let mut need = false;
        let ok = critical_section::with(|cs| unsafe {
            if (*t).state != TS_SUSPEND {
                return false;
            }
            (*t).suspend_node.remove();
            wakeup_thread(t, cs);
            need = resched_needed(t);
            true
        });
        if !ok {
            return Err(Error::Invalid);
        }
        if need {
            sched::schedule();
        }
        Ok(())
    }
}

/// 创建并启动一个线程 (RT-Thread `rt_thread_create` + `rt_thread_startup`)
///
/// 参数: 名称 / 栈大小 (字节) / 优先级 (0 最高, 31 为最低/空闲) /
/// 时间片 (tick, 0 = 不参与轮转) / 入口函数 (`extern "C" fn(usize)`) / 参数。
///
/// 返回 [`Arc<Thread>`] 用户句柄: 线程退出/删除后 TCB 由句柄维持存活
/// (栈已被空闲线程回收), 句柄不会悬垂; 丢弃句柄不影响线程运行
/// (内核侧强引用维持 TCB)。
pub fn thread_create(
    name: &'static str,
    stack_size: usize,
    priority: u8,
    timeslice: u32,
    entry: extern "C" fn(usize),
    param: usize,
) -> Arc<Thread> {
    assert!(priority < PRIORITY_MAX, "thread_create: 优先级超出范围");
    assert!(stack_size >= 256, "thread_create: 栈过小");

    // 分配线程栈 (8 字节对齐), 填充溢出检测魔数
    let layout = Layout::from_size_align(stack_size, 8).expect("栈布局无效");
    let stack = unsafe { alloc(layout) };
    assert!(!stack.is_null(), "thread_create: 栈分配失败");
    unsafe {
        let words = stack as *mut u32;
        for i in 0..stack_size / 4 {
            words.add(i).write_volatile(STACK_PATTERN);
        }
    }

    // TCB 以 Arc 管理: 用户句柄 + 内核侧强引用 (kernel_self) 各持一份
    // 引用计数, TCB 在两者全部释放前保持存活 (见 Thread::kernel_self)。
    let arc: Arc<Thread> = Arc::new(Thread {
        kernel_self: None,
        sp: 0,
        stack_addr: stack as usize,
        stack_size,
        parameter: param,
        name,
        init_priority: priority,
        current_priority: priority,
        init_tick: timeslice,
        remaining_tick: timeslice,
        state: TS_INIT,
        yielded: false,
        error: 0,
        ready_node: ListHead::const_new(),
        suspend_node: ListHead::const_new(),
        defunct_node: ListHead::const_new(),
        list_node: ListHead::const_new(),
        thread_timer: Timer::new(),
        taken_list: ListHead::const_new(),
        pending_mutex: ptr::null_mut(),
        event_wanted: 0,
        event_opt: EventOpt::Or,
        event_recv_bits: 0,
    });
    // 计数: new=1 → clone(kernel_arc)=2 → into_raw 后由 from_raw
    // 重建用户句柄 (消费同一计数), kernel_arc 与 user_arc 各持 1
    let kernel_arc = Arc::clone(&arc);
    let t = Arc::into_raw(arc) as *mut Thread;
    let user_arc = unsafe { Arc::from_raw(t) };

    // 构造初始栈帧: 入口返回后硬件跳入 thread_exit
    unsafe {
        (*t).sp = context::init_stack(
            stack,
            stack_size,
            entry as usize,
            param,
            thread_exit as *const () as usize,
        );
        // 登记内核侧强引用: 之后 TCB 由 kernel_self 与 user_arc 共同维持
        (*t).kernel_self = Some(kernel_arc);
    }

    // 启动: 加入就绪队列 + 全局线程链表
    critical_section::with(|cs| unsafe {
        (*t).state = TS_READY;
        sched::ready_insert(t, cs);
        // volatile 读屏障: 防止"写后无读"将链表写入判定为死存储而消除
        let head = ALL_THREADS.get(cs) as *mut ListHead;
        let _ = core::ptr::read_volatile(head);
        (*ALL_THREADS.get(cs)).push_back(&mut (*t).list_node);
        let _ = core::ptr::read_volatile(head);
    });

    user_arc
}

/// 线程状态 → 名称 (诊断显示)
pub fn thread_state_name(state: u8) -> &'static str {
    match state {
        TS_INIT => "init",
        TS_READY => "ready",
        TS_RUNNING => "running",
        TS_SUSPEND => "suspend",
        TS_CLOSE => "close",
        _ => "?",
    }
}

/// 线程快照 (供 `ps` 等诊断命令使用)
#[derive(Clone, Copy)]
pub struct ThreadInfo {
    /// 线程名
    pub name: &'static str,
    /// 当前优先级 (0 最高)
    pub priority: u8,
    /// 线程状态 (见 [`thread_state_name`])
    pub state: u8,
}

/// 全部已创建线程的快照列表 (按创建顺序, 供诊断命令使用)
pub fn thread_info_list() -> alloc::vec::Vec<ThreadInfo> {
    let mut list = alloc::vec::Vec::new();
    critical_section::with(|cs| unsafe {
        let head = ALL_THREADS.get(cs) as *const ListHead as *mut ListHead;
        let mut node = (*head).next_node();
        while !node.is_null() && node != head {
            let t = crate::rtos::klist::container_of!(node, Thread, list_node);
            list.push(ThreadInfo {
                name: (*t).name,
                priority: (*t).current_priority,
                state: (*t).state,
            });
            node = (*node).next_node();
        }
    });
    list
}

/// 线程入口返回后由硬件跳入 (初始帧 lr), 执行退出清理并切换, 永不返回
unsafe extern "C" fn thread_exit() -> ! {
    unsafe { exit_and_schedule(sched::current()) };
}

/// 退出清理 + 触发切换 (当前线程调用, 永不返回)
unsafe fn exit_and_schedule(t: *mut Thread) -> ! {
    critical_section::with(|cs| unsafe { delete_bookkeeping(t, cs) });
    sched::schedule();
    // PendSV 即将切换走, 此循环仅在切换前短暂执行
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

/// 临界区内: 线程删除/退出公共清理 (须持有临界区令牌)
unsafe fn delete_bookkeeping(t: *mut Thread, cs: CriticalSection<'_>) {
    // 已回收 (TS_CLOSE, 僵尸队列中): 拒绝重复删除, 防二次释放
    if (*t).state == TS_CLOSE {
        return;
    }
    // 释放持有的互斥量 (所有权转移给等待者)
    mutex_release_all_held(t, cs);
    // 停止线程定时器 (防止超时回调唤醒已删除线程)
    (*t).thread_timer.stop_internal();
    // 从就绪/挂起队列移除
    if (*t).ready_node.is_linked() {
        sched::ready_remove(t, cs);
    }
    (*t).suspend_node.remove();
    // 置关闭状态, 进入僵尸队列 (由空闲线程回收)
    (*t).state = TS_CLOSE;
    defunct_push(t, cs);
}

/// 线程延时 (tick); 0 时等价于让出 CPU
pub fn thread_delay(ticks: u32) {
    if ticks == 0 {
        unsafe { sched::yield_thread() };
        return;
    }
    critical_section::with(|cs| unsafe { delay_suspend(sched::current(), ticks, cs) });
    sched::schedule();
}

/// 线程延时 (ms)
pub fn thread_delay_ms(ms: u32) {
    thread_delay(ms.saturating_mul(TICKS_PER_SEC) / 1000);
}

/// 主动让出 CPU (同优先级轮转)
pub fn yield_now() {
    unsafe { sched::yield_thread() };
}

/// 临界区内: 当前线程挂起 `ticks` tick (进入睡眠队列)
unsafe fn delay_suspend(cur: *mut Thread, ticks: u32, cs: CriticalSection<'_>) {
    (*cur).error = 0;
    (*cur).suspend_node.remove();
    (*cur)
        .thread_timer
        .start_internal(ticks, 0, thread_timer_cb, cur as usize, cs);
    (*SLEEP_LIST.get(cs)).push_back(&mut (*cur).suspend_node);
    (*cur).state = TS_SUSPEND;
    sched::ready_remove(cur, cs);
}

/// 线程定时器超时回调: 唤醒挂起的线程 (延时超时 / IPC 超时)
///
/// 与显式唤醒的竞争由状态检查消解: 仅当线程仍处于挂起态时才唤醒。
pub(crate) extern "C" fn thread_timer_cb(param: usize) {
    let t = param as *mut Thread;
    let mut need = false;
    critical_section::with(|cs| unsafe {
        if (*t).state != TS_SUSPEND {
            return;
        }
        (*t).error = -1; // -RT_ETIMEDOUT
        (*t).pending_mutex = core::ptr::null_mut(); // 超时: 清理继承链残留
        (*t).suspend_node.remove();
        wakeup_thread(t, cs);
        need = resched_needed(t);
    });
    if need {
        sched::schedule();
    }
}

/// 挂起节点 → 线程
pub(crate) unsafe fn thread_from_suspend(node: *mut ListHead) -> *mut Thread {
    crate::rtos::klist::container_of!(node, Thread, suspend_node)
}

/// 就绪节点 → 线程
pub(crate) unsafe fn thread_from_ready(node: *mut ListHead) -> *mut Thread {
    crate::rtos::klist::container_of!(node, Thread, ready_node)
}

/// 临界区内: 唤醒挂起线程 (对齐 RT-Thread `rt_thread_resume` 的核心步骤)
///
/// 停止线程定时器 → 状态就绪 → 入就绪队列。调用方须保证线程处于
/// 挂起态 (状态检查由各调用路径完成, 超时回调与显式唤醒互斥)。
pub(crate) unsafe fn wakeup_thread(t: *mut Thread, cs: CriticalSection<'_>) {
    (*t).thread_timer.stop_internal();
    (*t).state = TS_READY;
    sched::ready_insert(t, cs);
}

/// 被唤醒线程是否比当前线程更紧急 (需要重新调度)
pub(crate) fn resched_needed(w: *mut Thread) -> bool {
    let cur = sched::current();
    !cur.is_null() && unsafe { (*w).current_priority < (*cur).current_priority }
}

/// 阻塞等待后返回: 是否已超时
///
/// 挂起后的统一路径: 触发重新调度; 恢复后线程错误码非零即超时唤醒
/// (由 [`thread_timer_cb`] 写入 `-1`)。
pub(crate) fn blocked_wait() -> bool {
    sched::schedule();
    (unsafe { (*sched::current()).error }) != 0
}

/// 释放线程栈与内核侧强引用 (由空闲线程的僵尸回收调用)
///
/// 线程已退出/被删除且已切换离开, 栈不再被使用, 立即释放;
/// TCB 由内核侧强引用维持 (本函数取出并释放), 若用户仍持有
/// [`Arc<Thread>`] 句柄, TCB 继续存活 —— 句柄不会悬垂。
pub(crate) unsafe fn free_thread(t: *mut Thread) {
    (*t).list_node.remove();
    let layout = Layout::from_size_align((*t).stack_size, 8).expect("内存布局无效");
    unsafe { dealloc((*t).stack_addr as *mut u8, layout) };
    // 释放内核侧强引用; 若用户句柄仍存活, 此处仅递减引用计数,
    // TCB 在最后一个句柄释放时才被回收
    unsafe { (*t).kernel_self.take() };
}
