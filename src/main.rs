// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]

// 使用 Rust 堆数据结构 (Vec/Box/String 等), 分配器见 heap 模块
extern crate alloc;

// -- 启动与内核基础设施 --
mod crc; // CRC 硬件加速器: CRC16/32 (X25/CCITT/IEEE), 累加模式
mod rtc; // 实时时钟 (RTC): LRC 源/时间日期/闹钟, 日志时间戳
mod critical_section; // PRIMASK 临界区 (中断安全的基础)
mod efm; // 片内 Flash (EFM): 扇区擦除/字编程/读等待周期/UID
mod heap; // 全局堆分配器 (边界标记 + 首次适配)
mod icg; // ICG 硬件配置段
mod intc; // 中断控制器: 事件源→SEL→NVIC 路由 + 注册 API
mod panic;
mod sram; // 片内 SRAM (SRAMC): 等待周期/奇偶·ECC 错误检测
mod startup; // 复位入口: SRAM/FPU/时钟等待周期 + .data/.bss
mod vector_table; // 复位/异常/144 外设中断向量表 // panic 与硬件 fault 诊断

// -- 外设驱动 (寄存器级, 零依赖) --
mod clk; // 时钟链: XTAL + MPLL → 200MHz, 失败自动回退
mod console;
mod gpio; // GPIO: 寄存器/端口/引脚分层, const 泛型封装
mod log; // 应用日志: 可开关+分级+彩色, 与内核打印分离 (见 log.rs)
mod systick; // SysTick 节拍 (1kHz, RTOS 的时钟源)
mod uart; // USART1~4: 波特率/过采样/小数分频 // 控制台: 打印锁 (优先级继承) + 原子整行输出

// -- RTOS 内核 (RT-Thread 架构移植) --
mod rtos;
// -- 应用 --
mod banner; // 启动横幅 (应用层, 依赖 clk/heap/rtos 公共状态)
mod config;
mod shell; // 仿 Ubuntu 终端: 登录 + 命令提示符 + 系统信息命令 // 编译期配置 (.cargo/config.toml [env] → env!)

use core::sync::atomic::{AtomicU32, Ordering};
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
    // 注: selftest 不在此运行, 由 shell 命令 `selftest` 同步执行
    // (见 selftest_run, 受 CFG_APP_SELFTEST_ENABLE 控制)
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

    // 使能控制台 UART 接收中断 (NVIC 线/优先级来自 .cargo/config.toml)
    uart.enable_rx_interrupt(
        intc::Line::new(config::UART_RX_IRQ_CHANNEL as u8),
        config::UART_RX_IRQ_PRIORITY,
    );

    // 内核启动横幅 (创建线程后、启动前, 就绪统计包含所有线程;
    // 开头先清屏, 使每次启动与上次输出明确分隔)
    banner::show();

    // 应用日志 (与内核打印分离): 输出与否由 CFG_LOG_ENABLE / CFG_LOG_LEVEL
    // 决定, 运行时可经 shell `log` 命令切换
    log_info!(
        "系统启动: {} @ {} MHz",
        config::CORE,
        clk::system_clock_hz() / 1_000_000
    );

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

/// 阻塞发送线程: 向容量 2 的邮箱连发 3 条 (第 3 条在满时阻塞,
/// 由接收者取走消息后唤醒) —— 回归"唤醒不重试丢消息"缺陷
static BLK_MB: rtos::Mailbox = rtos::Mailbox::new(2);

extern "C" fn blk_sender(_param: usize) {
    for i in 0..3 {
        let r = BLK_MB.send(1000 + i, Timeout::Forever);
        log_trace!("[selftest] 阻塞发送 {}: {:?}", i, r);
    }
}

/// 检测用户是否按下 ESC (0x1B): 轮询并清空接收缓冲
///
/// 自检期间终端输入一律丢弃 (ESC 除外); 返回 true 表示请求中断。
fn selftest_abort_requested() -> bool {
    let uart = config::ConsoleUart::take();
    let mut esc = false;
    while let Some(b) = uart.read_rx() {
        if b == 0x1B {
            esc = true;
        }
    }
    esc
}

