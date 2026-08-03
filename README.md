# hc32f460jeua-evb

HC32F460JEUA (Cortex-M4F, 200MHz) 开发板的**纯 Rust 裸机**工程:零第三方依赖,
全部外设驱动为手写寄存器访问,并内置一个**从 RT-Thread v5.2.2 移植的 RTOS 内核**
(接口按 Rust 风格重新设计)。

## 特性

- 零依赖裸机 Rust (edition 2024, `thumbv7em-none-eabihf`),无 PAC/HAL crate;
- 寄存器级外设驱动:时钟 (XTAL+MPLL→200MHz,失败自动回退)、GPIO、SysTick、USART;
- 全局堆分配器 (边界标记 + 首次适配,中断安全),支持 `Vec`/`Box`/`String`;
- **RTOS 内核**:32 级位图调度 + 时间片轮转、优先级继承互斥量、硬定时器、
  信号量/事件/邮箱/消息队列、线程生命周期与僵尸回收;
- **原子打印**:打印锁 (优先级继承) 保证输出整行不交错,高优先级线程
  不会无界等待低优先级线程;
- 完整的 panic/fault 诊断 (CFSR/HFSR 解码 + 栈回溯)。

## 工程配置 (.cargo/config.toml)

所有可调参数集中在 `.cargo/config.toml` 的 `[env]` 段统一管理, 经 `env!`
在**编译期**读取 (`src/config.rs`), 非法值直接编译报错。修改后重新编译
即生效, 无需改动代码 (cargo 自动追踪该文件变化并触发重编译):

| 前缀 | 内容 |
|---|---|
| `CFG_CHIP_MODEL` / `CFG_CORE` | 芯片型号 / 内核名 (横幅与 shell 提示符显示) |
| `CFG_XTAL_HZ` / `CFG_CLK_SOURCE` | 晶振频率 / 时钟源 (mrc/xtal/pll) |
| `CFG_PLL_*` | MPLL 倍频分频 |
| `CFG_DIV_*` | 总线分频 (1/2/4/8/16) |
| `CFG_SYSTICK_HZ` / `CFG_TICKS_PER_SEC` | 节拍频率 (两者必须一致, 编译期校验) |
| `CFG_PRIORITY_MAX` / `CFG_IDLE_*` | RTOS 优先级与空闲线程 |
| `CFG_UART_*` | 控制台单元 / 引脚·功能号 / 波特率 / 过采样 / 缓冲 / 中断参数 |
| `CFG_LED_PIN` / `CFG_LED_LEVEL` | 板载 LED 引脚与初始电平 |
| `CFG_SHELL_*` | 登录用户名 / 密码 / 失败次数 / 输入缓冲区 / **命令启用列表** (原 `shell.conf` 并入) |
| `CFG_LOG_ENABLE` / `CFG_LOG_LEVEL` | 应用日志默认开关 / 级别阈值 (运行时可用 `log` 命令切换) |
| `CFG_APP_*` | 演示线程参数 (栈/优先级/时间片) / 自检开关 / LED 翻转周期 / 定时器周期 |

约束:
- 数值均为字符串, 编译期解析 (支持 `_` 分隔), 溢出/非法字符/非法枚举
  (如 `CFG_UART_OVERSAMPLE` 非 8/16) 在编译期报错;
- UART/LED 的**端口类型** (PortA/PortC) 由 Rust 类型系统编码, 固定在
  `main.rs` 中, 引脚号/功能号等数值参数可在此配置; 引脚存在性仍由
  `Pin::new()` 编译期校验 (JEUA 封装引脚表);
- `build.rs` 仅负责构建日期与 rustc 版本 (启动横幅显示用)。

## 目录结构

