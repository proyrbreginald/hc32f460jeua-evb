#![no_std]
#![allow(dead_code)]

//! 固件中可在宿主机复用和测试的纯逻辑。
//!
//! 裸机可执行文件的入口是 `src/main.rs`。本库只导出不访问 MMIO、不中断调度、
//! 不依赖链接脚本符号的算法，使同一份实现可由 x86_64 主机测试覆盖。新增模块
//! 只有满足这一边界时才应从这里导出；寄存器驱动和 RTOS 状态仍属于固件目标。

extern crate alloc;

pub mod can_timing;
pub mod exception_frame;
pub mod heap_layout;
pub mod heap_tlsf;
pub mod logfile_core;
pub mod logring;
pub mod notify;
pub mod soak_report_core;
pub mod zmodem;
