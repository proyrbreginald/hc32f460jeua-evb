//! 长期稳定性测试 (soak): 在苛刻条件下长时间持续验证系统稳定性,
//! 用于说服使用者"这套系统可以放心长期运行"。
//!
//! **同步执行** (由 shell 的 `soak` 命令调用, 完成后才出下一提示符);
//! 期间按 ESC 可中断, 已完成的压力线程结果仍会汇总输出。
//!
//! # 设计思路
//!
//! 同时启动一组**压力线程** (不同优先级), 持续轰炸各子系统:
//!
//! | 线程 | 压力对象 |
//! |------|----------|
//! | `cpu-a/b` | 调度器: 多优先级抢占 + 时间片轮转 + 上下文切换 |
//! | `sem-a/b` | 信号量: 阻塞获取/释放 + 超时 |
//! | `mtx-a/b` | 互斥量: 双线程竞争, 数据一致性/丢失更新检测 |
//! | `evt-p/c` | 事件: 置位/等待/清除 + 超时 |
//! | `mb-p/c` | 邮箱: 满时阻塞发送/空时阻塞接收, 序号连续性 |
//! | `mq-p/c` | 消息队列: 二进制负载 + 序号连续性 |
//! | `heap` | 堆: 随机大小分配/释放 + 模式回读 (数据完整性) |
//! | `thread` | 线程: 反复创建/自然退出, defunct 回收无泄漏 |
//! | `timer` | 内核定时器: 周期回调计数 vs 墙钟漂移 |
//! | `delay` | 调度延迟: 延时精度与最坏调度延迟 |
//! | `flash` | EFM: 擦除/编程/回读 (节流保护寿命) |
//! | `crc` | CRC 加速器: 硬件结果 vs 软件参考实现 |
//! | `can` | CAN: 内部回环持续收发校验 (可用时) |
//!
//! shell 线程兼任**监控器**: 每秒检查各压力线程心跳 (停滞即判挂起)、
//! 堆用量趋势、线程数、SRAM 奇偶/ECC 错误与栈水位, 并定期输出进度;
//! 任一线程报错即提前结束并给出 FAIL 汇总。
//!
//! # 结果判定
//!
//! - 无错误 + 堆/线程无泄漏 + SRAM 无错误 → **PASS** (放心使用);
//! - 任一压力线程错误 / 心跳停滞 / 泄漏 / SRAM 错误 → **FAIL**;
//! - 建议与 `CFG_WDT_ENABLE=true` 配合: 调度器彻底停滞会被硬件看门狗
//!   复位, 复位即失败证据。
//!
//! 日志分级: 进度与结果走 info 级 (可经 `log` 命令控制), 汇总始终打印。
//! 命令: `soak [分钟]` — 无参数用 `CFG_SOAK_MINUTES`, `0` = 直到 ESC。

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::rtos::{Event, EventOpt, Mailbox, MessageQueue, Mutex, Semaphore, Timeout};

use alloc::vec;
use alloc::vec::Vec;

// ============================== 共享状态 ==============================

/// 全局停止标志: 监控器设置, 压力线程轮询退出
static STOP: AtomicBool = AtomicBool::new(false);

/// 互斥量压力对象: 双线程竞争, 各加自己的增量 (A=10, B=20),
/// 值始终为 10 的倍数 —— 任何丢失更新/撕裂写都会被整除性检查捕获
static SOAK_MTX: Mutex<u64> = Mutex::new(0);
static MTX_LOCKS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// 其他 IPC 压力对象 (静态, 跨线程共享)
static SOAK_SEM: Semaphore = Semaphore::new(1, 1);
static SOAK_EVT: Event = Event::new();
static SOAK_MB: Mailbox<u32> = Mailbox::new(4);
static SOAK_MQ: MessageQueue = MessageQueue::new(8, 16);

/// 内核定时器压力: 100ms 周期回调计数, 与墙钟对比漂移
static SOAK_TIMER: crate::rtos::Timer = crate::rtos::Timer::new();
static TIMER_TICKS: AtomicU32 = AtomicU32::new(0);

/// CPU 压力线程的运算汇 (volatile 效果: 防止编译器消除计算循环)
static CPU_SINK: AtomicU32 = AtomicU32::new(0);

/// 最长调度延迟观测值 (delay 线程, 供汇总报告)
static MAX_DELAY_MS: AtomicU32 = AtomicU32::new(0);

/// 本次 soak 开始时刻 (uptime_ms, 供 fail/报告换算运行时长)
static SOAK_START_MS: AtomicU32 = AtomicU32::new(0);

/// 各压力线程的栈使用峰值 (字节, 监控器从 thread_info_list 采样)
static STACK_PEAK: [AtomicU32; 19] = [
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
];

/// 压力线程状态
struct Worker {
    name: &'static str,
    cycles: AtomicU32,
    errors: AtomicU32,
    last_error: AtomicU32,
    /// 首次失败时的循环数 (供汇总报告定位)
    fail_cycle: AtomicU32,
    /// 首次失败时的 uptime (供汇总报告定位)
    fail_time: AtomicU32,
    last_beat: AtomicU32,
    active: AtomicBool,
}

impl Worker {
    const fn new(name: &'static str) -> Self {
        Self {
            name,
            cycles: AtomicU32::new(0),
            errors: AtomicU32::new(0),
            last_error: AtomicU32::new(0),
            fail_cycle: AtomicU32::new(0),
            fail_time: AtomicU32::new(0),
            last_beat: AtomicU32::new(0),
            active: AtomicBool::new(false),
        }
    }

    fn reset(&self) {
        self.cycles.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
        self.last_error.store(0, Ordering::Relaxed);
        self.fail_cycle.store(0, Ordering::Relaxed);
        self.fail_time.store(0, Ordering::Relaxed);
        // 初始化为当前运行时刻: 监控器按"距上次心跳的间隔"判定停滞,
        // 新线程尚未跑第一轮前不应被误判为挂起
        self.last_beat.store(crate::rtos::uptime_ms(), Ordering::Relaxed);
        self.active.store(false, Ordering::Relaxed);
    }

