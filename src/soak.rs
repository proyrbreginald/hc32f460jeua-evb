//! 长期稳定性测试 (soak): 在苛刻条件下长时间持续验证系统稳定性,
//! 用于说服使用者"这套系统可以放心长期运行"。
//!
//! **同步执行** (由 shell 的 `soak` 命令调用, 完成后才出下一提示符);
//! 期间按 ESC 可中断, 已完成的压力线程结果仍会汇总输出。
//!
//! # 用法 (产品场景化)
//!
//! 压力项可按产品形态/使用场景选择, 便于现场与业务共存、专项验证:
//!
//! ```text
//! soak [分钟] [压力项|场景]...
//!
//!   soak           全量压力 (默认 CFG_SOAK_MINUTES 分钟)
//!   soak 480       全量压力 8 小时
//!   soak 480 basic RTOS 核心场景 (调度/IPC/堆/线程/定时器/中断)
//!   soak 480 periph 外设场景 (Flash 擦写/CRC/CAN 回环)
//!   soak 60 can flash  专项验证 CAN + Flash
//!   soak 0         直到按 ESC
//! ```
//!
//! | 压力项 | 短名 | 分组 | 压力对象 |
//! |--------|------|------|----------|
//! | `cpu-a/b` | `cpu-a` | basic | 调度器: 多优先级抢占 + 时间片轮转 |
//! | `sem-a/b` | `sem-a` | basic | 信号量: 阻塞获取/释放 + 超时 |
//! | `mtx-a/b` | `mtx-a` | basic | 互斥量: 竞争 + 丢失更新检测 |
//! | `evt-p/c` | `evt-p` | basic | 事件: 置位/等待/清除 + 超时 |
//! | `mb-p/c` | `mb-p` | basic | 邮箱: 满阻塞/空阻塞 + 序号连续性 |
//! | `mq-p/c` | `mq-p` | basic | 消息队列: 二进制负载 + 序号连续性 |
//! | `heap` | `heap` | basic | 堆: 随机分配/释放 + 模式回读 |
//! | `thread` | `thread` | basic | 线程: 反复创建/退出, defunct 回收 |
//! | `timer` | `timer` | basic | 内核定时器: 100ms 周期漂移 |
//! | `irq` | `irq` | basic | 中断上下文: 2ms 高频回调 vs 墙钟 |
//! | `delay` | `delay` | basic | 调度延迟: 延时精度 + 延迟直方图 |
//! | `flash` | `flash` | periph | EFM: 擦除/编程/回读 (节流) |
//! | `crc` | `crc` | periph | CRC 加速器: 硬件 vs 软件参考 |
//! | `can` | `can` | periph | CAN: 内部回环收发校验 (可用时) |
//!
//! # 监控判定 (每秒)
//!
//! shell 线程兼任**监控器**: 检查各压力线程心跳 (停滞即判挂起)、堆用量
//! 峰值突破趋势 (疑似泄漏预警)、线程数、SRAM 奇偶/ECC 错误与栈水位;
//! 任一线程报错即提前结束并给出 FAIL 汇总。
//!
//! # 堆泄漏判定
//!
//! 泄漏基准取**运行预热后** (压力线程首轮执行完成全部一次性惰性初始化)
//! 的堆用量, 而非运行前: 外设/驱动的一次性初始化成本不计入泄漏。结束值
//! 取**多次间隔采样中的最小值** (稳态用量), 避免恰好落在瞬时分配
//! (如日志落盘线程的临时缓冲) 上虚高误报。判定准则: 结束稳态用量 −
//! 预热基准 ≤ 2048B 即无泄漏; 结束采样波动较大时报告会附注采样明细。
//!
//! # 结果判定
//!
//! - 无错误 + 堆/线程无泄漏 + SRAM 无错误 → **PASS**;
//! - 任一压力线程错误 / 心跳停滞 / 泄漏 / SRAM 错误 → **FAIL**;
//! - 建议与 `CFG_WDT_ENABLE=true` 配合: 调度器彻底停滞会被硬件看门狗
//!   复位, 复位即失败证据。汇总报告会标注本次运行看门狗是否生效。
//!
//! 日志分级: 进度与结果走 info 级 (可经 `log` 命令控制), 汇总始终打印。

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::rtos::{Event, EventOpt, Mailbox, MessageQueue, Mutex, Semaphore, Timeout};

use alloc::vec::Vec;

// ============================== 错误码 ==============================

/// 压力线程错误类型 (跨线程以 `u32` 编码存储于 [`Worker`], 见
/// [`SoakError::from_code`])
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
enum SoakError {
    /// 未知/非法错误码 (容错解码)
    Unknown = 0,
    /// IPC 超时 (应被同伴唤醒却超时)
    Timeout = 1,
    /// 数据不匹配/损坏
    Data = 2,
    /// 序号乱序/丢失
    Seq = 3,
    /// 心跳停滞 (线程挂起)
    Hang = 4,
    /// SRAM 奇偶/ECC 错误
    Sram = 5,
    /// 堆/线程泄漏
    Leak = 6,
    /// Flash 校验失败
    Flash = 7,
    /// CRC 校验失败
    Crc = 8,
    /// 定时器漂移
    Timer = 9,
    /// 调度异常 (延时不可靠)
    Delay = 10,
    /// CAN 收发失败
    Can = 11,
    /// 互斥量数据损坏
    Mtx = 12,
    /// 优先级继承失效 (高优先级等待无界)
    Inherit = 13,
}

impl SoakError {
    /// 错误文本 (汇总报告显示)
    const fn text(self) -> &'static str {
        match self {
            SoakError::Unknown => "未知",
            SoakError::Timeout => "IPC 超时",
            SoakError::Data => "数据不匹配",
            SoakError::Seq => "序号乱序/丢失",
            SoakError::Hang => "心跳停滞 (挂起)",
            SoakError::Sram => "SRAM 奇偶/ECC 错误",
            SoakError::Leak => "堆/线程泄漏",
            SoakError::Flash => "Flash 校验失败",
            SoakError::Crc => "CRC 校验失败",
            SoakError::Timer => "定时器漂移",
            SoakError::Delay => "调度异常",
            SoakError::Can => "CAN 收发失败",
            SoakError::Mtx => "互斥量数据损坏",
            SoakError::Inherit => "优先级继承失效",
        }
    }

    /// 解码原子存储的错误码 (未知码容错为 [`SoakError::Unknown`])
    const fn from_code(code: u32) -> SoakError {
        match code {
            1 => SoakError::Timeout,
            2 => SoakError::Data,
            3 => SoakError::Seq,
            4 => SoakError::Hang,
            5 => SoakError::Sram,
            6 => SoakError::Leak,
            7 => SoakError::Flash,
            8 => SoakError::Crc,
            9 => SoakError::Timer,
            10 => SoakError::Delay,
            11 => SoakError::Can,
            12 => SoakError::Mtx,
            13 => SoakError::Inherit,
            _ => SoakError::Unknown,
        }
    }
}

// ============================== 共享状态 ==============================

/// 全局停止标志: 监控器设置, 压力线程轮询退出
static STOP: AtomicBool = AtomicBool::new(false);