```
src/
├── main.rs            # 应用入口: 硬件初始化 + 演示线程 (led/shell) + 定时器 + selftest 启动
├── config.rs          # 编译期配置入口 (.cargo/config.toml [env] → 类型化常量)
├── banner.rs          # 启动横幅 (应用层): 块字符大标题 + 内核信息面板
├── startup.rs         # 复位入口: SRAM 等待周期/FPU/.data/.bss → main
├── vector_table.rs    # 复位/异常/144 外设中断向量表 + INT000~007 中断分发
├── panic.rs           # panic 与硬件 fault 诊断 (CFSR/HFSR 解码, 栈回溯)
├── critical_section.rs# PRIMASK 临界区 (嵌套安全, 中断安全的基础)
├── heap.rs            # 全局堆分配器 (边界标记 + 首次适配 + 前后合并)
├── icg.rs             # ICG 硬件配置段 (flash 0x400, 全 0xFF)
├── clk.rs             # 时钟链: MRC/XTAL/PLL → 200MHz, 回退与运行时查询
├── gpio.rs            # GPIO: 寄存器→端口→引脚→接口四层, const 泛型校验
├── systick.rs         # SysTick 1kHz 节拍 (RTOS 时钟源)
├── uart.rs            # USART1~4 驱动 (波特率/过采样) + 中断接收环形缓冲
├── console.rs         # 控制台: 打印锁 (优先级继承) + 原子整行输出
├── log.rs             # 应用日志: 分级+彩色标签, 与内核打印分离 (可开关)
├── build.rs           # 构建元数据 (日期/rustc 版本, 供启动横幅使用)
└── rtos/              # RTOS 内核 (RT-Thread 架构移植, 不依赖应用模块)
    ├── mod.rs         # 公共 API: init/start/tick/thread_create 等
    ├── klist.rs       # 侵入式链表 + container_of 宏 (rt_list 移植)
    ├── sched.rs       # 位图就绪表 (32 级) + 时间片轮转 + 栈溢出检测
    ├── thread.rs      # TCB/创建/删除/挂起/延时 + 公共唤醒/调度判定辅助
    ├── timer.rs       # 有序链表硬定时器 (tick 回绕安全)
    ├── ipc.rs         # 信号量/互斥量(优先级继承)/事件/邮箱/消息队列
    ├── idle.rs        # 空闲线程 (wfi) + 僵尸线程回收
    └── context.rs     # Cortex-M4 PendSV 上下文切换汇编 (含 FPU)
```

## 启动流程

```
reset_handler (startup.rs)
 ├─ SRAMC 等待周期 / FLASH 等待周期 / FPU 使能
 ├─ .data 拷贝 / .bss 清零
 └─ main
     ├─ clk::init(Pll200)          # 200MHz, 失败回退 MRC/XTAL
     ├─ GPIO (LED/串口引脚) / SysTick 1kHz / USART1 115200
     ├─ rtos::init()               # PendSV/SysTick 优先级 + 空闲线程
     ├─ rtos::thread_create(...)   # 创建演示线程
     └─ rtos::start()              # 首次切换, 永不返回
```

## RTOS 内核

架构移植自 RT-Thread v5.2.2 (`CRust/src/libs/rtos/`):

| RT-Thread 源文件 | 本模块 | 内容 |
|---|---|---|
| `scheduler_up.c` | `sched` | 位图就绪表 + 时间片轮转 |
| `thread.c` | `thread` | 线程创建/退出/延时/挂起 |
| `idle.c`/`defunct.c` | `idle` | 空闲线程 + 僵尸回收 |
| `timer.c` | `timer` | 有序链表硬定时器 |
| `ipc.c` | `ipc` | 信号量/互斥量/事件/邮箱/消息队列 |
| `context_gcc.S` | `context` | PendSV 上下文切换 (PSP + FPU) |

### 使用流程

```rust
pub extern "C" fn sys_tick_handler() {
    rtos::tick_increase();          // 1. SysTick ISR 驱动节拍
}
rtos::init();                       // 2. 初始化内核
rtos::thread_create("led", 2048, 2, 10, led_thread, 0); // 3. 创建线程
rtos::start();                      // 4. 启动调度器 (永不返回)

extern "C" fn led_thread(_p: usize) {
    loop { LED.toggle(); rtos::thread_delay_ms(500); }
}
```

- 优先级:0(最高)~ 31(最低,空闲线程);时间片单位 = 节拍 (1ms);
- 线程栈由堆分配,打印线程建议 ≥2KB (debug 构建下格式化打印栈消耗较大),
  调度器在每次切换时检测栈溢出;
- 阻塞 API:线程上下文使用;中断上下文仅可用非阻塞调用
  (`Timeout::Ticks(0)` 探测、`release`/`send`/`unlock`、`Timer::start/stop`);
