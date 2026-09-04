# hc32f460jeua-evb

HC32F460JEUA (Cortex-M4F, 200MHz) 开发板的**纯 Rust 裸机**工程:零第三方依赖,
全部外设驱动为手写寄存器访问,并内置一个**从 RT-Thread v5.2.2 移植的 RTOS 内核**
(接口按 Rust 风格重新设计)。

## 特性

- 零依赖裸机 Rust (edition 2024, `thumbv7em-none-eabihf`),无 PAC/HAL crate;
- 寄存器级外设驱动:时钟 (XTAL+MPLL→200MHz,失败自动回退)、GPIO、SysTick、
  USART、经典 CAN 2.0B、DMA (DMA1/DMA2 各 4 通道, 控制台 UART 发送卸载);
- 全局堆分配器 (**TLSF 两级隔离空闲链表**: O(1) 分配/释放, 关中断时间
  有界 —— 硬实时),完整支持 `Layout` 的任意 2 的幂对齐，并以 checked
  算术拒绝越界布局，支持 `Vec`/`Box`/`String`;
- **RTOS 内核**:32 级位图调度 + 时间片轮转、优先级继承互斥量、硬定时器、
  信号量/事件/邮箱/消息队列、线程生命周期与僵尸回收;
- **内核对象零临界区分配**: 邮箱/消息队列的消息池在临界区**外**经 CAS
  一次性发布 —— 关中断区间绝不发生 malloc;
- **硬实时指标测量**: DWT 周期计数器实测最长关中断 (PRIMASK) 时间与
  SysTick ISR 到达延迟 (基线法全值; Flash 擦写 bus hold 期间排队的
  样本经 原子标志 + 到达时点 双重判定, 单独归类不污染指标), `soak`
  按编译期阈值 (`CFG_SOAK_MAX_CRITICAL_US`/`CFG_SOAK_MAX_IRQ_LATENCY_US`)
  纳入 PASS/FAIL 判定并写入 HTML 报告;
- **原子打印**:打印锁 (优先级继承) 保证输出整行不交错,高优先级线程
  不会无界等待低优先级线程;
- 完整的 panic/fault 诊断 (CFSR/HFSR 解码 + 基本/浮点异常帧 + 安全栈回溯)。

## 工程配置 (.cargo/config.toml)

所有可调参数集中在 `.cargo/config.toml` 的 `[env]` 段统一管理, 经 `env!`
在**编译期**读取 (`src/config.rs`), 非法值直接编译报错。修改后重新编译
即生效, 无需改动代码 (cargo 自动追踪该文件变化并触发重编译):

| 前缀 | 内容 |
|---|---|
| `CFG_CHIP_MODEL` / `CFG_CORE` | 芯片型号 / 内核名 (横幅与 shell 提示符显示) |
| `CFG_XTAL_HZ` / `CFG_CLK_SOURCE` | 晶振频率 / 时钟源 (mrc/hrc/xtal/pll) |
| `CFG_XTAL_STABLE_TIME` / `CFG_XTAL_DRV` | 晶振起振稳定时间 (1~9) / 驱动能力 (ulow~high) |
| `CFG_HRC_FREQ` / `CFG_HRC_STOP` | HRC 频率 16/20MHz / 复位后停止·振荡 (写 flash ICG1 配置字, 复位生效) |
| `CFG_PLL_*` | MPLL 倍频分频 (含源选择 0=XTAL/1=HRC; 位宽/VCO 范围编译期校验) |
| `CFG_DIV_*` | 总线分频 (1/2/4/8/16) |
| `CFG_SYSTICK_HZ` / `CFG_TICKS_PER_SEC` | 节拍频率 (两者必须一致, 编译期校验) |
| `CFG_PRIORITY_MAX` / `CFG_IDLE_*` | RTOS 优先级与空闲线程 |
| `CFG_UART_*` | 控制台单元 / 引脚·功能号 / 波特率 / 数据位 / 校验 / 停止位 / 过采样 / 流控 / 噪声滤波 / 缓冲 / 中断参数 |
| `CFG_DMA_*` | DMA 开关 / 控制台 TX 单元·通道·最小长度 / 大块拷贝单元·通道·最小长度 |
| `CFG_CAN_*` | CAN 启用 / selftest / 引脚 / 位速率·采样点·SJW·误差 / 模式 / PTB·STB / RX / 过滤器 / 超时 |
| `CFG_LED_PIN` / `CFG_LED_LEVEL` | 板载 LED 引脚与初始电平 |
| `CFG_SHELL_*` | 登录用户名 / 密码 / 失败次数 / 输入缓冲区 / **命令启用列表** (原 `shell.conf` 并入) |
| `CFG_SHELL_HISTORY_SIZE` | RAM 中保留的历史命令条数 (1~16，默认 8，复位后清空) |
| `CFG_NANO_COLUMNS` / `CFG_NANO_ROWS` / `CFG_NANO_MAX_BYTES` | nano 终端探测回退尺寸 / 单文件编辑上限 |
| `CFG_LOG_ENABLE` / `CFG_LOG_LEVEL` / `CFG_LOG_COLOR` | 应用日志默认开关 / 级别阈值 / 控制台 ANSI 颜色 (运行时可用 `log` 命令切换开关与级别) |
| `CFG_LOG_FILE_*` / `CFG_LOG_RING` / `CFG_LOG_FLUSH_MS` | 日志落盘开关 / 单文件上限 / 轮转槽数 / RAM 缓冲 / 刷新间隔 (自动保存到 `/log/`) |
| `CFG_APP_LOGFILE_*` | 日志落盘线程栈 / 优先级 / 时间片 |
| `CFG_CONSOLE_LINE_GAP_MS` | 控制台整行输出行间间隙 (防 USB 转串口突发丢字节) |
| `CFG_PANIC_STRATEGY` | panic/fault 后行为: halt=停机 (调试) / reset=软复位 (产品) |
| `CFG_RTC_ENABLE` | RTC 与日志运行时长时间戳开关 |
| `CFG_WDT_ENABLE` / `CFG_WDT_*` | WDT 开关 / supervisor 栈、最高优先级与喂狗周期 |
| `CFG_MPU_ENABLE` / `CFG_MPU_STACK_GUARD` | FLASH/SRAM/外设属性与栈守卫开关 / 守卫区大小 (须与 link.ld 一致) |
| `CFG_APP_*` | 演示线程参数 (栈/优先级/时间片) / 自检开关 / LED 翻转周期 / 定时器周期 |
| `CFG_SOAK_*` | 长期稳定性测试开关 / 默认时长 / 进度间隔 / 停滞判定 / Flash 节流 |
| `CFG_ZMODEM_*` | ZMODEM 帧超时 / 发送子包长度 / 接收文件大小上限 (`sz`/`rz` 命令) |

约束:
- 数值均为字符串, 编译期解析 (支持 `_` 分隔), 溢出/非法字符/非法枚举
  (如 `CFG_UART_OVERSAMPLE` 非 8/16) 在编译期报错;
- UART/CAN/LED 的**端口类型** (PortA/PortB/PortC) 由 Rust 类型系统编码, 固定在
  `board.rs` 中, 引脚号/功能号等数值参数可在此配置; 引脚存在性仍由
  `Pin::new()` 编译期校验 (JEUA 封装引脚表);
- `build.rs` 仅负责构建日期与 rustc 版本 (启动横幅显示用)；设置
  `SOURCE_DATE_EPOCH` 可固定横幅中的 UTC 构建日期，非法值会使构建失败。

## 目录结构

可移植性分层、已完成改造和后续迁移顺序见 [`PORTING.md`](PORTING.md)。

