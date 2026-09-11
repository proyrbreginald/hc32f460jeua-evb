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

### CAN 外部总线测试（配合 CAN 转 USB 适配器）

`selftest can` 与 soak 的 CAN 项都是**内部回环**（不驱动 TX 引脚、控制器自动
ACK），只验证控制器逻辑。要验证真实电气链路，先以 `CFG_CAN_ENABLE=true` 编译
（该开关同时编译下面的 `can` 命令），再使用 shell 的 `can` 命令：

```text
can status                        # 模式/位时序/错误计数/RX FIFO/状态位
can init [normal|silent|int|ext|ext-silent]
can deinit                        # 释放控制器时钟
can send 123 DE AD BE EF          # 标准帧；--ext 扩展、--rtr 远程、--dlc=N 指定长度
can recv 4 3000                   # 收 4 帧或 3s 超时，打印 ID/DLC/数据/错误
can listen 30 normal              # 连续监听 30s（normal 会发 ACK；silent 只听不 ACK）
```

接线与前提：

- PB7(TX)/PB6(RX) 是 3.3V 逻辑引脚（JP2），**必须外接 CAN 收发器**（TJA1050、
  SN65HVD230 等），不能直接接 CANH/CANL；总线两端各 120Ω 终端电阻，两端共地；
- XTAL 必须起振：CAN 是全工程唯一以 XTAL 为通信时钟的模块，`can init` 失败时
  会返回 `XtalNotReady`；
- 板端与适配器的位速率/采样点必须一致（默认 500 kbps、75%、SJW=2，
  `CFG_CAN_BITRATE`/`CFG_CAN_SAMPLE_POINT_PERMILLE`/`CFG_CAN_SJW`）；
- `can` 命令是**编译期开关**：需 `CFG_CAN_ENABLE=true` 才会编译（该开关同时
  决定开机是否自动初始化应用 CAN；默认 false 时 CAN 驱动与命令都不参与链接，
  省 ~11.5 KiB）。开机已初始化时 `selftest can` 会自动跳过（RESET 会清空收发
  队列），要跑内部回环自检先 `can deinit`。

主机侧（CANable/slcan，Linux）：

```bash
sudo slcand -o -c -s6 /dev/ttyACM0 can0
sudo ip link set can0 up type can bitrate 500000 sample-point 0.75 sjw 2
candump can0                      # 收（配合板端 can send）
cansend can0 123#DEADBEEF         # 发（配合板端 can recv / can listen）
cangen can0 -g 10 -I 5            # 周期压测（配合 can listen 观察错误计数）
```

推荐顺序：`can init ext` + `can send` 单节点验证收发器接线（外部回环驱动 TX 并经
收发器回读，`CFG_CAN_SELF_ACK=true` 时无需对端 ACK）→ 接适配器后 `can init normal`
+ `can send` 对比 `candump`（验 TX）→ 主机 `cansend` + 板端 `can recv`（验 RX 与
过滤器）→ `cangen` + `can listen`（验采样点容忍度与错误计数）。发送没有对端应答
时控制器会持续重发，`can send` 会在 200ms 后返回并报告 TEC/REC 与错误类型。

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