/// 互斥量压力对象: 双线程竞争, 各加自己的增量 (A=10, B=20),
/// 值始终为 10 的倍数 —— 任何丢失更新/撕裂写都会被整除性检查捕获
static SOAK_MTX: Mutex<u64> = Mutex::new(0);
/// 优先级继承测试互斥量 (soak-mtx-pi ↔ soak-pi-lock)
static PI_MTX: Mutex<u64> = Mutex::new(0);
/// PI 测试: 高优先级获锁最长等待 (ms; 有界即继承生效)
static PI_MAX_WAIT: AtomicU32 = AtomicU32::new(0);
/// PI 测试完成轮数 (0 = 辅助线程未运行, 覆盖不完整)
static PI_ROUNDS: AtomicU32 = AtomicU32::new(0);
/// 堆压力分配失败次数 (压力下分配失败属正常: allocator 优雅拒绝)
static HEAP_ALLOC_FAILURES: AtomicU32 = AtomicU32::new(0);
/// PI 霸占窗口握手: H 置位请求霸占, hog 确认 (ACK) 后开始忙转
static PI_HOG_GO: AtomicBool = AtomicBool::new(false);
static PI_HOG_ACK: AtomicBool = AtomicBool::new(false);
static MTX_LOCKS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// 其他 IPC 压力对象 (静态, 跨线程共享)
static SOAK_SEM: Semaphore = Semaphore::new(1, 1);
static SOAK_EVT: Event = Event::new();
static SOAK_MB: Mailbox<u32> = Mailbox::new(4);
static SOAK_MQ: MessageQueue = MessageQueue::new(8, 16);

/// 内核定时器压力: 100ms 周期回调计数, 与墙钟对比漂移
static SOAK_TIMER: crate::rtos::Timer = crate::rtos::Timer::new();
static TIMER_TICKS: AtomicU32 = AtomicU32::new(0);

/// 中断上下文压力: 2ms 高频周期回调 (SysTick 中断内执行), 与墙钟对比
static IRQ_TIMER: crate::rtos::Timer = crate::rtos::Timer::new();
static IRQ_TICKS: AtomicU32 = AtomicU32::new(0);

/// CPU 压力线程的运算汇 (volatile 效果: 防止编译器消除计算循环)
static CPU_SINK: AtomicU32 = AtomicU32::new(0);

/// CPU 压力循环迭代次数 (负载强度调参点; 数值越大单线程占用 CPU 越久)
const CPU_ITERATIONS: u32 = 4096;

/// 调度延迟直方图 (delay worker): 桶 `i` 覆盖 `[EDGES[i], EDGES[i+1])` ms,
/// 最后一个桶为尾部 (≥ 末边界)。实时性产品关注延迟**分布**而非仅最坏值。
const DELAY_EDGES: [u32; 10] = [0, 1, 2, 4, 8, 16, 32, 64, 128, 256];
const DELAY_BUCKETS: usize = DELAY_EDGES.len();
static DELAY_HIST: [AtomicU32; DELAY_BUCKETS] = [const { AtomicU32::new(0) }; DELAY_BUCKETS];
static DELAY_SAMPLES: AtomicU32 = AtomicU32::new(0);
static DELAY_MAX: AtomicU32 = AtomicU32::new(0);

/// 本次 soak 开始时刻 (uptime_ms, 供 fail/报告换算运行时长)
static SOAK_START_MS: AtomicU32 = AtomicU32::new(0);

// ============================== 压力线程表 ==============================

/// 压力项分组 (命令行场景选择)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Group {
    /// RTOS 核心: 调度/IPC/堆/线程/定时器/中断路径
    Basic,
    /// 外设: Flash/CRC/CAN
    Periph,
}

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
        self.last_beat
            .store(crate::rtos::uptime_ms(), Ordering::Relaxed);
        self.active.store(false, Ordering::Relaxed);
    }

    /// 心跳: 每轮循环调用, 供监控器判定"线程是否活着"
    fn beat(&self) {
        self.cycles.fetch_add(1, Ordering::Relaxed);
        self.last_beat
            .store(crate::rtos::uptime_ms(), Ordering::Relaxed);
    }

    /// 记录错误并**立即输出失败日志** (console 整行原子输出, 不与其他
    /// 线程输出交错)。
    ///
    /// 停止阶段 (STOP 已置位) 产生的阻塞超时属于正常退出路径, 不计入
    /// 失败; 监控器的停滞判定在 STOP 置位前执行, 不受此门控影响。
    fn fail(&self, err: SoakError) {
        if !STOP.load(Ordering::Relaxed) {
            self.errors.fetch_add(1, Ordering::Relaxed);
            self.last_error.store(err as u32, Ordering::Relaxed);
            self.fail_cycle
                .store(self.cycles.load(Ordering::Relaxed), Ordering::Relaxed);
            self.fail_time
                .store(crate::rtos::uptime_ms(), Ordering::Relaxed);
            let elapsed =
                crate::rtos::uptime_ms().wrapping_sub(SOAK_START_MS.load(Ordering::Relaxed));
            crate::log_error!(
                "[soak] {} 失败: {} (第 {} 循环, 运行 {})",
                self.name,
                err.text(),
                self.fail_cycle.load(Ordering::Relaxed),
                fmt_elapsed(elapsed)
            );
        }
    }
}

/// 压力项创建参数 (名称/短名/栈/优先级/入口/分组)
struct Spec {
    id: WorkerId,
    name: &'static str,
    short: &'static str,
    stack: usize,
    priority: u8,
    entry: extern "C" fn(usize),
    group: Group,
}

/// 一次性声明压力项表: 生成枚举标识、线程状态表、创建参数表并做
/// 编译期校验。条目顺序即索引顺序 (枚举按 `repr(usize)` 对齐)。
macro_rules! soak_table {
    (
        $( $id:ident: [$name:literal, $short:literal, $stack:expr, $prio:expr, $entry:ident, $group:ident] ),+ $(,)?
    ) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        #[repr(usize)]
        enum WorkerId {
            $( $id ),+
        }

        /// 压力项总数 (由宏按条目计数)
        const WORKER_COUNT: usize = soak_table!(@count $( $id ),+);

        /// 压力线程状态表 (与 [`SPECS`] 按索引一一对应)
        static WORKERS: [Worker; WORKER_COUNT] = [
            $( Worker::new($name) ),+
        ];

        /// 压力项创建参数表 (与 [`WORKERS`] 按索引一一对应)
        static SPECS: [Spec; WORKER_COUNT] = [
            $( Spec {
                id: WorkerId::$id,
                name: $name,
                short: $short,
                stack: $stack,
                priority: $prio,
                entry: $entry,
                group: Group::$group,
            } ),+
        ];

        impl WorkerId {
            /// 项索引 (与 [`WORKERS`]/[`SPECS`] 对齐)
            const fn index(self) -> usize {
                self as usize
            }

            /// 按短名/完整名查找 (命令行选择解析)
            fn from_token(token: &str) -> Option<WorkerId> {
                SPECS
                    .iter()
                    .find(|s| s.short == token || s.name == token)
                    .map(|s| s.id)
            }
        }

        // 编译期校验: 栈大小与优先级必须满足内核约束
        const _: () = {
            let mut i = 0;
            while i < WORKER_COUNT {
                assert!(
                    SPECS[i].stack >= 256 && SPECS[i].stack % 8 == 0,
                    "soak: 压力线程栈大小非法"
                );
                assert!(
                    (SPECS[i].priority as usize) < crate::config::PRIORITY_MAX as usize,
                    "soak: 压力线程优先级越界"
                );
                i += 1;
            }
        };
    };
    (@count) => { 0 };
    (@count $head:ident $(, $tail:ident)*) => { 1 + soak_table!(@count $($tail),*) };
}

