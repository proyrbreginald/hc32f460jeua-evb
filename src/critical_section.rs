//! 临界区 (PRIMASK 关中断)
//!
//! 供 gpio / heap 等模块复用 (此前各处重复实现)。
//!
//! 嵌套安全: 仅当进入前中断开启 (PRIMASK=0) 时才执行 `cpsid`/`cpsie`,
//! 内层嵌套不会提前打开外层临界区的中断。

/// 在临界区内执行 `f`, 退出时按进入前的中断状态恢复
pub fn with<T>(f: impl FnOnce() -> T) -> T {
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

    let result = f();

    // 只有自己关闭了中断才重新开启
    if primask & 1 == 0 {
        unsafe {
            core::arch::asm!("cpsie i");
        }
    }
    result
}