    /// 心跳: 每轮循环调用, 供监控器判定"线程是否活着"
    fn beat(&self) {
        self.cycles.fetch_add(1, Ordering::Relaxed);
        self.last_beat.store(crate::rtos::uptime_ms(), Ordering::Relaxed);
    }

    /// 记录错误并**立即输出失败日志** (console 整行原子输出, 不与其他
    /// 线程输出交错)。
    ///
    /// 停止阶段 (STOP 已置位) 产生的阻塞超时属于正常退出路径, 不计入
    /// 失败; 监控器的停滞判定在 STOP 置位前执行, 不受此门控影响。
    fn fail(&self, code: u32) {
        if !STOP.load(Ordering::Relaxed) {
            self.errors.fetch_add(1, Ordering::Relaxed);
            self.last_error.store(code, Ordering::Relaxed);
            self.fail_cycle
                .store(self.cycles.load(Ordering::Relaxed), Ordering::Relaxed);
            self.fail_time.store(crate::rtos::uptime_ms(), Ordering::Relaxed);
            let elapsed = crate::rtos::uptime_ms().wrapping_sub(SOAK_START_MS.load(Ordering::Relaxed));
            crate::log_error!(
                "[soak] {} 失败: {} (第 {} 循环, 运行 {})",
                self.name,
                error_text(code),
                self.fail_cycle.load(Ordering::Relaxed),
                fmt_elapsed(elapsed)
            );
        }
    }
}

// ============================== 压力线程定义 ==============================

const W_CPU_A: usize = 0;
const W_CPU_B: usize = 1;
const W_SEM_A: usize = 2;
const W_SEM_B: usize = 3;
const W_MTX_A: usize = 4;
const W_MTX_B: usize = 5;
const W_EVT_P: usize = 6;
const W_EVT_C: usize = 7;
const W_MB_P: usize = 8;
const W_MB_C: usize = 9;
const W_MQ_P: usize = 10;
const W_MQ_C: usize = 11;
const W_HEAP: usize = 12;
const W_THREAD: usize = 13;
const W_TIMER: usize = 14;
const W_DELAY: usize = 15;
const W_FLASH: usize = 16;
const W_CRC: usize = 17;
const W_CAN: usize = 18;

static W_CPU_A_W: Worker = Worker::new("soak-cpu-a");
static W_CPU_B_W: Worker = Worker::new("soak-cpu-b");
static W_SEM_A_W: Worker = Worker::new("soak-sem-a");
static W_SEM_B_W: Worker = Worker::new("soak-sem-b");
static W_MTX_A_W: Worker = Worker::new("soak-mtx-a");
static W_MTX_B_W: Worker = Worker::new("soak-mtx-b");
static W_EVT_P_W: Worker = Worker::new("soak-evt-p");
static W_EVT_C_W: Worker = Worker::new("soak-evt-c");
static W_MB_P_W: Worker = Worker::new("soak-mb-p");
static W_MB_C_W: Worker = Worker::new("soak-mb-c");
static W_MQ_P_W: Worker = Worker::new("soak-mq-p");
static W_MQ_C_W: Worker = Worker::new("soak-mq-c");
static W_HEAP_W: Worker = Worker::new("soak-heap");
static W_THREAD_W: Worker = Worker::new("soak-thread");
static W_TIMER_W: Worker = Worker::new("soak-timer");
static W_DELAY_W: Worker = Worker::new("soak-delay");
static W_FLASH_W: Worker = Worker::new("soak-flash");
static W_CRC_W: Worker = Worker::new("soak-crc");
static W_CAN_W: Worker = Worker::new("soak-can");

/// 全部压力线程 (供监控器遍历)
const ALL_WORKERS: [&Worker; 19] = [
    &W_CPU_A_W, &W_CPU_B_W, &W_SEM_A_W, &W_SEM_B_W, &W_MTX_A_W, &W_MTX_B_W, &W_EVT_P_W,
    &W_EVT_C_W, &W_MB_P_W, &W_MB_C_W, &W_MQ_P_W, &W_MQ_C_W, &W_HEAP_W, &W_THREAD_W, &W_TIMER_W,
    &W_DELAY_W, &W_FLASH_W, &W_CRC_W, &W_CAN_W,
];

/// 压力线程创建参数 (名称/优先级/栈/入口)
struct SpawnSpec {
    index: usize,
    stack: usize,
    priority: u8,
    entry: extern "C" fn(usize),
}

const SPAWN_SPECS: [SpawnSpec; 19] = [
    SpawnSpec { index: W_CPU_A, stack: 1024, priority: 3, entry: cpu_worker },
    SpawnSpec { index: W_CPU_B, stack: 1024, priority: 4, entry: cpu_worker },
    SpawnSpec { index: W_SEM_A, stack: 1024, priority: 5, entry: sem_worker },
    SpawnSpec { index: W_SEM_B, stack: 1024, priority: 6, entry: sem_worker },
    SpawnSpec { index: W_MTX_A, stack: 1024, priority: 7, entry: mtx_worker },
    SpawnSpec { index: W_MTX_B, stack: 1024, priority: 8, entry: mtx_worker },
    SpawnSpec { index: W_EVT_P, stack: 1024, priority: 9, entry: evt_producer },
    SpawnSpec { index: W_EVT_C, stack: 1024, priority: 10, entry: evt_consumer },
    SpawnSpec { index: W_MB_P, stack: 1024, priority: 11, entry: mb_producer },
    SpawnSpec { index: W_MB_C, stack: 1024, priority: 12, entry: mb_consumer },
    SpawnSpec { index: W_MQ_P, stack: 1024, priority: 13, entry: mq_producer },
    SpawnSpec { index: W_MQ_C, stack: 1024, priority: 14, entry: mq_consumer },
    SpawnSpec { index: W_HEAP, stack: 2048, priority: 8, entry: heap_worker },
    SpawnSpec { index: W_THREAD, stack: 1024, priority: 10, entry: thread_worker },
    SpawnSpec { index: W_TIMER, stack: 1024, priority: 12, entry: timer_worker },
    SpawnSpec { index: W_DELAY, stack: 1024, priority: 15, entry: delay_worker },
    SpawnSpec { index: W_FLASH, stack: 2048, priority: 6, entry: flash_worker },
    SpawnSpec { index: W_CRC, stack: 1024, priority: 14, entry: crc_worker },
    SpawnSpec { index: W_CAN, stack: 2048, priority: 16, entry: can_worker },
];

