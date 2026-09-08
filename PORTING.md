# 可移植性设计与迁移路线

本文描述当前工程的可移植边界、已经完成的第一阶段改造，以及迁移到其他
MCU/CPU 架构时应继续抽象的接口。目标不是隐藏所有硬件差异，而是让依赖
方向稳定：应用依赖能力，板级代码负责绑定，芯片后端保留寄存器语义。

## 目标分层

```text
application (main / shell / selftest / soak)
                 |
                 v
board/BSP -------+-------- OS adapters (uart_rtos)
   |                            |
   v                            v
SoC drivers                  RTOS core
   |                            |
   +-------------> arch/CPU port <---+
```

- `arch`: PRIMASK、异常上下文、WFI、屏障、系统复位等 CPU 原语；后续还应
  接管 RTOS 上下文切换端口。
- `rtos`: 调度、线程、定时器和 IPC；提供通用 idle/context-switch hook，
  不反向依赖具体板级设备。MPU 使用切换 hook，WDT 由 BSP supervisor 管理。
- SoC drivers: HC32F460 的寄存器地址、位域、时钟门控和外设状态机。
- `board`: 消费芯片能力，绑定 PC13 LED、控制台引脚、IRQ line/priority、
  时钟与初始化顺序，并向应用交付受限资源。
- OS adapters: 把裸驱动的非阻塞/ISR 通知转换成 RTOS semaphore、timeout
  或未来的 async wake，不让寄存器驱动依赖某个操作系统。

## 第一阶段已落地

1. `arch` facade 集中 Cortex-M CPU 原语，临界区、panic、shell reset、
   SysTick ISR 和 RTOS idle 不再各自嵌入相同汇编或 SCB 地址。
2. RTOS 增加无分配的 idle/context-switch hook。MPU 栈守卫由 `board`
   注册 context-switch hook；WDT 由 `board` 创建最高优先级 supervisor
   周期喂狗。内核不直接调用芯片模块；守卫大小与对齐来自 CPU backend。
3. `board::Board` 集中时钟、MPU、GPIO、SysTick、UART、CAN、RTC 初始化及板载
   资源绑定，并用一次性获取与 ready 状态检查约束启动顺序。
4. UART 裸驱动只维护寄存器、ISR、SPSC 接收环和非阻塞 API；
   `uart_rtos` 用容量为 1 的 semaphore 实现阻塞读取。
5. RTOS 时间换算不再假定 1 tick 等于 1 ms；配置范围、延时向上取整及
   定时器回绕窗口均有明确约束。
6. 定时器在 PRIMASK 临界区内完成摘链/状态迁移，在临界区外执行 ISR
   回调，缩短全局关中断时间并允许回调自停或重启。
7. 堆分配器为 **TLSF 两级隔离结构** (O(1) 分配/释放, 关中断时间有界);
   按任意 2 的幂 `Layout` 对齐 payload, 在 payload 前记录原块地址并以
   checked 算术关闭溢出路径；纯布局规划与 TLSF 尺寸级映射拆到
   `heap_layout`, 可在主机测试高对齐 padding、空间不足、整数溢出、
   零大小布局与尺寸级边界不变量。邮箱/消息队列池在临界区外经 CAS
   一次性发布, 关中断区间无 malloc。
8. `Thread` 收敛为 `Arc<Thread>` 不可变外壳和唯一
   `UnsafeCell<ThreadInner>`。内核状态仅在单核关中断临界区访问，
   `kernel_self` 保证 TCB 的原始指针、侵入式节点和内建 Timer 活动期间
   不会提前释放；公共句柄不暴露内部引用。
9. `Timer` 通过 `PhantomPinned` 成为 `!Unpin`，公开链表操作要求
   `Pin<&Timer>`，`Drop` 在临界区内自动摘链。IPC 的 timeout API 在启动前
   返回 `KernelNotStarted`，ISR 中拒绝阻塞等待并返回 `InterruptContext`；
   `Timeout::Ticks(0)` 非阻塞探测在调度器启动后仍可用于 ISR。