```
src/
├── main.rs            # 应用入口: 线程/定时器创建与板级资源编排
├── lib.rs             # 可由主机测试的硬件无关算法入口
├── heap_layout.rs     # 分配布局规划 (checked 对齐/溢出处理)
├── config.rs          # 编译期配置入口 (.cargo/config.toml [env] → 类型化常量)
├── board.rs           # BSP: 板载资源绑定与硬件初始化顺序
├── arch/              # CPU 原语 backend (当前为 Cortex-M)
├── banner.rs          # 启动横幅 (应用层): 块字符大标题 + 内核信息面板
├── startup.rs         # 两阶段复位入口: 无栈 SRAM3 配置 → Rust 初始化
├── vector_table.rs    # 复位/异常/144 外设中断向量表 + INT000~007 中断分发
├── panic.rs           # fault 帧校验、CFSR/HFSR 解码与安全栈回溯
├── critical_section.rs# PRIMASK 临界区 (嵌套安全, 中断安全的基础)
├── mmio.rs            # 内存映射寄存器访问原语 (全部外设驱动共用, 含 u8/u16/u32 与 RMW)
├── notify.rs          # 原子回调槽: ISR → 应用无锁通知 (uart/dma 共用, 可在主机测试)
├── heap.rs            # 全局堆分配器适配层 (临界区 + 链接脚本边界)
├── latency.rs         # 硬实时指标: DWT 实测最长关中断 + 节拍 ISR 到达延迟
├── icg.rs             # ICG 初始化配置段 (flash 0x400, 由 CFG_HRC_FREQ 生成)
├── efm.rs             # 片内 Flash (EFM): 扇区擦除/字编程/读等待周期/UID
├── filesystem.rs      # 精简断电安全文件系统的片内 Flash 分区适配
├── crc.rs             # CRC 硬件加速器: CRC16/32 (X25/CCITT/IEEE), 累加模式
├── rtc.rs             # 实时时钟 (RTC): LRC 源/时间日期/闹钟, 日志时间戳
├── sram.rs            # 片内 SRAM (SRAMC): 等待周期/奇偶·ECC 错误检测
├── intc.rs            # 中断控制器: 事件源→SEL→NVIC 路由 + 注册 API
├── clk.rs             # 时钟链: MRC/XTAL/PLL → 200MHz, 回退与运行时查询
├── gpio.rs            # GPIO: 寄存器→端口→引脚→接口四层, const 泛型校验
├── systick.rs         # SysTick 1kHz 节拍 (RTOS 时钟源)
├── dma.rs             # DMA1/DMA2: 外设触发/软件触发传输 + 控制台 TX 卸载
├── uart.rs            # USART1~4 驱动 (波特率/过采样) + 中断接收环形缓冲
├── uart_rtos.rs       # UART 非阻塞通知到 RTOS semaphore 的适配层
├── can.rs             # 经典 CAN 2.0B: 过滤器、PTB/STB、RX FIFO、状态/IRQ
├── can_timing.rs      # CAN 位时序搜索与 SBT 编码 (可在主机测试)
├── console.rs         # 控制台: 打印锁 (优先级继承) + 原子整行输出
├── log.rs             # 应用日志: 分级+彩色标签, 与内核打印分离 (可开关)
├── logring.rs         # 日志 RAM 缓冲 (纯逻辑, 满时丢最旧, 主机单测)
├── logfile.rs         # 日志落盘线程: RAM 缓冲 → /log/ 轮转文件
├── shell.rs           # 登录、命令注册与文件系统命令
├── zmodem.rs          # ZMODEM 协议 (帧/FCS/转义/收发会话, 参考 lrzsz)
├── shell/
│   ├── editor.rs      # nano 风格 ANSI 全屏文本编辑器
│   ├── path.rs        # 固定容量 Linux 风格路径解析与当前目录维护
│   └── zmodem.rs      # ZMODEM 的 shell 集成: UART 端口 + sz/rz 命令
└── rtos/              # RTOS 内核 (RT-Thread 架构移植, 不依赖应用模块)
    ├── mod.rs         # 公共 API: init/start/tick/thread_create 等
    ├── klist.rs       # 侵入式链表 + container_of 宏 (rt_list 移植)
    ├── sched.rs       # 位图就绪表 (32 级) + 时间片轮转 + 栈溢出检测
    ├── thread.rs      # TCB/创建/删除/挂起/延时 + 公共唤醒/调度判定辅助
    ├── timer.rs       # 有序链表硬定时器 (tick 回绕安全)
    ├── ipc.rs         # 信号量/互斥量(优先级继承)/事件/邮箱/消息队列
    ├── idle.rs        # 空闲线程 (wfi) + 僵尸线程回收
    ├── hooks.rs       # 通用 idle/context-switch hook (MPU 使用后者)
    └── context.rs     # Cortex-M4F PendSV 上下文切换汇编 (惰性 FPU 上下文)

build.rs               # 构建元数据 (日期/rustc 版本, 供启动横幅使用)
```

文件系统算法位于独立的 `no_std` workspace crate：

```text
crates/littlefs/  # 块设备/磁盘格式 + 文件/目录/原子快照操作
```

## 堆布局与主机测试

TLSF 两级隔离状态机位于 `src/heap_tlsf.rs` (纯逻辑, **主机压力测试**:
随机 churn + 模式校验、线程栈/TCB 精确内核模式、全部释放顺序合并、
高对齐往返、分配失败优雅拒绝), `heap.rs` 仅提供临界区串行化与链接
脚本堆边界。位图在常数步内定位"装得下需求的最小尺寸级"并取链首,
释放经前/后块合并 (块头内嵌 `prev_phys`) 后按尺寸级插回 —— 分配/
释放与空闲块总数无关, **关中断时间有界** (硬实时要求)。在每个返回
payload 前保存所属分配块的地址，因此即使高对齐请求在块头后产生
padding，释放时仍能准确找回边界标记。地址、大小、padding 均使用
checked 算术；布局无法完整落入空闲块时返回 null。

硬件无关的布局规划位于 `src/heap_layout.rs`，主机测试覆盖 1B 到 4096B
的代表性二次幂对齐、高对齐 padding、空间不足/整数溢出以及零大小布局。
`src/can_timing.rs` 的主机测试覆盖常用位速率、DDL 边界、SBT 编码、误差
上限及溢出输入。`src/zmodem.rs` 的主机测试覆盖 CRC 标准向量 (与 lrzsz
实测一致)、转义/帧收发往返、收发双端内存回环 (多文件/空文件/跳过)，
以及宿主机装有 lrzsz 时与真实 `sz`/`rz` 的互通测试。`crates/littlefs`
的测试覆盖文件系统语义、断电穷举、磨损均衡与**坏块标记/重试/跳过**。
它们验证纯算法，不替代目标板上的寄存器路径、总线电气连接、完整链表
分配器和临界区测试：

```bash
cargo test --workspace --target x86_64-unknown-linux-gnu
```

## 时钟管理 (clk, CMU 模块)

- **配置驱动**: `clk::init()` 按 `.cargo/config.toml` 编排, 无参数;
- 时钟源: MRC 8MHz (复位默认) / HRC 16·20MHz (无需外部器件) / XTAL 直通 /
  MPLL 倍频, 经 `CFG_CLK_SOURCE` 选择, 非法值编译期报错;
- **PLL 源可配**: `CFG_PLL_SRC` 选 0=XTAL / 1=HRC —— `init()` 自动启动
  对应振荡器, 无晶振的板子可用 HRC 源倍频 (如 16MHz×15÷2=120MHz);
  PLL 锁定失败自动降级为 **PLL 源直通**;
- 切换序列对齐 DDL `CLK_SetSysClockSrc`: 先按目标频率配置 FLASH/SRAM
  等待周期与 GPIO 读等待, 高性能模式切换, 再写 CKSWR;
- 晶振起振参数 (稳定时间/驱动能力) 来自 `CFG_XTAL_*`, 对齐 DDL
  `CLK_XTAL_STB_*` / `CLK_XTAL_DRV_*`;
- **HRC 频率 (16/20MHz) 与复位状态经配置设置**: `CFG_HRC_FREQ` +
  `CFG_HRC_STOP` → 生成 flash `0x404` 的 ICG1 配置字 (HRCFREQSEL/
  HRCSTOP), 复位时硬件载入运行期只读的 ICG1 寄存器 (`icg` 模块);
  `clk::hrc_hz()` 按该位查询;
- 振荡器命令: `hrc_cmd` / `xtal_cmd` (对齐 DDL `CLK_HrcCmd`/`CLK_XtalCmd`);
- 配置摘要可由 shell `info`/`sysinfo` 命令输出 (时钟源/振荡器/总线分频/
  UART/日志/线程等编译期常量);
- 总线分频来自 `CFG_DIV_*`, 各总线频率查询: `system_clock_hz` /
  `hclk_hz` / `pclk0_hz` / `pclk1_hz` / `pclk2_hz` / `pclk3_hz` /
  `pclk4_hz` / `exclk_hz` (对齐 DDL `CLK_GetBusClockFreq`);
- MCO1 时钟输出 (PA8): `mco1_config(source, div)` + `mco1_cmd`,
  用于示波器/频率计测量 (对齐 DDL `CLK_MCOConfig`/`CLK_MCOCmd`);
- MPLL 参数编译期校验: 寄存器位宽 + 有效倍频/分频范围 + XTAL/HRC
  两种源下的 VCO 输入 (1~25MHz) 与输出 (240~480MHz) 范围;
- 所有 CMU 寄存器偏移已与 DDL v3.3.0 头文件逐项核对一致。

## 中断系统 (intc + vector_table)

HC32F460 三级中断架构 (对齐 DDL `hc32_ll_interrupts.c`):
**事件源 → INTC.SEL → NVIC 线**:

- 事件源 (`en_int_src_t`, 如 `USART1_RI=279`) → 写 `INTC.SELx` (SEL0 偏移
  0x5C, 每线 4 字节, 复位值 0x1FF=未映射) → NVIC 线 `INTxxx` (IRQn=x,
  共 144 条) → ISER/IPR 使能/优先级;
- **注册 API** (`src/intc.rs`): `intc::register(源, 线, 优先级, 回调)`
  一步完成 路由+装回调+清挂起+设优先级+使能 (对齐 DDL 例程流程);
  失败返回占用、事件源范围或 NVIC 优先级错误，且不留下半注册状态;
- **事件源常量** `intc::src::*`: USART1~4 全部事件 (EI/RI/TI/TCI/RTO,
  USART1=278~282, 每单元 +5)、EIRQ0~15、TIM0/TIM6_1~3/TMRA、DMA、RTC、
  USBFS、I2C、CMP、LVD、ADC、TRNG、EFM、WDT 等;
- **NVIC 原语**: `enable`/`disable`/`pend` (软件触发)/`clear_pend`/
  `set_priority` (0~15, 写 IPR 高半字节, 默认 PRIGROUP=0 无子优先级);
- **向量表** (`vector_table.rs`): 15 异常 + 144 外设中断全部预置分发入口,
  RAM 回调表运行时注册 — 任意 INT000~INT143 线可用 (旧版仅 8 槽);
  未注册槽位触发时静默返回; 异常走 `default_handler` 死循环/`fault_handler`;
- INT128~143 为**共享中断线** (VSSEL 位掩码 + 外设状态轮询, DDL
  `hc32f460_ll_interrupts_share.c` 模式), 本模块暂不支持 (注册限制
  INT000~127, 配置层已校验);
- ISR 约束: 中断上下文只能使用非阻塞操作 (与 `print!`/`log!` 同约束)。

## 片内 Flash (efm)

对齐 DDL v3.3.0 `hc32_ll_efm.c/h` (FWMC 模式 + 写 Flash 地址触发模型):