// ============================== 错误码 → 文本 ==============================

const ERR_TIMEOUT: u32 = 1; // IPC 超时 (应被同伴唤醒却超时)
const ERR_DATA: u32 = 2; // 数据不匹配/损坏
const ERR_SEQ: u32 = 3; // 序号乱序/丢失
const ERR_HANG: u32 = 4; // 心跳停滞 (线程挂起)
const ERR_SRAM: u32 = 5; // SRAM 奇偶/ECC 错误
const ERR_LEAK: u32 = 6; // 堆/线程泄漏
const ERR_FLASH: u32 = 7; // Flash 校验失败
const ERR_CRC: u32 = 8; // CRC 校验失败
const ERR_TIMER: u32 = 9; // 定时器漂移
const ERR_DELAY: u32 = 10; // 调度异常 (延时不可靠)
const ERR_CAN: u32 = 11; // CAN 收发失败
const ERR_MTX: u32 = 12; // 互斥量数据损坏

fn error_text(code: u32) -> &'static str {
    match code {
        ERR_TIMEOUT => "IPC 超时",
        ERR_DATA => "数据不匹配",
        ERR_SEQ => "序号乱序/丢失",
        ERR_HANG => "心跳停滞 (挂起)",
        ERR_SRAM => "SRAM 奇偶/ECC 错误",
        ERR_LEAK => "堆/线程泄漏",
        ERR_FLASH => "Flash 校验失败",
        ERR_CRC => "CRC 校验失败",
        ERR_TIMER => "定时器漂移",
        ERR_DELAY => "调度异常",
        ERR_CAN => "CAN 收发失败",
        ERR_MTX => "互斥量数据损坏",
        _ => "未知",
    }
}

// ============================== 压力线程入口 ==============================