10. `thread_delay`、`thread_delay_ms` 和 `yield_now` 改为返回 `Result`，
    启动前及 ISR 误用不再依赖 debug 断言或隐式行为。
11. Cortex-M4F PendSV 保存 `r4-r11` 和逐线程 `EXC_RETURN`，仅在 bit4 为
    0 时保存 `d8-d15`；硬件扩展帧负责 `s0-s15/FPSCR`，新线程从基本帧
    `0xffff_fffd` 启动。
12. reset 第一阶段改为无函数序言的汇编，在任何栈访问前配置 SRAM3 并执行
    `DSB`/`ISB`；fault 入口按 `EXC_RETURN` 选择 MSP/PSP 和基本/FP 帧，
    对编码、范围、对齐、溢出与 CFSR 压栈错误做防御检查后才读取现场。
13. 异步强制删除线程会跳过 Rust 栈析构，因此只保留为
    `unsafe Thread::force_delete`；可移植应用应使用协作式停止并让线程入口
    正常返回。有限 timeout 与 `Forever` 分离，超时排序限制在 tick 半周期。
14. 经典 CAN 核心已收敛为板级唯一句柄和非阻塞寄存器 API，覆盖位时序、
    验收过滤器、PTB/STB、RX FIFO、错误状态与聚合 IRQ；RTOS 仅在 selftest
    中提供 timeout 策略，驱动本身不依赖调度器。

这些改动仍保持静态函数指针分发、零堆分配的 ISR 通知和既有寄存器实现，
没有引入 trait object 或运行时设备树。

## 同为 Cortex-M 的换芯片路径

迁移到另一颗 Cortex-M MCU 时，目标是复用 `sched`、`thread`、`timer`、
`ipc`、`klist`、上层 shell 和业务逻辑。需要替换或实现：

1. 新芯片的 startup、linker script、向量表与 SoC 寄存器后端。
2. 时钟、GPIO、UART、Flash、RTC、WDT 等驱动。
3. 新开发板的 `board` 资源绑定与初始化编排。
4. 若内核型号/FPU 保存规则不同，提供对应 RTOS context port。

应用不应再出现端口号、引脚复用号、IRQ 事件号或 SCB 地址；这些信息应
分别停留在 BSP、SoC interrupt router 或 CPU port。

## 跨 CPU 架构的 RTOS 路径

当前 `rtos/context.rs` 仍硬编码 Cortex-M4F PendSV、异常帧、向下生长栈和
惰性 FPU 保存。软件帧包含 `r4-r11 + EXC_RETURN`；仅当
`EXC_RETURN.bit4 == 0`、硬件已经使用含 `s0-s15/FPSCR` 的扩展帧时，软件
再保存 `d8-d15`。新线程的 `0xffff_fffd` 则表示线程模式、PSP、基本帧。
这些规则都是当前 M4F port 的 ABI，不是 RTOS 通用约定。要支持
Cortex-M0/M33、无 FPU 内核或 RISC-V，应将整个帧布局与保存/恢复逻辑迁到
独立 CPU backend，并向 RTOS core 只提供以下静态接口：

```rust
fn init_port();
fn init_stack(storage: StackStorage, entry: ThreadEntry) -> SavedContext;
fn start_first(next: SavedContext) -> !;
fn request_switch(current: &mut SavedContext, next: &SavedContext);
```

通用临界区已经通过不透明 `InterruptState` 调用 CPU backend，不解释
PRIMASK 位；下一步栈保护应传递 `StackProtection { base, size }`，不假定
MPU 最小区域或栈增长方向。当前内核是单核 UP 设计，多核目标需要另一套
同步和调度模型，不能仅替换中断状态函数。

## 后续抽象顺序

### ✅ P0：所有权与时钟能力 (已落地)

