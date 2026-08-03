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
| `CFG_XTAL_HZ` / `CFG_CLK_SOURCE` | 晶振频率 / 时钟源 (mrc/hrc/xtal/pll) |
| `CFG_XTAL_STABLE_TIME` / `CFG_XTAL_DRV` | 晶振起振稳定时间 (1~9) / 驱动能力 (ulow~high) |
| `CFG_HRC_FREQ` / `CFG_HRC_STOP` | HRC 频率 16/20MHz / 复位后停止·振荡 (写 flash ICG1 配置字, 复位生效) |
| `CFG_PLL_*` | MPLL 倍频分频 (含源选择 0=XTAL/1=HRC; 位宽/VCO 范围编译期校验) |
| `CFG_DIV_*` | 总线分频 (1/2/4/8/16) |
| `CFG_SYSTICK_HZ` / `CFG_TICKS_PER_SEC` | 节拍频率 (两者必须一致, 编译期校验) |
| `CFG_PRIORITY_MAX` / `CFG_IDLE_*` | RTOS 优先级与空闲线程 |
| `CFG_UART_*` | 控制台单元 / 引脚·功能号 / 波特率 / 数据位 / 校验 / 停止位 / 过采样 / 流控 / 噪声滤波 / 缓冲 / 中断参数 |
| `CFG_LED_PIN` / `CFG_LED_LEVEL` | 板载 LED 引脚与初始电平 |
| `CFG_SHELL_*` | 登录用户名 / 密码 / 失败次数 / 输入缓冲区 / **命令启用列表** (原 `shell.conf` 并入) |
| `CFG_LOG_ENABLE` / `CFG_LOG_LEVEL` | 应用日志默认开关 / 级别阈值 (运行时可用 `log` 命令切换) |
| `CFG_OTS_*` | OTS 片内温度传感器开关 / 时钟源 / 定标参数 K·M / 自关断 / 超时 (运行时可用 `temp` 命令切换与查看) |
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
├── icg.rs             # ICG 初始化配置段 (flash 0x400, 由 CFG_HRC_FREQ 生成)
├── efm.rs             # 片内 Flash (EFM): 扇区擦除/字编程/读等待周期/UID
├── crc.rs             # CRC 硬件加速器: CRC16/32 (X25/CCITT/IEEE), 累加模式
├── rtc.rs             # 实时时钟 (RTC): LRC 源/时间日期/闹钟, 日志时间戳
├── ots.rs             # 片内温度传感器 (OTS): XTAL/HRC 源轮询测温 + 定标实验
├── sram.rs            # 片内 SRAM (SRAMC): 等待周期/奇偶·ECC 错误检测
├── intc.rs            # 中断控制器: 事件源→SEL→NVIC 路由 + 注册 API
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
- MPLL 参数编译期校验: 寄存器位宽 + 有效倍频/分频范围 + XTAL 源下
  VCO 输入 (1~25MHz) 与输出 (240~480MHz) 范围;
- 所有 CMU 寄存器偏移已与 DDL v3.3.0 头文件逐项核对一致。

## 中断系统 (intc + vector_table)

HC32F460 三级中断架构 (对齐 DDL `hc32_ll_interrupts.c`):
**事件源 → INTC.SEL → NVIC 线**:

- 事件源 (`en_int_src_t`, 如 `USART1_RI=279`) → 写 `INTC.SELx` (SEL0 偏移
  0x5C, 每线 4 字节, 复位值 0x1FF=未映射) → NVIC 线 `INTxxx` (IRQn=x,
  共 144 条) → ISER/IPR 使能/优先级;
- **注册 API** (`src/intc.rs`): `intc::register(源, 线, 优先级, 回调)`
  一步完成 路由+装回调+清挂起+设优先级+使能 (对齐 DDL 例程流程);
  失败返回 `IrqError::LineTaken` (线被其他源占用); `unregister` 逆操作;
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
- **bus hold**: 擦写期间总线被占用, CPU stall 至完成 (从 Flash 运行安全);
  全片擦除/序列编程需 RAM 运行, 模块不提供;
- 读: `read_byte`/`read_word` (Flash 内存映射) / `uid()` (96 位唯一 ID);
- **读等待周期**归属本模块: `set_wait_cycle`/`wait_cycle` (表 7-1),
  `clk` 切换时钟时调用 (从原 clk 模块迁入);
