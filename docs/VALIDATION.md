# 验证记录与测试矩阵

## 自动化验证

统一入口是：

```bash
bash scripts/verify.sh
```

脚本执行格式检查、workspace 主机测试、目标 clippy、ARM debug/release 构建，以及
开启 nano/selftest/soak/ZMODEM 的全功能构建。快速模式：

```bash
bash scripts/verify.sh --quick
```

主机测试重点如下：

| 区域 | 覆盖 |
|---|---|
| `heap_layout`/`heap_tlsf` | checked 对齐、溢出、随机 churn、合并、OOM 和高对齐 |
| `can_timing` | 位速率、采样点、SJW、SBT 编码、误差和边界 |
| `zmodem` | CRC、转义、帧收发、多文件/空文件、lrzsz 互通 |
| `logring`/`logfile_core` | 固定容量淘汰、截断标记、启动序号和轮转 |
| `exception_frame` | 基本/浮点异常帧布局与安全解码 |
| `crates/littlefs` | 文件/目录语义、掉电穷举、磨损均衡、坏块和真实 16×8 KiB 几何 |

主机测试不执行 MMIO、真实中断、Flash bus hold、CAN 电气链路或完整目标调度时序。

## 真机回归矩阵

| 改动 | 最小目标板验证 |
|---|---|
| 启动/链接/MPU | 冷启动、软复位、栈守卫、fault 输出、ELF 段边界 |
| 时钟/总线 | 启动源/频率日志、MCO 测量、UART 波特率、SysTick、WDT 余量 |
| UART/DMA/console | 登录、长行、并发日志、RX 溢出、ZMODEM 双向传输 |
| EFM/文件系统 | `mkfs`/`mount`/`fsck`、跨块文件、复位恢复、坏块和容量错误 |
| RTOS/IPC | `selftest`、超时、优先级继承、线程退出回收、持续负载 |
| CAN | 内部回环（`selftest can`）；外部总线用 shell `can` 命令 + CAN 转 USB 适配器（外接 PHY、终端电阻、位速率一致），步骤见 [驱动参考](DRIVERS.md) |
| 实时性 | soak 报告中的软件指标、Flash 窗口样本、WDT 状态 |

## 已有验证记录

历史真机记录包括 debug/release 烧录运行、连续复位自检、UART ASCII/二进制接收、
大包溢出保护和启动横幅验证。它们用于说明测试方法，不代表每次代码变更自动继承
通过结论；涉及 CAN 的改动尤其需要重新做内部回环和外部总线测试。

## 已知限制

- 115200 无流控时，PC 端读取不及时可能造成 USB 转串口缓冲溢出；
- 主机测试无法证明寄存器偏移、时钟电气稳定性、CAN 收发器连接或 Flash 实际寿命；
- 文件系统是整快照写入，容量和写放大都受“新旧快照共存”约束；
- CAN 当前只支持经典 CAN，不支持 TTCAN 和 INT128~143 共享中断线；
- panic/fault 诊断优先保证不死锁和可复位，不保证在堆或栈已经严重损坏时输出完整。

## 历史修复摘要

历史上曾修复过浮点格式化导致的裸机内存破坏、线程 TCB 可变性边界、定时器回调
临界区生命周期、CAN 签名变更漏改调用点、DMA TXE 边沿触发和日志/soak 输出互相
干扰等问题。详细变更应以 Git 历史和代码注释为准，不在本文件复制完整变更日志。
