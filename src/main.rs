// 禁用标准库，只使用 core 库
#![no_std]
// 禁用操作系统默认的标准入口
#![no_main]
// unsafe 卫生: 所有 unsafe 操作必须显式包裹 unsafe 块
// (rtos/heap 等以"模块级契约 + 整模块 allow"设计的模块单独豁免)
#![deny(unsafe_op_in_unsafe_fn)]

// 使用 Rust 堆数据结构 (Vec/Box/String 等), 分配器见 heap 模块
extern crate alloc;

// ---- 编译期配置 (.cargo/config.toml [env] → env!) ----
mod can_timing; // CAN 位时序纯算法 (与主机单测共享)
mod config;

// ---- 板级支持 (HC32F460JEUA-EVB 资源与初始化编排) ----
mod board;

// ---- 内核基础设施 ----
mod arch; // CPU 架构原语 facade (PRIMASK / WFI / DSB / 系统复位)
mod critical_section; // PRIMASK 临界区 + 中断上下文检测 (ISR 误用防护)
mod heap; // 全局堆分配器 (边界标记 + 首次适配)
mod heap_layout; // 堆分配布局规划 (纯逻辑, 可在主机测试)
mod panic; // panic/fault 诊断: 寄存器解码 + 栈回溯 + 停机/复位策略
mod startup; // 复位入口: SRAM/FPU/时钟等待周期 + .data/.bss
mod vector_table; // 复位/异常/144 外设中断向量表 (原子回调槽)

// ---- 片内资源驱动 (寄存器级, 零依赖) ----
mod crc; // CRC 硬件加速器: CRC16/32 (X25/CCITT/IEEE), 累加模式
mod efm; // 片内 Flash (EFM): 擦除/编程/读等待/缓存/引导交换
mod filesystem; // 断电安全的精简文件系统 + 片内 Flash 分区适配
mod icg; // ICG 硬件配置段 (flash 0x400, 复位时硬件载入)
mod intc; // 中断控制器: 事件源→SEL→NVIC 路由 + 注册 API
mod mpu; // 内存保护单元: FLASH 只读 + SRAM/外设 XN + 线程栈守卫
mod rtc; // 实时时钟 (RTC): LRC 源/时间日期/闹钟, 日志时间戳
mod sram; // 片内 SRAM (SRAMC): 等待周期/奇偶·ECC 错误检测
mod wdt; // 硬件看门狗 (WDT): 高优先级 supervisor 周期喂狗

// ---- 外设驱动 ----
mod can; // CAN 控制器: CAN2.0B, 位时间计算, 回环自测支持
mod clk; // 时钟链: XTAL + MPLL → 200MHz, 失败自动回退
mod gpio; // GPIO: 寄存器/端口/引脚分层, const 泛型封装
mod systick; // SysTick 节拍 (1kHz, RTOS 的时钟源)
mod uart; // USART1~4: 波特率/过采样/小数分频 + 无锁原子接收环

// ---- 输出通道 ----
mod console; // 控制台: 打印锁 (优先级继承) + 原子整行输出
mod log; // 应用日志: 可开关+分级+彩色, 与内核打印分离

// ---- RTOS 内核 (RT-Thread 架构移植) ----
mod rtos;
mod uart_rtos; // UART 的 RTOS 阻塞接收适配层

// ---- 应用 ----
mod banner; // 启动横幅 (依赖 clk/heap/rtos 公共状态)
mod selftest; // 内核自检 (shell `selftest` 命令同步执行)
mod shell; // 仿 Ubuntu 终端: 登录 + 命令提示符 + 系统信息命令

use core::sync::atomic::{AtomicU32, Ordering};
/// 全局堆分配器 (边界标记 + 首次适配, 见 heap 模块)
#[global_allocator]
static ALLOCATOR: heap::HeapAllocator = heap::HeapAllocator;

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
    arch::data_sync_barrier();
}

/// 应用入口: 由 [`startup::reset_handler`] 在完成硬件与内存初始化后调用
pub(crate) fn main() -> ! {
    // 板级初始化 (时钟 / MPU / LED / SysTick / 控制台 UART / RTC)
    let resources = board::Board::take().expect("Board 已被获取").init();

    // RTOS 初始化: 中断优先级 + 空闲线程
    rtos::init();

    // 创建应用线程 (栈/优先级/时间片来自 .cargo/config.toml)。默认配置下
    // shell 优先级最高，首次运行时先启动并独占文件系统，再进入登录流程。
    // selftest 不在此运行, 由 shell 命令 `selftest` 同步执行。
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
    TIMER.pin_static().start_ms(
        config::APP_TIMER_PERIOD_MS,
        config::APP_TIMER_PERIOD_MS,
        timer_cb,
        0,
    );

    // 使能控制台 UART 接收中断 (NVIC 线/优先级来自 .cargo/config.toml)
    resources.enable_console_rx_interrupt();

    // 硬件看门狗 (CFG_WDT_ENABLE): 启动计数并创建最高优先级 supervisor;
    // 合法长输出不会因 idle 饥饿误复位，调度/节拍停滞仍会触发硬件复位。
    resources.start_watchdog();

    // 内核启动横幅 (创建线程后、启动前, 就绪统计包含所有线程;
    // 开头先清屏, 使每次启动与上次输出明确分隔)
    banner::show();

    // 应用日志 (与内核打印分离): 输出与否由 CFG_LOG_ENABLE / CFG_LOG_LEVEL
    // 决定, 运行时可经 shell `log` 命令切换
    log_info!(
        "系统启动: {} @ {} MHz",
        config::CORE,
        resources.system_clock_hz() / 1_000_000
    );

    // 启动调度器, 永不返回
    rtos::start();
}

// ---- 演示线程 ----

/// LED 线程: 每 500ms 翻转一次 (周期来自配置, 由线程调度而非中断分频)
extern "C" fn led_thread(_param: usize) {
    loop {
        board::BoardResources::get().toggle_led();
        // debug 级: 默认阈值 (info) 不输出, 可经 `log level debug` 打开
        log_debug!("LED 翻转, uptime = {} ms", rtos::uptime_ms());
        rtos::thread_delay_ms(config::APP_LED_BLINK_MS).expect("LED 延时必须在线程上下文");
    }
}

/// 周期定时器回调 (中断上下文): 仅做计数, 不调用阻塞 API
extern "C" fn timer_cb(_param: usize) {
    TIMER_COUNT.fetch_add(1, Ordering::Relaxed);
}