- 自检 (`selftest` 命令) 含 Flash 实测: 末扇区擦除/64B 混合数据编程/
  逐字节回读校验, 完成后还原擦除态。

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
- 启动阶段 (`startup.rs`) 的 SRAM3 配置保持**内联** (栈未建立不能调用
  函数), 与 DDL `SetSRAM3Wait` 逐字节一致;
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
  **RW 模式** (CR2.RWREQ/RWEN); `set_time`/`get_time`/`set_date`/`get_date`;
- 周期中断: 0.5s/1s/1min/1hour/1day/1month (CR1.PRDS);
- 闹钟: 时+分匹配 + 星期位掩码 (0x7F=每天), 事件源 `intc::src::RTC_ALM`
  (81) / `RTC_PRD` (82);
- 无 VBAT 备份域: VDD 供电, 掉电后需重新初始化 (软件复位 + 重设);
- **日志时间戳**: `CFG_RTC_ENABLE` 启用时开机初始化 (LRC/24H, 基准
  2000-01-01 00:00:00), 日志输出带 **`[天:时:分:秒]`** 前缀
  (自启动起的运行时长, Howard Hinnant 公历算法跨月/闰年正确,
  RTC 未运行时省略)。

## 片内温度传感器 (ots)

对齐 DDL v3.3.0 `hc32_ll_ots.c/h` 与例程 `ots_base` (寄存器基址
0x4004A400, 16 位 CTL/DR1/DR2/ECR 已与 SFR 逐项核对):

- **原理**: OTS 以 LRC (32.768kHz) 为工作时序基准、XTAL/HRC 计数,
  采样结果存 DR1/DR2 (HRC 源另有误差校准值 ECR);
  温度 `T = K × (1/DR1 − 1/DR2) × ECR + M`;
- **时钟依赖** (参考手册 17.2 节): LRC 必须使能 (`clk::lrc_cmd`,
  对齐 DDL `CLK_LrcCmd`); **HRC 源必须同时启动 XTAL32** (PC14/PC15
  外接 32.768kHz 晶振, 本板 JEUA UU 板载 Y2) 消除 HRC 频率误差,
  否则采样永不完成 (`clk::xtal32_cmd`, 对齐 DDL `CLK_Xtal32Cmd`);
  XTAL 源经 `clk::xtal_init` 完整配置引脚并启动; 外设时钟
  FCG3.bit12 (清位使能, 无写保护);
- **纯整数定点** (本工程**禁止浮点**, 见验证记录: 浮点代码进入固件后
  系统出现随机的格式化/调度器内存破坏): 定标参数按 **×1000 千分度
  整数** 存储 (K=3002.59 → 3002590), 温度计算用 i64/i128 定点:
  `T_milli = (K1000 × ECR × A) / 1e12 + M1000`,
  `A = 1e12/DR1 − 1e12/DR2`; 输出全部为整数 (`to_deci` 十分度 /
  `split_milli` 千分度), 与真机实测误差 <0.1°C;
- **定标参数**: K/M 每颗芯片不同, 可由定标实验获得 (`temp raw` 输出
  DR1/DR2/ECR 反推); 默认采用 DDL 例程内置参数, **必须与时钟源配套**:
  hrc → K=3002.59, M=27.92; xtal → K=737272.73, M=27.55
  (`.cargo/config.toml` `CFG_OTS_SLOPE_K` / `CFG_OTS_OFFSET_M`,
  定点字符串, 如 "3002.59" → 3002590);
- **自动关断** (CTL.TSSTP): 采样完成自动关断 (默认, 轮询依赖 OTSST
  自动清零; 注: DDL 头文件 `OTS_AUTO_OFF_*` 两条注释写反, 驱动按实际
  位语义命名);
- **API**: `init`/`deinit`/`polling` (轮询采样, 超时返回)/`polling_until`
  (按 uptime 时间预算, shell 命令用)/`calculate_temp`/
  `read_raw`/`scaling_experiment` (定标实验, 返回 A 参数)/`int_enable`
  (中断模式, 事件源 `intc::src::OTS`=435, 独立线 INT110, 路由由应用
  接入, 默认轮询);
- **开关**: 编译期 `CFG_OTS_ENABLE` 控制开机是否初始化; shell `temp on|off`
  运行时切换 (重启恢复配置默认);
