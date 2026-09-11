# hc32f460jeua-evb

HC32F460JEUA（Cortex-M4F，200 MHz）开发板的纯 Rust 裸机固件。工程使用
`no_std`/`no_main`，手写寄存器驱动，不依赖 PAC 或 HAL，并包含一个按
RT-Thread v5.2.2 语义移植的单核 RTOS。

## 文档导航

- [系统架构](docs/ARCHITECTURE.md)：模块分层、启动时序、所有权、并发、内存和
  Flash 布局。首次阅读建议从这里开始。
- [开发与验证](docs/DEVELOPMENT.md)：工具链、构建、主机测试、烧录、调试和常见
  故障定位。
- [配置参考](docs/CONFIGURATION.md)：`.cargo/config.toml` 的全部配置分组、范围、
  依赖和产品部署注意事项。
- [Shell 与应用功能](docs/SHELL.md)：登录、命令、日志、ZMODEM、自检和 soak。
- [RTOS 内核](docs/RTOS.md)：调度、线程生命周期、IPC、定时器和上下文切换。
- [驱动参考](docs/DRIVERS.md)：时钟、GPIO、UART、DMA、CAN、Flash、RTC、CRC、
  SRAM、MPU 和中断。
- [实时性与故障处理](docs/REALTIME.md)：临界区、DWT、SysTick 延迟、WDT、MPU
  守卫和 panic/fault 路径。
- [验证记录](docs/VALIDATION.md)：测试矩阵、真机验证、测试边界和历史修复摘要。
- [移植指南](PORTING.md)：同系列换芯片和跨 CPU 架构的迁移顺序。
- [文件系统设计](crates/littlefs/DESIGN.md)：快照格式、提交顺序、掉电恢复、磨损
  均衡和坏块处理。

## 核心特性

- `thumbv7em-none-eabihf` 目标，Cortex-M4F FPU，定制启动代码、链接脚本和完整
  异常/144 路外设中断向量表；
- MRC/HRC/XTAL/MPLL 时钟链（12 MHz XTAL → 200 MHz），失败自动回退并把实际频率
  快照传给各驱动；
- GPIO、USART1~4、DMA1/2、经典 CAN 2.0B、RTC、EFM Flash、CRC、SRAMC、MPU；
- TLSF 全局堆，支持任意 2 的幂对齐，checked 算术拒绝越界布局；
- 32 级位图抢占调度、时间片轮转、优先级继承互斥量、信号量、事件、邮箱、消息
  队列、硬定时器和线程僵尸回收；
- 固定容量日志环、分级/彩色日志、Flash 日志轮转、串口 shell 和 ZMODEM；
- 有界、无堆、断电安全的整快照文件系统；
- DWT 实时指标、MPU 栈守卫、panic/fault 诊断、自检和长期 soak；
- 两套看门狗：MCU 内部 WDT（PCLK3 计数、溢出复位）与**板载外部硬件看门狗**
  （PB4 高=禁用/低=使能，PB5 按 `CFG_HWDT_FEED_MS` 周期喂狗，默认启动禁用）。

## 快速开始

默认目标由 `.cargo/config.toml` 指定。安装 stable Rust、`rustfmt`、`clippy` 和
两个编译目标：

```bash
rustup component add rustfmt clippy
rustup target add thumbv7em-none-eabihf x86_64-unknown-linux-gnu
```

运行完整验证：

```bash
bash scripts/verify.sh
```

只构建固件：

```bash
cargo build                    # debug
cargo build --release          # release
```

连接调试器并安装 pyOCD 后构建、复制 ELF 并烧录：

```bash
cargo run --release
```

默认控制台为 USART3：PC13 TX、PH2 RX（Func_Grp2 复用），115200 8N1、无流控。
系统时钟由 12 MHz 外部晶振经 MPLL 倍频到 200 MHz，同一晶振同时作为 CAN 通信
时钟；板载三路 LED：PB12=WORK（心跳）、PB14=SUCCESS（启动完成）、
PB13=ERROR（故障），均为高电平点亮。启动后使用
`root`/`root` 登录；产品部署前必须修改 `.cargo/config.toml` 中的凭据，开启板载
外部看门狗（`CFG_HWDT_ENABLE=true`）并按现场需求调整 WDT 和 panic 策略 ——
注意烧录/调试必须保持 `CFG_WDT_ENABLE=false` + `CFG_HWDT_ENABLE=false`。

## 目录结构

```text
src/main.rs              固件入口和应用线程编排
src/startup.rs           复位阶段、RAM/FPU 初始化
src/board.rs             EVB 资源绑定和硬件初始化顺序
src/rtos/                RTOS 内核
src/{clk,gpio,uart,...}  寄存器级驱动
src/shell.rs             登录、命令注册和 shell 主循环
src/filesystem.rs        内部 Flash 文件系统适配层
src/lib.rs               主机可测纯逻辑的导出入口
crates/littlefs/         独立 no_std 快照文件系统 crate
docs/                    架构、开发、配置、功能和验证文档
link.ld                  Flash/SRAM/文件系统分区和链接断言
build.rs                 cfg 生成、构建日期和 rustc 版本注入
```

## 主机测试边界

`src/lib.rs` 和 `crates/littlefs` 的测试覆盖 CAN 位时序、堆布局/TLSF、日志、
ZMODEM、异常帧解码，以及文件系统语义、掉电切点、磨损均衡和坏块处理。主机测试
不替代目标板上的寄存器访问、真实中断、Flash bus hold、CAN 收发器、电气连接和
完整调度时序；这些内容见 [验证文档](docs/VALIDATION.md)。

## 许可证与来源

工程根目录的 `LICENSE` 和 `NOTICE.md` 记录项目许可。`crates/littlefs` 的设计
借鉴 littlefs 2.11.3 的掉电安全原则，但磁盘格式不兼容 littlefs；第三方来源和
版权信息见对应 crate 的 `NOTICE.md`。
