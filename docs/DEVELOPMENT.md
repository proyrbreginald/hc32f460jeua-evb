# 开发与验证

## 1. 前置环境

需要 stable Rust，并安装格式化、静态检查和两个编译目标：

```bash
rustup component add rustfmt clippy
rustup target add thumbv7em-none-eabihf x86_64-unknown-linux-gnu
```

烧录还需要 Python 虚拟环境中的 `pyocd`。仓库的 `Containerfile` 提供一致的
Debian/Rust 基础环境，但调试器 USB 透传仍由宿主机配置。

## 2. 日常验证

推荐在提交前运行统一入口：

```bash
bash scripts/verify.sh
```

该脚本依次检查格式、运行 clippy、执行 workspace 主机测试、构建 ARM debug/release
固件，并在工具链可用时输出 ELF 体积。链接脚本的 `ASSERT` 会在链接阶段核对关键
段和容量边界。单独定位问题时可运行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --target x86_64-unknown-linux-gnu --all-targets
cargo test --workspace --target x86_64-unknown-linux-gnu
cargo build --target thumbv7em-none-eabihf
cargo build --release --target thumbv7em-none-eabihf
```

主机测试覆盖纯算法和内存 NOR 故障注入，不会执行 MMIO、真实中断、Flash bus hold、
板级时钟或 CAN 电气链路；涉及这些路径的变更还必须做目标板验证。

## 3. 配置修改

所有配置位于 `.cargo/config.toml` 的 `[env]`。数值虽然以字符串保存，但由
`src/config.rs` 在 const 求值阶段解析；非法枚举、范围、分频和跨配置组合会使构建
失败。修改时遵循：

1. 先阅读配置项旁的单位、范围和硬件说明；
2. 引脚端口变化同时检查原理图与 `src/board.rs` 的端口类型；
3. 时钟变化检查各总线最大频率、Flash/SRAM 等待周期、UART/CAN 误差和 WDT 超时；
4. MPU 守卫尺寸同时更新链接脚本常量；
5. 功能开关关闭后，验证对应命令和代码确实被 cfg 裁剪。

## 4. 构建、烧录与连接

```bash
# 只构建 release ELF
cargo build --release

# 使用 .cargo/config.toml 的 runner 调用 scripts/flash.sh
cargo run --release
```

烧录脚本优先使用仓库 `.venv/bin/pyocd`，并把 ELF 复制到固定输出位置供调试。
默认串口参数为 USART1、PA9/PA10、115200 8N1、无流控。上电后应看到启动横幅，
默认使用 `root` / `root` 登录；产品部署前必须修改凭据和 panic/WDT 策略。

## 5. 目标板回归清单

改动按影响范围选择最小但完整的检查集合：

| 改动范围 | 必做验证 |
|---|---|
| 纯算法 | 主机单测、clippy、ARM debug/release 构建 |
| 启动/链接/MPU | 冷启动、软复位、fault 注入、栈守卫、ELF 段边界 |
| 时钟/总线 | 实际源与频率日志、MCO 测量、UART 波特率、SysTick、WDT 余量 |
| UART/DMA/console | 登录、长行输出、并发日志、RX 溢出、ZMODEM 双向传输 |
| EFM/文件系统 | mount/mkfs/fsck、跨块文件、复位恢复、坏块/容量错误路径 |
| RTOS/IPC | `selftest`、超时、优先级继承、线程退出回收、持续负载 |
| CAN | 内部回环；正常模式还需外接收发器、终端电阻和另一节点 |
| 实时性 | `soak` 报告中的临界区与 IRQ 延迟阈值、Flash 样本分类 |

## 6. 注释与文档约定

- 模块注释说明职责、依赖方向、并发模型和硬件依据；
- 公共 API 注释说明单位、上下文限制、错误语义和成功后的持久化/所有权效果；
- `unsafe` 附近记录调用方必须维持的可验证不变量；
- 行内注释解释寄存器顺序、屏障、特殊硬件语义和不直观的取舍，不重复代码字面行为；
- 配置、链接布局、shell 命令或磁盘格式变化时，同步更新 README、架构文档或
  `crates/littlefs/DESIGN.md` 中对应的唯一事实来源。

## 7. 常见问题

`cargo` 报缺少目标：运行 `rustup target add thumbv7em-none-eabihf`。

链接时报 RAM/Flash `ASSERT`：检查线程栈、功能开关、主栈/守卫和文件系统边界，
不要通过删除断言绕过真实的内存重叠。

时钟失败后串口参数异常：确认消费方使用 `BoardResources` 中冻结后的实际 `Clocks`
快照，而不是按配置常量推断运行频率。

文件系统修改返回 `RecoveryRequired`：事务在擦除开始后遇到不确定 I/O 错误；取回
块设备并重新 mount，不能继续沿用旧的 RAM 活动快照。

调试器断点后 WDT 复位：开发调试时关闭 `CFG_WDT_ENABLE`，或避免暂停超过硬件
溢出时间；这是硬件计数行为，不是调度器超时。
