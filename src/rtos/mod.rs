//! RTOS 内核 — RT-Thread 的 Rust 移植 (单核 UP, Cortex-M4F)
//!
//! 架构移植自 `CRust/src/libs/rtos/` 的 RT-Thread v5.2.2 内核:
//!
//! | RT-Thread 源文件    | 本模块                | 内容 |
//! |---------------------|-----------------------|------|
//! | `scheduler_up.c`    | [`sched`]            | 位图就绪表 + 时间片轮转 |
//! | `thread.c`          | [`thread`]           | 线程创建/退出/延时 |
//! | `idle.c`/`defunct.c`| [`idle`]             | 空闲线程 + 僵尸回收 |
//! | `timer.c`           | [`timer`]            | 有序链表定时器 |
//! | `ipc.c`             | [`ipc`]              | 信号量/互斥量/事件/邮箱/消息队列 |
//! | `libcpu/arm/cortex-m4/context_gcc.S` | [`context`] | PendSV 上下文切换 |
//!
//! # 使用流程
//!
//! 1. 配置 SysTick (频率 = [`TICKS_PER_SEC`] Hz);
//! 2. SysTick ISR 中调用 [`tick_increase`];
//! 3. 调用 [`init`] 初始化内核 (创建空闲线程);
//! 4. 调用 [`thread_create`] 创建用户线程;
//! 5. 调用 [`start`] 启动调度器 (永不返回)。
//!
//! # 与 RT-Thread 的差异
//!
//! - 接口名不同: `rt_thread_create` → [`thread_create`],
//!   `rt_sem_take` → [`Semaphore::take`] 等;
//! - 临界区 = 关中断 (PRIMASK), 调度锁由临界区覆盖;
//! - 时间片为 0 表示不参与轮转 (RT-Thread 的 `tick` 字段);
//! - 定时器仅硬定时器 (回调在中断上下文)。
#![allow(dead_code)]

pub(crate) mod context;
pub(crate) mod idle;
pub(crate) mod ipc;
pub(crate) mod klist;
pub(crate) mod sched;
pub(crate) mod thread;
pub(crate) mod timer;

use core::sync::atomic::{AtomicU32, Ordering};

/// 时钟节拍频率 (Hz), 须与 SysTick 配置一致
pub const TICKS_PER_SEC: u32 = 1000;
/// 优先级数量 (0 = 最高, 31 = 最低)
pub const PRIORITY_MAX: u8 = 32;
/// 空闲线程优先级 (最低)
pub const IDLE_PRIORITY: u8 = 31;

/// 全局节拍计数 (由 [`tick_increase`] 在时钟中断中累加)
static TICK: AtomicU32 = AtomicU32::new(0);

/// 当前节拍 (毫秒, 与 SysTick 频率一致)
pub fn tick() -> u32 {
    TICK.load(Ordering::Relaxed)
}

/// 调度器是否已启动 (`start` 之后)
///
/// 启动前系统为单执行流 (main), 无并发; 中断上下文返回当前被打断
/// 的线程 (非空), 因此该判定只对"启动前"为真。
pub(crate) fn scheduler_started() -> bool {
    !sched::current().is_null()
}

/// 运行时间 (ms)
pub fn uptime_ms() -> u32 {
    tick()
}

/// 初始化内核: 设置 PendSV/SysTick 中断优先级, 创建空闲线程。
///
/// 须在创建用户线程之前调用。
pub fn init() {
    context::scb_priority_init();
    idle::create_idle();
}

/// 启动调度器: 切换到最高优先级线程, **永不返回**。
pub fn start() -> ! {
    let first = unsafe { sched::highest_ready_thread() }.expect("rtos::start: 没有可运行的线程");
    unsafe {
        sched::set_current(first);
        context::switch_to_first(&mut (*first).sp as *mut usize as usize);
    }
    // PendSV 即将执行首个切换; 此处永不返回
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

/// 时钟节拍中断服务 — 由 SysTick ISR 调用
///
/// 依次: 节拍递增 → 时间片轮转 → 定时器检查 (超时回调唤醒线程)。
pub fn tick_increase() {
    TICK.fetch_add(1, Ordering::Relaxed);
    if unsafe { sched::tick_increase() } {
        sched::schedule();
    }
    timer::check();
}

// 内核 API 全集, 部分供应用选用 (二进制 crate 中未使用项会告警)
#[allow(unused_imports)]
pub use ipc::{Error, Event, EventOpt, Mailbox, MessageQueue, Mutex, Semaphore, Timeout};
#[allow(unused_imports)]
pub use thread::{
    Thread, ThreadInfo, thread_create, thread_delay, thread_delay_ms, thread_info_list,
    thread_state_name, yield_now,
};
#[allow(unused_imports)]
pub use timer::Timer;
