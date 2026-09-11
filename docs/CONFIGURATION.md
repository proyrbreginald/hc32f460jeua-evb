# 配置参考

工程配置唯一来源是 `.cargo/config.toml` 的 `[env]` 段。值以字符串保存，由
`src/config.rs` 在 const 求值阶段解析和校验；非法字符、溢出、枚举值、范围或
跨项组合会直接导致编译失败。配置改变后必须重新构建，设备上不存在运行时配置
文件。

## 配置分组

| 前缀 | 作用 | 关键约束 |
|---|---|---|
| `CFG_CHIP_*` | 芯片和内核显示名 | 仅影响横幅和 shell 提示符 |
| `CFG_XTAL_*`、`CFG_HRC_*`、`CFG_PLL_*` | 振荡器、PLL 和 ICG | XTAL 4~25 MHz；HRC 16/20 MHz；PLL 位宽/VCO 范围编译期校验 |
| `CFG_CLK_SOURCE`、`CFG_DIV_*` | 系统/总线时钟 | 源为 `mrc`/`hrc`/`xtal`/`pll`；分频为 1/2/4/8/16 |
| `CFG_SYSTICK_HZ`、`CFG_TICKS_PER_SEC` | RTOS 节拍 | 两者必须一致；默认 1000 Hz |
| `CFG_PRIORITY_*`、`CFG_IDLE_*` | 调度器 | 优先级数值越小越高；idle 默认 31 |
| `CFG_UART_*` | 控制台 USART、引脚、格式、RX 缓冲和 IRQ | USART 1~4；本板 USART3(PC13/PH2)；单元与 Func 组合编译期校验 |
| `CFG_DMA_*` | DMA 控制台 TX 和大块内存拷贝 | 单元 1/2、通道 0~3；TX 与 COPY 通道不得相同 |
| `CFG_CAN_*` | 经典 CAN 模式、位时序、过滤器和 RX 策略 | 最高 1 Mbit/s；时序、ID、掩码和采样点编译期校验 |
| `CFG_LED_*` | 板载三路 LED（work/success/error）与点亮极性 | 均为 PortB；引脚号编译期校验且不得重复 |
| `CFG_SHELL_*` | 登录、历史、输入缓冲和命令列表 | 命令列表只控制注册；大功能另有编译期开关 |
| `CFG_NANO_*`、`CFG_ZMODEM_*` | nano 和 ZMODEM 参数 | 受对应功能开关裁剪；接收大小不得超过快照容量 |
| `CFG_LOG_*`、`CFG_LOGFILE_*` | 应用日志、RAM 环和 Flash 轮转 | 刷新越频繁，Flash 擦写越多；落盘文件无 ANSI 颜色 |
| `CFG_CONSOLE_*` | 控制台输出节流 | 行间间隙用于降低 USB 转串口丢字节风险 |
| `CFG_RTC_*`、`CFG_WDT_*` | RTC 时间戳和 MCU 内部看门狗 | WDT 调试时建议关闭；supervisor 运行于最高优先级 |
| `CFG_HWDT_*` | 板载**外部**硬件看门狗（PB4 高=禁用/低=使能，PB5 周期喂狗） | 喂狗周期 ≤1000 ms；引脚不得与 CAN/LED 冲突；烧录/调试必须 `CFG_HWDT_ENABLE=false` |
| `CFG_MPU_*` | 内存属性和栈守卫 | 守卫大小为 2 的幂，且必须与 `link.ld` 一致 |
| `CFG_APP_*`、`CFG_SOAK_*` | 演示线程、自检和长期压力测试 | `soak` 开启时自动编译 selftest |

## 编译期开关

以下选项关闭时，相关命令、模块和模板整体不参与编译：

| 开关 | 关闭的功能 |
|---|---|
| `CFG_SHELL_NANO_ENABLE` | `nano` 全屏编辑器 |
| `CFG_SHELL_ZMODEM_ENABLE` | `sz`/`rz` 文件传输 |
| `CFG_APP_SELFTEST_ENABLE` | `selftest` 命令和测试实现 |
| `CFG_SOAK_ENABLE` | `soak` 压力测试和 HTML 报告 |

`CFG_SHELL_COMMANDS` 是构建时确定的命令启用列表；它不能重新启用已经被上述功能
开关裁剪掉的代码。`build.rs` 在 debug 配置中强制打开测试相关 cfg，以免默认
release 裁剪掩盖类型错误。

`CFG_CAN_SELFTEST_ENABLE` 是运行期常量开关（不裁剪代码）：为 false 时
`selftest`/`soak` 跳过 CAN 项；为 true 时两者在应用 CAN 已占用控制器的情况下
会自动临时接管（清空收发队列）并在结束后按原工作模式恢复，只有 CAN IRQ 消费者
仍注册时才跳过。注意 `soak can` 的接管持续整个压力测试期间，期间本节点不参与
外部总线。

## 修改检查清单

1. 检查单位、合法范围和配置项旁的硬件说明；
2. 修改时钟后核对 Flash/SRAM 等待周期、UART/CAN 误差和 WDT 超时；
3. 修改引脚时同时检查 EVB 原理图、端口类型和功能复用号；
4. 修改 MPU 守卫时同步检查 `link.ld` 的 `MPU_GUARD_SIZE`；
5. 修改 Flash 日志间隔时评估掉电丢失窗口和扇区寿命；
6. 修改功能开关后运行 `bash scripts/verify.sh`，确保裁剪和全开矩阵都能构建。

## 产品部署注意

默认 shell 凭据是开发值，不适用于产品；`CFG_PANIC_STRATEGY=halt` 便于调试，
无人值守设备通常应评估 `reset`；WDT 会在调试器断点期间继续计数，调试时应关闭或
避免暂停超过溢出时间。CAN 正常模式需要外接收发器，板上 PB9/PB8 只是引出信号。

产品部署时另需开启**板载外部看门狗**（`CFG_HWDT_ENABLE=true`）：它不依赖 MCU
内部状态，内核锁死或时钟异常也能复位整机；但调试器停机期间无法喂狗，因此**烧录
与断点调试必须保持 `CFG_HWDT_ENABLE=false`**（内部 `CFG_WDT_ENABLE` 同理）。