- 内核对象 (`Semaphore`/`Mutex`/`Event`/`Mailbox`/`MessageQueue`/`Timer`)
  可作 `static` 常量构造,启动后不可移动。

### IPC 速查

```rust
static SEM: Semaphore = Semaphore::new(0, 1);
static MUT: Mutex = Mutex::new();                 // 优先级继承
static EVT: Event = Event::new();
static MB: Mailbox = Mailbox::new(4);             // 机器字消息
static MQ: MessageQueue = MessageQueue::new(32, 4);

SEM.take(Timeout::Forever);    SEM.release();
MUT.lock(Timeout::Forever);    MUT.unlock();
EVT.send(0x01);
EVT.recv(0x01, EventOpt::OrClear, Timeout::Ticks(3000));
MB.send(42usize, Timeout::Forever);  MB.recv(Timeout::Forever);
MQ.send(b"hi", Timeout::Forever);    MQ.recv(&mut buf, Timeout::Forever);
```

### 打印系统 (console)

- `println!` 整行原子输出:内容 + CRLF 在同一次加锁内完成 (优先级继承互斥量),
  多线程输出不交错;等待打印锁的高优先级线程会把持有者提升到自己的优先级,
  **不会出现高优先级线程无界等待低优先级线程**;
- 中断上下文 / panic 诊断走无锁通道 `write_fmt_raw` (仅诊断, 可能交错);
- 调度器启动前 (boot 阶段) 自动退化为无锁输出;
- 注意:UART 为 115200 无流控,输出速率接近 PC 读取能力时 CH340 缓冲可能
  溢出丢字节 (表现为行尾截断, 与打印交错无关)。

## 构建 / 烧录 / 调试

目标: `thumbv7em-none-eabihf`,自定义链接脚本 `link.ld`
(FLASH 512K + RAM 188K,8K 主栈,`.heap` 段)。

```bash
cargo build                          # debug 构建
cargo build --release                # release 构建
cargo run                            # 构建 + 烧录 (pyocd, 见 scripts/flash.sh)
```

`debug` 构建默认已启用 `opt-level = 1` (见 `Cargo.toml` `[profile.dev]`):
保持可调试性 (debuginfo/帧指针/栈回溯不变) 的同时, 固件 flash 占用约为
无优化时的 60% (~61KB, 可换 `"s"` 再降 ~8%); `release` 构建采用
`opt-level = "z"` (体积优先), 固件 ~37KB。

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
  `trace`(白), 彩色标签 `[ERR]`~`[TRC]`, 整行原子输出 (不交错);
- **宏**: `log_error!` / `log_warn!` / `log_info!` / `log_debug!` /
  `log_trace!` (线程上下文使用, 与 `println!` 同约束);
- **默认值来自配置**: `CFG_LOG_ENABLE` (默认开启) + `CFG_LOG_LEVEL`
  (默认 `info`, 输出 ≤ 阈值的级别); 非法值编译期报错;
- **运行时控制** (shell 命令, 重启后恢复配置默认):
  - `log` — 显示当前开关与级别;
  - `log on` / `log off` — 切换日志开关;
  - `log level error|warn|info|debug|trace` — 调整级别阈值。

### 终端 (仿 Ubuntu shell)

- 启动后先登录: 用户名 + 密码 (密码不显示), 配置见 `.cargo/config.toml`
  的 `CFG_SHELL_*` (编译期读取, 改密码无需改代码);
- 密码错误次数可配置 (默认 3 次), 超限提示 "Too many login failures";
- 命令提示符 `root@HC32F460JEUA:~$` (用户名@芯片型号);
- **命令系统**: 命令注册在 `src/shell.rs` 的静态命令表 [`COMMANDS`]
  (名称/别名/帮助/执行函数), 分发与实现解耦; **新增命令 = 表内追加一项
  + 加入 `CFG_SHELL_COMMANDS` 启用列表**, 无需修改分发/帮助逻辑;
- **每个命令可单独启用/禁用**: `CFG_SHELL_COMMANDS` 为逗号分隔的命令名
  列表, 未列出的命令执行时提示 "未启用" 且不出现在 `help` 中;
- 命令: `help` / `sysinfo`(info) / `uptime` / `ps` / `free`(mem) / `echo` /
  `led on|off` / `log` / `selftest` / `clear` / `whoami` / `reboot` /
  `logout`(exit);