- 主 Flash 512KB / 64 个 **8KB 扇区** (最小擦除单位, 无页擦除);
- **擦除**: `sector_erase(addr)` (字对齐, 擦除所在整个扇区, ~ms 级);
- **编程**: `program(addr, &data)` (任意长度, 4 字节对齐, 尾部 0xFF 补齐)
  / `program_word(addr, word)`; 单字编程逐字等待结束;
- 流程对齐 DDL: FAPRT 解锁 → FWMC.PEMODE → 设 PEMOD 模式 → 写地址触发
  → 等 FSR.RDY+OPTEND → 恢复只读锁定; 操作结束检查 FSR 错误位
  (PEWERR/PEPRTERR/PGSZERR/PGMISMTCH/COLERR) 返回 `EfmError`;
- 擦除/编程由非阻塞控制器 guard 串行化，RTOS 抢占竞争会返回 `Busy`，
  ISR 调用会返回 `InterruptContext`，不会交错改写 FWMC/cache/保护状态；
- **bus hold**: 每次进入擦写模式都显式保持 `BUSHLDCTL=0`, 擦写期间总线被
  占用, CPU stall 至完成 (从 Flash 运行安全);
  全片擦除/序列编程需 RAM 运行, 模块不提供;
- 读: `read_byte`/`read_word` (Flash 内存映射) / `uid()` (96 位唯一 ID);
- **读等待周期**归属本模块: `ConfigurationGuard::set_wait_cycle` /
  `wait_cycle` (表 7-1)，由 `clk` 在持有完整切换 guard 时调用;
- 自检 (`selftest` 命令) 含 Flash 实测: 扇区 62 擦除/64B 混合数据编程/
  逐字节回读校验, 完成后还原擦除态。

## 断电安全文件系统 (filesystem + crates/littlefs)

首版不是 littlefs 2.x 的 Rust 翻译，也不兼容其磁盘格式。它保留 littlefs
最关键的原则（新数据先落盘、CRC 校验、最后发布引用、旧版本在发布前不
擦除），再以**有界目录树 + 完整不可变快照**取代 CTZ、metadata pair
追加日志、FCRC、orphan/move 状态机：

- API：`format` / `mount` / 整文件 `write` / `read` / `stat` / `list` /
  `read_dir` / `mkdir` / `rmdir` / `max_write_size` / `remove` / `rename` /
  `clear` / `verify` / `level`；`stat` 返回文件或目录类型，`read_dir` 只枚举
  指定目录的直接子项；无堆分配、无 `unsafe`；
- 根目录隐式存在，核心 API 在 `stat` / `read_dir` 中以空字符串表示根；
  持久路径采用无前导 `/` 的规范 UTF-8 路径，完整路径最长 63B，禁止尾随
  `/`、空分量、`.`、`..` 和 NUL；文件与目录合计最多 32 个条目。Shell
  在此之上提供以 `/` 为根的绝对/相对路径解析；
- `mkdir` 要求父目录已存在，`rmdir` 只删除空目录；`rename` 可移动文件或
  完整目录树，所有后代路径在同一个候选快照中改名，不会在恢复后暴露
  部分移动的目录树；
- 每次变更写到当前快照之后的不重叠扇区，完整 `sync` + 回读验证后，最后
  单独编程一个此前未写过的 4B commit word；挂载只接受 marker、header
  CRC、payload CRC、逐文件 CRC、父目录关系和全部结构边界同时有效的版本；
- generation 使用回绕序列比较；**磨损均衡 (格式 v1.1)**：每段快照的
  payload 起点携带每扇区 `u16` 擦除计数表 (由 payload CRC 覆盖, 跨格式化
  单调不减), 每次变更在全部不重叠候选位置中选择"最大擦除计数最小"的落点
  (动态磨损均衡, 均匀磨损时退化为原来的环形顺序轮转); `level` 命令/API
  在计数分布不均时把快照原子搬移到磨损最低区域 (静态磨损均衡), 计数均匀
  时为无写操作; `df` 显示最小/最大擦除计数;
- 任一擦除、编程字节或同步点掉电后，只会挂载到完整旧版本或完整新版本；
  写事务返回设备错误后必须 remount，防止继续使用不确定的内存 generation；
- 不支持递归删除、隐式创建父目录、随机写、打开句柄、符号链接、属性、
  权限、时间戳、坏块迁移；
- 预留扇区 46~61 (`0x5C000..0x7BFFF`, 128KiB)，旧/新快照必须共存，
  因而单个序列化快照最多占 8 个扇区，可用容量略小于 64KiB；完整快照会
  放大写入，适合小型配置/状态文件与轮转日志，不适合高频大日志;
- 快照头记录分区几何 (块数/块大小), 磁盘格式随分区大小变化: 从 64KiB
  分区升级到 128KiB 后, 旧快照因几何不符不会自动挂载, 需手动
  `mkfs --force` 重建 (旧数据保留现场, 不会被自动破坏)。

磁盘格式、提交顺序与安全论证见
[`crates/littlefs/DESIGN.md`](crates/littlefs/DESIGN.md)。主机模拟 NOR 会在
4B 编程字内部按多种字节顺序制造部分 `1 -> 0`，并枚举 `write` / `remove` /
文件 `rename` / `mkdir` / `rmdir` / 目录树 `rename` / `format` 以及三扇区
跨尾部快照的每个掉电边界：

```bash
cargo test --workspace --target x86_64-unknown-linux-gnu
```

真机适配 `filesystem::InternalFlash` 检查相对分区、4B 对齐、目标全擦除，
并对每次 program/erase 做完整回读。它依赖已初始化的时钟、MPU 与 EFM，
不能在 ISR 或硬实时路径调用（单扇区擦除最长约 20ms）。挂载后的文件系统
实例存放在全局 `Mutex<Option<FileSystem>>`（优先级继承），shell 命令与
日志落盘线程 (`logfile`) 分时独占；`InternalFlash` 的 `!Send` 标记经一处
带安全契约的 `unsafe impl Send` 放宽（所有访问经互斥量串行化，中断上下文
被阻塞检测拒绝，单核临界区保证一致性）。已有有效快照时只读挂载；整个
128KiB 分区全为擦除态时自动创建空文件系统；分区含数据但无有效快照时保留
现场并保持未挂载，不会把损坏误判成首次使用。此时检查后可显式执行
`mkfs --force`。

常用 Shell 命令：

```text
pwd                          # 显示当前路径，上电/登录后默认为 /
mkdir /etc                  # 原子创建目录；父目录必须已存在
cd /etc                     # 切换当前路径；无参数时回到 /
write config mode=normal    # 相对路径：原子创建或完整覆盖文件 (别名 put)
nano ./config               # ANSI 全屏编辑文件；不存在时新建
cat /etc/config             # 绝对路径：分块读取，非打印字节显示为 \xNN
stat config                 # 显示文件或目录类型；文件另含大小与 CRC
ls .                        # 列出目录的直接子项；也可用 ls [路径]
cd /
mv /etc /settings           # 在一个快照中原子移动完整目录树
rm /settings/config         # 原子删除文件
rmdir /settings             # 原子删除空目录
df                          # 容量/条目数/generation (别名 fsinfo)
fsck                        # 只读校验当前快照
mount                       # 丢弃内存状态并重新挂载，当前路径回到 /
mkfs --force                # 显式清空全部文件与目录
history                     # 按编号查看 RAM 中的历史命令
history -c                  # 清空命令历史
sz /etc/config              # ZMODEM 发送文件到主机 (主机执行 rz -y 接收)
rz                          # ZMODEM 接收主机文件 (主机执行 sz <文件>)
```

Shell 当前路径默认为根目录 `/`；以 `/` 开头的是绝对路径，其余路径相对
当前目录解析。重复 `/`、`.` 和 `..` 会被规范化，根目录下的 `..` 仍停留
在根目录。文件/目录移动后若当前路径位于被移动的目录树中，提示符会同步
更新；`rmdir` 会拒绝删除当前目录或其祖先。Shell 命令输入仅接受 ASCII，
非 ASCII 或超过默认 128B 上限的命令会整行拒绝，不会静默改写或截断执行；
`write` 面向短单行文本，文件系统本身仍支持约 64KiB 快照。每个变更命令
成功返回时已经完成同步和回读，无需额外 `sync` 命令。

普通命令输入时可用方向键上/下浏览历史；首次向上前的未提交输入会作为
草稿保存，向下越过最新记录时恢复。历史保存在固定容量 RAM 中，相邻重复
命令不重复记录；`history` 查看，`history -c` 清空，容量由
`CFG_SHELL_HISTORY_SIZE` 配置。退出登录后历史仍保留，复位后清空。

`nano <文件>` 提供适合串口终端的精简全屏编辑：方向键、Home/End、翻页、
插入、退格和 Delete 均可用；`Ctrl+O` 保存，`Ctrl+X` 退出，`Ctrl+G` 显示
快捷键帮助。首版只编辑 ASCII 文本（允许 LF 和 TAB），遇到其他控制字节、
UTF-8 或二进制内容会拒绝打开且不改原文件。进入编辑器时会通过 ANSI CPR
自动探测窗口行列并铺满终端；不支持探测时回退到 80x24，可通过
`CFG_NANO_COLUMNS` / `CFG_NANO_ROWS` 调整回退值。受串口刷新时延约束，自动
探测支持 40..240 列、8..100 行，超出范围会明确拒绝进入编辑器。编辑上限
默认为 16KiB，由 `CFG_NANO_MAX_BYTES` 调整；界面会进一步扣除其他文件和
记录开销，采用当前真实可写上限。编辑过程不自动写 Flash；仅显式保存会
提交完整原子快照，
成功返回时已同步并回读。若设备错误导致结果不确定，编辑器会重挂载并核对
持久内容，同时保留 RAM 中的编辑缓冲供再次保存。若串口环溢出或出现
PE/FE/ORE，编辑器会锁存 `INPUT LOST` 并禁止本会话保存，避免缺字内容覆盖
原文件；此时应退出并重新打开。

