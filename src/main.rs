// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]

// 使用 Rust 堆数据结构 (Vec/Box/String 等), 分配器见 heap 模块
extern crate alloc;

mod clk;
mod console;
mod critical_section;
mod gpio;
mod heap;
mod icg;
mod panic;
mod rtos;
mod startup;
mod systick;
mod uart;
mod vector_table;

use core::sync::atomic::{AtomicU32, Ordering};
use gpio::{Config, Drive, Gpio, Level, Mode, Pin, PortA, PortC};
use rtos::{Event, EventOpt, Mailbox, MessageQueue, Mutex, Semaphore, Timeout};
use uart::{Uart1, UartConfig};

/// 全局堆分配器 (边界标记 + 首次适配, 见 heap 模块)
#[global_allocator]
static ALLOCATOR: heap::HeapAllocator = heap::HeapAllocator;

/// SysTick 中断频率 (Hz), 同时是 RTOS 的节拍频率
const SYSTICK_FREQ_HZ: u32 = 1000;

/// PC13 LED, const 构造 (引脚号在编译期校验)
const LED: Pin<PortC, 13> = Pin::new();

// ---- RTOS 演示对象 ----
/// 信号量: 生产者/消费者
static SEM: Semaphore = Semaphore::new(0, 1);
/// 事件: 发送/等待 (带超时)
static EVT: Event = Event::new();
/// 互斥量: 优先级继承演示
static MUT: Mutex = Mutex::new();
/// 邮箱: 机器字消息
static MB: Mailbox = Mailbox::new(4);
/// 消息队列: 变长消息
static MQ: MessageQueue = MessageQueue::new(32, 4);
/// 周期定时器触发计数
static TIMER_COUNT: AtomicU32 = AtomicU32::new(0);

/// SysTick 中断服务函数: 驱动 RTOS 时钟节拍
///
/// 由向量表 [`vector_table::EXCEPTIONS`] 的 SysTick 槽位 (异常 15) 指向。
/// 节拍驱动: 节拍递增 → 时间片轮转 → 定时器检查 → 调度。
#[unsafe(no_mangle)]
pub extern "C" fn sys_tick_handler() {
    rtos::tick_increase();
    // Arm Errata 838869: ISR 末尾加 DSB, 确保中断唤醒低功耗模式的行为可靠
    unsafe {
        core::arch::asm!("dsb sy");
    }
}

/// 应用入口: 由 [`startup::reset_handler`] 在完成硬件与内存初始化后调用
pub(crate) fn main() -> ! {
    // 硬件初始化 (时钟 200MHz / LED / UART 引脚 / SysTick / USART1)
    let _uart = hardware_init();
    report_startup();

    // RTOS 初始化: 中断优先级 + 空闲线程
    rtos::init();

    // ---- 创建演示线程 ----
    // 线程优先级: 0 最高, 31 最低 (空闲); 时间片单位 = 节拍 (1ms)
    // 栈大小: 需兼顾 debug 构建 (格式化打印的栈消耗远大于 release)
    rtos::thread_create("led", 2048, 2, 10, led_thread, 0);
    rtos::thread_create("report", 2048, 10, 10, report_thread, 0);
    rtos::thread_create("mtx-high", 2048, 5, 10, mutex_high, 0);
    rtos::thread_create("mtx-low", 2048, 25, 10, mutex_low, 0);
    rtos::thread_create("mq-send", 2048, 8, 10, mq_sender, 0);
    rtos::thread_create("mq-recv", 2048, 8, 10, mq_receiver, 0);
    rtos::thread_create("mb-send", 2048, 12, 10, mb_sender, 0);
    rtos::thread_create("mb-recv", 2048, 12, 10, mb_receiver, 0);
    rtos::thread_create("sem-prod", 1024, 15, 10, sem_producer, 0);
    rtos::thread_create("sem-cons", 2048, 15, 10, sem_consumer, 0);
    rtos::thread_create("evt-send", 2048, 20, 10, event_sender, 0);
    rtos::thread_create("evt-wait", 2048, 20, 10, event_waiter, 0);
    // 同优先级 + 有限时间片 → 演示时间片轮转
    rtos::thread_create("rr-a", 2048, 30, 5, rr_thread, b'a' as usize);
    rtos::thread_create("rr-b", 2048, 30, 5, rr_thread, b'b' as usize);
    // 线程删除 / 僵尸回收演示
    rtos::thread_create("worker", 2048, 18, 10, worker_thread, 0);

    // 周期定时器 (回调在中断上下文执行)
    static TIMER: rtos::Timer = rtos::Timer::new();
    TIMER.start(2000, 2000, timer_cb, 0);

    // 启动调度器, 永不返回
    rtos::start();
}