- **查看温度**: shell `temp` 输出当前温度, `temp raw` 附加 DR1/DR2/ECR/
  K/M (千分度); 超时预算 `CFG_OTS_TIMEOUT_MS` (默认 100ms, 覆盖冷启动
  传感器稳定时间); `sysinfo` 含 OTS 状态行 (开关/时钟源/定标参数);
- 开机初始化日志 (info 级): `OTS 已初始化 (源 hrc, K 3002.590 M 27.920),
  \`temp\` 查看芯片温度`。

## 启动流程## 启动流程

```
reset_handler (startup.rs)
 ├─ SRAMC 等待周期 / FLASH 等待周期 / FPU 使能
 ├─ .data 拷贝 / .bss 清零
 └─ main
     ├─ clk::init()                # 按配置选源 (默认 pll), 失败自动回退
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

- **阻塞语义对齐 RT-Thread**: 所有阻塞等待 (take/lock/send/recv) 在唤醒后
  **回到临界区重新检查条件**再完成操作 —— 满邮箱的发送者被取走后不丢
  消息, 空队列的接收者被发送后不假超时 (selftest 含阻塞唤醒回归项);

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
- **就绪门**: UART 初始化完成前 (`console::mark_ready` 前) 的打印静默丢弃,
  防止在 UART 时钟未使能时访问 USART 导致 TXE 等待死循环 (早期 boot 日志
  不会丢失 —— 它们本就低于默认日志阈值);
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
  `led on|off` / `temp [raw|on|off]` / `log` / `selftest` / `clear` /
  `whoami` / `reboot` / `logout`(exit);
- 输入: 回车提交, 退格删除, Ctrl+C 清行;
- 输入采用中断驱动 (RX ISR 释放信号量, 线程阻塞等待, 无轮询)。

### 内核自检 (selftest)

- 不再开机自动运行: 由 `CFG_APP_SELFTEST_ENABLE` 控制启用 (默认 `true`),
  shell 中输入 `selftest` **同步执行** —— 完成后才输出下一命令提示符;
- **ESC 中断**: 执行期间按 ESC 立即停止剩余项 (终端输入在自检期间
  一律丢弃, ESC 除外), 汇总提示 `被中断 (ESC): 已完成 N 项`;
- 测试对象 (信号量/互斥量/事件/邮箱/队列) 每次运行**全新创建** (局部
  变量), 多次执行结果确定; 自检在 shell 线程内同步运行, shell 线程
  栈因此配置为 8KB (`CFG_APP_SHELL_STACK`);
- `CFG_APP_SELFTEST_ENABLE = false` 时命令提示 "未启用";
- 自检依次验证信号量 / 互斥量 (递归) / 事件 (AND/OR/清除) / 邮箱 (含紧急
  插队) / 消息队列 (含二进制) / 线程延时 / 线程删除 (delete) / 线程自然
  退出 (defunct 回收);
- 逐项结果 (`[PASS]`/`[FAIL]`/进度) 走**应用日志** (info/debug 级, 可经
  `log` 命令控制); `log level trace` 可输出**每项执行细节** (实际返回值/
  耗时/参数, 用于故障定位); **汇总始终打印** (不受日志开关影响):
  `[selftest] 完成: N 通过, 0 失败`。

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
  `read_rx_blocking()` 阻塞等待;
- 错误处理对齐 `USART_ClearStatus`: 读 RDR 清 RXNE, 写 CR1 的
  CPE/CFE/CORE 清 PE/FE/ORE; ISR 同时累加 PE/FE/ORE 计数,
  `rx_error_counts()` 读取并清零 (诊断波特率/接线/读取不及时);
- 外设中断通过 `vector_table::register_irq` 分发 (INT000~007 槽位,
  向量表在 FLASH, 槽位预置分发入口, 回调运行时注册);
- 真机验证 (115200, PC→板): ASCII/二进制/混合/连续数据均完整接收,
  500B 单包零丢失。

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

- debug 与 release 构建均已在真机烧录验证:0 panic,稳定运行 60s+;
- OTS 测温 (shell `temp`):多次实测 29~31°C (室温 + 芯片温升),`temp raw`
  输出 DR1/DR2/ECR 与定点计算值一致 (误差 <0.1°C),连续 10 次采样 + 3 次
  复位循环零 panic/fault;
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
