# 驱动参考

驱动位于 `src/`，均以 HC32F460 数据手册、参考手册、SVD 和 DDL v3.3.0 为依据。
驱动只实现硬件能力；所有权、初始化顺序和线程适配由 `board.rs`、`peripherals.rs`
及平台服务负责。

## 时钟与启动

`clk` 配置 MRC/HRC/XTAL/MPLL，按目标频率先设置 EFM/SRAM 等待周期再切换系统时钟。
PLL 或振荡器失败会回退到可用源，`Clocks` 快照记录实际 system/HCLK/PCLK/EXCLK。
UART、CAN、SysTick 和 WDT 必须使用该快照计算分频，不能假定配置源一定成功。
`icg` 把 HRC/WDT 等复位配置固定放到 Flash `0x400`。

## GPIO、UART 与 DMA

- `gpio::Pin<P, N>` 以端口和引脚 const 泛型编码资源，配置、复用和写 1 原子输出
  操作由 `with_unlocked` 临界区保护；HC32F460 无内部下拉；
- `uart` 支持 USART1~4、8/9 位数据、校验、停止位、过采样、小数波特率和中断 RX
  环；裸驱动只发出 OS 无关通知；
- `uart_rtos` 把通知转换为容量为 1 的 RTOS 信号量，重复通知可以合并，数据真值仍
  在 RX 环中；
- `dma` 提供 DMA1/2 四通道、外设触发、软件拷贝、TC/错误中断和 LLP 重配置；当前
  集成包括长控制台输出 TX 卸载及 Flash→RAM 大块拷贝，忙或过短请求回退轮询。

## Flash、文件系统与 CRC

`efm` 管理 512 KiB 主 Flash、8 KiB 扇区、写保护、擦除/编程等待、UID 和读等待周期。
擦写使用非阻塞 guard 串行化，并标注 bus-hold 窗口；ISR 调用返回错误，不会交错
修改控制器状态。

`filesystem` 把 EFM 适配为 `crates/littlefs` 的 `BlockDevice`，通过全局优先级继承
互斥量串行化 shell 与日志线程。文件系统每次修改写完整候选快照，commit word 最后
发布；详细格式和恢复保证见 [DESIGN.md](../crates/littlefs/DESIGN.md)。

`crc` 驱动固定 CRC16/CRC32 多项式，支持 X25/CCITT/IEEE 配置、一次性计算和分帧累加。

## CAN

`can` 仅实现经典 CAN 2.0B：11/29 位 ID、数据帧/RTR、8 个过滤器、PTB 发送、STB
队列、10 槽 RX FIFO、状态和错误计数。位时序由 `can_timing` 纯算法搜索后编码。
CANCLK 固定来自 XTAL，配置和硬件限制在编译期校验。EVB 没有 CAN PHY，正常模式和
外部回环必须外接收发器与终端电阻；内部回环不需要 PHY。

## RTC、SRAM、MPU 与中断

- `rtc` 默认使用 LRC，提供日期、时间、闹钟和日志时间戳；
- `sram` 配置各 bank 等待周期并读取奇偶/ECC 错误状态；
- `mpu` 建立 Flash RO+X、SRAM/外设 XN、主栈和线程栈守卫区域；
- `intc` 将事件源经 SEL 路由到 NVIC INT000~127，注册时一次完成回调、优先级、
  清挂起和使能；`vector_table` 提供 144 条预置分发入口；
- `systick` 使用 HCLK 产生 RTOS 节拍，ISR 入口先测量延迟再调用 `rtos::tick`。

## 驱动开发约束

新增驱动时先复用 `mmio::Reg`、`critical_section` 和 `intc`，不要重复实现 volatile
访问、解锁序列或 IRQ 分发。公共 API 必须说明单位、上下文限制、超时和错误语义；
寄存器写入顺序、屏障、W1C 和 bus-hold 等非直观硬件行为应写在代码注释中，并在
目标板上做最小回归。