soak_table! {
    CpuA:   ["soak-cpu-a",   "cpu-a",   1024, 3,  cpu_worker,    Basic],
    CpuB:   ["soak-cpu-b",   "cpu-b",   1024, 4,  cpu_worker,    Basic],
    SemA:   ["soak-sem-a",   "sem-a",   1024, 5,  sem_worker,    Basic],
    SemB:   ["soak-sem-b",   "sem-b",   1024, 6,  sem_worker,    Basic],
    MtxA:   ["soak-mtx-a",   "mtx-a",   1024, 7,  mtx_worker,    Basic],
    MtxB:   ["soak-mtx-b",   "mtx-b",   1024, 8,  mtx_worker,    Basic],
    MtxPi:  ["soak-mtx-pi",  "mtx-pi",  1024, 2,  pi_worker,     Basic],
    EvtP:   ["soak-evt-p",   "evt-p",   1024, 9,  evt_producer,  Basic],
    EvtC:   ["soak-evt-c",   "evt-c",   1024, 10, evt_consumer,  Basic],
    MbP:    ["soak-mb-p",    "mb-p",    1024, 11, mb_producer,   Basic],
    MbC:    ["soak-mb-c",    "mb-c",    1024, 12, mb_consumer,   Basic],
    MqP:    ["soak-mq-p",    "mq-p",    1024, 13, mq_producer,   Basic],
    MqC:    ["soak-mq-c",    "mq-c",    1024, 14, mq_consumer,   Basic],
    Heap:   ["soak-heap",    "heap",    2048, 8,  heap_worker,   Basic],
    Thread: ["soak-thread",  "thread",  1024, 10, thread_worker, Basic],
    Timer:  ["soak-timer",   "timer",   1024, 12, timer_worker,  Basic],
    Irq:    ["soak-irq",     "irq",     1024, 14, irq_worker,    Basic],
    Delay:  ["soak-delay",   "delay",   1024, 15, delay_worker,  Basic],
    Flash:  ["soak-flash",   "flash",   2048, 6,  flash_worker,  Periph],
    Crc:    ["soak-crc",     "crc",     1024, 14, crc_worker,    Periph],
    Can:    ["soak-can",     "can",     2048, 12, can_worker,    Periph],
}

/// 各压力线程的栈使用峰值 (字节, 监控器从 thread_info_list 采样)
static STACK_PEAK: [AtomicU32; WORKER_COUNT] = [const { AtomicU32::new(0) }; WORKER_COUNT];

/// 优先级继承辅助线程名 (线程压力创建的临时线程之外的另一个名)
const PI_LOCK_NAME: &str = "soak-pi-lock";
const PI_HOG_NAME: &str = "soak-pi-hog";

/// 压力项选择 (命令行): 场景别名或单项
#[derive(Clone, Copy)]
enum Selection {
    /// `basic` 场景: RTOS 核心分组
    Basic,
    /// `periph` 场景: 外设分组
    Periph,
    /// 单项
    Item(WorkerId),
}

impl Selection {
    fn from_token(token: &str) -> Option<Selection> {
        match token {
            "basic" => Some(Selection::Basic),
            "periph" => Some(Selection::Periph),
            _ => WorkerId::from_token(token).map(Selection::Item),
        }
    }

    /// 该选择是否覆盖压力项 `id`
    fn includes(self, id: WorkerId) -> bool {
        match self {
            Selection::Basic => SPECS[id.index()].group == Group::Basic,
            Selection::Periph => SPECS[id.index()].group == Group::Periph,
            Selection::Item(w) => w == id,
        }
    }
}

// ============================== 压力线程入口 ==============================

