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
mod log; // 应用日志: 可开关+分级+彩色, 与内核打印分离 (见 log.rs)
mod gpio; // GPIO: 寄存器/端口/引脚分层, const 泛型封装
mod systick; // SysTick 节拍 (1kHz, RTOS 的时钟源)
mod uart; // USART1~4: 波特率/过采样/小数分频 // 控制台: 打印锁 (优先级继承) + 原子整行输出

// -- RTOS 内核 (RT-Thread 架构移植) --
mod rtos;
// -- 应用 --
mod banner; // 启动横幅 (应用层, 依赖 clk/heap/rtos 公共状态)
mod shell; // 仿 Ubuntu 终端: 登录 + 命令提示符 + 系统信息命令
mod config; // 编译期配置 (.cargo/config.toml [env] → env!)

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use gpio::{Config, Drive, Gpio, Mode, Pin, PortA, PortC};
use uart::UartConfig;

/// 全局堆分配器 (边界标记 + 首次适配, 见 heap 模块)
#[global_allocator]
static ALLOCATOR: heap::HeapAllocator = heap::HeapAllocator;

/// SysTick 中断频率 (Hz), 同时是 RTOS 的节拍频率 (.cargo/config.toml)
const SYSTICK_FREQ_HZ: u32 = config::SYSTICK_FREQ_HZ;

/// 板载 LED (PC13): 端口由类型系统固定, 引脚号来自配置, 存在性编译期校验
const LED: Pin<PortC, { config::LED_PIN }> = Pin::new();

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

    // 创建演示线程 (栈/优先级/时间片来自 .cargo/config.toml)
    // 注: selftest 线程不在此创建, 由 shell 命令 `selftest` 手动启动
    // (见 start_selftest, 受 CFG_APP_SELFTEST_ENABLE 控制)
    rtos::thread_create(
        "led",
        config::APP_LED_STACK,
        config::APP_LED_PRIORITY,
        config::APP_LED_TIMESLICE,
        led_thread,
        0,
    );
    rtos::thread_create(
        "shell",
        config::APP_SHELL_STACK,
        config::APP_SHELL_PRIORITY,
        config::APP_SHELL_TIMESLICE,
        shell::shell_entry,
        0,
    );

    // 周期定时器 (回调在中断上下文执行)
    static TIMER: rtos::Timer = rtos::Timer::new();
    TIMER.start(
        config::APP_TIMER_PERIOD_MS,
        config::APP_TIMER_PERIOD_MS,
        timer_cb,
        0,
    );

    // 使能控制台 UART 接收中断 (INTC 通道/NVIC 优先级来自 .cargo/config.toml)
    uart.enable_rx_interrupt(config::UART_RX_IRQ_CHANNEL, config::UART_RX_IRQ_PRIORITY);

    // 内核启动横幅 (创建线程后、启动前, 就绪统计包含所有线程;
    // 开头先清屏, 使每次启动与上次输出明确分隔)
    banner::show();

    // 应用日志 (与内核打印分离): 输出与否由 CFG_LOG_ENABLE / CFG_LOG_LEVEL
    // 决定, 运行时可经 shell `log` 命令切换
    log_info!("系统启动: {} @ {} MHz", config::CORE, clk::system_clock_hz() / 1_000_000);

    // 启动调度器, 永不返回
    rtos::start();
}

// ---- 演示线程 ----

