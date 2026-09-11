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
//! [`with`] 把 ZST 令牌 [`CriticalSection`] 传入闭包，其生命周期绑定
//! 临界区作用域。令牌证明当前 CPU 已关中断；`KCell` 等内部 unsafe
//! 容器仍要求调用方另外证明同一存储不存在重叠可变借用。

use core::marker::PhantomData;

/// 临界区令牌 (ZST): 仅存在于 [`with`] 闭包内
///
/// 生命周期 `'cs` 绑定临界区作用域, 用于类型层面证明某段代码
/// 运行在关中断上下文; 无法在闭包外构造 (私有字段)。
/// `Copy` (零大小类型)，可用于多个互不重叠的内核对象。它只证明同步
/// 条件，不代表对某个具体对象的独占所有权。
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

    // 硬实时指标: 实测 PRIMASK 保持时间 (见 latency 模块, 未初始化时
    // 为无操作; 测量开销计入自身, 结果只高不低)
    let latency_begin = crate::latency::critical_begin();

    let result = f(CriticalSection {
        _lifetime: PhantomData,
    });

    crate::latency::critical_end(latency_begin);

    // 始终恢复入口状态；即使闭包内部改动中断状态也不会泄漏到外层。
    crate::arch::restore_interrupt_lock(interrupt_state);
    result
}

/// 当前是否运行在中断上下文 (IPSR != 0)
///
/// 阻塞 IPC、线程延时和控制台等公共入口用它在所有构建配置下拒绝
/// 错误上下文；内部诊断断言也可复用。
#[inline]
pub fn in_isr() -> bool {
    crate::arch::in_exception()
}