- 输入: 回车提交, 退格删除, Ctrl+C 清行;
- 输入采用中断驱动 (RX ISR 释放信号量, 线程阻塞等待, 无轮询)。

### 内核自检 (selftest)

- 不再开机自动运行: 由 `CFG_APP_SELFTEST_ENABLE` 控制启用 (默认 `true`),
  启用后在 shell 中输入 `selftest` **手动启动** (运行中再次执行会提示
  "已在运行", 结束后可再次启动);
- `CFG_APP_SELFTEST_ENABLE = false` 时命令提示 "未启用";
- 自检依次验证信号量 / 互斥量 (递归) / 事件 (AND/OR/清除) / 邮箱 (含紧急
  插队) / 消息队列 (含二进制) / 线程延时 / 线程删除 (delete) / 线程自然
  退出 (defunct 回收);
- 全部通过输出 `[selftest] 完成: N 通过, 0 失败` (失败项逐条标 `[FAIL]`)。

### UART 中断接收 (USART1)

- `uart.enable_rx_interrupt(irq_n, priority)`: INTC 事件源映射 +
  NVIC 使能 + `CR1.RIE` (对齐 DDL `INTC_IrqSignIn` / `USART_FuncCmd`);
- 接收中断把字节写入 512 字节环形缓冲 (溢出丢弃新字节), 应用侧
  `rx_count()` / `read_rx()` / `drain_rx()` 非阻塞读取;
- 错误处理对齐 `USART_ClearStatus`: 读 RDR 清 RXNE, 写 CR1 的
  CPE/CFE/CORE 清 PE/FE/ORE;
- 外设中断通过 `vector_table::register_irq` 分发 (INT000~007 槽位,
  向量表在 FLASH, 槽位预置分发入口, 回调运行时注册);
- 真机验证 (115200, PC→板): ASCII/二进制/混合/连续数据均完整接收,
  500B 单包零丢失。

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

- debug 与 release 构建均已在真机烧录验证:0 panic,稳定运行 60s+;
- 内核自检 (selftest) 连续 5 次复位全部 22 项通过;
- RX 中断接收:ASCII/二进制/混合数据完整回显, 90 秒后系统仍正常响应;
- 大包高速输入 (超 512B 缓冲) 按设计丢弃新字节, 不崩溃;
- 已知现象:115200 无流控下 PC 端读取不及时 (如 `cat`) 会丢字节
  (表现为行尾截断, 非打印设计缺陷), 建议使用交互式终端查看;
  启动横幅经 `screen` 捕获验证零丢失。

## 代码整理与设计优化记录

- banner 移出 `rtos` 内核 (应用层 `src/banner.rs`), 内核不再依赖
  `clk`/`heap` 等应用模块;
- `klist.rs` 新增 `container_of!` 宏与 `KCell::get_mut`, 统一 5 处
  "链表节点 → 内核对象" 转换 (thread/timer/ipc/idle);
- `thread.rs` 提取公共辅助: `wakeup_thread` (唤醒统一路径) /
  `resched_needed` (优先级抢占判定) / `blocked_wait` (阻塞恢复判定),
  消除 `ipc.rs` 6 处重复的"调度 + 超时检查"模式与 3 处唤醒序列;
- `timer::check` 的"摘除 + 回调"改为临界区原子 (消除线程删除与
  定时器回调之间的 use-after-free 竞态);
- `context.rs` 统一寄存器写入辅助; 全项目修复历史 clippy 警告,
  当前 0 警告 0 错误。

## 终端 (shell) 调试中修复的问题

- **浮点格式化崩溃**: `core` 的浮点格式化 (flt2dec/dragon) 在
  no_std 裸机环境下导致内存破坏 (表现为系统崩溃/输出垃圾字节)。
  `free` 命令改用整数百分比计算, 完全规避浮点格式化;
- **静态链表写入被消除**: 静态对象的 `KCell` 链表写入 (thread_create
  的线程登记) 曾因"写后无读"被编译器判定为死存储而消除 (ps 列表为空),
  通过 `get_mut` + volatile 读屏障强制保留;
- **RX 输入中断驱动化**: read_line 从 5ms 轮询改为信号量阻塞等待
  (RX ISR 释放), 消除高频定时器操作对调度的扰动。
