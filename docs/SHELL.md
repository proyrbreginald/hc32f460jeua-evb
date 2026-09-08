# Shell 与应用功能

shell 运行在独立 RTOS 线程中，经控制台 UART 的中断接收环获得输入。登录前不会
执行文件系统命令；文件系统挂载失败时会显示错误并保持系统其他线程运行。

## 登录与交互

- 默认用户名和密码均为 `root`，失败次数由 `CFG_SHELL_LOGIN_TRIES` 控制；
- 密码输入不回显，成功后提示符包含用户、芯片型号和当前路径；
- 回车提交，退格删除，Ctrl+C 清行，上/下方向键浏览 RAM 历史；
- 路径采用 shell 层的 `/` 根和当前工作目录，传给文件系统前规范化为无前导 `/`
  的 canonical UTF-8 路径；
- UART RX 为中断驱动，线程通过 `uart_rtos` 阻塞等待通知，不轮询硬件。

## 命令一览

| 命令 | 作用 |
|---|---|
| `help`、`whoami`、`pwd` | 帮助、身份和当前目录 |
| `sysinfo`、`uptime`、`ps`、`free` | 系统、时间、线程和堆信息 |
| `cd`、`ls`、`mkdir`、`rmdir` | 目录切换、枚举、创建和删除空目录 |
| `cat`、`write`、`stat`、`rm`、`mv` | 文件读写、元数据、删除和原子重命名 |
| `df`、`fsck`、`mount`、`mkfs --force` | 容量、校验、挂载和强制格式化 |
| `led`、`clear`、`echo`、`history` | 板载 LED、终端和历史操作 |
| `log` | 应用日志开关、级别、颜色和落盘开关 |
| `sz`、`rz` | ZMODEM 文件发送和接收（需启用） |
| `nano` | ASCII 全屏编辑器（需启用） |
| `selftest` | 内核、Flash、CRC、CAN 等同步自检（需启用） |
| `soak` | 长期压力测试并生成 HTML 报告（需启用） |
| `reboot`、`logout` | 落盘后复位或退出登录 |

实际可用命令由 `CFG_SHELL_COMMANDS` 决定；`help` 只显示已启用命令。新增命令需要
同时修改 `src/shell.rs` 的静态命令表和配置列表。

## 文件系统命令约束

文件系统只接受 canonical root-relative 路径：禁止前导/尾随 `/`、空分量、`.`、
`..` 和 NUL；父目录不会隐式创建，`rmdir` 只能删除空目录，`write` 是整文件原子
替换而不是追加写。`mkfs --force` 会生成空快照并保留磨损计数，属于破坏性操作。

## 日志与 ZMODEM

`log on|off` 控制应用日志，`log level error|warn|info|debug|trace` 调整阈值，
`log file on|off` 控制 `/log/` 落盘。日志先写固定容量 RAM 环，后台线程按
`CFG_LOG_FLUSH_MS` 刷新；`reboot` 会先同步刷新。

执行 `sz`/`rz` 时普通控制台输出会静默丢弃，以免业务日志破坏协议帧；主机端分别
使用 `rz -y` 或 `sz <file>`。接收文件整体缓存在 RAM 后再一次性原子写入，大小受
`CFG_ZMODEM_RX_MAX` 限制。

## 自检与 soak

`selftest` 在 shell 线程内同步执行，按 ESC 可中断剩余项目；覆盖 RTOS IPC、线程
延时/退出、Flash、CRC 和 CAN 内部回环。`soak [分钟] [basic|periph|can|flash]`
运行压力线程，控制台只显示进度，完整结果写入 `/test/soak_<时间戳>.html`。
压力测试的阈值、报告槽数和 Flash 节流参数见 [配置参考](CONFIGURATION.md) 与
[实时性文档](REALTIME.md)。
