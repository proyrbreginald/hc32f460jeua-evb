//! 内核自检: 依次验证信号量/互斥量/事件/邮箱/消息队列/延时/
//! 线程删除/线程退出/Flash/CRC。
//!
//! **同步执行** (由 shell 的 `selftest` 命令调用, 完成后才出下一提示符);
//! 每项检查后轮询 ESC, 按下即中断剩余项。
//!
//! 日志分级: **trace** 级输出每项执行细节 (实际返回值/耗时等,
//! `log level trace` 打开), **info** 级输出 PASS/FAIL 结果;
//! 汇总一行始终打印 (命令的执行结果, 不受日志开关影响)。

use crate::rtos::{EventOpt, Timeout};

/// 被删除的线程: 空转等待删除
extern "C" fn victim_thread(_param: usize) {
    loop {
        crate::rtos::thread_delay_ms(50);
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
fn abort_requested() -> bool {
    let uart = crate::config::ConsoleUart::take();
    let mut esc = false;
    while let Some(b) = uart.read_rx() {
        if b == 0x1B {
            esc = true;
        }
    }
    esc
}

/// CAN 内部回环自测: 初始化 → 发送模式数据帧 → 接收校验
///
/// 内部回环模式 (ILB) 下信号在芯片内部环回, 无需 PB6/PB7 引脚与
/// 外部收发器; 自应答使能保证发送帧回环到接收缓冲 (RX.CTRL.TX=1)。
/// 对齐 DDL `can_loopback` 例程的 CanTx/CanRx 校验流程。
fn can_loopback_test() -> bool {
    // 初始化: 内部回环 + 自应答 + 全接受滤波 + 500Kbps
    let cfg = crate::can::Config {
        mode: crate::can::WorkMode::InternalLoopback,
        self_ack: true,
        baudrate: 500_000,
        ..Default::default()
    };
    if crate::can::init(cfg).is_err() {
        return false; // XTAL 未起振/波特率不可实现等
    }

    // 发送一帧: ID=0x5A5 (标准帧), 8 字节递增模式数据
    let mut data = [0u8; 8];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (0xA0 + i as u8).wrapping_add(1);
    }
    let tx = crate::can::TxFrame {
        id: 0x5A5,
        ide: false,
        rtr: false,
        dlc: 8,
        data,
    };
    if crate::can::send(&tx).is_err() {
        crate::can::local_reset();
        return false;
    }

    // 接收回环帧 (带超时 ~50ms) 并校验: ID/数据/自发标志
    let ok = match crate::can::recv_timeout() {
        Ok(rx) => {
            rx.id == 0x5A5
                && rx.self_tx
                && rx.dlc == 8
                && rx.data == data
                && crate::can::error_counts() == (0, 0)
        }
        Err(_) => false,
    };

    // 清理: 本地复位, 退出回环 (不影响后续使用)
    crate::can::local_reset();
    ok
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
            crate::log_info!("[FAIL] {}", name);
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
        format_args!("释放后 lock(0) = {:?}, data = {:#010X}", g3, val.unwrap_or(0)),
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

    // 延时: uptime 前进
    let t0 = crate::rtos::uptime_ms();
    crate::rtos::thread_delay_ms(20);
    let t1 = crate::rtos::uptime_ms();
    check(
        t1 >= t0 + 20,
        "线程延时: uptime 前进 ≥ 20ms",
        format_args!("延时 20ms, 实际 {}ms", t1 - t0),
    );

    // 线程删除 (delete API) 与自然退出 (defunct 回收)
    if !aborted.get() {
        crate::log_debug!("[selftest] 创建 victim 线程");
        let victim = crate::rtos::thread_create("victim", 1024, 24, 0, victim_thread, 0);
        crate::log_debug!("[selftest] victim 已创建, 延时 50ms");
        crate::rtos::thread_delay_ms(50);
        crate::log_debug!("[selftest] 调用 victim.delete()");
        victim.delete();
        crate::log_debug!("[selftest] delete() 已返回, 延时 50ms");
        crate::rtos::thread_delay_ms(50);
        // 可观测断言: victim 已从线程列表消失
        let gone = !crate::rtos::thread_info_list().iter().any(|t| t.name == "victim");
        check(
            gone,
            "线程删除: victim 已删除并从列表消失",
            format_args!("victim 已删除, 系统无异常"),
        );
        crate::log_debug!("[selftest] 创建 exit-me 线程");
        crate::rtos::thread_create("exit-me", 1024, 25, 0, exit_thread, 0);
        crate::rtos::thread_delay_ms(100);
        let gone = !crate::rtos::thread_info_list().iter().any(|t| t.name == "exit-me");
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
        crate::rtos::thread_delay_ms(20); // 让发送者线程退出并回收
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
        let mut ok = crate::efm::sector_erase(FLASH_TEST_ADDR).is_ok();
        if ok && crate::efm::program(FLASH_TEST_ADDR, &data).is_err() {
            ok = false;
        }
        if ok {
            for (i, &b) in data.iter().enumerate() {
                if crate::efm::read_byte(FLASH_TEST_ADDR + i as u32) != b {
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
            "Flash: 扇区擦除/编程/回读",
            format_args!("addr=0x{:08X} len={}B", FLASH_TEST_ADDR, data.len()),
        );
    }

    // CRC 硬件加速器: 标准测试向量 "123456789" (四个标准配置)
    if !aborted.get() {
        let data: &[u8] = b"123456789";
        let x25 = crate::crc::calculate(data, crate::crc::DataWidth::Byte, crate::crc::Config::x25());
        let ccitt = crate::crc::calculate(data, crate::crc::DataWidth::Byte, crate::crc::Config::ccitt_false());
        let ieee = crate::crc::calculate(data, crate::crc::DataWidth::Byte, crate::crc::Config::crc32());
        let mpeg2 = crate::crc::calculate(data, crate::crc::DataWidth::Byte, crate::crc::Config::crc32_mpeg2());
        check(
            x25 == 0x906E && ccitt == 0x29B1 && ieee == 0xCBF4_3926 && mpeg2 == 0x0376_E6E7,
            "CRC: 标准向量 X25/CCITT/CRC32/MPEG2",
            format_args!(
                "X25={:#06X} CCITT-F={:#06X} CRC32={:#010X} MPEG2={:#010X}",
                x25, ccitt, ieee, mpeg2
            ),
        );
    }

    // CAN 控制器: 内部回环收发一致 (ILB 模式无需引脚/外部收发器;
    // CANCLK = XTAL, 本配置下 8MHz → 500Kbps, 位时间 16 TQ)
    if !aborted.get() {
        let ok = can_loopback_test();
        check(
            ok,
            "CAN: 内部回环收发一致",
            format_args!("ILB 500Kbps, 标准帧 ID=0x5A5, 8B 模式数据"),
        );
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
    }
}