/// LED 线程: 每 500ms 翻转一次 (周期来自配置, 由线程调度而非中断分频)
extern "C" fn led_thread(_param: usize) {
    loop {
        LED.toggle();
        // debug 级: 默认阈值 (info) 不输出, 可经 `log level debug` 打开
        log_debug!("LED 翻转, uptime = {} ms", rtos::uptime_ms());
        rtos::thread_delay_ms(config::APP_LED_BLINK_MS);
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

/// 内核自检线程是否正在运行 (防止 `selftest` 命令重复启动)
static SELFTEST_RUNNING: AtomicBool = AtomicBool::new(false);

/// 启动内核自检线程 (由 shell 命令 `selftest` 调用)
///
/// 受 `CFG_APP_SELFTEST_ENABLE` 控制 (见 shell::cmd_selftest),
/// 线程运行结束后可再次启动。
pub(crate) fn start_selftest() {
    if SELFTEST_RUNNING.swap(true, Ordering::SeqCst) {
        println!("[selftest] 已在运行");
        return;
    }
    rtos::thread_create(
        "selftest",
        config::APP_SELFTEST_STACK,
        config::APP_SELFTEST_PRIORITY,
        0,
        selftest_thread,
        0,
    );
}

/// rtos 自检线程: 依次验证信号量/互斥量/事件/邮箱/消息队列/
/// 延时/线程删除/线程退出。
///
/// 逐项结果走**应用日志** (info 级, 可经 `log` 命令控制/关闭);
/// 汇总一行始终打印 (命令的执行结果, 不受日志开关影响)。
extern "C" fn selftest_thread(_param: usize) {
    log_info!("[selftest] 开始 (rtos 内核功能自检)");
    let mut pass = 0u32;
    let mut fail = 0u32;
    let mut check = |ok: bool, name: &str| {
        if ok {
            pass += 1;
            log_info!("[PASS] {}", name);
        } else {
            fail += 1;
            log_info!("[FAIL] {}", name);
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
    log_debug!("[selftest] 创建 victim 线程");
    let victim = rtos::thread_create("victim", 1024, 24, 0, victim_thread, 0);
    log_debug!("[selftest] victim 已创建, 延时 50ms");
    rtos::thread_delay_ms(50);
    log_debug!("[selftest] 调用 victim.delete()");
    victim.delete();
    log_debug!("[selftest] delete() 已返回, 延时 50ms");
    rtos::thread_delay_ms(50);
    check(true, "线程删除: delete() 完成且系统正常");
    log_debug!("[selftest] 创建 exit-me 线程");
    rtos::thread_create("exit-me", 1024, 25, 0, exit_thread, 0);
    rtos::thread_delay_ms(100);
    check(true, "线程退出: 入口返回后经 defunct 回收");

    // 汇总始终打印 (内核打印, 不受日志开关影响)
    println!("[selftest] 完成: {} 通过, {} 失败", pass, fail);
    // 允许再次通过 `selftest` 命令启动
    SELFTEST_RUNNING.store(false, Ordering::SeqCst);
}

/// 周期定时器回调 (中断上下文): 仅做计数, 不调用阻塞 API
extern "C" fn timer_cb(_param: usize) {
    TIMER_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// 硬件初始化: 时钟 → GPIO (LED + UART 引脚) → SysTick → 控制台 USART
///
/// 各参数 (时钟源/引脚/波特率/节拍频率等) 均来自 .cargo/config.toml。
fn hardware_init() -> config::ConsoleUart {
    // 时钟初始化 (时钟源来自配置, 失败自动回退)
    let _ = clk::init(config::CLOCK_SOURCE);
    log_debug!(
        "时钟: {} Hz (源 {:?})",
        clk::system_clock_hz(),
        config::CLOCK_SOURCE
    );

    // GPIO
    let gpio = Gpio::take();
    gpio.pin::<PortC, { config::LED_PIN }>()
        .configure(Config {
            mode: Mode::Output,
            pull_up: false,
            drive: Drive::Low,
            initial_level: config::LED_INITIAL_LEVEL,
            invert: false,
        });
    // UART 引脚复用 (PA9=TX / PA10=RX; 引脚号/功能号来自配置)
    gpio.pin::<PortA, { config::UART_TX_PIN }>()
        .set_func(config::UART_TX_FSEL);
    gpio.pin::<PortA, { config::UART_RX_PIN }>()
        .set_func(config::UART_RX_FSEL);

    // SysTick (RTOS 节拍源)
    systick::init(SYSTICK_FREQ_HZ).expect("SysTick 配置失败!");
    log_debug!("SysTick: {} Hz", SYSTICK_FREQ_HZ);

    // 控制台 USART (波特率/数据位/校验/停止位/流控等来自配置)
    let uart = config::ConsoleUart::take();
    uart.init(UartConfig {
        baudrate: config::UART_BAUDRATE,
        oversample: config::UART_OVERSAMPLE,
        clock_div: config::UART_CLOCK_DIV,
        data_bits: config::UART_DATA_BITS,
        parity: config::UART_PARITY,
        stop_bits: config::UART_STOP_BITS,
        first_bit: config::UART_FIRST_BIT,
        flow_control: config::UART_FLOW_CTRL,
        noise_filter: config::UART_NOISE_FILTER,
    })
    .expect("UART 初始化失败!");
    log_debug!(
        "控制台 UART: USART{} {} bps (过采样 {:?}, 分频 {:?})",
        config::UART_UNIT,
        config::UART_BAUDRATE,
        config::UART_OVERSAMPLE,
        config::UART_CLOCK_DIV
    );
    uart
}