/// 内核自检: 依次验证信号量/互斥量/事件/邮箱/消息队列/延时/
/// 线程删除/线程退出/Flash。
///
/// **同步执行** (由 shell 的 `selftest` 命令调用, 完成后才出下一提示符);
/// 每项检查后轮询 ESC, 按下即中断剩余项。
///
/// 日志分级: **trace** 级输出每项执行细节 (实际返回值/耗时等,
/// `log level trace` 打开), **info** 级输出 PASS/FAIL 结果;
/// 汇总一行始终打印 (命令的执行结果, 不受日志开关影响)。
pub(crate) fn selftest_run() {
    log_info!("[selftest] 开始 (rtos 内核功能自检), 按 ESC 可中断");
    let mut pass = 0u32;
    let mut fail = 0u32;
    let aborted = core::cell::Cell::new(false);
    let mut check = |ok: bool, name: &str, detail: core::fmt::Arguments<'_>| {
        if aborted.get() {
            return; // 已中断: 跳过剩余项
        }
        // trace 级: 执行细节 (实际返回值/参数), 用于故障定位
        log_trace!("[selftest] {} → {}", name, detail);
        if ok {
            pass += 1;
            log_info!("[PASS] {}", name);
        } else {
            fail += 1;
            log_info!("[FAIL] {}", name);
        }
        // 每项后检查 ESC (中断剩余项)
        if selftest_abort_requested() {
            aborted.set(true);
            log_info!("[selftest] 收到 ESC, 中断剩余项");
        }
    };

    // 信号量: 计数获取 / 立即超时 / release 唤醒
    // (测试对象用局部变量: 每次运行全新状态, 不依赖 static 持久化)
    let sem = rtos::Semaphore::new(1, 1);
    let r = sem.take(Timeout::Ticks(0));
    check(
        r.is_ok(),
        "信号量: 初始计数可获取",
        format_args!("take(0) = {:?}", r),
    );
    let r = sem.take(Timeout::Ticks(0));
    check(
        r.is_err(),
        "信号量: 计数 0 立即超时",
        format_args!("take(0) = {:?}", r),
    );
    sem.release();
    let r = sem.take(Timeout::Ticks(0));
    check(
        r.is_ok(),
        "信号量: release 后可获取",
        format_args!("release() 后 take(0) = {:?}", r),
    );

    // 互斥量: 获取 / 递归持有 / 释放
    let mtx = rtos::Mutex::new();
    let r = mtx.lock(Timeout::Ticks(0));
    check(
        r.is_ok(),
        "互斥量: 可获取",
        format_args!("lock(0) = {:?}", r),
    );
    let r = mtx.lock(Timeout::Ticks(0));
    check(
        r.is_ok(),
        "互斥量: 递归持有合法",
        format_args!("递归 lock(0) = {:?}", r),
    );
    mtx.unlock();
    mtx.unlock();
    let r = mtx.lock(Timeout::Ticks(0));
    check(
        r.is_ok(),
        "互斥量: 释放后可重新获取",
        format_args!("unlock×2 后 lock(0) = {:?}", r),
    );
    mtx.unlock();

    // 事件: AND / OR / 立即超时
    let evt = rtos::Event::new();
    evt.send(0x05);
    let r = evt.recv(0x05, EventOpt::And, Timeout::Ticks(0));
    check(
        r == Ok(0x05),
        "事件: AND 匹配返回等待全集",
        format_args!("send(0x05) recv(0x05, And) = {:?}", r),
    );
    let r = evt.recv(0x02, EventOpt::And, Timeout::Ticks(0));
    check(
        r.is_err(),
        "事件: 不满足立即超时",
        format_args!("recv(0x02, And) = {:?}", r),
    );
    evt.send(0x08);
    let r = evt.recv(0x08, EventOpt::Or, Timeout::Ticks(0));
    check(
        r == Ok(0x08),
        "事件: OR 匹配返回实际位",
        format_args!("send(0x08) recv(0x08, Or) = {:?}", r),
    );
    let r = evt.recv(0x10, EventOpt::OrClear, Timeout::Ticks(0));
    check(
        r.is_err(),
        "事件: 无匹配位立即超时",
        format_args!("recv(0x10, OrClear) = {:?}", r),
    );

    // 邮箱: 收发 / 紧急插队 / 满返回 Full / 空返回 TimedOut
    let mb = rtos::Mailbox::new(4);
    let r = mb.send(100, Timeout::Ticks(0));
    check(r.is_ok(), "邮箱: 发送", format_args!("send(100) = {:?}", r));
    let r = mb.recv(Timeout::Ticks(0));
    check(
        r == Ok(100),
        "邮箱: 接收一致",
        format_args!("recv() = {:?}", r),
    );
    let r = mb.recv(Timeout::Ticks(0));
    check(
        r.is_err(),
        "邮箱: 空立即超时",
        format_args!("recv() = {:?}", r),
    );
    for i in 0..4 {
        mb.send(1000 + i, Timeout::Ticks(0)).ok();
    }
    let r = mb.send(9999, Timeout::Ticks(0));
    check(
        r.is_err(),
        "邮箱: 满返回 Full",
        format_args!("满 4 条后 send(9999) = {:?}", r),
    );
    // 取出一条腾出空间后再紧急发送 (urgent 在满时同样返回 Full)
    mb.recv(Timeout::Ticks(0)).ok();
    let r = mb.urgent(42, Timeout::Ticks(0));
    check(
        r.is_ok(),
        "邮箱: 紧急发送",
        format_args!("urgent(42) = {:?}", r),
    );
    let r = mb.recv(Timeout::Ticks(0));
    check(
        r == Ok(42),
        "邮箱: 紧急消息插到队首",
        format_args!("recv() = {:?}", r),
    );

    // 消息队列: 收发一致 (含二进制)
    let mq = rtos::MessageQueue::new(16, 4);
    let hello: &[u8] = &[0x52, 0x00, 0xFF, b'!'];
    let r = mq.send(hello, Timeout::Ticks(0));
    check(
        r.is_ok(),
        "消息队列: 发送",
        format_args!("send({:02X?}) = {:?}", hello, r),
    );
    let mut buf = [0u8; 16];
    let n = mq.recv(&mut buf, Timeout::Ticks(0));
    check(
        n == Ok(4) && buf[..4] == *hello,
        "消息队列: 接收内容一致",
        format_args!("recv() = {:?}, data = {:02X?}", n, &buf[..4]),
    );
    let r = mq.recv(&mut buf, Timeout::Ticks(0));
    check(
        r.is_err(),
        "消息队列: 空立即超时",
        format_args!("recv() = {:?}", r),
    );

    // 延时: uptime 前进
    let t0 = rtos::uptime_ms();
    rtos::thread_delay_ms(20);
    let t1 = rtos::uptime_ms();
    check(
        t1 >= t0 + 20,
        "线程延时: uptime 前进 ≥ 20ms",
        format_args!("延时 20ms, 实际 {}ms", t1 - t0),
    );

    // 线程删除 (delete API) 与自然退出 (defunct 回收)
    if !aborted.get() {
        log_debug!("[selftest] 创建 victim 线程");
        let victim = rtos::thread_create("victim", 1024, 24, 0, victim_thread, 0);
        log_debug!("[selftest] victim 已创建, 延时 50ms");
        rtos::thread_delay_ms(50);
        log_debug!("[selftest] 调用 victim.delete()");
        victim.delete();
        log_debug!("[selftest] delete() 已返回, 延时 50ms");
        rtos::thread_delay_ms(50);
        // 可观测断言: victim 已从线程列表消失
        let gone = !rtos::thread_info_list().iter().any(|t| t.name == "victim");
        check(
            gone,
            "线程删除: victim 已删除并从列表消失",
            format_args!("victim 已删除, 系统无异常"),
        );
        log_debug!("[selftest] 创建 exit-me 线程");
        rtos::thread_create("exit-me", 1024, 25, 0, exit_thread, 0);
        rtos::thread_delay_ms(100);
        let gone = !rtos::thread_info_list().iter().any(|t| t.name == "exit-me");
        check(
            gone,
            "线程退出: 入口返回后经 defunct 回收",
            format_args!("exit-me 已退出并从列表消失"),
        );
    }

    // IPC 阻塞唤醒回归 (P0): 发送者在满邮箱上阻塞, 接收者取走消息后
    // 唤醒并完成发送 —— 旧实现唤醒后直接返回 Full, 消息丢失
    if !aborted.get() {
        let sender = rtos::thread_create("blk-send", 1024, 24, 0, blk_sender, 0);
        let mut got = [usize::MAX; 3];
        for slot in &mut got {
            *slot = BLK_MB.recv(Timeout::Forever).unwrap_or(usize::MAX);
        }
        rtos::thread_delay_ms(20); // 让发送者线程退出并回收
        let _ = sender;
        check(
            got == [1000, 1001, 1002],
            "IPC: 满邮箱阻塞发送/接收, 消息不丢",
            format_args!("got = {:?}", got),
        );
    }

    // Flash (EFM): 扇区擦除 + 多字编程 + 回读校验 (末扇区, 远离固件/swap 标记)
    if !aborted.get() {
        const FLASH_TEST_ADDR: u32 = 0x0007_C000; // 扇区 62 (0x7C000)
        let data: [u8; 64] = [
            0x52, b'F', b'L', b'A', b'S', b'H', 0x00, 0xFF, // 二进制 + ASCII 混合
            b'-', b't', b'e', b's', b't', b' ', b'o', b'k', 0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34,
            0x56, 0x78, b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h', b'i', b'j', b'k', b'l',
            b'm', b'n', b'o', b'p', b'q', b'r', b's', b't', b'u', b'v', b'w', b'x', b'y', b'z',
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7',
        ];
        let mut ok = efm::sector_erase(FLASH_TEST_ADDR).is_ok();
        if ok && efm::program(FLASH_TEST_ADDR, &data).is_err() {
            ok = false;
        }
        if ok {
            for (i, &b) in data.iter().enumerate() {
                if efm::read_byte(FLASH_TEST_ADDR + i as u32) != b {
                    ok = false;
                    break;
                }
            }
        }
        // 还原为擦除态 (0xFF), 保持扇区干净
        if efm::sector_erase(FLASH_TEST_ADDR).is_err() {
            ok = false;
        }
        check(
            ok,
            "Flash: 扇区擦除/编程/回读",
            format_args!("addr=0x{:08X} len={}B", FLASH_TEST_ADDR, data.len()),
        );
    }

    // CRC 硬件加速器: 标准测试向量 "123456789" (四个标准配置)
    if !aborted.get() {
        let data: &[u8] = b"123456789";
        let x25 = crc::calculate(data, crc::DataWidth::Byte, crc::Config::x25());
        let ccitt = crc::calculate(data, crc::DataWidth::Byte, crc::Config::ccitt_false());
        let ieee = crc::calculate(data, crc::DataWidth::Byte, crc::Config::crc32());
        let mpeg2 = crc::calculate(data, crc::DataWidth::Byte, crc::Config::crc32_mpeg2());
        check(
            x25 == 0x906E && ccitt == 0x29B1 && ieee == 0xCBF4_3926 && mpeg2 == 0x0376_E6E7,
            "CRC: 标准向量 X25/CCITT/CRC32/MPEG2",
            format_args!(
                "X25={:#06X} CCITT-F={:#06X} CRC32={:#010X} MPEG2={:#010X}",
                x25, ccitt, ieee, mpeg2
            ),
        );
    }

    // 汇总始终打印 (内核打印, 不受日志开关影响)
    if aborted.get() {
        println!(
            "[selftest] 被中断 (ESC): 已完成 {} 项, 通过 {}, 失败 {}",
            pass + fail,
            pass,
            fail
        );
    } else {
        println!("[selftest] 完成: {} 通过, {} 失败", pass, fail);
    }
}