// ---- 演示线程 ----

/// LED 线程: 每 500ms 翻转一次 (由线程调度而非中断分频)
extern "C" fn led_thread(_param: usize) {
    loop {
        LED.toggle();
        rtos::thread_delay_ms(500);
    }
}

/// 报告线程: 每秒打印运行时间与定时器计数
extern "C" fn report_thread(_param: usize) {
    loop {
        rtos::thread_delay_ms(1000);
        println!(
            "[report] up = {} ms, timer fires = {}",
            rtos::uptime_ms(),
            TIMER_COUNT.load(Ordering::Relaxed)
        );
    }
}

/// 周期定时器回调 (中断上下文): 仅做计数, 不调用阻塞 API
extern "C" fn timer_cb(_param: usize) {
    TIMER_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// 信号量生产者: 每 2 秒释放一次
extern "C" fn sem_producer(_param: usize) {
    loop {
        rtos::thread_delay_ms(2000);
        SEM.release();
    }
}

/// 信号量消费者: 获取到信号量后打印
extern "C" fn sem_consumer(_param: usize) {
    loop {
        SEM.take(Timeout::Forever).expect("sem take");
        println!("[sem] consumer got the semaphore");
    }
}

/// 事件发送者: 每 5 秒发送事件位 0x01
extern "C" fn event_sender(_param: usize) {
    loop {
        rtos::thread_delay_ms(5000);
        EVT.send(0x01);
        println!("[event] sent 0x01");
    }
}

/// 事件等待者: OR 模式 + 3 秒超时, 交替演示"收到/超时"
extern "C" fn event_waiter(_param: usize) {
    loop {
        match EVT.recv(0x01, EventOpt::OrClear, Timeout::Ticks(3000)) {
            Ok(bits) => println!("[event] waiter got bits = 0x{:08x}", bits),
            Err(rtos::Error::TimedOut) => println!("[event] waiter timed out"),
            Err(_) => {}
        }
    }
}

/// 互斥量演示 — 高优先级线程: 阻塞等待低优先级线程释放互斥量
///
/// 优先级继承: mtx-low 持有互斥量期间被提升到本线程的优先级,
/// 不会被中间优先级线程插队, 从而在 2 秒内获得锁。
extern "C" fn mutex_high(_param: usize) {
    loop {
        MUT.lock(Timeout::Forever).expect("mutex lock");
        println!("[mutex] high: acquired (mtx-low was boosted via priority inheritance)");
        MUT.unlock();
        rtos::thread_delay_ms(7000);
    }
}

/// 互斥量演示 — 低优先级线程: 持有互斥量 2 秒
extern "C" fn mutex_low(_param: usize) {
    loop {
        rtos::thread_delay_ms(1000);
        MUT.lock(Timeout::Forever).expect("mutex lock");
        println!("[mutex] low: holding mutex for 2s...");
        rtos::thread_delay_ms(2000);
        MUT.unlock();
        println!("[mutex] low: released");
    }
}

/// 邮箱发送者: 每 1.5 秒发送一个自增计数
extern "C" fn mb_sender(_param: usize) {
    let mut n: usize = 0;
    loop {
        rtos::thread_delay_ms(1500);
        MB.send(n, Timeout::Forever).expect("mb send");
        n = n.wrapping_add(1);
    }
}

/// 邮箱接收者
extern "C" fn mb_receiver(_param: usize) {
    loop {
        if let Ok(msg) = MB.recv(Timeout::Forever) {
            println!("[mb] recv = {}", msg);
        }
    }
}

/// 消息队列发送者: 每 1 秒发送一条文本消息
extern "C" fn mq_sender(_param: usize) {
    let mut n: u32 = 0;
    loop {
        rtos::thread_delay_ms(1000);
        let msg = alloc::format!("mq message #{}", n);
        MQ.send(msg.as_bytes(), Timeout::Forever).expect("mq send");
        n = n.wrapping_add(1);
    }
}

/// 消息队列接收者
extern "C" fn mq_receiver(_param: usize) {
    let mut buf = [0u8; 32];
    loop {
        if let Ok(len) = MQ.recv(&mut buf, Timeout::Forever) {
            let text = core::str::from_utf8(&buf[..len]).unwrap_or("<bad utf8>");
            println!("[mq] recv {} bytes: {:?}", len, text);
        }
    }
}

/// 时间片轮转演示: 两个同优先级线程交替输出 'a'/'b'
extern "C" fn rr_thread(param: usize) {
    let ch = param as u8 as char;
    loop {
        println!("[rr] {}", ch);
        // 主动让出: 同优先级队尾轮转 (配合 5 tick 时间片)
        rtos::yield_now();
        for _ in 0..800_000 {
            unsafe { core::arch::asm!("nop") };
        }
    }
}

/// 线程生命周期演示: 创建临时线程并删除, 最后由入口返回退出
///
/// - 删除其他线程: `Thread::delete` (临时线程进入僵尸队列, 空闲线程回收);
/// - 删除自身: 入口函数 return → 硬件跳入线程退出函数 (thread_exit 机制)。
extern "C" fn worker_thread(_param: usize) {
    for i in 0..5 {
        rtos::thread_delay_ms(1000);
        println!("[worker] tick {}", i);
    }
    // 创建并删除一个临时线程 (演示 Thread 句柄 + 生命周期 API)
    let temp = rtos::thread_create("temp", 1024, 30, 10, temp_thread, 0);
    println!("[worker] created '{}' (priority {})", temp.name(), temp.priority());
    // 挂起 → 恢复 → 删除 (Thread::suspend / resume / delete)
    temp.suspend().unwrap();
    println!("[worker] '{}' suspended", temp.name());
    rtos::thread_delay_ms(500);
    temp.resume().unwrap();
    println!("[worker] '{}' resumed", temp.name());
    rtos::thread_delay_ms(500);
    temp.delete();
    println!("[worker] deleted; continuing...");
    rtos::thread_delay_ms(2000);
    println!("[worker] exiting via thread_exit (stack will be reclaimed by idle)");
    // return → thread_exit → defunct 队列 → 空闲线程回收栈与 TCB
}

/// 临时线程: 空转循环 (保持就绪态, 供 worker 演示挂起/恢复/删除)
extern "C" fn temp_thread(_param: usize) {
    #[allow(clippy::empty_loop)] // 有意空转: 保持就绪态便于挂起/恢复/删除演示
    loop {}
}

// ---- 原有硬件初始化 (保持不变) ----

/// 硬件初始化: 时钟 → GPIO (LED + UART 引脚) → SysTick → USART1
fn hardware_init() -> Uart1 {
    // 时钟初始化: 外部晶振 + MPLL → 200MHz (失败自动回退 MRC)
    let _ = clk::init(clk::ClockSource::Pll200);

    // GPIO
    let gpio = Gpio::take();
    gpio.pin::<PortC, 13>().configure(Config {
        mode: Mode::Output,
        pull_up: false,
        drive: Drive::Low,
        initial_level: Level::High,
    });
    // UART 引脚复用: PA9=USART1_TX (FSEL 32), PA10=USART1_RX (FSEL 33)
    gpio.pin::<PortA, 9>().set_func(32);
    gpio.pin::<PortA, 10>().set_func(33);

    // SysTick (RTOS 节拍源)
    systick::init(SYSTICK_FREQ_HZ).expect("SysTick config failed!");

    // USART1 (console 绑定目标): 115200, 8N1
    let uart = Uart1::take();
    uart.init(UartConfig::default()).expect("UART init failed!");
    uart
}

/// 启动报告: 输出通道就绪后打印时钟配置结果
fn report_startup() {
    println!("HC32F460 RTOS demo (RT-Thread architecture ported to Rust)");
    println!("console uart = {}", stringify!(Uart1));
    match clk::xtal_status() {
        clk::XtalStatus::Active => {
            println!("XTAL active: system clock = {} Hz", clk::system_clock_hz());
        }
        clk::XtalStatus::Failed => {
            println!(
                "WARNING: XTAL init failed! fallback MRC, system clock = {} Hz",
                clk::system_clock_hz()
            );
        }
        clk::XtalStatus::NotAttempted => {
            println!(
                "XTAL not attempted: system clock = {} Hz",
                clk::system_clock_hz()
            );
        }
    }
}
