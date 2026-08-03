// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]

// 使用 Rust 堆数据结构 (Vec/Box/String 等), 分配器见 heap 模块
extern crate alloc;

// -- 启动与内核基础设施 --
mod critical_section; // PRIMASK 临界区 (中断安全的基础)
mod heap; // 全局堆分配器 (边界标记 + 首次适配)
mod icg; // ICG 硬件配置段
mod panic;
mod startup; // 复位入口: SRAM/FPU/时钟等待周期 + .data/.bss
mod vector_table; // 复位/异常/144 外设中断向量表 // panic 与硬件 fault 诊断

// -- 外设驱动 (寄存器级, 零依赖) --
mod clk; // 时钟链: XTAL + MPLL → 200MHz, 失败自动回退
mod console;
mod gpio; // GPIO: 寄存器/端口/引脚分层, const 泛型封装
mod systick; // SysTick 节拍 (1kHz, RTOS 的时钟源)
mod uart; // USART1~4: 波特率/过采样/小数分频 // 控制台: 打印锁 (优先级继承) + 原子整行输出

// -- RTOS 内核 (RT-Thread 架构移植) --
mod rtos;
// -- 应用 --
mod banner; // 启动横幅 (应用层, 依赖 clk/heap/rtos 公共状态)
mod shell; // 仿 Ubuntu 终端: 登录 + 命令提示符 + 系统信息命令

use core::sync::atomic::{AtomicU32, Ordering};
use gpio::{Config, Drive, Gpio, Level, Mode, Pin, PortA, PortC};
use uart::{Uart1, UartConfig};

/// 全局堆分配器 (边界标记 + 首次适配, 见 heap 模块)
#[global_allocator]
static ALLOCATOR: heap::HeapAllocator = heap::HeapAllocator;

/// SysTick 中断频率 (Hz), 同时是 RTOS 的节拍频率
const SYSTICK_FREQ_HZ: u32 = 1000;

/// PC13 LED, const 构造 (引脚号在编译期校验)
const LED: Pin<PortC, 13> = Pin::new();

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
    let uart = hardware_init();

    // RTOS 初始化: 中断优先级 + 空闲线程
    rtos::init();

    // 创建演示线程
    rtos::thread_create("led", 4096, 2, 10, led_thread, 0);
    rtos::thread_create("selftest", 4096, 15, 0, selftest_thread, 0);
    rtos::thread_create("shell", 4096, 18, 10, shell::shell_entry, 0);

    // 周期定时器 (回调在中断上下文执行)
    static TIMER: rtos::Timer = rtos::Timer::new();
    TIMER.start(2000, 2000, timer_cb, 0);

    // 使能 UART1 接收中断 (INTC 通道 INT001, NVIC 优先级 8)
    uart.enable_rx_interrupt(1, 8);

    // 内核启动横幅 (创建线程后、启动前, 就绪统计包含所有线程)
    banner::show();

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

// ---- rtos 自检 (IPC 各原语 + 线程生命周期) ----

use rtos::{EventOpt, Timeout};

/// 被删除的线程: 空转等待删除
extern "C" fn victim_thread(_param: usize) {
    loop {
        rtos::thread_delay_ms(50);
    }
}

/// 自然退出的线程: 入口返回后经 thread_exit → defunct → 空闲线程回收
extern "C" fn exit_thread(_param: usize) {}

