//! 内核自检: 依次验证信号量/互斥量/事件/邮箱/消息队列/延时/
//! 线程删除/线程退出/Flash/CRC/CAN，亦可用 `selftest can` 单独验证 CAN。
//!
//! **同步执行** (由 shell 的 `selftest` 命令调用, 完成后才出下一提示符);
//! 每项检查后轮询 ESC, 按下即中断剩余项。
//!
//! CAN 项在应用 CAN (`CFG_CAN_ENABLE`) 或 shell `can init` 已占用控制器时
//! **临时接管** (RESET 会清空收发队列), 测试结束按原工作模式恢复; 只有
//! CAN IRQ 消费者仍注册 (接管会架空其中断路由) 时跳过。
//!
//! 日志分级: **trace** 级输出每项执行细节 (实际返回值/耗时等,
//! `log level trace` 打开), **info** 级输出 PASS/FAIL 结果;
//! 汇总一行始终打印 (命令的执行结果, 不受日志开关影响)。

use crate::rtos::{EventOpt, Timeout};

/// 被删除的线程: 空转等待删除
extern "C" fn victim_thread(_param: usize) {
    loop {
        crate::rtos::thread_delay_ms(50).expect("selftest 延时必须在线程上下文");
    }
}

/// 自然退出的线程: 入口返回后经 thread_exit → defunct → 空闲线程回收
extern "C" fn exit_thread(_param: usize) {}

/// 阻塞发送线程: 向容量 2 的邮箱连发 3 条 (第 3 条在满时阻塞,
/// 由接收者取走消息后唤醒) —— 回归"唤醒不重试丢消息"缺陷。
/// 消息类型 `usize` 编码在类型中, 收发类型不一致在编译期报错。
static BLK_MB: crate::rtos::Mailbox<usize> = crate::rtos::Mailbox::new(2);

extern "C" fn blk_sender(_param: usize) {
    for i in 0..3 {
        let r = BLK_MB.send(1000 + i, Timeout::Forever);
        crate::log_trace!("[selftest] 阻塞发送 {}: {:?}", i, r);
    }
}

/// 检测用户是否按下 ESC (0x1B): 轮询并清空接收缓冲
///
/// 自检期间终端输入一律丢弃 (ESC 除外); 返回 true 表示请求中断。
pub(crate) fn abort_requested() -> bool {
    crate::board::abort_requested()
}

static CAN_SELFTEST_FILTERS: [crate::can::Filter; 4] = [
    crate::can::Filter {
        id: 0x0A0,
        mask: 0x001,
        kind: crate::can::FilterType::StandardOnly,
    },
    crate::can::Filter {
        id: 0x01AB_CDEF,
        mask: 0,
        kind: crate::can::FilterType::ExtendedOnly,
    },
    crate::can::Filter {
        id: 0x321,
        mask: 0,
        kind: crate::can::FilterType::StandardOnly,
    },
    crate::can::Filter {
        id: 0x5AA,
        mask: 0,
        kind: crate::can::FilterType::ExtendedOnly,
    },
];

const CAN_SELFTEST_FRAMES: [crate::can::TxFrame; 3] = [
    crate::can::TxFrame::data(
        crate::can::Id::Standard(0x0A1),
        8,
        [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77],
    ),
    crate::can::TxFrame::data(
        crate::can::Id::Extended(0x01AB_CDEF),
        3,
        [0xD1, 0xD2, 0xD3, 0, 0, 0, 0, 0],
    ),
    crate::can::TxFrame::remote(crate::can::Id::Standard(0x321), 0),
];

#[derive(Clone, Copy, Debug)]
enum CanSelftestError {
    IrqConsumerRegistered,
    Aborted,
    Driver(crate::can::CanError),
    TxTimeout(crate::can::TxBuffer),
    Controller(crate::can::Status, crate::can::ErrorInfo),
    FilterAcceptedUnexpectedFrame(crate::can::RxFrame),
    RxTimeout {
        received: u8,
    },
    FrameMismatch {
        index: u8,
        frame: crate::can::RxFrame,
    },
    RxFifoNotEmpty,
    ErrorCountersIncreased {
        before: crate::can::ErrorInfo,
        after: crate::can::ErrorInfo,
    },
    RestoreApplication(crate::can::CanError),
    RestoreClock(crate::clk::ClkError),
}

impl From<crate::can::CanError> for CanSelftestError {
    fn from(value: crate::can::CanError) -> Self {
        Self::Driver(value)
    }
}