## ZMODEM 文件传输 (sz / rz)

协议核心 `src/zmodem.rs` 参考 lrzsz (`zm.c`/`lrz.c`/`lsz.c`) 移植为纯
Rust 模块, 零硬件依赖; 与主机 lrzsz 工具经串口互通:

- **发送** `sz <文件> [文件...]`: 板端发起 ZRQINIT → ZRINIT 握手后逐个
  发送文件, 主机侧执行 `rz -y` 接收 (Windows 可用 Tera Term/SecureCRT
  的 ZMODEM 接收); 支持多文件与 16/32 位 FCS 自动协商 (板端声明支持
  CRC32, 兼容 `sz -o` 的 16 位模式);
- **接收** `rz`: 板端周期性发送 ZRINIT 等待主机, 主机侧执行
  `sz <文件>`; 整文件先缓存在 RAM (上限 `CFG_ZMODEM_RX_MAX`, 默认
  64KiB, 快照文件系统要求整文件原子写入), 完整接收后经 CRC 校验与
  FCS 残差校验后原子落盘; 超过上限或文件系统剩余容量不足的文件发送
  ZSKIP 跳过, 会话继续;
- **帧级实现与 lrzsz 逐条对齐**: 十六进制/二进制 (16/32 位 FCS) 头,
  CRC-16 CCITT 与 CRC-32 (残差校验 0xDEBB20E3), ZDLE 转义 (XON/XOFF/
  ^P/ZDLE 及 `@` 后 CR), 数据子包 ZCRCE/G/Q/W 语义, ZACK/ZRPOS 重传
  与位置校验, 5×CAN 取消, ZFIN"OO"握手;
- **流控**: 发送端每子包 (默认 1KiB, `CFG_ZMODEM_SUBPACKET`) 以 ZCRCW
  结束并等待 ZACK (乒乓式), 适配板上 512B UART 接收环, 不做 ZCRCG
  流水; 接收端对 FCS 错误以 ZRPOS 重新同步 (发送端从错误位置重传);
- **中止**: 帧间等待时按 ESC 取消会话并发送 5×CAN 通知对端; 帧等待
  超时 `CFG_ZMODEM_TIMEOUT_MS` (默认 10s, 对齐 lrzsz);
- **输出纪律**: 协议字节与控制台文本共用 UART。接收端对帧前字节一律
  按垃圾跳过, 因此状态文本只在帧间空闲点输出 (会话/文件开始/结束),
  数据流中不打印, 避免干扰主机端流式发送;
- 主机互通已在 `cargo test` 中自动化: 单测含 CRC 标准向量、转义/帧
  往返与收发双端内存回环; 若宿主机装有 lrzsz (`/usr/bin/sz`/`rz`),
  还会自动运行真实 `sz` → 板端接收、板端发送 → 真实 `rz -y` 的互通
  测试 (含多文件与空文件)。

常用流程 (115200 8N1 串口):

```text
# 板端发送 → 主机接收
root@HC32F460JEUA:/$ sz /etc/config
# 主机: rz -y

# 主机发送 → 板端接收
root@HC32F460JEUA:/$ rz
# 主机: sz /path/to/file
rz: 已接收 file.txt (2048 B)
```

## 片内 SRAM (sram)

对齐 DDL v3.3.0 `hc32_ll_sram.c/h` 与参考手册表 8-1:

- 布局: SRAMH 32K (0x1FFF8000) / SRAM1·2 各 64K / SRAM3 28K (栈区,
  0x20020000~0x20026FFF) / Ret 4K; SRAMH/1/2/Ret 偶校验 (恒使能),
  **SRAM3 用 ECC** (CKCR.ECCMOD 配 MD1~3);
- **等待周期**: `set_wait_cycles(hclk)` 按表 8-1 自动配置 (SRAMH 恒 0,
  SRAM1/2/Ret >100MHz→1, **SRAM3 恒 1** —— 栈顶在 SRAM3 末尾的脚注
  要求), 由 `clk` 切换时钟时调用 (从原 clk 模块迁入); `wait_cycles_now`
  读取当前配置;
- **错误检测**: 奇偶/ECC 错误经 NMI 上报 (CKCR.PYOAD/ECCOAD 可改复位),
  `error()` 查询 / `clear_status()` 清除 / `set_fault_action()` 配置动作 /
  `set_ecc_mode()` 配置 SRAM3 ECC 模式;
- 启动阶段 (`startup.rs`) 先进入无函数序言、无栈访问的汇编入口，配置
  SRAM3 等待周期、回读确认并执行 `DSB`/`ISB`，随后才启用 FPU/惰性
  上下文并跳入 Rust；回读失败会在使用 SRAM3 栈前 fail-stop。该寄存器
  序列与 DDL `SetSRAM3Wait` 一致;
- 寄存器写保护: WTPR/CKPR 键值 0x77 解锁 / 0x76 锁定。

## CRC 硬件加速器 (crc)

对齐 DDL v3.3.0 `hc32_ll_crc.c/h`:

- 多项式硬件固定: CRC16 = 0x1021 (X25/CCITT 系), CRC32 = 0x04C11DB7 (IEEE 802.3);
- `Config` 预设标准组合: `x25()` / `ccitt_false()` / `crc32()` /
  `crc32_mpeg2()` (初值 + REFIN/REFOUT/XOROUT 开关);
- 输入宽度: 8/16/32 位 (`DataWidth`), 写 DAT0 即触发 (硬件流水);
- 一次性计算 `calculate(data, width, cfg)`; 分帧累加
  `init` + `accumulate`×N + `result()` (可 `set_init_value` 中途重置);
  `check()` 与期望值比较;
- 结果格式: REFIN+REFOUT+XOROUT 全使能时即标准 CRC (与软件按位建模
  逐位一致, 标准向量已验: "123456789" → X25=0x906E / CRC32=0xCBF43926);
- 时钟门控 FCG0.bit23 (FCG0PC 键 0xA5A50001 解锁), 模块初始化时自动使能;
- 自检 (`selftest` 命令) 含 CRC 实测: 四个标准配置计算 "123456789"
  并与标准向量比对。

## 实时时钟 (rtc)

对齐 DDL v3.3.0 `hc32_ll_rtc.c/h`:

- 时钟源: **LRC** (内部 32.768kHz, 无外部器件, 默认; JEUA 48pin 无
  XTAL32 引脚对) / XTAL32 (需自行启动晶振);
- 时间/日期寄存器 **BCD** 编码, 24/12 小时制可配; 读/写自动进出
  **RW 模式** (CR2.RWREQ/RWEN); `set_time`/`get_time`/`set_date`/`get_date`
  返回 `Result`，硬件状态切换超时不会被静默忽略；完整事务由临界区串行化;
- 周期中断: 0.5s/1s/1min/1hour/1day/1month (CR1.PRDS);
- 闹钟: 时+分匹配 + 星期位掩码 (0x7F=每天), 事件源 `intc::src::RTC_ALM`
  (81) / `RTC_PRD` (82);
- 无 VBAT 备份域: VDD 供电, 掉电后需重新初始化 (软件复位 + 重设);
- **日志时间戳**: `CFG_RTC_ENABLE` 启用时开机初始化 (LRC/24H, 基准
  2000-01-01 00:00:00), 日志输出带 **`[天:时:分:秒]`** 前缀
  (自启动起的运行时长, Howard Hinnant 公历算法跨月/闰年正确,
  RTC 未运行时省略)。

## 启动流程

```
reset_handler (startup.rs)
 ├─ 无栈汇编: SRAM3 等待周期 → DSB / ISB
 └─ reset_handler_rust
     ├─ FLASH 等待周期 / FPU 使能
     ├─ .data 拷贝 / .bss 清零 / 主栈 canary
     └─ main
         ├─ Board::init()              # 时钟/MPU/GPIO/SysTick/UART/RTC
         ├─ rtos::init()               # PendSV/SysTick 优先级 + 空闲线程
         ├─ rtos::thread_create(...)   # 创建 LED / Shell 线程
         └─ rtos::start()              # 首次切换, 永不返回
             └─ shell_entry
                 ├─ filesystem::start  # 挂载；仅全擦除的新分区自动格式化
                 └─ login / 命令循环
```

复位向量的第一阶段不能使用普通 Rust 函数：复位时 MSP 已位于 SRAM3，而
SRAM3 的安全等待周期尚未建立，编译器生成的函数序言可能在第一条业务指令
前压栈。只有完成寄存器配置和屏障后，第二阶段才允许使用栈及 Rust 代码。

## Panic / Fault 诊断

fault 汇编入口在生成任何 Rust 函数序言前捕获 `IPSR`、`EXC_RETURN` 和
现场 `r7`，按 `EXC_RETURN.bit2` 选择 MSP 或 PSP，并按 bit4 定位基本帧
或 FP 扩展帧后的基本寄存器区。读取前会验证 `EXC_RETURN` 编码、地址范围、
对齐和加法溢出；CFSR 指示压栈/出栈错误时不读取不可信帧。帧指针回溯只沿
向更高地址单调前进且完全位于 SRAM 的链继续，返回地址还必须落在 Flash
Thumb 代码范围内。

## RTOS 内核

架构移植自 RT-Thread v5.2.2 (`CRust/src/libs/rtos/`):

| RT-Thread 源文件 | 本模块 | 内容 |
|---|---|---|
| `scheduler_up.c` | `sched` | 位图就绪表 + 时间片轮转 |
| `thread.c` | `thread` | 线程创建/退出/延时/挂起 |
| `idle.c`/`defunct.c` | `idle` | 空闲线程 + 僵尸回收 |
| `timer.c` | `timer` | 有序链表硬定时器 |
| `ipc.c` | `ipc` | 信号量/互斥量/事件/邮箱/消息队列 |
| `context_gcc.S` | `context` | PendSV 上下文切换 (PSP + M4F 惰性 FPU 上下文) |

