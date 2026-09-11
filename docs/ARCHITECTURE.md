# 系统架构

本文面向首次维护本工程的开发者，说明模块边界、启动和运行时数据流，以及修改
底层代码时必须保持的约束。具体寄存器值以 `doc/chip/` 中的数据手册、参考手册
和 SVD 为准；可调参数以 `.cargo/config.toml` 为准。

## 1. 系统定位

目标硬件为 HC32F460JEUA（Cortex-M4F），工程使用 `thumbv7em-none-eabihf`
目标、`no_std`/`no_main`，不依赖 PAC、HAL 或第三方运行时。固件包含：

- 自定义复位入口、链接脚本和完整异常/中断向量表；
- 手写寄存器驱动与 HC32F460JEUA-EVB 板级资源绑定；
- 从 RT-Thread v5.2.2 语义移植的单核抢占式 RTOS；
- TLSF 全局堆、串口 shell、分级日志、ZMODEM 和自检/soak 工具；
- 内部 Flash 上的有界、整快照、断电安全文件系统。

`src/main.rs` 是目标板固件。`src/lib.rs` 只重新导出硬件无关算法，供宿主机用
同一份源码测试；它不是另一套实现。

## 2. 分层与依赖方向

```text
应用层       shell / logfile / selftest / soak / banner / LED demo
   |          只通过板级资源、RTOS API 和平台服务工作
平台服务     board / filesystem / console / log / uart_rtos
   |          组合驱动，提供所有权、锁和线程语义
内核         rtos / heap / critical_section / latency / panic
   |          调度、IPC、分配、异常诊断，不依赖应用层
设备驱动     clk / gpio / uart / dma / can / rtc / efm / crc / ...
   |          MMIO、寄存器时序、超时和中断状态
架构/启动    arch / startup / vector_table / mmio / link.ld
   |          Cortex-M 原语、内存初始化和硬件入口
硬件         HC32F460JEUA + EVB 板级连线
```

依赖只能向下。UART 驱动通过原子回调报告 RX 事件，不直接依赖 RTOS；
`uart_rtos` 才把通知转换成信号量。通用文件系统 crate 只认识 `BlockDevice`，
`src/filesystem.rs` 才把 EFM 和全局优先级继承互斥量接入。这两个适配层是新增
平台能力时应遵循的模式。

## 3. 启动时序

```text
复位向量
  -> reset_handler_stage0（不使用栈，先配置 SRAM3 等待周期）
  -> reset_handler（FPU、.data 拷贝、.bss 清零）
  -> main
     -> Board::take / Board::init
        -> 时钟冻结与实际频率快照
        -> latency、MPU、GPIO、SysTick、UART、DMA、CAN、RTC
     -> rtos::init（异常优先级、空闲线程）
     -> 创建 led / shell / logfile 线程和周期定时器
     -> 注册 UART RX 中断、可选启动 WDT supervisor
     -> rtos::start（首次上下文切换，永不返回）
```

时钟初始化失败不会继续假定配置频率。`ClockController::freeze()` 返回实际
`Clocks` 快照，UART、CAN、SysTick 和 WDT 均据此计算参数。修改初始化顺序时，
不得让依赖频率的驱动早于快照建立。

## 4. 所有权与并发模型

系统为单核，但线程与 ISR 可在任意指令边界交错：

- `Peripherals::take()` 用 CAS 保证一个复位周期内只发放一次外设集合；
- `BoardResources` 初始化一次后以 Release/Acquire 发布为全局只读能力集合；
- RTOS 内核共享状态只在 PRIMASK 临界区内访问，`CriticalSection` 令牌限制引用
  生命周期；侵入式节点一旦入链必须固定地址；
- 线程间共享的文件系统和控制台使用带优先级继承的 `rtos::Mutex`；
- UART RX 与 DMA 完成路径使用固定容量环和原子回调，ISR 不分配、不阻塞；
- 定时器回调运行在中断上下文，只能调用明确标记为中断安全的 API；
- panic/fault 路径不获取普通线程锁，控制台未就绪时允许静默失败。

`unsafe` 主要集中在 MMIO、启动、异常帧、上下文切换、侵入式链表和分配器。
每处安全性依赖的是硬件地址、固定地址、独占所有权或临界区不变量；修改时应更新
相邻的 `Safety` 说明，而不是仅描述语句行为。

## 5. 内存与 Flash 布局

布局由 `link.ld` 定义，关键边界如下：