impl core::fmt::Display for CanSelftestError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::IrqConsumerRegistered => write!(f, "CAN IRQ consumer 仍已注册"),
            Self::Aborted => write!(f, "用户按 ESC 中止"),
            Self::Driver(error) => write!(f, "驱动错误: {:?}", error),
            Self::TxTimeout(buffer) => write!(f, "{:?} 发送超时并已中止", buffer),
            Self::Controller(status, info) => write!(
                f,
                "控制器错误: status={:#010X}, kind={:?}, ALC={}, REC={}, TEC={}",
                status.bits(),
                info.kind,
                info.arbitration_lost_position,
                info.rx_count,
                info.tx_count
            ),
            Self::FilterAcceptedUnexpectedFrame(frame) => {
                write!(f, "验收筛选器错误接收了帧: {:?}", frame)
            }
            Self::RxTimeout { received } => {
                write!(
                    f,
                    "接收超时: 仅收到 {}/{} 帧",
                    received,
                    CAN_SELFTEST_FRAMES.len()
                )
            }
            Self::FrameMismatch { index, frame } => {
                write!(f, "第 {} 帧不匹配: {:?}", index, frame)
            }
            Self::RxFifoNotEmpty => write!(f, "读取预期帧后 RX FIFO 仍非空"),
            Self::ErrorCountersIncreased { before, after } => write!(
                f,
                "内部回环使错误计数增加: REC {}->{}, TEC {}->{}",
                before.rx_count, after.rx_count, before.tx_count, after.tx_count
            ),
            Self::RestoreApplication(error) => write!(f, "恢复应用 CAN 失败: {:?}", error),
            Self::RestoreClock(error) => write!(f, "恢复 XTAL 状态失败: {:?}", error),
        }
    }
}

fn wait_can_tx(
    can: &crate::can::Can,
    buffer: crate::can::TxBuffer,
) -> Result<(), CanSelftestError> {
    let complete = match buffer {
        crate::can::TxBuffer::Primary => crate::can::Status::PTB_TX,
        crate::can::TxBuffer::Secondary => crate::can::Status::STB_TX,
    };
    let start = crate::rtos::uptime_ms();
    loop {
        if abort_requested() {
            can.abort(buffer);
            return Err(CanSelftestError::Aborted);
        }
        let status = can.status();
        if status.intersects(crate::can::Status::TX_ERRORS) {
            can.abort(buffer);
            return Err(CanSelftestError::Controller(status, can.error_info()));
        }
        if status.contains(complete) {
            can.clear_status(complete);
            return Ok(());
        }
        if crate::rtos::uptime_ms().wrapping_sub(start) >= crate::config::CAN_TIMEOUT_MS {
            can.abort(buffer);
            return Err(CanSelftestError::TxTimeout(buffer));
        }
        crate::rtos::thread_delay_ms(1).expect("CAN selftest 必须在线程上下文运行");
    }
}

fn frame_matches(rx: &crate::can::RxFrame, tx: &crate::can::TxFrame) -> bool {
    rx.id == tx.id
        && rx.rtr == tx.rtr
        && rx.dlc == tx.dlc
        && rx.self_tx
        && rx.error == crate::can::ErrorKind::None
        && rx.data[..usize::from(tx.dlc)] == tx.data[..usize::from(tx.dlc)]
}

fn expect_filter_rejection(
    can: &crate::can::Can,
    frame: &crate::can::TxFrame,
) -> Result<(), CanSelftestError> {
    can.try_transmit_ptb(frame)?;
    wait_can_tx(can, crate::can::TxBuffer::Primary)?;
    crate::rtos::thread_delay_ms(2).expect("CAN selftest 必须在线程上下文运行");
    if let Some(frame) = can.try_receive() {
        return Err(CanSelftestError::FilterAcceptedUnexpectedFrame(frame));
    }
    Ok(())
}

fn restore_can(
    board: &crate::board::BoardResources,
    previous_mode: Option<crate::can::WorkMode>,
    xtal_was_enabled: bool,
) -> Result<(), CanSelftestError> {
    board
        .can_release(previous_mode)
        .map_err(CanSelftestError::RestoreApplication)?;
    // 接管恢复后控制器仍在使用 XTAL; 只有测试前本就未初始化 (因而也未
    // 使能 XTAL) 时才关闭它, 恢复测试前的电源状态。
    if previous_mode.is_none() && !xtal_was_enabled {
        crate::clk::xtal_cmd(false).map_err(CanSelftestError::RestoreClock)?;
    }
    Ok(())
}