### 使用流程

```rust
pub extern "C" fn sys_tick_handler() {
    rtos::tick_increase();          // 1. SysTick ISR 驱动节拍
}
rtos::init();                       // 2. 初始化内核
rtos::thread_create("led", 2048, 2, 10, led_thread, 0); // 3. 创建线程
rtos::start();                      // 4. 启动调度器 (永不返回)

extern "C" fn led_thread(_p: usize) {
    loop {
        board::BoardResources::get().toggle_led();
        rtos::thread_delay_ms(500).expect("LED 延时必须在线程上下文");
    }
}
```

- 优先级:0(最高)~ 31(最低,空闲线程);时间片单位 = 节拍,实际时长由
  `CFG_TICKS_PER_SEC` 决定;
- `thread_delay` / `thread_delay_ms` / `yield_now` 返回 `Result<(), Error>`：
  调度器启动前返回 `KernelNotStarted`，中断上下文返回 `InterruptContext`；
  `thread_delay_ms` / `Timer::start_ms` 将非零毫秒向上取整到至少 1 tick，
  `Timer::start` 等原始接口仍以 tick 为单位；零延时定时器在下一 tick
  触发，所有有限延时钳位到回绕安全的 `i32::MAX` tick;
- 线程栈由堆分配,打印线程建议 ≥2KB (debug 构建下格式化打印栈消耗较大),
  调度器在每次切换时检测栈溢出;
- 带 `Timeout` 且返回 `Result` 的 IPC 操作要求调度器已经启动，否则返回
  `KernelNotStarted`；ISR 中只允许 `Timeout::Ticks(0)` 非阻塞探测，任何
  可能阻塞的 timeout 返回 `InterruptContext`。`release`/事件 `send` 等
  唤醒操作及 `Timer::start/stop` 可在 ISR 使用，定时器回调本身也不得阻塞;
- `Thread` 是不可变 `Arc` 外壳加唯一的 `UnsafeCell<ThreadInner>` 可变存储。
  TCB 字段和侵入式链表只在单核关中断临界区访问，公共句柄不暴露内部引用；
  `kernel_self` 在原始指针、链表节点和线程内建定时器仍活动时维持 TCB 存活。
  `unsafe Thread::force_delete` 会跳过目标 Rust 栈析构，仅能用于已证明无
  活动借用、守卫或待析构资源的线程；常规停止应由入口协作返回;
- `Timer` 的节点会进入全局侵入式链表，因此类型为 `!Unpin`，公开
  `start/start_ms/stop/is_active` 均要求 `Pin<&Timer>`。静态对象使用
  `pin_static()`，堆对象使用 `Box::pin`；`Drop` 会在临界区内自动摘链，
  防止释放后遗留悬垂节点。其他 IPC 对象可按普通 Rust 所有权规则使用;
- PendSV 逐线程保存 `r4-r11` 和 `EXC_RETURN`；bit4 为 0 时再保存
  `d8-d15`，硬件扩展帧负责 `s0-s15/FPSCR`。新线程以
  `0xffff_fffd` 基本帧启动，未使用 FPU 的线程不承担浮点保存开销。

### IPC 速查

- **阻塞语义对齐 RT-Thread**：Semaphore 唤醒即转移 token，Mutex 唤醒即
  转移所有权；Mailbox/MessageQueue 的发送与接收在唤醒后回到临界区重查
  环形缓冲或空闲块，避免满队列发送丢消息和空队列接收假超时;
- `Timeout::Forever` 使用独立枚举分支，不再与 `u32::MAX` 共用哨兵；
  `Ticks(u32::MAX)` 仍是有限等待，并钳位到定时器半周期;

```rust
static SEM: Semaphore = Semaphore::new(0, 1);
static MUT: Mutex<u32> = Mutex::new(0);           // 优先级继承 + RAII guard
static EVT: Event = Event::new();
static MB: Mailbox<usize> = Mailbox::new(4);      // 类型安全的机器字消息
static MQ: MessageQueue = MessageQueue::new(32, 4);

fn use_ipc() -> Result<(), Error> {
    SEM.take(Timeout::Forever)?;
    SEM.release();

    let mut guard = MUT.lock(Timeout::Forever)?;
    *guard += 1;
    drop(guard); // 自动解锁；不能跨线程移动或重复解锁

    EVT.send(0x01);
    let _flags = EVT.recv(0x01, EventOpt::OrClear, Timeout::Ticks(3000))?;
    MB.send(42, Timeout::Forever)?;
    let _message = MB.recv(Timeout::Forever)?;
    MQ.send(b"hi", Timeout::Forever)?;
    let mut buf = [0u8; 32];
    let _len = MQ.recv(&mut buf, Timeout::Forever)?;
    Ok(())
}
```

### 打印系统 (console)

- `println!` 整行原子输出:内容 + CRLF 在同一次加锁内完成 (优先级继承互斥量),
  多线程输出不交错;等待打印锁的高优先级线程会把持有者提升到自己的优先级,
  **不会出现高优先级线程无界等待低优先级线程**;
- **就绪门**: UART 初始化完成前 (`console::mark_ready` 前) 的打印静默丢弃,
  防止在 UART 时钟未使能时访问 USART 导致 TXE 等待死循环 (早期 boot 日志
  不会丢失 —— 它们本就低于默认日志阈值);
- 中断上下文 / panic 诊断走无锁通道 `write_fmt_raw` (仅诊断, 可能交错);
- 调度器启动前 (boot 阶段) 自动退化为无锁输出;
- 注意:UART 为 115200 无流控,输出速率接近 PC 读取能力时 CH340 缓冲可能
  溢出丢字节 (表现为行尾截断/乱码, 与打印交错无关)。整行输出默认带
  行间间隙 (`CFG_CONSOLE_LINE_GAP_MS`, 0 关闭) 让 PC 端及时读取;
  若仍丢字节请调大该值 (或改用带流控的接口)。

## 构建 / 烧录 / 调试

目标: `thumbv7em-none-eabihf`,自定义链接脚本 `link.ld`
(固件 FLASH 368K + 文件系统 128K + 自检/交换保留 16K；RAM 188K、8K 主栈、
`.heap` 段)。链接断言保证固件不会增长覆盖文件系统分区。

```bash
cargo build                          # debug 构建
cargo build --release                # release 构建
SOURCE_DATE_EPOCH=1767225600 cargo build --release # 固定横幅 UTC 构建日期
cargo run                            # 构建 + 烧录 (pyocd, 见 scripts/flash.sh)
```

`SOURCE_DATE_EPOCH` 只固定 `build.rs` 注入的横幅日期；完整 bit-for-bit
可复现构建仍要求相同的 Rust 工具链、目标、链接器、配置和其他构建输入。

`debug` 构建默认已启用 `opt-level = 1` (见 `Cargo.toml` `[profile.dev]`):
保留 debuginfo、帧指针和 panic 栈回溯所需信息；`release` 构建采用
`opt-level = "z"` + `lto = "fat"` + `codegen-units = 1` (体积优先)。
当前工具链与全功能配置下 `arm-none-eabi-size` 测得 `text + data`
约为 140.7KiB (release)；具体结果会随 Rust/LLVM 版本与功能增减而变化。
体积优化手段 (已落地): zmodem FCS 查表改逐位计算 (−1.7KiB)、报告文件名
排序改插入排序 (−3.9KiB)、panic 诊断文本按 `CFG_PANIC_VERBOSE` 分级
(−0.6KiB)、HTML 报告模板精简 (−0.4KiB)、shell/soak 帮助文本精简、
UART 波特率/CAN 位时序/PLL 时钟 u64 除法改 u32 数学。

**体积优化与功能裁剪**: 除 `CFG_*` 运行时配置外, 大体积功能均为
**编译期开关** (`build.rs` 把配置翻译为 `#[cfg]`, 关闭时连代码一起
不编译):

| 开关 | 功能 | 全关相比全开约省 |
|---|---|---|
| `CFG_SOAK_ENABLE` | soak 长稳测试 + HTML 报告模板 | ~30 KiB (含依赖) |
| `CFG_APP_SELFTEST_ENABLE` | selftest 内核自检 | ~19 KiB |
| `CFG_SHELL_ZMODEM_ENABLE` | sz/rz 文件传输 | ~10.5 KiB |
| `CFG_SHELL_NANO_ENABLE` | nano 全屏编辑器 | ~8 KiB |

四项全部关闭时 `text + data` 约 74.8KiB (相比全开省 ~67 KiB)。
注: soak 依赖 selftest 的 ESC 中断, soak 开启时 selftest 自动随带编译。

### 烧录 (pyocd)

```bash
pyocd list                           # 列出可用调试器
pyocd flash -u <调试器ID> --target hc32f460xe target/thumbv7em-none-eabihf/release/hc32f460.elf
```

可选参数: `--base-address 0x00000000`、`--erase auto`。

### GDB 调试

```bash
pyocd gdbserver --target hc32f460xe  # 默认端口 3333
arm-none-eabi-gdb -q target/thumbv7em-none-eabihf/debug/hc32f460.elf
target extended-remote localhost:3333
monitor reset halt
load
continue
```

### 串口控制台

- 板载 USB 串口 (CH340, 如 `/dev/ttyUSB0`),115200 8N1;
- 终端: `minicom -D /dev/ttyUSB0 -b 115200` 或 `screen /dev/ttyUSB0 115200`
  (建议使用交互式终端; `cat` 读取不及时会丢字节);
