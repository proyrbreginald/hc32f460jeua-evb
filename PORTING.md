# 可移植性设计与迁移路线

本文描述当前工程的可移植边界、已经完成的第一阶段改造，以及迁移到其他
MCU/CPU 架构时应继续抽象的接口。目标不是隐藏所有硬件差异，而是让依赖
方向稳定：应用依赖能力，板级代码负责绑定，芯片后端保留寄存器语义。

## 目标分层

```text
application (main / shell / selftest)
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
- `rtos`: 调度、线程、定时器和 IPC；通过静态 hook 使用 MPU、WDT 或追踪
  能力，不反向依赖具体板级设备。
- SoC drivers: HC32F460 的寄存器地址、位域、时钟门控和外设状态机。
- `board`: 消费芯片能力，绑定 PC13 LED、控制台引脚、IRQ line/priority、
  时钟与初始化顺序，并向应用交付受限资源。
- OS adapters: 把裸驱动的非阻塞/ISR 通知转换成 RTOS semaphore、timeout
  或未来的 async wake，不让寄存器驱动依赖某个操作系统。

## 第一阶段已落地

1. `arch` facade 集中 Cortex-M CPU 原语，临界区、panic、shell reset、
   SysTick ISR 和 RTOS idle 不再各自嵌入相同汇编或 SCB 地址。
2. RTOS 增加无分配的 idle/context-switch hook。WDT 喂狗和 MPU 栈守卫由
   `board` 注册，内核调度路径不再直接调用这两个芯片模块；守卫大小与
   对齐来自 CPU backend。
3. `board::Board` 集中时钟、MPU、GPIO、SysTick、UART、RTC 初始化及板载
   资源绑定，并用一次性获取与 ready 状态检查约束启动顺序。
4. UART 裸驱动只维护寄存器、ISR、SPSC 接收环和非阻塞 API；
   `uart_rtos` 用容量为 1 的 semaphore 实现阻塞读取。
5. RTOS 时间换算不再假定 1 tick 等于 1 ms；配置范围、延时向上取整及
   定时器回绕窗口均有明确约束。
6. 定时器在 PRIMASK 临界区内完成摘链/状态迁移，在临界区外执行 ISR
   回调，缩短全局关中断时间并允许回调自停或重启。

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
`d8-d15` 保存。要支持 Cortex-M0/M33、无 FPU 内核或 RISC-V，应将它迁到
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

### P0：所有权与时钟能力

1. 引入唯一 `Peripherals::take() -> Option<Peripherals>`，由 `Board` 消费并
   拆分资源；收紧 `Uart::take()`、`Gpio::take()`、`Pin::new()`、
   `Can::take()` 等可重复构造入口。
2. 将时钟配置改为 `ClockController::freeze(config) -> Clocks`。`Clocks`
   是初始化成功后的频率 token；驱动接收对应 bus clock，不再查询全局
   `clk::*_hz()` 或读取工程配置。

这两项是后续接口的基础，应先于大规模驱动 trait 化。

### P1：IRQ、驱动依赖与 RTOS port

1. 把中断拆成 CPU `Nvic`、HC32 `InterruptRouter<Event, Line>` 和 BSP
   `InterruptBinding` 三层。驱动只处理自身状态，不选择 NVIC line。
2. 以 UART 为样板，构造时注入 peripheral clock、pins 和 IRQ binding；
   RX buffer 容量改为 const generic 或由 adapter 提供静态存储。
3. 将 `rtos/context.rs` 移入 CPU port，把当前守卫大小/对齐常量升级为完整
   栈布局/保护描述，并移除 `thread.rs` 对 linker 符号的直接假设。
4. 把 tick rate、优先级数量、idle 栈等收敛为独立 `RtosConfig`，使内核
   不需要完整的 HC32 工程配置模块。

### P2：按设备语义抽象

- Flash：唯一句柄、相对地址、分区边界和可恢复的写事务 guard；MPU/cache
  切换留在平台策略。
- CAN：核心提供非阻塞收发，时钟 token 决定位时序；阻塞/timeout 放入
  RTOS adapter。
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