/// CAN 内部回环自检结果 (调用方据此报告接管与原模式恢复情况)。
#[derive(Clone, Copy, Debug)]
struct CanLoopbackReport {
    timing: crate::can_timing::BitTiming,
    /// 接管前的工作模式 (`None` = 控制器原本空闲, 测试后保持未初始化)
    previous_mode: Option<crate::can::WorkMode>,
}

/// Internal loopback does not drive the TX pin and automatically generates ACK, so it
/// is safe on this board without a PHY. The test still exercises both TX buffer
/// classes and the receive acceptance path used in normal operation.
///
/// 应用 CAN (`CFG_CAN_ENABLE`) 或 shell `can init` 已占用控制器时, 先临时接管
/// (RESET 清空收发队列), 测试结束后按原工作模式恢复; 仅当 IRQ 消费者仍注册
/// (接管会架空其路由) 时拒绝执行。
fn can_loopback_test() -> Result<CanLoopbackReport, CanSelftestError> {
    let board = crate::board::BoardResources::get();
    let can = board.can();
    if can.irq_registered() {
        return Err(CanSelftestError::IrqConsumerRegistered);
    }
    let clocks = board.clocks();
    let xtal_was_enabled = crate::clk::xtal_enabled();
    let previous_mode = board.can_takeover();
    let test = (|| {
        let timing = can.init(
            clocks,
            crate::can::Config {
                mode: crate::can::WorkMode::InternalLoopback,
                filters: &CAN_SELFTEST_FILTERS,
                self_ack: false,
                ptb_single_shot: false,
                stb_single_shot: false,
                stb_priority: crate::can::StbPriority::Fifo,
                rx_warn_limit: 10,
                rx_all_frames: false,
                rx_overflow: crate::can::RxOverflowMode::DiscardNewest,
                interrupts: crate::can::Interrupts::ALL,
                ..crate::config::CAN_CONFIG
            },
        )?;

        while can.try_receive().is_some() {}
        can.clear_status(can.status());
        let before = can.error_info();

        // Reject an unmatched ID and frames whose raw ID matches a filter of
        // the opposite IDE type. Filter 0 also accepts 0x0A1 through mask bit 0.
        for rejected in [
            crate::can::TxFrame::data(
                crate::can::Id::Standard(0x7AA),
                1,
                [0xE0, 0, 0, 0, 0, 0, 0, 0],
            ),
            crate::can::TxFrame::data(
                crate::can::Id::Extended(0x0A1),
                1,
                [0xE1, 0, 0, 0, 0, 0, 0, 0],
            ),
            crate::can::TxFrame::data(
                crate::can::Id::Standard(0x5AA),
                1,
                [0xE2, 0, 0, 0, 0, 0, 0, 0],
            ),
        ] {
            expect_filter_rejection(can, &rejected)?;
        }

        can.try_transmit_ptb(&CAN_SELFTEST_FRAMES[0])?;
        wait_can_tx(can, crate::can::TxBuffer::Primary)?;
        can.enqueue_stb(&CAN_SELFTEST_FRAMES[1])?;
        can.enqueue_stb(&CAN_SELFTEST_FRAMES[2])?;
        can.start_stb(crate::can::StbTransmit::All)?;
        wait_can_tx(can, crate::can::TxBuffer::Secondary)?;

        let start = crate::rtos::uptime_ms();
        let mut received = 0usize;
        while received < CAN_SELFTEST_FRAMES.len() {
            if abort_requested() {
                can.abort(crate::can::TxBuffer::Primary);
                can.abort(crate::can::TxBuffer::Secondary);
                return Err(CanSelftestError::Aborted);
            }
            if let Some(frame) = can.try_receive() {
                if !frame_matches(&frame, &CAN_SELFTEST_FRAMES[received]) {
                    return Err(CanSelftestError::FrameMismatch {
                        index: received as u8,
                        frame,
                    });
                }
                received += 1;
                continue;
            }
            let status = can.status();
            if status.intersects(crate::can::Status::TX_ERRORS) {
                return Err(CanSelftestError::Controller(status, can.error_info()));
            }
            if crate::rtos::uptime_ms().wrapping_sub(start) >= crate::config::CAN_TIMEOUT_MS {
                return Err(CanSelftestError::RxTimeout {
                    received: received as u8,
                });
            }
            crate::rtos::thread_delay_ms(1).expect("CAN selftest 必须在线程上下文运行");
        }
        if can.try_receive().is_some() || can.rx_buffer_status() != crate::can::BufferStatus::Empty
        {
            return Err(CanSelftestError::RxFifoNotEmpty);
        }

        let status = can.status();
        if status.intersects(crate::can::Status::TX_ERRORS) {
            return Err(CanSelftestError::Controller(status, can.error_info()));
        }
        let after = can.error_info();
        if after.rx_count > before.rx_count || after.tx_count > before.tx_count {
            return Err(CanSelftestError::ErrorCountersIncreased { before, after });
        }
        Ok(timing)
    })();

    match restore_can(board, previous_mode, xtal_was_enabled) {
        Ok(()) => test.map(|timing| CanLoopbackReport {
            timing,
            previous_mode,
        }),
        Err(error) => Err(error),
    }
}