- 启动横幅 (`banner::show()`): 先**清屏分隔** (仅清可视区, 保留滚动缓冲),
  再输出块字符大标题 + 内核信息面板
  (CPU 频率/节拍/堆大小/构建日期/rustc 版本/就绪线程数)。

### 应用日志 (log)

与**内核打印分离**的可开关诊断输出 (`src/log.rs`):

- **分层**: 内核打印 (启动横幅/panic 诊断/shell 输出, 经 console 打印锁)
  **无论如何都输出**; 应用日志是可选层, 输出与否 = (全局开关 × 级别阈值);
- **级别与色彩**: `error`(红) / `warn`(黄) / `info`(绿) / `debug`(青) /
  `trace`(白), 整行按级别着色 (`[ERR]`~`[TRC]` 标签), 整行原子输出 (不交错);
  落盘文件为无颜色纯文本;
- **宏**: `log_error!` / `log_warn!` / `log_info!` / `log_debug!` /
  `log_trace!` (线程上下文使用, 与 `println!` 同约束);
- **默认值来自配置**: `CFG_LOG_ENABLE` (默认开启) + `CFG_LOG_LEVEL`
  (默认 `info`, 输出 ≤ 阈值的级别); 非法值编译期报错;
- **自动落盘** (`logfile` 线程, 见下文): 每条日志同时以无颜色格式追加
  到 RAM 缓冲 (`src/logring.rs`, 容量 `CFG_LOG_RING`, 满时丢弃最旧),
  由日志落盘线程周期性写入 `/log/logN.log`;
- **运行时控制** (shell 命令, 重启后恢复配置默认):
  - `log` — 显示当前开关、级别与落盘状态;
  - `log on` / `log off` — 切换日志开关;
  - `log level error|warn|info|debug|trace` — 调整级别阈值;
  - `log file` / `log file on` / `log file off` — 查看/切换日志落盘。

### 日志自动保存 (logfile)

日志除终端输出外**自动保存到文件系统 `/log/` 目录** (`src/logfile.rs`):

- **单次格式化扇出**: 每条日志只做**一次** `core::fmt::write`, 控制台
  (整行着色) 与 RAM 落盘缓冲 (无颜色纯文本) 在打印锁内经同一格式化流
  同时输出 (`console::write_fmt_line_fanout` 的 `side` 回调 + 环条目
  句柄 `EntryMark` 增量构建), 不再双次格式化;
- **零中间分配落盘**: 独立 `logfile` 线程 (优先级
  `CFG_APP_LOGFILE_PRIORITY`) 每 `CFG_LOG_FLUSH_MS` 唤醒一次, 把 RAM
  缓冲**直接排空进文件镜像** (镜像常驻内存复用), 整文件原子写
  `/log/`; 缓冲为空时零 Flash 写入;
- **文件名严格有序**: 文件名为 `boot_<序号>[_<段号>].log`, 序号为
  **u64 单调递增** (20 位零填充), 每次启动 = 上次各文件最大序号 + 1 ——
  **字典序即时间序, 后续生成的文件保证严格有序**, 无需读内容即可判断
  新旧 (`ls` 直接按时间排列); 镜像超过 `CFG_LOG_FILE_MAX` (默认 4KiB)
  时段号 +1 另起文件;
- **跨重启不覆盖**: 保留最近 `CFG_LOG_FILE_SLOTS` (默认 4) 个文件,
  超预算时删除最旧 `(序号, 段号)` —— 普通重启**不会**覆盖上次日志,
  只有文件数超过预算才淘汰最旧; 旧版本固件的 `logN.log` 识别为最旧并
  最终淘汰, 迁移不破坏历史;
- **reboot 先落盘**: shell `reboot` 命令复位前先同步执行一次
  `logfile::flush_now()` (记录 "系统重启" 日志并排空缓冲), 再触发复位;
  日志线程与 reboot 的刷新经文件系统互斥量 + 落盘状态锁串行化, 不会
  互相覆盖;
- **共享文件系统**: 快照文件系统的实例移入全局
  `Mutex<Option<FileSystem>>` (优先级继承), shell 命令与 logfile 线程
  分时独占; `InternalFlash` 的 `!Send` 标记经一处带安全契约的
  `unsafe impl Send` 放宽 (所有访问经互斥量串行化, 中断上下文被阻塞
  检测拒绝, 单核临界区保证一致性);
- **写放大与磨损**: 快照文件系统每次提交重写整个快照, 因此仅当缓冲有
  日志时才落盘 (空闲零 Flash 写入), 刷新间隔不宜过小 (`CFG_LOG_FLUSH_MS`
  默认 2s); 文件系统空间不足时自动删除最旧日志文件后重试;
- **掉电窗口**: 非 `reboot` 路径的掉电/复位最多丢失最近一个刷新间隔 +
  RAM 缓冲内的日志 (ring 容量 `CFG_LOG_RING`, 默认 4KiB); panic/fault
  诊断仍只走控制台 (Flash 写入在异常上下文有风险);
- 查看: `cat /log/boot_00000000000000000001.log` (文件名即启动批次,
  字典序即时间序)。

### 终端 (仿 Ubuntu shell)

- 启动后先登录: 用户名 + 密码 (密码不显示), 配置见 `.cargo/config.toml`
  的 `CFG_SHELL_*` (编译期读取, 改密码无需改代码);
- 密码错误次数可配置 (默认 3 次), 超限提示 "Too many login failures";
- 命令提示符包含当前路径，例如根目录为 `root@HC32F460JEUA:/$`，进入
  `/etc` 后为 `root@HC32F460JEUA:/etc$`;
- **命令系统**: 命令注册在 `src/shell.rs` 的静态命令表 [`COMMANDS`]
  (名称/别名/帮助/执行函数), 分发与实现解耦; **新增命令 = 表内追加一项
  + 加入 `CFG_SHELL_COMMANDS` 启用列表**, 无需修改分发/帮助逻辑;
- **每个命令可单独启用/禁用**: `CFG_SHELL_COMMANDS` 为逗号分隔的命令名
  列表, 未列出的命令执行时提示 "未启用" 且不出现在 `help` 中;
- **大功能另有编译期开关**: `nano` / `sz` / `rz` / `selftest` / `soak`
  除受 `CFG_SHELL_COMMANDS` 控制外, 还受各自的 `CFG_SHELL_NANO_ENABLE` /
  `CFG_SHELL_ZMODEM_ENABLE` / `CFG_APP_SELFTEST_ENABLE` / `CFG_SOAK_ENABLE`
  编译期开关约束 — 关闭时命令与代码整体不编译 (见"体积优化"一节);
- 命令: `help` / `sysinfo`(info) / `uptime` / `ps` / `free`(mem) / `echo` /
  `history` / `pwd` / `cd` / `ls` / `mkdir` / `rmdir` / `cat` / `write`(put) /
  `nano` / `sz` / `rz` / `rm` / `mv` / `stat` / `df`(fsinfo) / `fsck` / `mount` /
  `mkfs --force` / `led` / `log` / `selftest` / `soak` / `clear` / `whoami` / `reboot` /
  `logout`(exit);
- 输入: 回车提交, 退格删除, Ctrl+C 清行, 方向键上/下浏览历史；
- 输入采用中断驱动 (RX ISR 发出 OS 无关通知, `uart_rtos` 释放信号量,
  线程阻塞等待, 无轮询)。

### 自检 (selftest)

- 不再开机自动运行: 由 `CFG_APP_SELFTEST_ENABLE` 控制启用 (默认 `true`),
  shell 中输入 `selftest` 或 `selftest all` **同步执行**全量项目；
  `selftest can` 只执行 CAN 内部回环，完成后才输出下一命令提示符;
- **ESC 中断**: 执行期间按 ESC 立即停止剩余项 (终端输入在自检期间
  一律丢弃, ESC 除外), 汇总提示 `被中断 (ESC): 已完成 N 项`;
- 测试对象 (信号量/互斥量/事件/邮箱/队列) 每次运行**全新创建** (局部
  变量), 多次执行结果确定; 自检在 shell 线程内同步运行, shell 线程
  栈因此配置为 8KB (`CFG_APP_SHELL_STACK`);
- `CFG_APP_SELFTEST_ENABLE = false` 时 selftest 命令与其代码**不编译**
  (编译期开关, 省 ~19 KiB; soak 开启时自动随带编译);
  `CFG_CAN_SELFTEST_ENABLE = false` 时 CAN 项显示为跳过;
- 全量自检依次验证信号量 / 互斥量 (非递归) / 事件 (AND/OR/清除) / 邮箱 (含紧急
  插队) / 消息队列 (含二进制) / 线程延时 / 线程删除 (delete) / 线程自然
  退出 (defunct 回收) / Flash / CRC / CAN;
- CAN 项使用内部回环，不驱动 TX 引脚且由控制器自动 ACK；覆盖过滤器 mask
  与标准/扩展类型隔离、PTB 标准数据帧、STB 扩展数据帧与远程帧、RX FIFO
  顺序、状态和错误计数。测试会进入本地复位并清空硬件收发队列，因此
  `CFG_CAN_ENABLE=true` 时明确 **SKIP**，不会接管业务 CAN；默认关闭应用 CAN
  时若仍有通过驱动注册的 IRQ consumer 也会 **SKIP**。测试结束会关闭 CAN
  外设时钟，并恢复测试前的 XTAL 启停状态;
- 逐项结果 (`[PASS]`/`[FAIL]`/进度) 走**应用日志** (info/debug 级, 可经
  `log` 命令控制); `log level trace` 可输出**每项执行细节** (实际返回值/
  耗时/参数, 用于故障定位); **汇总始终打印** (不受日志开关影响):
  `[selftest] 完成: N 通过, 0 失败`。

### 长期稳定性测试 (soak)

