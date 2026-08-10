//! 架构中断临界区与临界区令牌
//!
//! 供 gpio / heap / 内核 (rtos) 等模块复用 (此前各处重复实现)。
//!
//! # 嵌套安全
//!
//! CPU backend 捕获并恢复完整中断状态，内层嵌套不会提前打开外层
//! 临界区的中断。
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
    let interrupt_state = crate::arch::acquire_interrupt_lock();

    let result = f(CriticalSection {
        _lifetime: PhantomData,
    });

    // 始终恢复入口状态；即使闭包内部改动中断状态也不会泄漏到外层。
    crate::arch::restore_interrupt_lock(interrupt_state);
    result
}

/// 当前是否运行在中断上下文 (IPSR != 0)
///
/// 供 `debug_assert!` 拦截"中断上下文调用线程专用 API"的误用
/// (阻塞式 IPC/延时/加锁打印等 —— 在 ISR 中会挂起被打断的线程,
/// 或对打印互斥量死锁)。release 构建下 `debug_assert!` 被移除,
/// 零运行时开销。
#[inline]
pub fn in_isr() -> bool {
    crate::arch::in_exception()
}
