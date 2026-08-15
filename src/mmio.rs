//! 内存映射寄存器访问原语 (零依赖, 各外设驱动共用)
//!
//! 本模块是全部寄存器级外设驱动 (uart/dma/efm/can/...) 共用的最小
//! 抽象: 一个可复制的 32 位寄存器句柄, 提供 volatile 读/写/读-改-写。
//! 新驱动优先复用本模块, 避免在每个驱动里重复实现 `Reg`。
//!
//! - 句柄只存地址, **零大小运行时状态**, 可随意复制 (Copy);
//! - 所有访问均为 volatile (编译器不重排/合并/删除);
//! - 不区分读/写权限与位域 —— 位域由各驱动自己的掩码常量表达
//!   (对齐本工程"手写寄存器访问"的设计, 无 PAC 生成)。
#![allow(dead_code)] // 全 API 为各驱动预留, 未用项不报死代码

/// 32 位内存映射寄存器句柄 (地址在构造时固定)。
#[derive(Clone, Copy)]
pub struct Reg {
    addr: usize,
}

impl Reg {
    /// 直接以绝对地址构造 (编译期可用)。
    pub const fn new(addr: usize) -> Self {
        Self { addr }
    }

    /// 以基址 + 偏移构造 (编译期可用)。
    pub const fn at(base: usize, offset: usize) -> Self {
        Self::new(base + offset)
    }

    /// 读 32 位 (volatile)
    pub fn read(&self) -> u32 {
        unsafe { core::ptr::read_volatile(self.addr as *mut u32) }
    }

    /// 写 32 位 (volatile)
    pub fn write(&self, value: u32) {
        unsafe { core::ptr::write_volatile(self.addr as *mut u32, value) }
    }

    /// 读-改-写 32 位 (volatile; `f` 收到的为当前值, 返回值写回)
    pub fn modify(&self, f: impl FnOnce(u32) -> u32) {
        self.write(f(self.read()));
    }

    /// 读 16 位 (volatile; 用于 16 位寄存器如 USART TDR/RDR)
    pub fn read_u16(&self) -> u16 {
        unsafe { core::ptr::read_volatile(self.addr as *mut u16) }
    }

    /// 写 16 位 (volatile)
    pub fn write_u16(&self, value: u16) {
        unsafe { core::ptr::write_volatile(self.addr as *mut u16, value) }
    }

    /// 读-改-写 16 位 (volatile; 用于 GPIO 等 16 位寄存器)
    pub fn modify_u16(&self, f: impl FnOnce(u16) -> u16) {
        self.write_u16(f(self.read_u16()));
    }

    /// 读 8 位 (volatile; 用于 CAN/RTC 等 8 位寄存器)
    pub fn read_u8(&self) -> u8 {
        unsafe { core::ptr::read_volatile(self.addr as *mut u8) }
    }

    /// 写 8 位 (volatile)
    pub fn write_u8(&self, value: u8) {
        unsafe { core::ptr::write_volatile(self.addr as *mut u8, value) }
    }

    /// 读-改-写 8 位 (volatile)
    pub fn modify_u8(&self, f: impl FnOnce(u8) -> u8) {
        self.write_u8(f(self.read_u8()));
    }
}