- **目标**: 在苛刻条件下长时间持续运行, 验证"系统可放心长期使用"——
  与 selftest 的"功能一次正确"互补, soak 验证的是"连续运行不退化";
- **用法 (产品场景化)**: shell 中输入 `soak [分钟] [压力项|场景]...`:
  - `soak` — 全量压力 (时长取 `CFG_SOAK_MINUTES`, 默认 10 分钟);
  - `soak 480` — 全量压力 8 小时; `soak 0` — 直到按 ESC;
  - `soak 480 basic` — RTOS 核心场景 (调度/IPC/堆/线程/定时器/中断);
  - `soak 480 periph` — 外设场景 (Flash 擦写/CRC/CAN 回环);
  - `soak 60 can flash` — 专项验证 (按短名任意组合, 便于现场与业务共存);
  - 期间 shell 兼任监控器, **控制台只刷新单行进度条** (每秒 `\r` 原位
    刷新: 已用/总时长 + 百分比 + 堆用量 + 线程数 + 错误数), 输出保持
    简洁; 完整的测试结果/数据/关键信息均写入**单个 HTML 报告**
    (`/test/soak_<时间戳>.html`, 内联 CSS 深色主题, 可离线打开,
    `CFG_TEST_REPORT_SLOTS` 控制保留数量); 堆峰值突破趋势预警仍即时
    输出 (疑似泄漏早期发现);
- **压力线程** (21 个, 按场景分组): 调度器 (双 CPU 计算线程, 抢占+时间片) /
  信号量 / 互斥量 (双线程竞争 + 整除性/单调性/最终值三重校验, 捕获丢失
  更新) / **优先级继承 (中优先级霸占 CPU 时高优先级获锁等待有界,
  无继承必超时)** / 事件 / 邮箱 / 消息队列 / 堆 (随机分配+模式回读,
  `try_reserve` 优雅处理 OOM: 分配失败计数而非 panic 复位, 同时验证
  allocator 在极限下优雅拒绝) / 线程创建回收 (defunct 无泄漏) / 内核
  定时器漂移 / **中断上下文压力 (2ms 高频回调, 验证中断路径长期稳定)** /
  调度延迟 / Flash 擦写 (节流) / CRC (硬件 vs 软件参考) / CAN 内部回环
  (可用时);
  **每个压力线程每轮至少让出 1ms**, 避免某对线程形成"满↔空"忙碌乒乓
  饿死低优先级线程 (该问题曾导致 CAN/CRC/MQ 压力线程 0 循环);
- **监控判定** (每秒):
  - **停滞检测**: 任一压力线程心跳超过 `CFG_SOAK_HANG_GRACE_MS` 未推进
    → 判挂起 (IPC 丢失唤醒/死锁/调度停滞), **立即输出警告**指明线程与
    停滞时长;
  - **错误**: 任一压力线程报错 → **立即输出失败日志** (线程名/错误类型/
    第几个循环/运行时长), 并提前结束给出 FAIL 汇总;
  - **SRAM 奇偶/ECC**、堆用量峰值与峰值突破趋势、线程数、各线程栈水位
    持续采样; 另采集 **CPU 利用率 (空闲迭代估算)**、**上下文切换次数**、
    **堆最大连续空闲块 (碎片化证据)**;
- **结束报告**: 运行时长 / 本次压力项回显 / PASS·FAIL 结论 / 每个
  压力线程的循环数与错误数 (**失败时附带首次失败的位置与时刻**) /
  0 循环线程告警 / 堆基线→峰值→结束 (泄漏判定) + **堆增长率** (B/h) /
  线程数泄漏判定 / 互斥量最终值校验 / **调度延迟分布 (样本数 +
  p50/p90/p99/最坏值 + 直方图)** / **看门狗状态** / 各线程栈峰值
  百分比、**CPU 利用率**、**上下文切换/秒**、**堆碎片 (最大连续空闲块)**、
  **分配失败计数**、**优先级继承最长等待**、**看门狗实测喂狗余量**、
  **系统画像 (主频/RTOS 配置/固件版本)**、**判定准则表 (每条判定的
  阈值 + 结果, 可审计)** 与**结论区 (证明了什么 + 测试局限)**——全部
  写入 `/test/soak_<时间戳>.html` (RTC 未运行时文件名退化为启动序号;
  报告超预算自动删除最旧), 控制台只打印
  `结果: PASS/FAIL — 报告: /test/soak_xxx.html` 一行 (报告写入失败
  时回退为两行紧凑摘要, 结果不丢失);
- **推荐实践**: 正式部署验证时开启 `CFG_WDT_ENABLE=true` —— 若调度器
  彻底停滞, 硬件看门狗会复位系统并留下失败证据 (汇总报告会标注本次
  看门狗是否生效); 建议连续运行至少 **8 小时** (如 `soak 480`) 或过夜
  (如 `soak 720`), Flash 压力默认 10s 一次擦写循环, 24 小时约 8.6k 次,
  远低于片内 Flash 寿命上限;
- `CFG_SOAK_ENABLE = false` 时 soak 命令与其代码 (含 HTML 报告模板)
  **不编译** (编译期开关, 省 ~54 KiB); 失败日志由压力线程
  即时输出 (console 整行原子, 不与其他线程输出交错), 运行期间控制台
  仅保留进度条, 其余信息 (含各线程启动明细) 均进报告与 debug 级日志。

### CAN 驱动

- 实现 HC32F460 单路**经典 CAN 2.0B**；TTCAN 扩展暂未配置。板级持有唯一
  `Can` 句柄，裸驱动 API 提供 PTB 非阻塞发送、4 槽 STB 入队/单帧或全部
  启动/中止，以及 10 槽 RX FIFO 非阻塞读取;
- 支持 11 位标准 ID、29 位扩展 ID、数据帧和 RTR，最多 8 个验收过滤器；
  同时提供 DDL 布局的状态快照、W1C 标志清除、仲裁丢失/错误类型、REC/TEC
  计数和 CAN 聚合中断注册/注销;
- CAN 通信时钟固定来自 `XTAL`。初始化按 RM Rev1.71 校验
  `EXCLK >= 1.5 * CANCLK`；纯 Rust 位时序搜索遵循 DDL 寄存器边界，并排除
  RM 不建议使用的实际预分频 1。默认 8MHz / 500kbps / 75% / SJW=2 对应
  `PRESC=2, SEG1=6, SEG2=2`，SBT 为 `0x01010104`;
- `.cargo/config.toml` 的 `CFG_CAN_*` 控制启用、PortB 引脚复用、位速率、采样点、
  SJW、最大误差、工作模式、single-shot、STB 优先级、RX 阈值/溢出策略、
  self-ACK、启动过滤器和 selftest 超时；非法枚举、范围、ID 或不可实现的
  位时序在编译期失败。TX/RX 必须分别使用支持 Func_Grp2 的不同 PortB 引脚
  及 Func50/51；默认是 JP2 上的 PB7/PB6。驱动 API 本身支持 8 个过滤器，
  启动配置提供 1 个;
- **本板没有板载 CAN PHY**：PB7(TX)/PB6(RX) 仅引到 JP2。正常模式和外部
  回环必须外接匹配电平的 CAN 收发器，并按总线拓扑配置终端电阻；不能将
  MCU 引脚直接接到 CANH/CANL。默认 `CFG_CAN_ENABLE=false`，内部回环
  selftest 不依赖 PHY;
- 多数 `RTIF` 标志只有对应中断使能位打开后才会置位。默认配置打开全部经典
  CAN 事件以支持轮询状态；调用方可注册聚合 IRQ，或按需修改 `Interrupts`。
  轮询与 ISR 不应同时消费同一 RX FIFO/完成标志；`init`/`deinit` 会进入本地
  复位并清 FIFO，调用前必须由应用层停止业务收发和 IRQ consumer。

### UART 驱动 (USART1~4)

- `UartConfig` 对齐 DDL `stc_usart_uart_init_t`: 波特率 / 过采样 (8·16) /
  时钟预分频 (1·4·16·64) / 数据位 (8·9) / 校验 (无·偶·奇) / 停止位 (1·2) /
  首字节 (LSB·MSB) / CTS 硬件流控 / 噪声滤波; 默认 115200 8N1,
  全部可经 `CFG_UART_*` 配置 (编译期校验);
- 波特率: 纯整数计算 DIV_INT/FRAC (与 DDL `USART_CalculateBrr` 一致),
  小数分频自动使能 FBME;
- 引脚复用: 功能号见 `gpio::func` 常量 (数据手册表 2-2, USART1=32/33,
  USART2=36~39, USART3=48~51, USART4=52~55); 各 USART 均挂 PCLK1;
- 发送: `write_byte` / `write_word` (9 位模式) / `write` / `write_str` /
  `flush` (等待发送完成 TC);
- 接收 (中断驱动): `enable_rx_interrupt` 注册 INTC 通道 + NVIC +
  `CR1.RIE` (对齐 DDL `INTC_IrqSignIn` / `USART_FuncCmd`);
- 接收中断把字节写入环形缓冲 (大小 `CFG_UART_RX_BUF_SIZE`, 溢出丢弃
  新字节), 应用侧 `rx_count()` / `read_rx()` / `drain_rx()` 非阻塞读取,
  `rx_dropped_count()` 读取软件丢包计数;
- 裸 UART ISR 只发出 OS 无关通知; `uart_rtos::UartRtosExt` 通过容量为 1
  的信号量提供 `read_rx_blocking()`, 重复通知可合并,接收环仍是数据真值;
- 错误处理对齐 `USART_ClearStatus`: 读 RDR 清 RXNE, 写 CR1 的
  CPE/CFE/CORE 清 PE/FE/ORE; ISR 同时累加 PE/FE/ORE 计数,
  `rx_error_counts()` 读取并清零 (诊断波特率/接线/读取不及时);
