//! 临界区 (PRIMASK 关中断) 与临界区令牌
//!
//! 供 gpio / heap / 内核 (rtos) 等模块复用 (此前各处重复实现)。
//!
//! # 嵌套安全
//!
//! 仅当进入前中断开启 (PRIMASK=0) 时才执行 `cpsid`/`cpsie`,
//! 内层嵌套不会提前打开外层临界区的中断。
//!
//! # 临界区令牌 ([`CriticalSection`])
//!
//! [`with`] 把 ZST 令牌 [`CriticalSection`] 传入闭包, 其生命周期
//! 绑定临界区作用域: 内核共享状态 (见 [`crate::rtos::klist::KCell`])
//! 的访问须出示该令牌, 派生的引用**无法逃逸出临界区** ——
//! "必须关中断访问"从文档约定变为编译期强制的类型契约。

use core::marker::PhantomData;

/// 临界区令牌 (ZST): 仅存在于 [`with`] 闭包内
///
/// 生命周期 `'cs` 绑定临界区作用域, 用于类型层面证明某段代码
/// 运行在关中断上下文; 无法在闭包外构造 (私有字段)。
/// `Copy` (零大小类型), 可自由传递/复用。
#[derive(Clone, Copy)]
pub struct CriticalSection<'cs> {
    _lifetime: PhantomData<&'cs ()>,
}

/// 在临界区内执行 `f`, 退出时按进入前的中断状态恢复
///
/// `f` 接收 [`CriticalSection`] 令牌, 供内核共享状态的类型安全访问
/// 使用 (见 [`crate::rtos::klist::KCell::get`])。
pub fn with<R>(f: impl FnOnce(CriticalSection<'_>) -> R) -> R {
    let primask: u32;
    unsafe {
        core::arch::asm!("mrs {}, primask", out(reg) primask);
    }

    // 仅当此前中断开启时才进入临界区 (嵌套调用时保持外层状态)
    if primask & 1 == 0 {
        unsafe {
            core::arch::asm!("cpsid i");
        }
    }

    let result = f(CriticalSection {
        _lifetime: PhantomData,
    });

    // 只有自己关闭了中断才重新开启
    if primask & 1 == 0 {
        unsafe {
            core::arch::asm!("cpsie i");
        }
    }
    result
}