/// Run only the CAN internal-loopback checks.
pub(crate) fn run_can() {
    if !crate::config::CAN_SELFTEST_ENABLE {
        crate::println!("[selftest] CAN 已跳过 (CFG_CAN_SELFTEST_ENABLE=false)");
        return;
    }
    let can = crate::board::BoardResources::get().can();
    if can.irq_registered() {
        crate::println!("[selftest] CAN 已跳过 (CAN IRQ consumer 仍已注册)");
        return;
    }
    if can.is_initialized() {
        crate::println!("[selftest] CAN: 临时接管控制器 (清空收发队列, 测试后按原模式恢复)");
    }
    crate::log_info!("[selftest] 开始 CAN 内部回环自检");
    match can_loopback_test() {
        Ok(report) => {
            crate::log_info!("[PASS] CAN: 过滤器/PTB/STB/标准帧/扩展帧/RTR");
            let bps = report.timing.actual_bitrate(crate::clk::XTAL_HZ);
            let sample = report.timing.sample_point_permille();
            match report.previous_mode {
                Some(mode) => crate::println!(
                    "[selftest] CAN 完成: 1 通过, 0 失败 ({bps} bps, 采样点 {sample}‰, 已恢复 {} 模式)",
                    mode.name()
                ),
                None => crate::println!(
                    "[selftest] CAN 完成: 1 通过, 0 失败 ({bps} bps, 采样点 {sample}‰)"
                ),
            }
        }
        Err(CanSelftestError::Aborted) => {
            crate::println!("[selftest] CAN 已中断 (ESC)");
        }
        Err(error) => {
            crate::log_error!("[FAIL] CAN: {}", error);
            crate::println!("[selftest] CAN 完成: 0 通过, 1 失败 ({})", error);
            crate::board::BoardResources::get().indicate_fault();
        }
    }
}

