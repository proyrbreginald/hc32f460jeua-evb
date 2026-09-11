# 实时性与故障处理

工程把“关中断窗口”“中断入口延迟”“Flash 擦写造成的硬件 bus hold”分开测量，
避免把硬件固有停顿误判为软件临界区问题。

## 指标

- 最长 PRIMASK 关中断时间：`critical_section::with` 进入和退出时由 DWT CYCCNT
  记录，覆盖所有线程/ISR 抢占不可用的窗口；
- SysTick 到达延迟：ISR 入口读取 SysTick CVR，按 `reload - VAL` 计算硬件触发到
  实际入口的偏差；
- Flash 窗口样本：EFM 擦写前后调用 `flash_window_begin/end`，重叠的临界区和 tick
  样本单独记录，不污染 soak 的软件阈值判定；
- 线程/堆观测：soak 还记录线程数、栈水位、堆峰值、最大连续空闲块、分配失败、
  上下文切换和 CPU 空闲估算。

CYCCNT 为 32 位，按回绕安全差值计算。`latency::init` 必须在时钟稳定后调用；
SysTick ISR 入口的测量必须保持为第一项操作。

## 阈值与 soak

`CFG_SOAK_MAX_CRITICAL_US` 和 `CFG_SOAK_MAX_IRQ_LATENCY_US` 定义 PASS/FAIL 阈值。
`CFG_SOAK_HANG_GRACE_MS` 用于判断压力线程心跳停滞，`CFG_SOAK_FLASH_INTERVAL_MS`
限制 Flash 压力频率。失败会立即记录线程、循环和错误位置，报告同时保留阈值与
结果，便于审计。

## 看门狗

有**两套互相独立**的看门狗，都由高优先级 supervisor 线程周期喂养，而不是由 idle
喂养：

- **MCU 内部 WDT**（`CFG_WDT_ENABLE`，溢出约 2.68 s）：合法的长输出或低优先级线程
  饥饿不会误触发；若 SysTick、PendSV 或调度器长期停滞，supervisor 无法再次运行，
  硬件复位。调试器断点不会暂停 WDT，调试时应关闭 `CFG_WDT_ENABLE`；
- **板载外部硬件看门狗**（`CFG_HWDT_ENABLE`，PB4 使能控制、PB5 每
  `CFG_HWDT_FEED_MS` 翻转喂狗，硬件要求 1 s 周期）：复位后使能脚为输入态、板上看门狗
  默认使能，因此 `Board::init` 在**时钟初始化之前**先把 PB4 拉高禁用，再按配置决定
  是否使能并首次喂狗；`false` 时不创建喂狗线程。

两者的差异在故障语义：内部 WDT 由时钟/调度停滞触发；外部看门狗不依赖 MCU 内部
状态，即使内核锁死、时钟异常或程序跑飞也能复位整机。`CFG_PANIC_STRATEGY=halt` 时
panic 会停在死循环里，此时外部看门狗（若使能）会在一个喂狗周期后自动复位恢复；
调试/烧录期间必须让两者都保持禁用，否则停机即触发复位。

## MPU 与栈守卫

MPU 静态区域将 Flash 设为只读可执行、SRAM/外设设为不可执行；动态 R5 区域在
PendSV 切换线程后指向新线程栈底的无访问守卫区，主栈另有静态守卫。守卫尺寸由
`CFG_MPU_STACK_GUARD` 和 `link.ld` 共同决定，必须保持一致。MemManage、BusFault、
UsageFault 和 HardFault 均进入不依赖普通锁的诊断路径。

## Panic / fault 路径

`panic` 输出 Rust panic 位置、异常号、CFSR/HFSR、BFAR/MMFAR、异常帧和栈使用量，
然后按 `CFG_PANIC_STRATEGY` 停机或软复位。异常上下文禁止获取普通互斥量、分配堆
或写 Flash；控制台未初始化时允许静默丢弃。`CFG_PANIC_VERBOSE` 控制逐位原因名，
关闭时只保留寄存器原值以节省 Flash。

## 实时修改准则

不要在临界区中加入格式化、Flash 操作、堆分配或不可预测循环；不要在 ISR 中调用
阻塞 API。修改 UART、日志、文件系统或 RTOS 后，应运行 soak 并检查软件指标和
Flash 窗口样本是否分别符合预期。
