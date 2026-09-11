# 开发与验证

相关设计和功能说明见 [系统架构](ARCHITECTURE.md)、[配置参考](CONFIGURATION.md)、
[Shell](SHELL.md)、[RTOS](RTOS.md)、[驱动参考](DRIVERS.md)、[实时性](REALTIME.md)
和[验证矩阵](VALIDATION.md)。

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
默认串口参数为 USART3、PC13(TX)/PH2(RX)、115200 8N1、无流控。上电后应看到启动
横幅（PB12 WORK 心跳闪烁、PB14 SUCCESS 常亮），
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

## 7. 烧录与调试故障

### `cannot read register ipsr because core #0 is not halted`

这是 **pyocd 烧录失败**（不是编译失败）：`Erasing...` 阶段 pyocd 把 flash 算法
（本芯片的算法在 **SRAM 0x2000_0000** 执行）调到目标上运行，等它命中结束断点；
算法没跑完时它会 `halt()` 再读 IPSR，此时若内核不在可停止状态就抛这句。判定
依据：pyocd 的擦除/编程超时是 10 s，**毫秒级失败 = 内核当时处于 RESET /
SLEEPING / LOCKUP，或 SWD 链路已断**。

按下面顺序排查（`scripts/flash.sh` 支持同名环境变量，失败时也会打印这份阶梯）：

1. **SWD 降速**：`FLASH_FREQ=250k cargo run --release`。新板/长排线/无地线回流
   时 1 MHz 默认时钟常常不稳；
2. **复位下连接**：`FLASH_CONNECT=under-reset cargo run --release`。需要调试器
   nRST 与板子相连；它在固件运行前就停住内核，可绕开下面两类固件干扰；
3. **整片擦除**：`FLASH_MASS_ERASE=1 cargo run --release`，清理半擦除/受保护的
   Flash（一次失败的擦除会留下部分扇区为空，重新烧录前先整片擦除更干净）；
4. **独立供电**：用板子自己的电源而不是调试器 3.3V，确认共地。擦除的电流尖峰
   造成欠压时，表现正是"擦到一半内核复位"；
5. **读寄存器确认现场**：
   ```bash
   .venv/bin/pyocd commander -t hc32f460xe -N -c "read32 0xE000EDF0"  # DHCSR
   .venv/bin/pyocd commander -t hc32f460xe -c "read32 0xE000ED90"     # MPU_CTRL
   ```
   DHCSR 的 S_RESET_ST/S_HALT/S_SLEEP/S_LOCKUP 位说明当时内核状态；MPU_CTRL=0x5
   表示固件的 MPU 正在生效。

两类**固件侧**干扰（都用 `connect_mode=under-reset` 规避）：

- **看门狗（两套都要关）**：
  - *MCU 内部 WDT*（`CFG_WDT_ENABLE=true`）溢出约 2.68 s，且硬件在调试停机期间
    继续计数，调试器一 halt，supervisor 停止喂狗，长时间擦除会被它复位打断；
  - *板载外部硬件看门狗*（`CFG_HWDT_ENABLE=true`，PB4 使能/PB5 喂狗）由主控 GPIO
    喂狗，停机即停止喂狗，1 s 周期内就会复位整机。本板实测：**外部看门狗使能时
    烧录会在擦除中途失败**（现象与本节开头的报错一致），`CFG_HWDT_ENABLE=false`
    （固件启动即把 PB4 拉高禁用）后烧录正常；
  - 因此烧录/产线/断点调试都必须用 `CFG_HWDT_ENABLE=false` + `CFG_WDT_ENABLE=false`；
    若目标上已运行"看门狗使能"的固件，可先用 `under-reset`（在固件配置 GPIO 之前
    停住内核）或板上的看门狗禁用措施把它停掉；
- **MPU 把 SRAM 设为 XN**：本工程 MPU 区域 R2 将 SRAM 标为"可读写、不可执行"
  （`src/mpu.rs`），而 pyocd 的 HC32F460 flash 算法恰恰在 SRAM 里执行。若在
  *已运行该固件* 的目标上以默认 `connect_mode=halt` 连接，算法取指会触发
  MemManage 故障。`under-reset` 在固件配置 MPU 之前停住内核，可避免此冲突；
  诊断对照可烧一版 `CFG_MPU_ENABLE=false`。

### 烧录脚本环境变量

`scripts/flash.sh` 支持 `PYOCD_PROBE`/`PYOCD_TARGET`/`PYOCD`/`FLASH_FREQ`/
`FLASH_CONNECT`/`FLASH_RESET_TYPE`/`FLASH_MASS_ERASE`/`FLASH_EXTRA`/
`FLASH_AUTO_RETRY`（首次失败自动用 500 kHz + under-reset 重试一次）/
`FLASH_DRY_RUN`（只打印命令）。