/// CPU/调度压力: 纯计算 + 周期延时, 多优先级抢占与时间片轮转
extern "C" fn cpu_worker(param: usize) {
    let w = &WORKERS[param];
    // param 在运行时传入, 使计算循环依赖运行时值 (防止编译期折叠)
    let mut state = param as u32 ^ 0x9E37_79B9;
    while !STOP.load(Ordering::Relaxed) {
        for _ in 0..CPU_ITERATIONS {
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
    let w = &WORKERS[param];
    while !STOP.load(Ordering::Relaxed) {
        if SOAK_SEM.take(Timeout::Ticks(1000)).is_err() {
            w.fail(SoakError::Timeout);
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
    let w = &WORKERS[param];
    let slot = if param == WorkerId::MtxA.index() {
        0
    } else {
        1
    };
    let add = if slot == 0 { 10u64 } else { 20u64 };
    let mut last = 0u64;
    while !STOP.load(Ordering::Relaxed) {
        let Ok(mut guard) = SOAK_MTX.lock(Timeout::Ticks(1000)) else {
            w.fail(SoakError::Timeout);
            break;
        };
        let v = *guard;
        if v % 10 != 0 || v < last {
            // 显式释放守卫: 线程退出不执行析构, 否则互斥量会永久锁死
            drop(guard);
            w.fail(SoakError::Mtx);
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

/// 优先级继承压力: 高优先级线程等待被中优先级霸占 CPU 时持有的
/// 互斥量, 验证内核优先级继承 (PI) 使等待时间有界。
///
/// 时窗式设计 (不挤占压力频带): 每秒一次测量窗口内, `soak-pi-hog`
/// (优先级 6) 确认开始忙转, `soak-pi-lock` (优先级 10) 持锁约 20ms
/// 忙计算 (5% 占空比), `soak-mtx-pi` (优先级 2) 阻塞等锁。窗口外
/// 霸占者/持锁者近零 CPU 消耗, 7~15 优先级压力频带调度不受影响。
///
/// 判别: 有 PI 时持锁者被提升至 2 抢占霸占者, 等待 ≈ 持锁剩余时间
/// (<25ms); 无 PI 时持锁者被霸占者永久饥饿 (窗口内霸占者不会自行
/// 停止), 等待必超 100ms → FAIL。
extern "C" fn pi_worker(param: usize) {
    let w = &WORKERS[param];
    crate::rtos::thread_create(PI_LOCK_NAME, 1024, 10, 0, pi_lock_thread, 0);
    crate::rtos::thread_create(PI_HOG_NAME, 1024, 6, 0, pi_hog_thread, 0);
    while !STOP.load(Ordering::Relaxed) {
        // 窗口握手: 置位并等待霸占者确认开始忙转 (≤2ms) 后才测量,
        // 保证测量瞬间持锁者确实处于被饥饿状态
        PI_HOG_GO.store(true, Ordering::Relaxed);
        let ack_start = crate::rtos::uptime_ms();
        while !PI_HOG_ACK.load(Ordering::Relaxed)
            && crate::rtos::uptime_ms().wrapping_sub(ack_start) < 10
        {
            crate::rtos::thread_delay_ms(1).ok();
        }
        let t0 = crate::rtos::uptime_ms();
        let lock_result = PI_MTX.lock(Timeout::Ticks(100));
        PI_HOG_GO.store(false, Ordering::Relaxed);
        let Ok(mut guard) = lock_result else {
            w.fail(SoakError::Inherit);
            break;
        };
        let wait = crate::rtos::uptime_ms().wrapping_sub(t0);
        PI_MAX_WAIT.fetch_max(wait, Ordering::Relaxed);
        PI_ROUNDS.fetch_add(1, Ordering::Relaxed);
        *guard = (*guard).wrapping_add(1);
        drop(guard);
        w.beat();
        crate::rtos::thread_delay_ms(900).ok();
    }
    PI_HOG_GO.store(false, Ordering::Relaxed);
    w.active.store(false, Ordering::Relaxed);
}

/// PI 低优先级持锁者: 持锁约 20ms 忙计算后睡眠 380ms (5% 占空比,
/// 不挤占 11~15 优先级频带)。忙转期间不主动让出: 只有在被继承
/// 提升 (或霸占者未激活) 时才有机会运行。
extern "C" fn pi_lock_thread(_param: usize) {
    let mut s = 0x5EEDu32;
    while !STOP.load(Ordering::Relaxed) {
        let Ok(mut guard) = PI_MTX.lock(Timeout::Ticks(2000)) else {
            continue;
        };
        let t0 = crate::rtos::uptime_ms();
        while crate::rtos::uptime_ms().wrapping_sub(t0) < 20 {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        }
        *guard = (*guard).wrapping_add(s as u64);
        drop(guard);
        crate::rtos::thread_delay_ms(380).ok();
    }
}

/// PI 中优先级 CPU 霸占者: 空闲时 1ms 轮询 (近零 CPU); 测量窗口内
/// 持续忙转, 直到 H 完成测量清除请求。窗口外不消耗任何调度带宽。
extern "C" fn pi_hog_thread(_param: usize) {
    let mut s = 0xBEEFu32;
    while !STOP.load(Ordering::Relaxed) {
        crate::rtos::thread_delay_ms(1).ok();
        if PI_HOG_GO.load(Ordering::Relaxed) {
            PI_HOG_ACK.store(true, Ordering::Relaxed);
            while PI_HOG_GO.load(Ordering::Relaxed) {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                CPU_SINK.store(s, Ordering::Relaxed);
            }
            PI_HOG_ACK.store(false, Ordering::Relaxed);
        }
    }
}

/// 事件压力: 生产者置位
extern "C" fn evt_producer(param: usize) {
    let w = &WORKERS[param];
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
    let w = &WORKERS[param];
    while !STOP.load(Ordering::Relaxed) {
        match SOAK_EVT.recv(0x0F, EventOpt::Or, Timeout::Ticks(1000)) {
            Ok(bits) => {
                if bits == 0 || bits & !0x0F != 0 {
                    w.fail(SoakError::Data);
                    break;
                }
                let _ = SOAK_EVT.recv(bits, EventOpt::OrClear, Timeout::Ticks(0));
            }
            Err(_) => {
                w.fail(SoakError::Timeout);
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
    let w = &WORKERS[param];
    let mut seq = 0u32;
    while !STOP.load(Ordering::Relaxed) {
        if SOAK_MB.send(seq, Timeout::Ticks(1000)).is_err() {
            w.fail(SoakError::Timeout);
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
    let w = &WORKERS[param];
    let mut expect = 0u32;
    while !STOP.load(Ordering::Relaxed) {
        match SOAK_MB.recv(Timeout::Ticks(1000)) {
            Ok(v) => {
                if v != expect {
                    w.fail(SoakError::Seq);
                    break;
                }
                expect = expect.wrapping_add(1);
            }
            Err(_) => {
                w.fail(SoakError::Timeout);
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
    let w = &WORKERS[param];
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
            w.fail(SoakError::Timeout);
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
    let w = &WORKERS[param];
    let mut expect = 0u32;
    let mut msg = [0u8; 8];
    while !STOP.load(Ordering::Relaxed) {
        match SOAK_MQ.recv(&mut msg, Timeout::Ticks(1000)) {
            Ok(8) => {
                let seq = u32::from_le_bytes([msg[1], msg[2], msg[3], msg[4]]);
                let chk = (msg[0..7].iter().fold(0u8, |a, b| a.wrapping_add(*b))).wrapping_mul(31);
                if msg[0] != 0x52 || msg[5] != 0xA5 || msg[6] != 0x5A || msg[7] != chk {
                    w.fail(SoakError::Data);
                    break;
                }
                if seq != expect {
                    w.fail(SoakError::Seq);
                    break;
                }
                expect = expect.wrapping_add(1);
            }
            _ => {
                w.fail(SoakError::Data);
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
    let w = &WORKERS[param];
    let mut rng = param as u32 ^ 0x00C0_FFEE;
    let mut live: Vec<(u32, Vec<u8>)> = Vec::new(); // (种子, 数据)
    let mut verified_all = 0u32;
    while !STOP.load(Ordering::Relaxed) {
        // 分配 1~3 块随机大小 (try_reserve: 分配失败是压力下的正常
        // 现象, 计数后跳过而非 OOM panic 复位 —— 证明 allocator 在
        // 极限下优雅拒绝而非崩溃)
        for _ in 0..(rng % 3 + 1) {
            rng = xorshift(rng);
            let size = 16 + (rng % 1024) as usize;
            let seed = rng;
            let mut buf = Vec::new();
            if buf.try_reserve_exact(size).is_err() {
                HEAP_ALLOC_FAILURES.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            buf.resize(size, 0);
            let mut s = seed;
            for b in &mut buf {
                s = xorshift(s);
                *b = s as u8;
            }
            if live.try_reserve(1).is_err() {
                HEAP_ALLOC_FAILURES.fetch_add(1, Ordering::Relaxed);
                continue;
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
                w.fail(SoakError::Data);
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
///
/// 入口通过 `param` 携带退出标志 (`*mut AtomicBool`, 0 表示无标志):
/// 最后写入标志后返回, 供创建方区分"从未被调度"(内核缺陷) 与
/// "已退出但 defunct 回收被调度延迟"(负载压力下的正常现象)。
extern "C" fn tmp_exit_thread(param: usize) {
    if param != 0 {
        // 参数非法时静默退出 (创建方阶段 1 超时判失败), 不 panic:
        // 压力测试中的 panic 会复位系统, 掩盖真实故障定位
        // SAFETY: 指针由 thread_worker 传入, 指向线程栈上的 AtomicBool,
        // 线程存活期间保持有效
        if let Some(flag) = unsafe { (param as *mut AtomicBool).as_ref() } {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

extern "C" fn thread_worker(param: usize) {
    let w = &WORKERS[param];
    while !STOP.load(Ordering::Relaxed) {
        // tmp 线程必须进入活跃频带: 曾以优先级 25 创建, 在满载的
        // 压力频带 (3~15) 之下被完全饿死, 永不调度/回收, 500ms 轮询
        // 超时即误报"泄漏"。
        let exit = AtomicBool::new(false);
        crate::rtos::thread_create(
            "soak-tmp",
            512,
            8,
            0,
            tmp_exit_thread,
            &exit as *const AtomicBool as usize,
        );

        // 阶段 1: tmp 必须在短时限内获得调度并退出 (证明线程可运行)。
        // 未能调度即内核缺陷, 判失败; 时限内正常完成则进入阶段 2。
        let start = crate::rtos::uptime_ms();
        let mut ran = false;
        while crate::rtos::uptime_ms().wrapping_sub(start) < 200 {
            if exit.load(Ordering::Relaxed) {
                ran = true;
                break;
            }
            w.beat();
            crate::rtos::thread_delay_ms(10).ok();
        }

        // 阶段 2: 等待 defunct 回收。回收由空闲线程执行, 需要完整的
        // 调度窗口; 压力频带满载时窗口可能长期不出现, 回收延迟属于
        // 正常现象而非泄漏。宽限与监控器的停滞判定一致, 期间持续
        // 心跳, 等待超过宽限才判失败 (真实泄漏由结束时的线程数
        // 校验兜底)。
        let mut stale = true;
        if ran {
            let start = crate::rtos::uptime_ms();
            while crate::rtos::uptime_ms().wrapping_sub(start) < crate::config::SOAK_HANG_GRACE_MS {
                let gone = !crate::rtos::thread_info_list()
                    .iter()
                    .any(|t| t.name == "soak-tmp");
                if gone {
                    stale = false;
                    break;
                }
                w.beat();
                crate::rtos::thread_delay_ms(10).ok();
            }
        }
        if !ran || stale {
            w.fail(SoakError::Leak);
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
    let w = &WORKERS[param];
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
            w.fail(SoakError::Timer);
            break;
        }
        last_ticks = ticks;
        last_time = now;
    }
    pin.stop();
    w.active.store(false, Ordering::Relaxed);
}

/// 中断上下文压力回调: 2ms 周期, 在 SysTick 中断 (ISR) 上下文执行。
/// 与 SysTick 1kHz 叠加构成持续中断负载, 验证中断路径 (定时器链表
/// 扫描/回调执行) 在长期高中断率下的稳定性 —— 无人值守产品关键路径。
extern "C" fn irq_timer_cb(_param: usize) {
    IRQ_TICKS.fetch_add(1, Ordering::Relaxed);
}

extern "C" fn irq_worker(param: usize) {
    let w = &WORKERS[param];
    let pin = IRQ_TIMER.pin_static();
    pin.start_ms(2, 2, irq_timer_cb, 0);
    let mut last_ticks = IRQ_TICKS.load(Ordering::Relaxed);
    let mut last_time = crate::rtos::uptime_ms();
    while !STOP.load(Ordering::Relaxed) {
        crate::rtos::thread_delay_ms(500).ok();
        w.beat();
        let now = crate::rtos::uptime_ms();
        let ticks = IRQ_TICKS.load(Ordering::Relaxed);
        let elapsed = now.wrapping_sub(last_time);
        let expected = elapsed / 2;
        let actual = ticks.wrapping_sub(last_ticks);
        // 高频下允许 ±3 个回调的抖动
        if expected >= 40 && (actual as i64 - expected as i64).abs() > 3 {
            w.fail(SoakError::Timer);
            break;
        }
        last_ticks = ticks;
        last_time = now;
    }
    pin.stop();
    w.active.store(false, Ordering::Relaxed);
}

/// 调度延迟压力: 延时精度 + 延迟分布直方图 (p50/p90/p99/最坏值)
extern "C" fn delay_worker(param: usize) {
    let w = &WORKERS[param];
    while !STOP.load(Ordering::Relaxed) {
        let t0 = crate::rtos::uptime_ms();
        crate::rtos::thread_delay_ms(100).ok();
        let actual = crate::rtos::uptime_ms().wrapping_sub(t0);
        // 延时只能晚不能早; 超过 2s 判调度异常
        if !(100..=2100).contains(&actual) {
            w.fail(SoakError::Delay);
            break;
        }
        let over = actual - 100;
        // 直方图: 按桶边界定位 (最后一个桶为尾部)
        let mut bucket = DELAY_BUCKETS - 1;
        for (i, &edge) in DELAY_EDGES.iter().enumerate().take(DELAY_BUCKETS - 1) {
            if over < edge {
                bucket = i;
                break;
            }
        }
        DELAY_HIST[bucket].fetch_add(1, Ordering::Relaxed);
        DELAY_SAMPLES.fetch_add(1, Ordering::Relaxed);
        // 最坏值 (CAS 循环)
        let mut max = DELAY_MAX.load(Ordering::Relaxed);
        while over > max {
            match DELAY_MAX.compare_exchange_weak(max, over, Ordering::Relaxed, Ordering::Relaxed) {
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
    let w = &WORKERS[param];
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
            w.fail(SoakError::Flash);
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
    let w = &WORKERS[param];
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
        let hw = crate::crc::calculate(
            &buf[..len],
            crate::crc::DataWidth::Byte,
            crate::crc::Config::crc32(),
        );
        let sw = soft_crc32(&buf[..len]);
        if hw != sw {
            w.fail(SoakError::Crc);
            break;
        }
        // 周期性核对标准向量
        if w.cycles.load(Ordering::Relaxed).is_multiple_of(64) {
            let v: &[u8] = b"123456789";
            let ok =
                crate::crc::calculate(v, crate::crc::DataWidth::Byte, crate::crc::Config::crc32())
                    == 0xCBF4_3926;
            if !ok {
                w.fail(SoakError::Crc);
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
/// 压力线程轮询窗口可能被高优先级负载拉长, 超时取 CAN_TIMEOUT_MS 与
/// 500ms 的较大值, 避免负载高峰误报。
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
    let w = &WORKERS[param];
    let can = crate::board::BoardResources::get().can();
    let xtal_was_enabled = crate::clk::xtal_enabled();
    let mut ok = true;

    if can
        .init(crate::can::Config {
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
            let frame = crate::can::TxFrame::data(crate::can::Id::Standard(0x100), 8, payload);
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
        w.fail(SoakError::Can);
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

/// 运行时长格式化 "H:MM:SS"
fn fmt_elapsed(elapsed_ms: u32) -> alloc::string::String {
    let s = elapsed_ms / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    alloc::format!("{}:{:02}:{:02}", h, m, s)
}

// ============================== 控制台进度条 ==============================

/// 进度条块数 (█░)
const BAR_BLOCKS: usize = 18;
/// 进度条行目标宽度 (终端字符数, 含 `\r`); 行内容按字符数补齐,
/// 内容缩短时 `\r` 原位刷新不会残留旧行尾
const BAR_CELLS: usize = 76;

/// 堆占用 "X.YK" 整数化 (避免拖入浮点格式化与软浮点库)
fn fmt_kib(used: usize) -> (usize, usize) {
    (used / 1024, (used % 1024) * 10 / 1024)
}

/// 原位刷新单行进度条 (不换行, 不进入日志环; soak 运行期间控制台
/// 唯一的持续输出)。`total_ms == 0` 表示"直到 ESC"模式, 无百分比。
fn render_bar(elapsed_ms: u32, total_ms: u32, used: usize, threads: usize, errors: u32) {
    let s = elapsed_ms / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    let mut line = if total_ms > 0 {
        // u32 精确进度: 乘积可容纳时直接算; 超长 (≥49.7 天) 时改用除法形式
        let pct = if total_ms <= u32::MAX / 100 {
            (elapsed_ms * 100 / total_ms).min(100) as usize
        } else {
            (elapsed_ms / (total_ms / 100)).min(100) as usize
        };
        let filled = (pct * BAR_BLOCKS) / 100;
        let mut bar = alloc::string::String::with_capacity(BAR_BLOCKS + 2);
        for _ in 0..filled {
            bar.push('█');
        }
        for _ in filled..BAR_BLOCKS {
            bar.push('░');
        }
        let (th, tm, ts) = (
            total_ms / 3_600_000,
            (total_ms / 60_000) % 60,
            (total_ms / 1000) % 60,
        );
        let (ki, kf) = fmt_kib(used);
        alloc::format!(
            "\r[soak] {h}:{m:02}:{s:02}/{th}:{tm:02}:{ts:02} [{bar}] {pct:3}%  堆 {}.{}K  线程 {threads}  错误 {errors}",
            ki,
            kf
        )
    } else {
        let (ki, kf) = fmt_kib(used);
        alloc::format!(
            "\r[soak] {h}:{m:02}:{s:02} (直到 ESC)  堆 {}.{}K  线程 {threads}  错误 {errors}",
            ki,
            kf
        )
    };
    // 定宽补齐 (字符数): 行缩短时覆盖旧内容
    while line.chars().count() < BAR_CELLS {
        line.push(' ');
    }
    crate::print!("{}", line);
}

/// 清空进度条行 (结束阶段换行前调用, 避免残留)
fn clear_bar() {
    crate::print!("\r");
    for _ in 0..BAR_CELLS {
        crate::print!(" ");
    }
    crate::print!("\r");
}

/// 分组标签 (报告线程表显示)
fn group_label(group: Group) -> &'static str {
    match group {
        Group::Basic => "RTOS 核心",
        Group::Periph => "外设",
    }
}

/// 文件系统错误 → 中文摘要 (报告写入失败回退显示)
fn fs_error_summary(error: &littlefs::Error<crate::filesystem::FlashError>) -> &'static str {
    match error {
        littlefs::Error::Device(_) => "Flash 设备错误",
        littlefs::Error::NoSpace => "文件系统空间不足",
        littlefs::Error::AlreadyExists => "目标已存在",
        littlefs::Error::NotFound => "未找到",
        littlefs::Error::IsDirectory => "目标是目录",
        _ => "其他文件系统错误",
    }
}

// ============================== 命令行解析 ==============================

/// 解析 `soak` 参数: `[分钟] [压力项|场景]...`
///
/// 返回 `(分钟, 压力项选择)`; 选择为 `None` 表示全量。参数非法返回
/// `None` (调用方打印用法)。
fn parse_args(args: &str) -> Option<(u32, Option<Vec<Selection>>)> {
    let mut minutes: Option<u32> = None;
    let mut selections: Option<Vec<Selection>> = None;
    for token in args.split_whitespace() {
        if let Ok(m) = token.parse::<u32>() {
            if minutes.replace(m).is_some() {
                return None; // 分钟重复
            }
        } else {
            selections
                .get_or_insert_with(Vec::new)
                .push(Selection::from_token(token)?);
        }
    }
    let minutes = minutes
        .unwrap_or(crate::config::SOAK_MINUTES)
        .min(24 * 60 * 7); // 上限 7 天
    Some((minutes, selections))
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

/// 调度延迟分位数 (直方图累计定位, 返回桶下界 ms)
fn delay_percentile(pct: u32) -> u32 {
    let samples = DELAY_SAMPLES.load(Ordering::Relaxed);
    if samples == 0 {
        return 0;
    }
    // u32 精确; 饱和仅出现在 50 天级长 soak (samples > 42.9M)
    let target = (samples.saturating_mul(pct) / 100).max(1);
    let mut acc = 0u32;
    for (i, bucket) in DELAY_HIST.iter().enumerate() {
        acc += bucket.load(Ordering::Relaxed);
        if acc >= target {
            return DELAY_EDGES[i];
        }
    }
    DELAY_EDGES[DELAY_BUCKETS - 1]
}

/// 运行长期稳定性测试 (由 shell `soak` 命令调用)
///
/// 参数: `[分钟] [压力项|场景]...` (见模块文档用法说明)。
pub(crate) fn run(args: &str) {
    let Some((minutes, selection)) = parse_args(args) else {
        crate::println!("[soak] 用法: soak [分钟] [压力项|场景]...");
        crate::println!("[soak]   压力项: {}", {
            let mut s = alloc::string::String::new();
            for (i, spec) in SPECS.iter().enumerate() {
                if i > 0 {
                    s.push(' ');
                }
                s.push_str(spec.short);
            }
            s
        });
        crate::println!("[soak]   场景: basic|periph (缺省=全部)");
        return;
    };
    let duration_ms = minutes.saturating_mul(60_000); // u32, 上限 ≈ 49.7 天

    // ---- 复位全局状态 ----
    STOP.store(false, Ordering::Relaxed);
    SOAK_START_MS.store(crate::rtos::uptime_ms(), Ordering::Relaxed);
    for w in WORKERS.iter() {
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
    // PI 握手复位 (防御: 上次异常退出残留)
    PI_HOG_GO.store(false, Ordering::Relaxed);
    PI_HOG_ACK.store(false, Ordering::Relaxed);
    // 累计统计复位: 本次运行的报告不得混入先前运行的数据
    PI_MAX_WAIT.store(0, Ordering::Relaxed);
    PI_ROUNDS.store(0, Ordering::Relaxed);
    HEAP_ALLOC_FAILURES.store(0, Ordering::Relaxed);
    TIMER_TICKS.store(0, Ordering::Relaxed);
    IRQ_TICKS.store(0, Ordering::Relaxed);
    for slot in STACK_PEAK.iter() {
        slot.store(0, Ordering::Relaxed);
    }
    // 直方图复位
    for b in DELAY_HIST.iter() {
        b.store(0, Ordering::Relaxed);
    }
    DELAY_SAMPLES.store(0, Ordering::Relaxed);
    DELAY_MAX.store(0, Ordering::Relaxed);
    // 硬实时指标复位: 关中断/节拍延迟峰值从本次运行起重新累计
    crate::latency::reset_peaks();

    // ---- 基线快照 ----
    let heap_base = crate::heap::used();
    let thread_base = crate::rtos::thread_info_list().len();
    crate::sram::clear_status(crate::sram::ERR_ALL);

    // ---- 创建压力线程 (按选择过滤) ----
    let mut spawned: Vec<WorkerId> = Vec::new();
    crate::println!(
        "[soak] 开始: 目标 {} 分钟 ({}), 按 ESC 可中断",
        minutes,
        if minutes == 0 { "直到 ESC" } else { "限时" }
    );
    for spec in SPECS.iter() {
        if spec.id == WorkerId::Can
            && let Some(reason) = can_skip_reason()
        {
            crate::log_debug!("[soak] CAN 压力跳过: {}", reason);
            continue;
        }
        if let Some(sel) = &selection
            && !sel.iter().any(|s| s.includes(spec.id))
        {
            continue;
        }
        crate::rtos::thread_create(
            spec.name,
            spec.stack,
            spec.priority,
            10,
            spec.entry,
            spec.id.index(),
        );
        WORKERS[spec.id.index()]
            .active
            .store(true, Ordering::Relaxed);
        spawned.push(spec.id);
        // 已启动明细进 debug 级 (控制台只保留进度条; 完整清单见报告)
        crate::log_debug!(
            "[soak] 压力线程 {} 已启动: 优先级 {}, 栈 {}B",
            spec.name,
            spec.priority,
            spec.stack
        );
    }
    // 回显本次压力项 (产品证据: 明确本次测了什么; 控制台保持简洁,
    // 完整清单进 debug 级与 HTML 报告)
    if spawned.is_empty() {
        crate::println!("[soak] 没有可运行的压力线程 (选择项均被跳过)");
        return;
    }
    crate::log_debug!(
        "[soak] 共 {} 个压力线程: {}",
        spawned.len(),
        spawned
            .iter()
            .map(|&id| SPECS[id.index()].short)
            .collect::<Vec<_>>()
            .join(" ")
    );

    // ---- 监控循环 ----
    let start = crate::rtos::uptime_ms();
    // 复位内核统计 (空闲迭代/上下文切换), 作为本次运行的计数基线
    crate::rtos::reset_stats();
    let mut last_report = start;
    let mut peak_heap = heap_base;
    let mut sram_errors = 0u32;
    let mut stop_reason = "完成";
    // 堆峰值突破连击 (疑似泄漏趋势): 连续报告期突破历史峰值才告警
    let mut peak_break_streak = 0u32;
    // 泄漏检测基准: 预热后 (压力线程首轮执行完成全部一次性惰性分配,
    // 如外设/驱动初始化) 的堆用量。以此时刻为基准, 一次性初始化成本
    // 不再误报"泄漏"。
    let mut leak_base = 0usize;
    // CPU 利用率 (空闲迭代/节拍估算) 与堆碎片采样
    let mut last_ticks = crate::rtos::tick();
    let mut last_idle = crate::rtos::idle_iterations();
    let mut util_max = 0u32;
    let mut util_end = 0u32;
    let mut lfb_min = crate::heap::largest_free_block();

    'monitor: loop {
        crate::rtos::thread_delay_ms(1000).ok();
        let now = crate::rtos::uptime_ms();
        if leak_base == 0 {
            leak_base = crate::heap::used();
        }

        // ESC 中断
        if crate::board::abort_requested() {
            stop_reason = "ESC 中断";
            break 'monitor;
        }
        // 时长到达
        if minutes != 0 && crate::rtos::uptime_ms().wrapping_sub(start) >= duration_ms {
            break 'monitor;
        }
        // 心跳停滞判定 (停滞即判挂起: 丢失唤醒/死锁/调度停滞)
        for &id in &spawned {
            let w = &WORKERS[id.index()];
            let idle = now.wrapping_sub(w.last_beat.load(Ordering::Relaxed));
            if w.active.load(Ordering::Relaxed) && idle > crate::config::SOAK_HANG_GRACE_MS {
                crate::log_warn!(
                    "[soak] {} 心跳停滞 {}ms (超过宽限 {}ms), 判挂起",
                    w.name,
                    idle,
                    crate::config::SOAK_HANG_GRACE_MS
                );
                w.fail(SoakError::Hang);
            }
        }
        // 任一压力线程报错 → 提前结束 (失败详情已由 fail() 即时输出)
        if spawned
            .iter()
            .any(|&id| WORKERS[id.index()].errors.load(Ordering::Relaxed) > 0)
        {
            stop_reason = "检测到压力线程错误";
            break 'monitor;
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
        // CPU 利用率 (估算): 空闲循环全空闲时约每节拍一次, 中断唤醒
        // 会使迭代偏多, 故按节拍数钳制
        let ticks_delta = crate::rtos::tick().wrapping_sub(last_ticks);
        let idle_delta = crate::rtos::idle_iterations().wrapping_sub(last_idle);
        last_ticks = crate::rtos::tick();
        last_idle = crate::rtos::idle_iterations();
        if let Some(util) = idle_delta
            .min(ticks_delta)
            .checked_mul(100)
            .and_then(|v| v.checked_div(ticks_delta))
            .map(|ratio| 100u32.saturating_sub(ratio))
        {
            util_max = util_max.max(util);
            util_end = util;
        }
        // 堆碎片: 最大连续空闲块 (最差时刻 = 运行期最小值)
        let lfb = crate::heap::largest_free_block();
        lfb_min = lfb_min.min(lfb);
        // 栈水位采样: 记录各压力线程峰值 (供汇总报告证明无溢出风险)
        for t in crate::rtos::thread_info_list() {
            if let Some(id) = SPECS.iter().find(|s| s.name == t.name).map(|s| s.id) {
                let peak = STACK_PEAK[id.index()].load(Ordering::Relaxed);
                if t.stack_used as u32 > peak {
                    STACK_PEAK[id.index()].store(t.stack_used as u32, Ordering::Relaxed);
                }
            }
        }
        // 周期进度报告 + 泄漏趋势检测
        if now.wrapping_sub(last_report) >= crate::config::SOAK_REPORT_INTERVAL_MS {
            // 峰值突破趋势: 连续报告期突破历史峰值 → 疑似泄漏 (不改判定,
            // 最终判定由结束值阈值决定; 正常压力平台期峰值稳定不触发)
            if used > peak_heap + 2048 {
                peak_break_streak += 1;
                if peak_break_streak >= 3 {
                    crate::log_warn!(
                        "[soak] 堆用量持续突破历史峰值 (疑似泄漏趋势): {}B (历史峰值 {}B)",
                        used,
                        peak_heap
                    );
                    peak_break_streak = 0;
                }
            } else {
                peak_break_streak = 0;
            }
            last_report = now;
        }
        // 进度条: 每秒原位刷新 (控制台唯一持续输出, 不进入日志环)
        render_bar(
            now.wrapping_sub(start),
            duration_ms,
            used,
            spawned.len(),
            spawned
                .iter()
                .map(|&id| WORKERS[id.index()].errors.load(Ordering::Relaxed))
                .sum(),
        );
    }

    // ---- 停止并等待压力线程退出 ----
    STOP.store(true, Ordering::Relaxed);
    let wait_start = crate::rtos::uptime_ms();
    loop {
        let all_gone = !crate::rtos::thread_info_list().iter().any(|t| {
            t.name == "soak-tmp"
                || t.name == PI_LOCK_NAME
                || t.name == PI_HOG_NAME
                || spawned.iter().any(|&id| WORKERS[id.index()].name == t.name)
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

    // ---- 等待僵尸回收稳定 ----
    // 线程退出的 TCB/栈由空闲线程异步回收; 若在回收前测量堆用量,
    // 残留僵尸会使结束值虚高, 误报"泄漏"。给回收一个受宽限约束的窗口。
    let recycle_start = crate::rtos::uptime_ms();
    while crate::rtos::pending_defuncts() > 0
        && crate::rtos::uptime_ms().wrapping_sub(recycle_start) < crate::config::SOAK_HANG_GRACE_MS
    {
        crate::rtos::thread_delay_ms(10).ok();
    }

    // ---- 结束堆稳定采样 ----
    // 单次测量可能恰好落在瞬时分配上 (如日志落盘线程刷新时的临时缓冲),
    // 使结束值虚高误报泄漏。间隔采样取最小值 = 稳态用量。
    let mut end_samples = [crate::heap::used(); 4];
    for sample in &mut end_samples[1..] {
        crate::rtos::thread_delay_ms(1000).ok();
        *sample = crate::heap::used();
    }
    let heap_end = *end_samples.iter().min().unwrap();

    // ---- 最终检查 ----
    let thread_end = crate::rtos::thread_info_list().len();
    // 互斥量最终值 == Σ(增量 × 次数); 取不到锁本身也是失败 (残留锁)
    let mut mtx_ok = false;
    if let Ok(guard) = SOAK_MTX.lock(Timeout::Ticks(100)) {
        let expected = MTX_LOCKS[0].load(Ordering::Relaxed) as u64 * 10
            + MTX_LOCKS[1].load(Ordering::Relaxed) as u64 * 20;
        mtx_ok = *guard == expected;
    }
    // 泄漏 = 结束稳态用量超出预热基准 (一次性初始化已计入基准)
    let net_growth = heap_end.saturating_sub(leak_base);
    let leak_ok = net_growth <= 2048 && thread_end == thread_base;
    let total_errors: u32 = spawned
        .iter()
        .map(|&id| WORKERS[id.index()].errors.load(Ordering::Relaxed))
        .sum();

    // ---- 硬实时指标判定 (DWT 实测峰值, 见 latency 模块) ----
    let critical_cycles = crate::latency::max_critical_cycles();
    let critical_flash_cycles = crate::latency::max_critical_flash_cycles();
    let tick_latency_cycles = crate::latency::max_tick_latency_cycles();
    let tick_latency_flash_cycles = crate::latency::max_tick_latency_flash_cycles();
    let critical_us = crate::latency::cycles_to_us(critical_cycles);
    let critical_flash_us = crate::latency::cycles_to_us(critical_flash_cycles);
    let tick_latency_us = crate::latency::cycles_to_us(tick_latency_cycles);
    let tick_latency_flash_us = crate::latency::cycles_to_us(tick_latency_flash_cycles);
    let latency_ok = critical_us <= crate::config::SOAK_MAX_CRITICAL_US
        && tick_latency_us <= crate::config::SOAK_MAX_IRQ_LATENCY_US;
    if !latency_ok {
        crate::log_error!(
            "[soak] 硬实时指标超限: 最长关中断 {}µs (限 {}µs), 节拍到达延迟 {}µs (限 {}µs)",
            critical_us,
            crate::config::SOAK_MAX_CRITICAL_US,
            tick_latency_us,
            crate::config::SOAK_MAX_IRQ_LATENCY_US
        );
    }

    let pass = total_errors == 0 && sram_errors == 0 && leak_ok && mtx_ok && latency_ok;

    // ---- 汇总报告: 单文件 HTML → /test/ (控制台仅保留结果一行) ----
    clear_bar();
    let elapsed = crate::rtos::uptime_ms().wrapping_sub(start);

    // RTC 时间戳 (报告头部与文件名); RTC 未运行或日期未设置 (上电
    // 默认 2000-01-01, 时间从复位起计数) 时退化为启动序号, 避免
    // 报告出现误导性的 "2000-01-01" 日期
    let rtc_stamp = crate::rtc::get_date()
        .ok()
        .zip(crate::rtc::get_time().ok())
        .filter(|(d, _t)| !(d.year == 0 && d.month == 1 && d.day == 1))
        .map(|(d, t)| (d.year, d.month, d.day, t.hour, t.minute, t.second));

    // 压力项清单 + 线程明细行
    let items = spawned
        .iter()
        .map(|&id| SPECS[id.index()].short)
        .collect::<Vec<_>>()
        .join(" ");
    let rows: Vec<crate::soak_report_core::Row> = spawned
        .iter()
        .map(|&id| {
            let w = &WORKERS[id.index()];
            let errs = w.errors.load(Ordering::Relaxed);
            let fail_text = if errs > 0 {
                let fail_elapsed = w
                    .fail_time
                    .load(Ordering::Relaxed)
                    .wrapping_sub(SOAK_START_MS.load(Ordering::Relaxed));
                Some(alloc::format!(
                    "{} (第 {} 循环, 运行 {})",
                    SoakError::from_code(w.last_error.load(Ordering::Relaxed)).text(),
                    w.fail_cycle.load(Ordering::Relaxed),
                    fmt_elapsed(fail_elapsed)
                ))
            } else {
                None
            };
            crate::soak_report_core::Row {
                name: SPECS[id.index()].name,
                short: SPECS[id.index()].short,
                group: group_label(SPECS[id.index()].group),
                priority: SPECS[id.index()].priority,
                stack: SPECS[id.index()].stack,
                cycles: w.cycles.load(Ordering::Relaxed),
                errors: errs,
                fail_text,
                stack_peak: STACK_PEAK[id.index()].load(Ordering::Relaxed),
            }
        })
        .collect();

    // 调度延迟直方图快照 (原子桶 → 数组)
    let delay_hist = {
        let mut h = [0u32; DELAY_BUCKETS];
        for (i, bucket) in DELAY_HIST.iter().enumerate() {
            h[i] = bucket.load(Ordering::Relaxed);
        }
        h
    };
    let delay_samples = DELAY_SAMPLES.load(Ordering::Relaxed);
    // 分位数取直方图桶下界, 可能高于实际最坏值; 钳制到最坏值,
    // 避免报告出现 "p99 > 最坏" 的自相矛盾
    let delay_max = DELAY_MAX.load(Ordering::Relaxed);

    // 调度饥饿 (测试覆盖不完整; 详情已在报告线程表标注)
    if spawned
        .iter()
        .any(|&id| WORKERS[id.index()].cycles.load(Ordering::Relaxed) == 0)
    {
        crate::log_warn!("[soak] 存在未获得执行的线程 (饥饿), 覆盖不完整, 见报告");
    }

    // 系统信息 + 调度统计 (报告"系统画像"区)
    let elapsed_secs = (elapsed / 1000).max(1);
    let ctx_switch_per_s = crate::rtos::context_switch_count() / elapsed_secs;
    let sys_note = alloc::format!(
        "HC32F460JEUA (Cortex-M4F @ {} MHz) · SRAM 192K · RTOS tick {} Hz · 优先级 {} · 固件 v{}",
        crate::clk::hclk_hz() / 1_000_000,
        crate::rtos::TICKS_PER_SEC,
        crate::rtos::PRIORITY_MAX,
        env!("CARGO_PKG_VERSION")
    );
    // 芯片唯一编号 (EFM UQID, 96 位): 报告与设备一一对应, 防止混淆
    let mut uid_buf = [0u8; crate::efm::UID_HEX_CAP];
    let uid = crate::efm::uid_hex(&mut uid_buf);
    // WDT 余量: 实测最大喂狗间隔 vs 硬件超时
    let wdt_feed_gap_ms = crate::board::wdt_feed_max_gap_ms();
    let wdt_timeout_ms = if crate::config::WDT_ENABLE {
        crate::wdt::DEFAULT
            .timeout_us(crate::clk::pclk3_hz())
            .unwrap_or(0) as u32
            / 1000
    } else {
        0
    };
    let heap_largest_free = crate::heap::largest_free_block();
    let pi_max_wait = PI_MAX_WAIT.load(Ordering::Relaxed);
    let pi_rounds = PI_ROUNDS.load(Ordering::Relaxed);

    let data = crate::soak_report_core::Data {
        pass,
        elapsed_ms: elapsed,
        stop_reason,
        rtc_stamp,
        start_uptime: start,
        sys_note: &sys_note,
        uid,
        ctx_switch_per_s,
        cpu_util_end: util_end,
        cpu_util_max: util_max,
        thread_base,
        thread_end,
        heap_base,
        peak_heap,
        heap_end,
        heap_capacity: crate::heap::capacity(),
        heap_largest_free,
        heap_largest_free_min: lfb_min,
        heap_alloc_failures: HEAP_ALLOC_FAILURES.load(Ordering::Relaxed),
        net_growth,
        end_samples,
        mtx_ok,
        pi_max_wait,
        pi_rounds,
        sram_errors,
        total_errors,
        spawned_count: spawned.len(),
        items: &items,
        delay_samples,
        delay_p50: delay_percentile(50).min(delay_max),
        delay_p90: delay_percentile(90).min(delay_max),
        delay_p99: delay_percentile(99).min(delay_max),
        delay_max,
        delay_hist: &delay_hist,
        wdt_enabled: crate::config::WDT_ENABLE,
        wdt_feed_gap_ms,
        wdt_timeout_ms,
        critical_us,
        critical_limit_us: crate::config::SOAK_MAX_CRITICAL_US,
        critical_flash_us,
        tick_latency_us,
        tick_latency_limit_us: crate::config::SOAK_MAX_IRQ_LATENCY_US,
        tick_latency_flash_us,
        latency_ok,
        rows: &rows,
    };
    let html = crate::soak_report_core::build(&data);
    let outcome = if pass { "PASS" } else { "FAIL" };

    // 报告写入失败/文件系统不可用时的紧凑控制台回退 (结果不丢失)
    let fallback_summary = |total_errors: u32, sram_errors: u32| {
        crate::println!(
            "[soak]   摘要: {} 压力线程, 总错误 {}, SRAM {}, 堆净增 {}B, 线程 {}→{}, 互斥量 {}, 实时性 {}",
            spawned.len(),
            total_errors,
            sram_errors,
            net_growth,
            thread_base,
            thread_end,
            if mtx_ok { "通过" } else { "失败" },
            if latency_ok { "通过" } else { "失败" }
        );
        crate::println!(
            "[soak]   实时性: 最长关中断 {}µs (限 {}µs), 节拍延迟 {}µs (限 {}µs)",
            critical_us,
            crate::config::SOAK_MAX_CRITICAL_US,
            tick_latency_us,
            crate::config::SOAK_MAX_IRQ_LATENCY_US
        );
    };
    if let Some(mut fs) = crate::filesystem::mounted() {
        match crate::soak_report::save(&mut fs, &html, &data) {
            Ok(path) => {
                crate::println!("\r[soak] 结果: {} — 报告: /{}", outcome, path);
            }
            Err(error) => {
                crate::println!(
                    "\r[soak] 结果: {} — 报告写入失败: {}",
                    outcome,
                    fs_error_summary(&error)
                );
                fallback_summary(total_errors, sram_errors);
            }
        }
    } else {
        crate::println!("\r[soak] 结果: {} — 文件系统不可用, 无法保存报告", outcome);
        fallback_summary(total_errors, sram_errors);
    }
}
