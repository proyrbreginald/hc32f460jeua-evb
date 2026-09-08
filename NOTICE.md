# Third-Party Notices

本工程 (LICENSE: MIT) 声明的开源许可仅适用于本仓库自己的代码。以下第三方
材料以**参考/灵感来源**形式出现, 本工程**未链接、未分发**其任何源码:

## lrzsz (参考实现)

- 上游: lrzsz (GPL-2.0, `tmp/lrzsz/` 本地副本, 工作区外 `tmp/` 已被
  `.gitignore` 排除, 不随仓库分发)。
- 用途: `src/zmodem.rs` 的 ZMODEM 帧格式/FCS/转义/会话语义**独立实现,
  仅行为对齐** (与上游源码逐条核对, 无代码拷贝; 详见 `src/zmodem.rs`
  模块级注释)。若法律上认定其构成衍生作品, 相应文件须另行遵循 GPL-2.0;
  使用时请自行评估。

## HDSC (华大半导体) DDL v3.3.0 (专有驱动库)

- 上游: 半导体厂商出站物料 (本地副本 `tmp/HC32F460_DDL_Rev3.3.0/`,
  不随仓库分发)。
- 用途: 各寄存器级驱动 (clk/gpio/uart/dma/can/efm/sram/crc/rtc/wdt/intc/
  icg 等) 的寄存器偏移/位域/操作时序**对照与对齐**参考; 本工程为实现
  的独立 Rust 代码, 未包含 DDL 源码。运行设备固件请遵守相应许可条款。

## littlefs (BSD-3-Clause)

- 上游: littlefs 2.11.3 The littlefs authors (BSD 3-Clause, 本地副本
  `tmp/littlefs-2.11.3/`, 不随仓库分发)。
- 用途: `crates/littlefs` 的掉电安全策略与测试方法论**参考**; 磁盘格式
  为自研且不兼容, 未链接其源码 (详见 `crates/littlefs/NOTICE.md`)。