- 外设中断通过 `vector_table::register_irq` 分发 (INT000~007 槽位,
  向量表在 FLASH, 槽位预置分发入口, 回调运行时注册);
- 真机验证 (115200, PC→板): ASCII/二进制/混合/连续数据均完整接收,
  500B 单包零丢失。

### DMA 驱动 (DMA1/DMA2)

对齐 DDL v3.3.0 `hc32_ll_dma.c/h` 与示例 `dmac_base` / `usart_uart_dma`,
寄存器级实现 (`src/dma.rs`), 零依赖:

- **硬件模型**: 外设事件 → AOS `DMAx_TRGSELy` 路由 → DMA 通道触发传输;
  每请求移动一块 (BLKSIZE ≤ 1024 项), 计数 (CNT ≤ 65535) 归零 → TC 标志
  + 通道自动失能; 软件触发 (SWREQ 解锁键 0xA1) 支持内存→内存搬运;
- **Rust 安全边界**: 单元/通道编码在 `Dma<UNIT, CH>` const 泛型中
  (越界编译期报错); `Dma::take()` 全系统唯一占用 (位图), 防止同一
  通道被重复配置; 地址/计数写入经 MON 影子寄存器回读确认, 修改 CHEN
  前等待其他通道空闲 (对齐 DDL `DMA_ChCmd` 的硬件约束);
- **中断**: `install_tc_irq` / `install_err_irq` 经 `intc::register` 路由
  DMA_TCx/DMA_ERR 事件, 回调通过原子槽位安装 (ISR 内清标志 + 通知);
- **预留能力**: 外设→内存 (RX) 路由 `route()` + LLP 重配置
  (`llp_enable` / `reconfig_llp` / `reconfig_cmd` / `sw_reconfig`) 可实现
  循环接收 (对齐 DDL 示例的 reconfig 流程); `copy_blocking` 阻塞式大块
  拷贝 (32 位宽度自动分块);
- **当前集成 — 控制台 UART 发送卸载**: `Uart::write` 对长度 ≥
  `CFG_DMA_TX_MIN` 的输出自动改用 DMA 整块发送, 长输出 (zmodem `sz` /
  `cat` / soak 报告 / 日志) 不再长时间占用 CPU; 通道忙 / 输出过短 /
  中断上下文 (panic 诊断) 时自动回退逐字节轮询, 行为与原来一致。
  **触发机制 (关键)**: DMA 请求为边沿捕获, 每次发送按 DDL 示例
  `usart_uart_dma` 的序列执行 —— 等待上次发送完全结束 (SR.TC) →
  `TE=0` (TXE 复位, 清陈旧请求) → 使能 DMA 通道 → `TE=1` (新的 TXE
  上升沿触发首字节) → 后续字节由每次 TXE 上升沿自然驱动;
- **当前集成 — Flash→RAM 大块读取加速**: 文件系统读 (`read`) / 擦除
  校验 (`verify_erased` / `partition_is_erased`) / 写后回读校验
  (`program` 回读) 对长度 ≥ `CFG_DMA_COPY_MIN` 的请求改用 DMA 整块
  拷贝 (`copy_try`, 通道由 `CFG_DMA_COPY_UNIT`/`CFG_DMA_COPY_CHANNEL`
  配置且编译期校验与 TX 通道不冲突), 比逐字节/逐字循环快约一个数量级;
  未接管时回退原路径, 行为不变;
- **暂不采用 DMA 的位置**: UART 接收 (每字节中断在 115200 下 <5% CPU,
  未知帧长需 RTO+reconfig 重写输入路径, 收益小于风险, 原语已预留);
  Flash 编程/擦除 (写地址触发 + 逐字等待 OPTEND, bus-hold 使 DMA 同样
  被 stall, 无法替代); CAN (无 DMA 请求线);
- **MPU/缓存说明**: DMA 是独立总线主设备, MPU 限制 (SRAM XN 等) 不作用于
  DMA; SRAM 无缓存、Flash 经 EFM 缓存读取对 DMA 一致, 无脏数据问题;
- **时钟**: FCG0 门控位 (DMA1=bit14/DMA2=bit15/AOS=bit17) 受写保护,
  经 FCG0PC 键 0xA5A5 解锁后使能 (`dma::init`, board 初始化调用)。

### GPIO 驱动 (寄存器→端口→引脚)

- 引脚层 `Pin<P, N>` (const 泛型): `configure` (模式/上拉/驱动/初始电平/
  反相, 对齐 DDL `GPIO_Init`) / `set_func` (PFSR.FSEL) / `set_high` /
  `set_low` / `toggle` / `is_high` / `output_is_high` / `set_output_enable`;
- 端口层: `read_input_port` / `read_output_port` / `write_output_port` /
  `set_output_enable_port` (对齐 DDL `GPIO_ReadInputPort` 等);
- 功能复用号常量 `gpio::func::*` (数据手册表 2-2, 32 个 USART/SPI 功能);
- 注意: HC32F460 **无内部下拉** (仅 PUU 上拉), 下拉需外部电阻。

```
ooooooooo.   ooooooooooooo         ooooooooo.                            .   
`888   `Y88. 8'   888   `8         `888   `Y88.                        .o8   
 888   .d88'      888               888   .d88' oooo  oooo   .oooo.o .o888oo 
 888ooo88P'       888               888ooo88P'  `888  `888  d88(  "8   888   
 888`88b.         888      8888888  888`88b.     888   888  `"Y88b.    888   
 888  `88b.       888               888  `88b.   888   888  o.  )88b   888 . 
o888o  o888o     o888o             o888o  o888o  `V88V"V8P' 8""888P'   "888" 

hc32f460jeua-evb v0.1.0  —  RT-Thread 架构的 Rust RTOS (HC32F460JEUA)
── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ──
处理器 : Cortex-M4F @ 200 MHz
节拍 : 1 ms (1000 Hz)
优先级 : 32 级 (空闲 = 31)
堆 : 179 KB
── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ── ──
构建 : 2026-08-02 [debug] rustc 1.97.1 (8bab26f4f 2026-07-14)
就绪 : 2 个线程 (位图 0x80000004)
```

> 提示: 横幅的块字符与中文均为 UTF-8 字节流, 终端需设置为 UTF-8 编码
> (现代终端默认支持), 否则会显示为乱码。

## 开发环境容器

`Containerfile` 定义包含 Python + Rust 的容器镜像:

```bash
podman build --no-cache -t <镜像名:版本号> -f Containerfile .
podman create -it -v "$PWD":/workspace --name <容器名称> <镜像名称>
podman start <容器名称> && podman exec -it <容器名称> bash
```

容器内需额外安装 Rust 目标与工具:

```bash
rustup target add thumbv7em-none-eabihf
python3 -m venv .venv && .venv/bin/pip install pyocd
```

## 验证记录

以下真机记录来自新增 CAN 驱动之前，不覆盖本次 CAN 变更：

- debug 与 release 构建均已在真机烧录验证:0 panic,稳定运行 60s+;
- 当时版本的内核自检连续 5 次复位全部 22 项通过;
- RX 中断接收:ASCII/二进制/混合数据完整回显, 90 秒后系统仍正常响应;
- 大包高速输入 (超 512B 缓冲) 按设计丢弃新字节, 不崩溃;
- 已知现象:115200 无流控下 PC 端读取不及时 (如 `cat`) 会丢字节
  (表现为行尾截断, 非打印设计缺陷), 建议使用交互式终端查看;
  启动横幅经 `screen` 捕获验证零丢失。

本次 CAN 变更已通过主机单测、目标 debug/release 构建和严格 Clippy；CAN
内部回环及外接收发器总线通信仍需在目标板执行，不能由主机测试替代。

## 代码整理与设计优化记录

- banner 移出 `rtos` 内核 (应用层 `src/banner.rs`), 内核不再依赖
  `clk`/`heap` 等应用模块;
- `klist.rs` 新增 `container_of!` 宏，统一链表节点到内核对象的受审计转换；
  链表遍历在临界区内只保留一个可变访问路径，避免重叠借用;
- `thread.rs` 提取公共辅助: `wakeup_thread` (唤醒统一路径) /
  `resched_needed` (优先级抢占判定) / `blocked_wait` (阻塞恢复判定),
  消除 `ipc.rs` 6 处重复的"调度 + 超时检查"模式与 3 处唤醒序列;
- `timer::check` 在临界区内完成摘链与状态迁移,退出临界区后执行 ISR
  回调;回调返回后不再解引用定时器,兼顾对象生命周期与中断延迟;
- RTOS 的 context-switch hook 将 MPU 栈守卫策略移到 BSP；WDT 则由 BSP
  创建最高优先级 supervisor 周期喂狗，二者均不让内核反向依赖设备;
- `context.rs` 统一寄存器写入辅助; 全项目修复历史 clippy 警告,
  当前 0 警告 0 错误。

## 终端 (shell) 调试中修复的问题

- **浮点格式化崩溃**: `core` 的浮点格式化 (flt2dec/dragon) 在
  no_std 裸机环境下导致内存破坏 (表现为系统崩溃/输出垃圾字节)。
  `free` 命令改用整数百分比计算, 完全规避浮点格式化;
- **线程内部可变性收敛**：`Arc<Thread>` 只公开不可变外壳，全部 TCB
  可变字段集中在唯一 `UnsafeCell<ThreadInner>`，并由单核关中断临界区
  串行化；不再依赖 volatile 读屏障维持静态链表写入;
- **RX 输入中断驱动化**: read_line 从 5ms 轮询改为信号量阻塞等待;
  UART ISR 经 OS 无关 notifier 唤醒 RTOS adapter,消除裸驱动对内核的依赖。