/// CPU/调度压力: 纯计算 + 周期延时, 多优先级抢占与时间片轮转
extern "C" fn cpu_worker(param: usize) {
    let w = ALL_WORKERS[param];
    // param 在运行时传入, 使计算循环依赖运行时值 (防止编译期折叠)
    let mut state = param as u32 ^ 0x9E37_79B9;
    while !STOP.load(Ordering::Relaxed) {
        for _ in 0..4096 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        }
        // 写入原子: 计算结果可观测, 循环不会被优化掉
        CPU_SINK.store(state, Ordering::Relaxed);
        crate::rtos::thread_delay_ms((state & 0x07) + 1).ok();
        w.beat();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// 信号量压力: 阻塞获取 + 释放, 同伴互相唤醒
extern "C" fn sem_worker(param: usize) {
    let w = ALL_WORKERS[param];
    while !STOP.load(Ordering::Relaxed) {
        if SOAK_SEM.take(Timeout::Ticks(1000)).is_err() {
            w.fail(ERR_TIMEOUT);
            break;
        }
        SOAK_SEM.release();
        w.beat();
        crate::rtos::thread_delay_ms(1).ok();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// 互斥量压力: 双线程竞争, 各加自己的增量 (A=10, B=20),
/// 校验值始终为 10 的倍数 + 单调不减, 最终值 == Σ(增量×次数)
extern "C" fn mtx_worker(param: usize) {
    let w = ALL_WORKERS[param];
    let slot = if param == W_MTX_A { 0 } else { 1 };
    let add = if slot == 0 { 10u64 } else { 20u64 };
    let mut last = 0u64;
    while !STOP.load(Ordering::Relaxed) {
        let Ok(mut guard) = SOAK_MTX.lock(Timeout::Ticks(1000)) else {
            w.fail(ERR_TIMEOUT);
            break;
        };
        let v = *guard;
        if v % 10 != 0 || v < last {
            // 显式释放守卫: 线程退出不执行析构, 否则互斥量会永久锁死
            drop(guard);
            w.fail(ERR_MTX);
            break;
        }
        last = v;
        *guard = v + add;
        drop(guard);
        MTX_LOCKS[slot].fetch_add(1, Ordering::Relaxed);
        w.beat();
        crate::rtos::thread_delay_ms(1).ok();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// 事件压力: 生产者置位
extern "C" fn evt_producer(param: usize) {
    let w = ALL_WORKERS[param];
    let mut bit = 0x05u32;
    while !STOP.load(Ordering::Relaxed) {
        SOAK_EVT.send(bit);
        bit = if bit == 0x05 { 0x0A } else { 0x05 };
        w.beat();
        crate::rtos::thread_delay_ms(1).ok();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// 事件压力: 消费者等待并清除 (返回值必须只含已知位)
extern "C" fn evt_consumer(param: usize) {
    let w = ALL_WORKERS[param];
    while !STOP.load(Ordering::Relaxed) {
        match SOAK_EVT.recv(0x0F, EventOpt::Or, Timeout::Ticks(1000)) {
            Ok(bits) => {
                if bits == 0 || bits & !0x0F != 0 {
                    w.fail(ERR_DATA);
                    break;
                }
                let _ = SOAK_EVT.recv(bits, EventOpt::OrClear, Timeout::Ticks(0));
            }
            Err(_) => {
                w.fail(ERR_TIMEOUT);
                break;
            }
        }
        w.beat();
        crate::rtos::thread_delay_ms(1).ok();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// 邮箱压力: 生产者发送递增序号 (容量 4, 满时阻塞)
///
/// 每轮至少让出 1ms: 防止生产/消费双方形成"满↔空"的忙碌乒乓,
/// 饿死低优先级压力线程 (首次真机运行即暴露该问题)。
extern "C" fn mb_producer(param: usize) {
    let w = ALL_WORKERS[param];
    let mut seq = 0u32;
    while !STOP.load(Ordering::Relaxed) {
        if SOAK_MB.send(seq, Timeout::Ticks(1000)).is_err() {
            w.fail(ERR_TIMEOUT);
            break;
        }
        seq = seq.wrapping_add(1);
        w.beat();
        crate::rtos::thread_delay_ms(1).ok();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// 邮箱压力: 消费者校验序号严格递增 (无丢失/乱序)
extern "C" fn mb_consumer(param: usize) {
    let w = ALL_WORKERS[param];
    let mut expect = 0u32;
    while !STOP.load(Ordering::Relaxed) {
        match SOAK_MB.recv(Timeout::Ticks(1000)) {
            Ok(v) => {
                if v != expect {
                    w.fail(ERR_SEQ);
                    break;
                }
                expect = expect.wrapping_add(1);
            }
            Err(_) => {
                w.fail(ERR_TIMEOUT);
                break;
            }
        }
        w.beat();
        crate::rtos::thread_delay_ms(1).ok();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// 消息队列压力: 生产者发送带魔数/序号的 8 字节负载
extern "C" fn mq_producer(param: usize) {
    let w = ALL_WORKERS[param];
    let mut seq = 0u32;
    let mut msg = [0u8; 8];
    while !STOP.load(Ordering::Relaxed) {
        msg[0] = 0x52;
        msg[1] = (seq & 0xFF) as u8;
        msg[2] = ((seq >> 8) & 0xFF) as u8;
        msg[3] = ((seq >> 16) & 0xFF) as u8;
        msg[4] = ((seq >> 24) & 0xFF) as u8;
        msg[5] = 0xA5;
        msg[6] = 0x5A;
        msg[7] = (msg[0..7].iter().fold(0u8, |a, b| a.wrapping_add(*b))).wrapping_mul(31);
        if SOAK_MQ.send(&msg, Timeout::Ticks(1000)).is_err() {
            w.fail(ERR_TIMEOUT);
            break;
        }
        seq = seq.wrapping_add(1);
        w.beat();
        crate::rtos::thread_delay_ms(1).ok();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// 消息队列压力: 消费者校验负载与序号
extern "C" fn mq_consumer(param: usize) {
    let w = ALL_WORKERS[param];
    let mut expect = 0u32;
    let mut msg = [0u8; 8];
    while !STOP.load(Ordering::Relaxed) {
        match SOAK_MQ.recv(&mut msg, Timeout::Ticks(1000)) {
            Ok(8) => {
                let seq = u32::from_le_bytes([msg[1], msg[2], msg[3], msg[4]]);
                let chk =
                    (msg[0..7].iter().fold(0u8, |a, b| a.wrapping_add(*b))).wrapping_mul(31);
                if msg[0] != 0x52 || msg[5] != 0xA5 || msg[6] != 0x5A || msg[7] != chk {
                    w.fail(ERR_DATA);
                    break;
                }
                if seq != expect {
                    w.fail(ERR_SEQ);
                    break;
                }
                expect = expect.wrapping_add(1);
            }
            _ => {
                w.fail(ERR_DATA);
                break;
            }
        }
        w.beat();
        crate::rtos::thread_delay_ms(1).ok();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// 堆压力: 随机大小分配/释放 + 模式回读, 周期性全量校验
extern "C" fn heap_worker(param: usize) {
    let w = ALL_WORKERS[param];
    let mut rng = param as u32 ^ 0x00C0_FFEE;
    let mut live: Vec<(u32, Vec<u8>)> = Vec::new(); // (种子, 数据)
    let mut verified_all = 0u32;
    while !STOP.load(Ordering::Relaxed) {
        // 分配 1~3 块随机大小
        for _ in 0..(rng % 3 + 1) {
            rng = xorshift(rng);
            let size = 16 + (rng % 1024) as usize;
            let seed = rng;
            let mut buf = vec![0u8; size];
            let mut s = seed;
            for b in &mut buf {
                s = xorshift(s);
                *b = s as u8;
            }
            live.push((seed, buf));
        }
        // 校验全部存活块 (周期: 每 32 轮)
        verified_all = verified_all.wrapping_add(1);
        if verified_all.is_multiple_of(32) {
            let mut ok = true;
            for (seed, buf) in &live {
                let mut s = *seed;
                for &b in buf.iter() {
                    s = xorshift(s);
                    if b != s as u8 {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    break;
                }
            }
            if !ok {
                w.fail(ERR_DATA);
                break;
            }
        }
        // 随机释放若干块
        let drops = rng as usize % 3;
        for _ in 0..drops {
            live.pop();
        }
        // 控制内存峰值: 超过 48 块全部释放一次
        if live.len() > 48 {
            live.clear();
        }
        w.beat();
        crate::rtos::thread_delay_ms(1).ok();
    }
    live.clear();
    w.active.store(false, Ordering::Relaxed);
}

/// 线程压力: 反复创建/自然退出, 校验 defunct 回收 (无泄漏)
extern "C" fn tmp_exit_thread(_param: usize) {}

extern "C" fn thread_worker(param: usize) {
    let w = ALL_WORKERS[param];
    while !STOP.load(Ordering::Relaxed) {
        crate::rtos::thread_create("soak-tmp", 512, 25, 0, tmp_exit_thread, 0);
        // 等待 tmp 线程退出并被空闲线程回收 (idle 需等到调度窗口,
        // 轮询最长 500ms; 超时即判回收泄漏)
        let start = crate::rtos::uptime_ms();
        let mut stale = true;
        while crate::rtos::uptime_ms().wrapping_sub(start) < 500 {
            let gone = !crate::rtos::thread_info_list()
                .iter()
                .any(|t| t.name == "soak-tmp");
            if gone {
                stale = false;
                break;
            }
            crate::rtos::thread_delay_ms(10).ok();
        }
        if stale {
            w.fail(ERR_LEAK);
            break;
        }
        w.beat();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// 内核定时器压力: 100ms 周期回调 vs 墙钟漂移
extern "C" fn soak_timer_cb(_param: usize) {
    TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
}

extern "C" fn timer_worker(param: usize) {
    let w = ALL_WORKERS[param];
    let pin = SOAK_TIMER.pin_static();
    pin.start_ms(100, 100, soak_timer_cb, 0);
    let mut last_ticks = TIMER_TICKS.load(Ordering::Relaxed);
    let mut last_time = crate::rtos::uptime_ms();
    while !STOP.load(Ordering::Relaxed) {
        // 分段睡眠并保持心跳, 每 5s 核对一次
        for _ in 0..10 {
            crate::rtos::thread_delay_ms(500).ok();
            w.beat();
        }
        let now = crate::rtos::uptime_ms();
        let ticks = TIMER_TICKS.load(Ordering::Relaxed);
        let elapsed = now.wrapping_sub(last_time);
        let expected = elapsed / 100;
        let actual = ticks.wrapping_sub(last_ticks);
        // 允许 ±2 个回调的抖动
        if expected >= 40 && (actual as i64 - expected as i64).abs() > 2 {
            w.fail(ERR_TIMER);
            break;
        }
        last_ticks = ticks;
        last_time = now;
    }
    pin.stop();
    w.active.store(false, Ordering::Relaxed);
}

/// 调度延迟压力: 延时精度 + 最坏调度延迟观测
extern "C" fn delay_worker(param: usize) {
    let w = ALL_WORKERS[param];
    while !STOP.load(Ordering::Relaxed) {
        let t0 = crate::rtos::uptime_ms();
        crate::rtos::thread_delay_ms(100).ok();
        let actual = crate::rtos::uptime_ms().wrapping_sub(t0);
        // 延时只能晚不能早; 超过 2s 判调度异常
        if !(100..=2100).contains(&actual) {
            w.fail(ERR_DELAY);
            break;
        }
        let over = actual - 100;
        let mut max = MAX_DELAY_MS.load(Ordering::Relaxed);
        while over > max {
            match MAX_DELAY_MS.compare_exchange_weak(max, over, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(observed) => max = observed,
            }
        }
        w.beat();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// Flash 压力: 擦除/编程/回读校验 (节流保护寿命)
extern "C" fn flash_worker(param: usize) {
    let w = ALL_WORKERS[param];
    const FLASH_ADDR: u32 = 0x0007_C000; // 扇区 62, 与 selftest 相同
    let mut seq = 0u32;
    let mut last_cycle = 0u32;
    let mut data = [0u8; 64];
    while !STOP.load(Ordering::Relaxed) {
        let now = crate::rtos::uptime_ms();
        let interval = crate::config::SOAK_FLASH_INTERVAL_MS;
        if now.wrapping_sub(last_cycle) < interval {
            crate::rtos::thread_delay_ms(100).ok();
            w.beat();
            continue;
        }
        last_cycle = now;
        seq = seq.wrapping_add(1);
        for (i, b) in data.iter_mut().enumerate() {
            *b = (seq as u8).wrapping_mul(31).wrapping_add(i as u8) ^ 0xA5;
        }
        let mut ok = crate::efm::sector_erase(FLASH_ADDR).is_ok()
            && crate::efm::program(FLASH_ADDR, &data).is_ok();
        if ok {
            for (i, &b) in data.iter().enumerate() {
                if crate::efm::read_byte(FLASH_ADDR + i as u32) != Ok(b) {
                    ok = false;
                    break;
                }
            }
        }
        // 还原为擦除态, 保持扇区干净
        if crate::efm::sector_erase(FLASH_ADDR).is_err() {
            ok = false;
        }
        if !ok {
            w.fail(ERR_FLASH);
            break;
        }
        w.beat();
    }
    w.active.store(false, Ordering::Relaxed);
}

/// 软件 CRC32 (IEEE 802.3, 反射/异或输出) — 硬件加速器的参考实现
fn soft_crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xFFFF_FFFF
}

/// CRC 压力: 随机数据硬件计算 vs 软件参考
extern "C" fn crc_worker(param: usize) {
    let w = ALL_WORKERS[param];
    let mut rng = param as u32 ^ 0x5EED_1234;
    let mut buf = [0u8; 128];
    while !STOP.load(Ordering::Relaxed) {
        rng = xorshift(rng);
        let len = 1 + (rng % 128) as usize;
        let mut s = rng;
        for b in &mut buf[..len] {
            s = xorshift(s);
            *b = s as u8;
        }
        let hw = crate::crc::calculate(&buf[..len], crate::crc::DataWidth::Byte, crate::crc::Config::crc32());
        let sw = soft_crc32(&buf[..len]);
        if hw != sw {
            w.fail(ERR_CRC);
            break;
        }
        // 周期性核对标准向量
        if w.cycles.load(Ordering::Relaxed).is_multiple_of(64) {
            let v: &[u8] = b"123456789";
            let ok = crate::crc::calculate(v, crate::crc::DataWidth::Byte, crate::crc::Config::crc32())
                == 0xCBF4_3926;
            if !ok {
                w.fail(ERR_CRC);
                break;
            }
        }
        w.beat();
        crate::rtos::thread_delay_ms(1).ok();
    }
    w.active.store(false, Ordering::Relaxed);
}

// ---- CAN 压力 (内部回环) ----

static SOAK_CAN_FILTERS: [crate::can::Filter; 1] = [crate::can::Filter {
    id: 0,
    mask: 0x1FFF_FFFF,
    kind: crate::can::FilterType::StandardAndExtended,
}];

/// 等待发送完成 (轮询, 带超时与错误检查)
///
/// 压力线程优先级低于其他线程, 轮询窗口可能被拉长, 超时取
/// CAN_TIMEOUT_MS 与 500ms 的较大值, 避免负载高峰误报。
fn wait_can_tx(can: &crate::can::Can, buffer: crate::can::TxBuffer) -> bool {
    let complete = match buffer {
        crate::can::TxBuffer::Primary => crate::can::Status::PTB_TX,
        crate::can::TxBuffer::Secondary => crate::can::Status::STB_TX,
    };
    let timeout = crate::config::CAN_TIMEOUT_MS.max(500);
    let start = crate::rtos::uptime_ms();
    loop {
        let status = can.status();
        if status.intersects(crate::can::Status::TX_ERRORS) {
            can.abort(buffer);
            return false;
        }
        if status.contains(complete) {
            can.clear_status(complete);
            return true;
        }
        if crate::rtos::uptime_ms().wrapping_sub(start) >= timeout {
            can.abort(buffer);
            return false;
        }
        crate::rtos::thread_delay_ms(1).ok();
    }
}

/// CAN 压力: 内部回环持续收发, 校验帧内容与错误计数
extern "C" fn can_worker(param: usize) {
    let w = ALL_WORKERS[param];
    let can = crate::board::BoardResources::get().can();
    let xtal_was_enabled = crate::clk::xtal_enabled();
    let mut ok = true;

    if can.init(crate::can::Config {
        mode: crate::can::WorkMode::InternalLoopback,
        filters: &SOAK_CAN_FILTERS,
        self_ack: false,
        ptb_single_shot: false,
        stb_single_shot: false,
        stb_priority: crate::can::StbPriority::Fifo,
        rx_warn_limit: 10,
        rx_all_frames: false,
        rx_overflow: crate::can::RxOverflowMode::DiscardNewest,
        interrupts: crate::can::Interrupts::ALL,
        ..crate::config::CAN_CONFIG
    })
    .is_err()
    {
        ok = false;
    } else {
        while can.try_receive().is_some() {}
        can.clear_status(can.status());
        let base_info = can.error_info();
        let mut seq = 0u32;
        'outer: while !STOP.load(Ordering::Relaxed) {
            let payload = [
                seq as u8,
                (seq >> 8) as u8,
                (seq >> 16) as u8,
                (seq >> 24) as u8,
                0xA5,
                0x5A,
                0x00,
                0x52,
            ];
            let frame =
                crate::can::TxFrame::data(crate::can::Id::Standard(0x100), 8, payload);
            let (buf, is_stb) = if seq & 1 == 0 {
                (crate::can::TxBuffer::Primary, false)
            } else {
                (crate::can::TxBuffer::Secondary, true)
            };
            let tx = if is_stb {
                can.enqueue_stb(&frame)
                    .and_then(|_| can.start_stb(crate::can::StbTransmit::All))
            } else {
                can.try_transmit_ptb(&frame)
            };
            if tx.is_err() || !wait_can_tx(can, buf) {
                ok = false;
                break 'outer;
            }
            // 接收回环帧并校验
            let start = crate::rtos::uptime_ms();
            let timeout = crate::config::CAN_TIMEOUT_MS.max(500);
            let mut received = false;
            while !received {
                if let Some(rx) = can.try_receive() {
                    if rx.id != crate::can::Id::Standard(0x100)
                        || !rx.self_tx
                        || rx.error != crate::can::ErrorKind::None
                        || rx.data[0..4] != payload[0..4]
                        || rx.data[5] != 0x5A
                    {
                        ok = false;
                    }
                    received = true;
                } else if crate::rtos::uptime_ms().wrapping_sub(start) >= timeout {
                    ok = false;
                    break;
                } else {
                    crate::rtos::thread_delay_ms(1).ok();
                }
            }
            if !ok {
                break 'outer;
            }
            seq = seq.wrapping_add(1);
            // 每 100 帧核对错误计数与状态
            if seq.is_multiple_of(100) {
                let status = can.status();
                if status.intersects(crate::can::Status::TX_ERRORS) {
                    ok = false;
                    break 'outer;
                }
                let info = can.error_info();
                if info.rx_count != base_info.rx_count || info.tx_count != base_info.tx_count {
                    ok = false;
                    break 'outer;
                }
            }
            // 每帧让出 1ms: 防止连续帧形成高速乒乓饿死更低优先级线程
            crate::rtos::thread_delay_ms(1).ok();
            w.beat();
        }
    }
    can.deinit();
    if !xtal_was_enabled {
        let _ = crate::clk::xtal_cmd(false);
    }
    if !ok {
        w.fail(ERR_CAN);
    }
    w.active.store(false, Ordering::Relaxed);
}

// ============================== 工具 ==============================

/// xorshift32 伪随机数 (压力数据生成)
fn xorshift(state: u32) -> u32 {
    let mut x = state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

/// 去掉压力线程名的 `soak-` 前缀, 用于紧凑进度行
fn short_name(full: &'static str) -> &'static str {
    full.strip_prefix("soak-").unwrap_or(full)
}

/// 运行时长格式化 "H:MM:SS"
fn fmt_elapsed(elapsed_ms: u32) -> alloc::string::String {
    let s = elapsed_ms / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    alloc::format!("{}:{:02}:{:02}", h, m, s)
}

// ============================== 监控器 ==============================

/// 可选的 CAN 压力跳过原因 (None = 可运行)
fn can_skip_reason() -> Option<&'static str> {
    if !crate::config::CAN_SELFTEST_ENABLE {
        return Some("CFG_CAN_SELFTEST_ENABLE=false");
    }
    if crate::config::CAN_ENABLE {
        return Some("应用 CAN 已启用 (会清空收发队列)");
    }
    if crate::board::BoardResources::get().can().irq_registered() {
        return Some("CAN IRQ consumer 仍已注册");
    }
    None
}

/// 运行长期稳定性测试 (由 shell `soak` 命令调用)
pub(crate) fn run(minutes_arg: u32) {
    if !crate::config::SOAK_ENABLE {
        crate::println!("[soak] 未启用 (CFG_SOAK_ENABLE=false)");
        return;
    }
    let minutes = minutes_arg.min(24 * 60 * 7); // 上限 7 天
    let duration_ms = (minutes as u64) * 60_000;

    // ---- 复位全局状态 ----
    STOP.store(false, Ordering::Relaxed);
    SOAK_START_MS.store(crate::rtos::uptime_ms(), Ordering::Relaxed);
    for w in ALL_WORKERS {
        w.reset();
    }
    for slot in MTX_LOCKS.iter() {
        slot.store(0, Ordering::Relaxed);
    }
    // 互斥量值复位 (先前运行可能残留)
    if let Ok(mut guard) = SOAK_MTX.lock(Timeout::Ticks(0)) {
        *guard = 0;
    }
    // 清空 IPC 残留消息
    while SOAK_MB.recv(Timeout::Ticks(0)).is_ok() {}
    let mut tmp = [0u8; 8];
    while SOAK_MQ.recv(&mut tmp, Timeout::Ticks(0)).is_ok() {}
    let _ = SOAK_EVT.recv(0x0F, EventOpt::OrClear, Timeout::Ticks(0));

    // ---- 基线快照 ----
    let heap_base = crate::heap::used();
    let thread_base = crate::rtos::thread_info_list().len();
    crate::sram::clear_status(crate::sram::ERR_ALL);

    // ---- 创建压力线程 ----
    let mut spawned: Vec<usize> = Vec::new();
    crate::println!(
        "[soak] 开始: 目标 {} 分钟 ({}), 按 ESC 可中断",
        minutes,
        if minutes == 0 { "直到 ESC" } else { "限时" }
    );
    for spec in SPAWN_SPECS {
        if spec.index == W_CAN
            && let Some(reason) = can_skip_reason()
        {
            crate::log_info!("[soak] CAN 压力跳过: {}", reason);
            continue;
        }
        crate::rtos::thread_create(ALL_WORKERS[spec.index].name, spec.stack, spec.priority, 10, spec.entry, spec.index);
        ALL_WORKERS[spec.index].active.store(true, Ordering::Relaxed);
        spawned.push(spec.index);
        crate::log_info!(
            "[soak] 压力线程 {} 已启动: 优先级 {}, 栈 {}B",
            ALL_WORKERS[spec.index].name,
            spec.priority,
            spec.stack
        );
    }
    crate::println!("[soak] 共 {} 个压力线程", spawned.len());

    // ---- 监控循环 ----
    let start = crate::rtos::uptime_ms();
    let mut last_report = start;
    let mut peak_heap = heap_base;
    let mut sram_errors = 0u32;
    let mut stop_reason = "完成";

    'monitor: loop {
        crate::rtos::thread_delay_ms(1000).ok();
        let now = crate::rtos::uptime_ms();

        // ESC 中断
        if crate::selftest::abort_requested() {
            stop_reason = "ESC 中断";
            break 'monitor;
        }
        // 时长到达
        if minutes != 0
            && (crate::rtos::uptime_ms() as u64).wrapping_sub(start as u64) >= duration_ms
        {
            break 'monitor;
        }
        // 心跳停滞判定 (停滞即判挂起: 丢失唤醒/死锁/调度停滞)
        for &i in &spawned {
            let w = ALL_WORKERS[i];
            let idle = now.wrapping_sub(w.last_beat.load(Ordering::Relaxed));
            if w.active.load(Ordering::Relaxed)
                && idle > crate::config::SOAK_HANG_GRACE_MS
            {
                crate::log_warn!(
                    "[soak] {} 心跳停滞 {}ms (超过宽限 {}ms), 判挂起",
                    w.name,
                    idle,
                    crate::config::SOAK_HANG_GRACE_MS
                );
                w.fail(ERR_HANG);
            }
        }
        // 任一压力线程报错 → 提前结束 (失败详情已由 fail() 即时输出)
        for &i in &spawned {
            if ALL_WORKERS[i].errors.load(Ordering::Relaxed) > 0 {
                stop_reason = "检测到压力线程错误";
                break 'monitor;
            }
        }
        // SRAM 奇偶/ECC 错误
        if let Some(e) = crate::sram::error() {
            sram_errors += 1;
            crate::sram::clear_status(crate::sram::ERR_ALL);
            crate::log_error!("[soak] SRAM 错误: {:?}", e);
        }
        // 堆用量峰值
        let used = crate::heap::used();
        if used > peak_heap {
            peak_heap = used;
        }
        // 栈水位采样: 记录各压力线程峰值 (供汇总报告证明无溢出风险)
        for t in crate::rtos::thread_info_list() {
            if let Some(pos) = ALL_WORKERS.iter().position(|w| w.name == t.name) {
                let peak = STACK_PEAK[pos].load(Ordering::Relaxed);
                if t.stack_used as u32 > peak {
                    STACK_PEAK[pos].store(t.stack_used as u32, Ordering::Relaxed);
                }
            }
        }
        // 周期进度报告
        if now.wrapping_sub(last_report) >= crate::config::SOAK_REPORT_INTERVAL_MS {
            let elapsed = now.wrapping_sub(start);
            let mut detail = alloc::string::String::new();
            for &i in &spawned {
                let w = ALL_WORKERS[i];
                write!(
                    &mut detail,
                    " {}={}",
                    short_name(w.name),
                    w.cycles.load(Ordering::Relaxed)
                )
                .ok();
            }
            crate::println!(
                "[soak] 进度 {}: 堆 {}B (峰值 {}B) 线程 {} | 循环:{}",
                fmt_elapsed(elapsed),
                used,
                peak_heap,
                crate::rtos::thread_info_list().len(),
                detail
            );
            last_report = now;
        }
    }

    // ---- 停止并等待压力线程退出 ----
    STOP.store(true, Ordering::Relaxed);
    let wait_start = crate::rtos::uptime_ms();
    loop {
        let all_gone = !crate::rtos::thread_info_list().iter().any(|t| {
            t.name == "soak-tmp"
                || spawned.iter().any(|&i| ALL_WORKERS[i].name == t.name)
        });
        if all_gone {
            break;
        }
        if crate::rtos::uptime_ms().wrapping_sub(wait_start) > 30_000 {
            crate::log_error!("[soak] 压力线程 30s 内未全部退出");
            break;
        }
        crate::rtos::thread_delay_ms(100).ok();
    }
    // 线程句柄已在创建时释放 (内核侧 kernel_self 强引用维持 TCB 至线程退出),
    // spawned 仅记录索引, 无需清理

    // ---- 最终检查 ----
    let heap_end = crate::heap::used();
    let thread_end = crate::rtos::thread_info_list().len();
    // 互斥量最终值 == Σ(增量 × 次数); 取不到锁本身也是失败 (残留锁)
    let mut mtx_ok = false;
    if let Ok(guard) = SOAK_MTX.lock(Timeout::Ticks(100)) {
        let expected = MTX_LOCKS[0].load(Ordering::Relaxed) as u64 * 10
            + MTX_LOCKS[1].load(Ordering::Relaxed) as u64 * 20;
        mtx_ok = *guard == expected;
    }
    let leak_ok = heap_end.saturating_sub(heap_base) <= 2048 && thread_end == thread_base;
    let total_errors: u32 = spawned.iter().map(|&i| ALL_WORKERS[i].errors.load(Ordering::Relaxed)).sum();
    let pass = total_errors == 0 && sram_errors == 0 && leak_ok && mtx_ok;

    // ---- 汇总报告 (始终打印) ----
    let elapsed = crate::rtos::uptime_ms().wrapping_sub(start);
    crate::println!("[soak] 完成: 运行 {}, 停止原因: {}", fmt_elapsed(elapsed), stop_reason);
    crate::println!("[soak] 结果: {}", if pass { "PASS — 请放心使用" } else { "FAIL" });
    crate::println!(
        "[soak]   压力线程 {} 个: 总错误 {}, SRAM 奇偶/ECC 错误 {}",
        spawned.len(),
        total_errors,
        sram_errors
    );
    // 逐线程明细: 循环数 / 错误数 / 失败详情 (位置与时刻)
    for &i in &spawned {
        let w = ALL_WORKERS[i];
        let errs = w.errors.load(Ordering::Relaxed);
        let mut line = alloc::format!(
            "[soak]   {}: {} 循环, {} 错误",
            w.name,
            w.cycles.load(Ordering::Relaxed),
            errs
        );
        if errs > 0 {
            let fail_elapsed = w.fail_time.load(Ordering::Relaxed).wrapping_sub(SOAK_START_MS.load(Ordering::Relaxed));
            write!(
                &mut line,
                " — 失败: {} (第 {} 循环, 运行 {})",
                error_text(w.last_error.load(Ordering::Relaxed)),
                w.fail_cycle.load(Ordering::Relaxed),
                fmt_elapsed(fail_elapsed)
            )
            .ok();
        }
        crate::println!("{}", line);
    }
    // 未获得任何执行机会的线程 (调度饥饿, 测试覆盖不完整)
    let starved: Vec<usize> = spawned
        .iter()
        .copied()
        .filter(|&i| ALL_WORKERS[i].cycles.load(Ordering::Relaxed) == 0)
        .collect();
    if !starved.is_empty() {
        crate::log_warn!(
            "[soak] {} 个压力线程从未获得执行 (调度饥饿), 结果覆盖不完整",
            starved.len()
        );
    }
    crate::println!(
        "[soak]   堆: 基线 {}B → 峰值 {}B → 结束 {}B{}",
        heap_base,
        peak_heap,
        heap_end,
        if heap_end.saturating_sub(heap_base) <= 2048 { " (无泄漏)" } else { " (泄漏!)" }
    );
    crate::println!(
        "[soak]   线程: 基线 {} → 结束 {}{}",
        thread_base,
        thread_end,
        if thread_end == thread_base { " (无泄漏)" } else { " (泄漏!)" }
    );
    crate::println!(
        "[soak]   互斥量最终值校验: {}",
        if mtx_ok { "通过 (无丢失更新)" } else { "失败 (丢失更新/损坏!)" }
    );
    crate::println!(
        "[soak]   最长调度延迟观测: {} ms",
        MAX_DELAY_MS.load(Ordering::Relaxed)
    );
    // 栈水位汇总 (证明压力线程未逼近栈上限)
    for &i in &spawned {
        let peak = STACK_PEAK[i].load(Ordering::Relaxed);
        if peak > 0 {
            let stack = SPAWN_SPECS[i].stack;
            crate::println!(
                "[soak]   {} 栈峰值 {}B / {}B ({:.0}%)",
                ALL_WORKERS[i].name,
                peak,
                stack,
                peak as f64 * 100.0 / stack as f64
            );
        }
    }
    crate::println!(
        "[soak]   建议: 正式部署时开启 CFG_WDT_ENABLE=true, 调度停滞会被硬件看门狗复位并留下失败证据"
    );
}