| 区域 | 地址/大小 | 用途 |
|---|---:|---|
| 固件 Flash | `0x0000_0000`, 368 KiB | 向量、ICG、代码、常量、`.data` 初值 |
| 文件系统 | `0x0005_C000`, 128 KiB | 16 个 8 KiB EFM 扇区 |
| EFM 自检 | `0x0007_C000`, 8 KiB | 自检专用扇区 62 |
| 保留/交换 | `0x0007_E000`, 8 KiB | 扇区 63，不由文件系统使用 |
| SRAM | `0x1FFF_8000`, 188 KiB | `.data`、`.bss`、堆、主栈与守卫 |

向量表从 Flash 起始地址开始，ICG 段固定在 `0x400`。堆从 `.bss` 末尾向上扩展；
链接脚本在 SRAM 顶部预留主栈和 MPU 守卫，并用 `ASSERT` 阻止区域重叠。修改
`CFG_MPU_STACK_GUARD` 时必须同步修改 `link.ld` 的 `MPU_GUARD_SIZE`，`build.rs`
会在构建期核对。

## 6. 调度与时间

SysTick 默认 1 kHz，是唯一 RTOS 节拍源。每次中断先记录到达延迟，再执行节拍
递增、时间片处理、定时器到期和必要的调度请求。PendSV 处于最低异常优先级，
统一完成线程和 ISR 引起的上下文切换；浮点上下文使用 Cortex-M4F 惰性保存语义。

优先级数值越小越高。默认 WDT supervisor 为 0、shell 为 1、LED 为 2、日志
落盘为 3、idle 为 31。阻塞 API 的超时最终都由线程内嵌定时器唤醒。tick 比较
使用回绕安全的有符号差值，不能改成普通绝对大小比较。

## 7. 文件系统事务

`crates/littlefs` 的磁盘格式不兼容 littlefs 2.x。每次写、删除、重命名或建目录
都会生成一个完整不可变快照：

1. 只读预检路径、目录树、条目数和容量；
2. 从不与活动快照重叠的候选块中选择磨损最轻的一段；
3. 擦除候选块，写未提交头、磨损表和全部记录；
4. `sync` 后回读校验头、结构及各级 CRC；
5. 最后一次性编程此前为空的 commit word，再同步和校验；
6. 仅成功后更新 RAM 中的活动快照。

因此成功返回就是持久化边界。擦写开始后的不确定错误要求丢弃实例并重新挂载；
只有块设备明确分类的永久介质错误才触发坏块标记与换位重试。更完整的格式和掉电
论证见 `crates/littlefs/DESIGN.md`。

## 8. 配置边界

`.cargo/config.toml` 的 `[env]` 是工程配置唯一来源。`src/config.rs` 用 const
函数解析并校验范围，`build.rs` 校验跨文件约束并生成构建元数据。配置值改变后
Cargo 会重新编译，不存在运行时配置文件。

引脚号和功能号可配置，但端口类型是板级电气连接的一部分，固定在 `board.rs`。
换板时先修改 BSP；不要在应用线程里直接构造另一组端口/外设句柄绕开所有权入口。

## 9. 故障与可观测性

- `console` 是启动、shell 和 fault 的底座；`log` 是可关闭、可分级的应用通道；
- 日志先进入固定容量 RAM 环，再由低优先级线程轮转写入 `/log/`；
- DWT 测量最长关中断和 SysTick 到达延迟；Flash bus-hold 样本独立统计；
- panic 与 MemManage/BusFault/UsageFault/HardFault 输出异常帧和状态寄存器，再按
  `CFG_PANIC_STRATEGY` 停机或复位；
- WDT 由最高优先级线程喂养，能检测节拍或调度长期停滞，而不依赖 idle 得到运行。

修改诊断路径时必须考虑“堆损坏、调度器不可用、已在异常上下文、UART 尚未初始化”
四种条件，避免 fault 处理再次进入会阻塞或分配的正常业务路径。

## 10. 代码导航

| 需求 | 首要入口 | 继续阅读 |
|---|---|---|
| 调整板级启动 | `src/board.rs` | `src/main.rs`, `src/peripherals.rs` |
| 增加外设驱动 | `src/mmio.rs` | `src/intc.rs`, 现有相似驱动 |
| 修改调度/IPC | `src/rtos/mod.rs` | `sched.rs`, `thread.rs`, `ipc.rs` |
| 修改堆 | `src/heap.rs` | `heap_tlsf.rs`, `heap_layout.rs` |
| 修改文件系统 | `src/filesystem.rs` | `crates/littlefs/src/fs.rs`, `DESIGN.md` |
| 修改 shell | `src/shell.rs` | `src/shell/`, `src/uart_rtos.rs` |
| 定位硬 fault | `src/panic.rs` | `exception_frame.rs`, `vector_table.rs` |
| 移植到新平台 | `PORTING.md` | `src/arch/`, `src/board.rs`, `link.ld` |