/// 运行内核自检 (由 shell `selftest` 命令调用)
pub(crate) fn run() {
    crate::log_info!("[selftest] 开始 (rtos 内核功能自检), 按 ESC 可中断");
    let mut pass = 0u32;
    let mut fail = 0u32;
    let aborted = core::cell::Cell::new(false);
    let mut check = |ok: bool, name: &str, detail: core::fmt::Arguments<'_>| {
        if aborted.get() {
            return; // 已中断: 跳过剩余项
        }
        // trace 级: 执行细节 (实际返回值/参数), 用于故障定位
        crate::log_trace!("[selftest] {} → {}", name, detail);
        if ok {
            pass += 1;
            crate::log_info!("[PASS] {}", name);
        } else {
            fail += 1;
            // 失败项以 error 级 (红色 [ERR]) 输出, 与通过项的绿色区分
            crate::log_error!("[FAIL] {}", name);
        }
        // 每项后检查 ESC (中断剩余项)
        if abort_requested() {
            aborted.set(true);
            crate::log_info!("[selftest] 收到 ESC, 中断剩余项");
        }
    };

    // 信号量: 计数获取 / 立即超时 / release 唤醒
    // (测试对象用局部变量: 每次运行全新状态, 不依赖 static 持久化)
    let sem = crate::rtos::Semaphore::new(1, 1);
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

    // 互斥量 Mutex<T>: 数据保护 (守卫提供 &mut T) + 非递归语义
    let mtx = crate::rtos::Mutex::new(0u32);
    let mut g1 = mtx.lock(Timeout::Ticks(0));
    check(
        g1.is_ok(),
        "互斥量: 可获取",
        format_args!("lock(0) = {:?}", g1),
    );
    // 守卫内 &mut 访问保护数据
    let mut wrote = false;
    if let Ok(g) = &mut g1 {
        **g = 0x5A5A_5A5A;
        wrote = **g == 0x5A5A_5A5A;
    }
    check(
        wrote,
        "互斥量: 守卫内 &mut 独占访问数据",
        format_args!("经守卫写入并读回 0x5A5A5A5A"),
    );
    // 非递归: 持有守卫时重复获取返回 Invalid (而非死锁)
    let g2 = mtx.lock(Timeout::Ticks(0));
    check(
        g2.is_err(),
        "互斥量: 非递归, 持有中重复获取返回 Invalid",
        format_args!("持有中 lock(0) = {:?}", g2),
    );
    drop(g1);
    // 释放后可重新获取, 且数据保留
    let g3 = mtx.lock(Timeout::Ticks(0));
    let val = g3.as_ref().map(|g| **g);
    check(
        g3.is_ok() && val == Ok(0x5A5A_5A5A),
        "互斥量: 释放后可重新获取且数据保留",
        format_args!(
            "释放后 lock(0) = {:?}, data = {:#010X}",
            g3,
            val.unwrap_or(0)
        ),
    );
    drop(g3);

    // 事件: AND / OR / 立即超时
    let evt = crate::rtos::Event::new();
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
    let mb = crate::rtos::Mailbox::<usize>::new(4);
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
    let mq = crate::rtos::MessageQueue::new(16, 4);
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

    // 延时: uptime 前进 (回绕安全比较, 与 soak 的 wrapping 语义一致)
    let t0 = crate::rtos::uptime_ms();
    crate::rtos::thread_delay_ms(20).expect("selftest 延时必须在线程上下文");
    let t1 = crate::rtos::uptime_ms();
    check(
        t1.wrapping_sub(t0) >= 20,
        "线程延时: uptime 前进 ≥ 20ms",
        format_args!("延时 20ms, 实际 {}ms", t1.wrapping_sub(t0)),
    );

    // 线程强制删除与自然退出 (defunct 回收)
    if !aborted.get() {
        crate::log_debug!("[selftest] 创建 victim 线程");
        let victim = crate::rtos::thread_create("victim", 1024, 24, 0, victim_thread, 0);
        crate::log_debug!("[selftest] victim 已创建, 延时 50ms");
        crate::rtos::thread_delay_ms(50).expect("selftest 延时必须在线程上下文");
        crate::log_debug!("[selftest] 调用 victim.force_delete()");
        // victim 入口只执行延时循环，不持有借用、守卫或需析构资源，
        // 满足强制删除跳过栈析构的安全前提。
        unsafe { victim.force_delete() }.expect("selftest 只能在线程上下文强制删除 victim");
        crate::log_debug!("[selftest] force_delete() 已返回, 延时 50ms");
        crate::rtos::thread_delay_ms(50).expect("selftest 延时必须在线程上下文");
        // 可观测断言: victim 已从线程列表消失
        let gone = !crate::rtos::thread_info_list()
            .iter()
            .any(|t| t.name == "victim");
        check(
            gone,
            "线程删除: victim 已删除并从列表消失",
            format_args!("victim 已删除, 系统无异常"),
        );
        crate::log_debug!("[selftest] 创建 exit-me 线程");
        crate::rtos::thread_create("exit-me", 1024, 25, 0, exit_thread, 0);
        crate::rtos::thread_delay_ms(100).expect("selftest 延时必须在线程上下文");
        let gone = !crate::rtos::thread_info_list()
            .iter()
            .any(|t| t.name == "exit-me");
        check(
            gone,
            "线程退出: 入口返回后经 defunct 回收",
            format_args!("exit-me 已退出并从列表消失"),
        );
    }

    // IPC 阻塞唤醒回归 (P0): 发送者在满邮箱上阻塞, 接收者取走消息后
    // 唤醒并完成发送 —— 旧实现唤醒后直接返回 Full, 消息丢失
    if !aborted.get() {
        let sender = crate::rtos::thread_create("blk-send", 1024, 24, 0, blk_sender, 0);
        let mut got = [usize::MAX; 3];
        for slot in &mut got {
            *slot = BLK_MB.recv(Timeout::Forever).unwrap_or(usize::MAX);
        }
        crate::rtos::thread_delay_ms(20).expect("selftest 延时必须在线程上下文"); // 让发送者线程退出并回收
        let _ = sender;
        check(
            got == [1000, 1001, 1002],
            "IPC: 满邮箱阻塞发送/接收, 消息不丢",
            format_args!("got = {:?}", got),
        );
    }

    // Flash (EFM): 扇区擦除 + 多字编程 + 回读校验 (扇区 62, 远离固件/swap 标记)
    if !aborted.get() {
        const FLASH_TEST_ADDR: u32 = 0x0007_C000; // 扇区 62 (0x7C000)
        let data: [u8; 64] = [
            0x52, b'F', b'L', b'A', b'S', b'H', 0x00, 0xFF, // 二进制 + ASCII 混合
            b'-', b't', b'e', b's', b't', b' ', b'o', b'k', 0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34,
            0x56, 0x78, b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h', b'i', b'j', b'k', b'l',
            b'm', b'n', b'o', b'p', b'q', b'r', b's', b't', b'u', b'v', b'w', b'x', b'y', b'z',
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7',
        ];
        let mut ok = crate::efm::sector_erase(FLASH_TEST_ADDR).is_ok();
        if ok && crate::efm::program(FLASH_TEST_ADDR, &data).is_err() {
            ok = false;
        }
        if ok {
            for (i, &b) in data.iter().enumerate() {
                if crate::efm::read_byte(FLASH_TEST_ADDR + i as u32) != Ok(b) {
                    ok = false;
                    break;
                }
            }
        }
        // 还原为擦除态 (0xFF), 保持扇区干净
        if crate::efm::sector_erase(FLASH_TEST_ADDR).is_err() {
            ok = false;
        }
        check(
            ok,
            "Flash: 扇区擦除/编程/读取校验",
            format_args!("addr=0x{:08X} len={}B", FLASH_TEST_ADDR, data.len()),
        );
    }

    // CRC 硬件加速器: 标准测试向量 "123456789" (四个标准配置)
    if !aborted.get() {
        let data: &[u8] = b"123456789";
        let x25 =
            crate::crc::calculate(data, crate::crc::DataWidth::Byte, crate::crc::Config::x25());
        let ccitt = crate::crc::calculate(
            data,
            crate::crc::DataWidth::Byte,
            crate::crc::Config::ccitt_false(),
        );
        let ieee = crate::crc::calculate(
            data,
            crate::crc::DataWidth::Byte,
            crate::crc::Config::crc32(),
        );
        let mpeg2 = crate::crc::calculate(
            data,
            crate::crc::DataWidth::Byte,
            crate::crc::Config::crc32_mpeg2(),
        );
        check(
            x25 == 0x906E && ccitt == 0x29B1 && ieee == 0xCBF4_3926 && mpeg2 == 0x0376_E6E7,
            "CRC: 标准向量 X25/CCITT/CRC32/MPEG2",
            format_args!(
                "X25={:#06X} CCITT-F={:#06X} CRC32={:#010X} MPEG2={:#010X}",
                x25, ccitt, ieee, mpeg2
            ),
        );
    }

    // 应用 CAN / shell `can init` 占用控制器时临时接管 (RESET 清空收发队列),
    // 测试后按原工作模式恢复; 仅 IRQ 消费者仍注册时无法安全接管。
    if !aborted.get() {
        let can = crate::board::BoardResources::get().can();
        if !crate::config::CAN_SELFTEST_ENABLE {
            crate::println!("[SKIP] CAN (CFG_CAN_SELFTEST_ENABLE=false)");
        } else if can.irq_registered() {
            crate::println!("[SKIP] CAN (CAN IRQ consumer 仍已注册)");
        } else {
            if can.is_initialized() {
                crate::println!(
                    "[selftest] CAN: 临时接管控制器 (清空收发队列, 测试后按原模式恢复)"
                );
            }
            let result = can_loopback_test();
            if matches!(result, Err(CanSelftestError::Aborted)) {
                aborted.set(true);
                crate::log_info!("[selftest] 收到 ESC, 中断剩余项");
            } else {
                check(
                    result.is_ok(),
                    "CAN: 内部回环过滤器/PTB/STB/帧格式",
                    format_args!("{:?}", result),
                );
            }
        }
    }

    // 汇总始终打印 (内核打印, 不受日志开关影响)
    if aborted.get() {
        crate::println!(
            "[selftest] 被中断 (ESC): 已完成 {} 项, 通过 {}, 失败 {}",
            pass + fail,
            pass,
            fail
        );
    } else {
        crate::println!("[selftest] 完成: {} 通过, {} 失败", pass, fail);
        if fail > 0 {
            crate::println!("[selftest] 存在失败项, 详见上方红色 [ERR] 输出");
            // 故障指示: 板载 ERROR LED 常亮 (成功时不改动 SUCCESS 的启动指示)
            crate::board::BoardResources::get().indicate_fault();
        }
    }
}