/// 周期定时器回调 (中断上下文): 仅做计数, 不调用阻塞 API
extern "C" fn timer_cb(_param: usize) {
    TIMER_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// 硬件初始化: 时钟 → GPIO (LED + UART 引脚) → SysTick → 控制台 USART
///
/// 各参数 (时钟源/引脚/波特率/节拍频率等) 均来自 .cargo/config.toml。
fn hardware_init() -> config::ConsoleUart {
    // 时钟初始化: 按配置选择时钟源 (mrc/hrc/xtal/pll, PLL 源可配 XTAL 或
    // HRC), 失败自动回退 (见 clk::init)
    let _ = clk::init();
    log_debug!(
        "时钟: {} Hz (源 {:?})",
        clk::system_clock_hz(),
        config::CLOCK_SOURCE
    );

    // GPIO
    let gpio = Gpio::take();
    gpio.pin::<PortC, { config::LED_PIN }>().configure(Config {
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
    // 控制台就绪: 此后 println!/log! 才真正输出 (就绪前打印被静默丢弃,
    // 防止 UART 时钟未使能时 TXE 等待死循环)
    console::mark_ready();
    log_debug!(
        "控制台 UART: USART{} {} bps (过采样 {:?}, 分频 {:?})",
        config::UART_UNIT,
        config::UART_BAUDRATE,
        config::UART_OVERSAMPLE,
        config::UART_CLOCK_DIV
    );

    // RTC (LRC 源, 24H; 基准 2000-01-01 00:00:00) — 日志时间戳 [天:时:分:秒]
    //
    // 注意: 必须在 UART 就绪后初始化 —— 确认日志为 info 级会实际打印,
    // 若在 UART 使能前打印, TXE 等待将死循环 (时钟未开, SR 读回 0)。
    if config::RTC_ENABLE {
        rtc::init(rtc::Config {
            clock_src: rtc::ClockSource::Lrc,
            hour_format: rtc::HourFormat::H24,
            int_period: rtc::IntPeriod::Sec,
        });
        rtc::set_date(rtc::Date {
            year: 0,
            month: 1,
            day: 1,
            weekday: 6,
        });
        rtc::set_time(rtc::Time {
            hour: 0,
            minute: 0,
            second: 0,
            pm: false,
        });
        rtc::start();
        log_info!("RTC 已启动 (LRC 源, 24H), 日志时间戳生效 [天:时:分:秒]");
    }
    uart
}