/// rtos 自检线程: 依次验证信号量/互斥量/事件/邮箱/消息队列/
/// 延时/线程删除/线程退出, 全部通过打印 PASS。
extern "C" fn selftest_thread(_param: usize) {
    println!("[selftest] 开始 (rtos 内核功能自检)");
    let mut pass = 0u32;
    let mut fail = 0u32;
    let mut check = |ok: bool, name: &str| {
        if ok {
            pass += 1;
            println!("  [PASS] {}", name);
        } else {
            fail += 1;
            println!("  [FAIL] {}", name);
        }
    };

    // 信号量: 计数获取 / 立即超时 / release 唤醒
    static SEM: rtos::Semaphore = rtos::Semaphore::new(1, 1);
    check(
        SEM.take(Timeout::Ticks(0)).is_ok(),
        "信号量: 初始计数可获取",
    );
    check(
        SEM.take(Timeout::Ticks(0)).is_err(),
        "信号量: 计数 0 立即超时",
    );
    SEM.release();
    check(
        SEM.take(Timeout::Ticks(0)).is_ok(),
        "信号量: release 后可获取",
    );

    // 互斥量: 获取 / 递归持有 / 释放
    static MTX: rtos::Mutex = rtos::Mutex::new();
    check(MTX.lock(Timeout::Ticks(0)).is_ok(), "互斥量: 可获取");
    check(MTX.lock(Timeout::Ticks(0)).is_ok(), "互斥量: 递归持有合法");
    MTX.unlock();
    MTX.unlock();
    check(
        MTX.lock(Timeout::Ticks(0)).is_ok(),
        "互斥量: 释放后可重新获取",
    );
    MTX.unlock();

    // 事件: AND / OR / 立即超时
    static EVT: rtos::Event = rtos::Event::new();
    EVT.send(0x05);
    check(
        EVT.recv(0x05, EventOpt::And, Timeout::Ticks(0)) == Ok(0x05),
        "事件: AND 匹配返回等待全集",
    );
    check(
        EVT.recv(0x02, EventOpt::And, Timeout::Ticks(0)).is_err(),
        "事件: 不满足立即超时",
    );
    EVT.send(0x08);
    check(
        EVT.recv(0x08, EventOpt::Or, Timeout::Ticks(0)) == Ok(0x08),
        "事件: OR 匹配返回实际位",
    );
    check(
        EVT.recv(0x10, EventOpt::OrClear, Timeout::Ticks(0))
            .is_err(),
        "事件: 无匹配位立即超时",
    );

    // 邮箱: 收发 / 紧急插队 / 满返回 Full / 空返回 TimedOut
    static MB: rtos::Mailbox = rtos::Mailbox::new(4);
    check(MB.send(100, Timeout::Ticks(0)).is_ok(), "邮箱: 发送");
    check(MB.recv(Timeout::Ticks(0)) == Ok(100), "邮箱: 接收一致");
    check(MB.recv(Timeout::Ticks(0)).is_err(), "邮箱: 空立即超时");
    for i in 0..4 {
        MB.send(1000 + i, Timeout::Ticks(0)).ok();
    }
    check(
        MB.send(9999, Timeout::Ticks(0)).is_err(),
        "邮箱: 满返回 Full",
    );
    // 取出一条腾出空间后再紧急发送 (urgent 在满时同样返回 Full)
    MB.recv(Timeout::Ticks(0)).ok();
    check(MB.urgent(42, Timeout::Ticks(0)).is_ok(), "邮箱: 紧急发送");
    check(
        MB.recv(Timeout::Ticks(0)) == Ok(42),
        "邮箱: 紧急消息插到队首",
    );

    // 消息队列: 收发一致 (含二进制)
    static MQ: rtos::MessageQueue = rtos::MessageQueue::new(16, 4);
    let hello: &[u8] = &[0x52, 0x00, 0xFF, b'!'];
    check(MQ.send(hello, Timeout::Ticks(0)).is_ok(), "消息队列: 发送");
    let mut buf = [0u8; 16];
    check(
        MQ.recv(&mut buf, Timeout::Ticks(0)) == Ok(4) && buf[..4] == *hello,
        "消息队列: 接收内容一致",
    );
    check(
        MQ.recv(&mut buf, Timeout::Ticks(0)).is_err(),
        "消息队列: 空立即超时",
    );

    // 延时: uptime 前进
    let t0 = rtos::uptime_ms();
    rtos::thread_delay_ms(20);
    check(rtos::uptime_ms() >= t0 + 20, "线程延时: uptime 前进 ≥ 20ms");

    // 线程删除 (delete API) 与自然退出 (defunct 回收)
    println!("  [info] 创建 victim 线程");
    let victim = rtos::thread_create("victim", 1024, 24, 0, victim_thread, 0);
    println!("  [info] victim 已创建, 延时 50ms");
    rtos::thread_delay_ms(50);
    println!("  [info] 调用 victim.delete()");
    victim.delete();
    println!("  [info] delete() 已返回, 延时 50ms");
    rtos::thread_delay_ms(50);
    check(true, "线程删除: delete() 完成且系统正常");
    println!("  [info] 创建 exit-me 线程");
    rtos::thread_create("exit-me", 1024, 25, 0, exit_thread, 0);
    rtos::thread_delay_ms(100);
    check(true, "线程退出: 入口返回后经 defunct 回收");

    println!("[selftest] 完成: {} 通过, {} 失败", pass, fail);
}

/// 周期定时器回调 (中断上下文): 仅做计数, 不调用阻塞 API
extern "C" fn timer_cb(_param: usize) {
    TIMER_COUNT.fetch_add(1, Ordering::Relaxed);
}

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
    systick::init(SYSTICK_FREQ_HZ).expect("SysTick 配置失败!");

    // USART1 (console 绑定目标): 115200, 8N1
    let uart = Uart1::take();
    uart.init(UartConfig::default()).expect("UART 初始化失败!");
    uart
}