1. **唯一 `Peripherals::take() -> Option<Peripherals>`** (`src/peripherals.rs`):
   由 `Board::take()` 消费并拆分为板级资源; CAN 句柄纳入
   `BoardResources` 统一持有; DMA 通道占用位图 (`Dma::take`) + 各构造
   入口收紧为 `pub(crate)`。模块内运行路径 (dma 卸载) 按编译期配置
   重建等价 ZST 句柄 —— 唯一性由入口一次获取固化, 句柄类型不阻止
   同 crate 重建。
2. **`ClockController::freeze() -> Clocks`** (`src/clk.rs`): `Clocks`
   是初始化成功后的频率 token (含错误快照), 驱动接收 `&Clocks` 计算
   分频, 不再查询全局 `clk::*_hz()` 或读取工程配置; 各外设
   (systick/uart/can/rtc/wdt) 已接入。

### P1：IRQ、驱动依赖与 RTOS port

1. 把中断拆成 CPU `Nvic`、HC32 `InterruptRouter<Event, Line>` 和 BSP
   `InterruptBinding` 三层。驱动只处理自身状态，不选择 NVIC line。
   现状: 注册 API (`intc::register(source, line, priority, handler)` +
   `intc::Line` 类型) 与 BSP 绑定 (`Board::enable_console_rx_interrupt`)
   已就位, 但仍是模块级函数而非三组独立类型 —— 迁移到目标芯片时
   保留 `Line` 编码层即可。
2. 以 UART 为样板，构造时注入 peripheral clock、pins 和 IRQ binding；
   RX buffer 容量改为 const generic 或由 adapter 提供静态存储。
3. 将 `rtos/context.rs` 移入 CPU port，把当前守卫大小/对齐常量升级为完整
   栈布局/保护描述，并移除 `thread.rs` 对 linker 符号的直接假设。
4. 把 tick rate、优先级数量、idle 栈等收敛为独立 `RtosConfig`，使内核
   不需要完整的 HC32 工程配置模块。

### P2：按设备语义抽象

- Flash（首版已落地）：`littlefs` 内部定义硬件无关 `BlockDevice`/磁盘
  codec，独占设备并实现可恢复快照事务；HC32 适配提供唯一句柄、
  相对地址、链接期分区边界、对齐/擦除态检查与回读验证，MPU/cache 操作仍
  留在 `efm` 平台后端。后续若引入其他写入者，仍需把 EFM 全局裸函数收紧到
  `Peripherals` 所有权体系。
- CAN（非阻塞核心已落地）：后续以时钟 token 决定位时序，并把业务级
  阻塞/timeout 放入 RTOS adapter。
- RTC：硬件层只负责 calendar/alarm，日志 elapsed 基准放到 time service；
  状态切换超时返回 `Result`。
- WDT：公共配置表达 timeout/window/sleep/action，后端根据 PCLK token
  选择寄存器编码并返回实际超时时间。

## 抽象原则

- 抽象消费者真正需要的能力和依赖，不抽象每一个寄存器位。
- HC32 特有的 FCG、INTC SEL、Flash cache/解锁序列留在 SoC backend。
- 通用驱动优先暴露非阻塞原语；RTOS、async 和轮询策略放适配层。
- 编译期常量、const generic 和静态函数指针优先于 trait object，保持固件
  可预测性和零动态分发。
- 每完成一个边界，至少验证默认 debug/release 构建，并为纯算法或 mock
  backend 增加主机测试；最终建立 M4F 与一个无 FPU Cortex-M 编译矩阵。
- 主机测试只证明被抽出的纯算法或 mock backend；侵入式分配器、临界区、
  PendSV、fault 和外设寄存器路径仍需目标构建及真机验证。
- 需要固定启动横幅日期时设置 `SOURCE_DATE_EPOCH`；它不替代工具链、链接器
  和所有输入均固定的完整可复现构建流程。
