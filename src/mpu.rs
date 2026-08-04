//! 内存保护单元 (MPU) — Cortex-M4 硬件内存保护
//!
//! # 设计 (对齐 ARMv7-M 架构与 RT-Thread 类 MPU 方案)
//!
//! 本工程线程运行在**特权模式** (PRIVDEFENA=1: 默认内存映射作为
//! 后台区域, 未覆盖区域特权代码照常访问), MPU 区域提供三类保护:
//!
//! 1. **静态区域** (init 时配置一次):
//!    - FLASH: 只读 + 可执行 —— 野指针写代码区立即 MemManage 故障;
//!    - SRAM: 读写 + **XN** (不可执行) —— 损坏的返回地址跳入 RAM
//!      (栈/堆缓冲区) 时精确故障, 而不是执行垃圾代码;
//!    - 外设: 设备内存属性 + XN —— 杜绝执行外设地址。
//! 2. **线程栈守卫** ([`set_thread_guard`]): 每次上下文切换前把
//!    R5 配置为"下一个线程栈底 32 字节、完全无访问权限"的区域。
//!    栈向下溢出进入守卫区 → 硬件 MemManage 故障 (精确, MMFAR
//!    指向违例地址) —— 比软件 canary 更早、更可靠; 守卫区位于
//!    线程自身堆分配内部, 堆分配器不会交给他人, 无假阳性。
//! 3. **故障异常使能**: MemManage/BusFault/UsageFault 使能, MPU
//!    违例直接进入 [`crate::panic::fault_handler`] (带 CFSR/MMFAR
//!    解码, 见 panic 模块); HFNMIENA=1 保证 NMI/HardFault 始终
//!    绕过 MPU 执行 (故障处理自身不被 MPU 卡死)。
//!
//! # 区域布局 (8 区域, 编号大者优先级高)
//!
//! | 区域 | 范围 | 属性 |
//! |---|---|---|
//! | R0 | FLASH 0x0000_0000, 512KB | RO + 可执行 |
//! | R1 | 外设 0x4000_0000, 512MB | Device RW + XN |
//! | R2 | SRAM1/2/3 0x2000_0000, 256KB | RW + XN |
//! | R3 | SRAMH 0x1FFF_8000, 32KB | RW + XN |
//! | R4 | RET_RAM 0x200F_0000, 4KB | RW + XN |
//! | R5 | **线程栈守卫 (动态)** | 无访问 + XN |
//!
//! # 开关
//!
//! `.cargo/config.toml` `CFG_MPU_ENABLE` (默认开启; 正确代码不受
//! 影响, 关闭仅用于排查 MPU 相关故障)。

// 完整 API 供应用按需选用 (子区域/缓存属性/守卫开关), 忽略未使用项
#![allow(dead_code)]

/// MPU 外设寄存器 (Cortex-M4 内核外设)
const MPU_CTRL: usize = 0xE000_ED94;
const MPU_RNR: usize = 0xE000_ED98;
const MPU_RBAR: usize = 0xE000_ED9C;
const MPU_RASR: usize = 0xE000_EDA0;
/// 系统处理器控制与状态寄存器 (fault 异常使能)
const SHCSR: usize = 0xE000_ED24;

/// MPU_CTRL 位
const CTRL_ENABLE: u32 = 1 << 0;
const CTRL_HFNMIENA: u32 = 1 << 1;
const CTRL_PRIVDEFENA: u32 = 1 << 2;

/// SHCSR 位 (fault 异常使能)
const SHCSR_MEMFAULTENA: u32 = 1 << 16;
const SHCSR_BUSFAULTENA: u32 = 1 << 17;
const SHCSR_USGFAULTENA: u32 = 1 << 18;

/// RASR 字段 (对齐 core_cm4.h, ARMv7-M)
const RASR_ENABLE: u32 = 1 << 0;
const RASR_SIZE_POS: u32 = 1; // [5:1]
const RASR_SRD_POS: u32 = 8; // [15:8] 子区域禁用
const RASR_B_POS: u32 = 16;
const RASR_C_POS: u32 = 17;
const RASR_S_POS: u32 = 18;
const RASR_TEX_POS: u32 = 19; // [21:19] 内存属性 (设备内存 = 1)
const RASR_AP_POS: u32 = 24; // [26:24] 访问权限 (3 位)
const RASR_XN_POS: u32 = 28;

/// AP 编码 (ARMv7-M 3 位, 对齐 core_cm4.h)
const AP_FULL: u32 = 0b011; // 读写 (特权 + 非特权)
const AP_RO: u32 = 0b110; // 只读 (两者)
const AP_NO_ACCESS: u32 = 0b000; // 完全无访问

/// SIZE 字段 = log2(字节数) - 1
const fn size_code(bytes: u32) -> u32 {
    bytes.trailing_zeros() - 1
}

/// 线程栈守卫区大小 (32B, MPU 最小区域粒度)
pub const STACK_GUARD_SIZE: usize = 32;

fn write32(addr: usize, value: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, value) };
}

fn read32(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// 配置一个 MPU 区域 (经 RNR 选择)
fn set_region(rn: u8, base: usize, bytes: u32, ap: u32, xn: bool, tex: u32, cacheable: bool) {
    // SIZE 字段 = log2(bytes) - 1; base 必须按 bytes 对齐 (调用方保证)
    debug_assert!(bytes.is_power_of_two() && (base as u32) % bytes == 0);
    write32(MPU_RNR, rn as u32);
    write32(MPU_RBAR, (base as u32) & !0x1F); // VALID=0: 由 RNR 选择区域
    let mut rasr = RASR_ENABLE | (size_code(bytes) << RASR_SIZE_POS) | (ap << RASR_AP_POS)
        | (tex << RASR_TEX_POS);
    if xn {
        rasr |= 1 << RASR_XN_POS;
    }
    if cacheable {
        rasr |= (1 << RASR_C_POS) | (1 << RASR_S_POS); // Normal, 可缓存
    }
    write32(MPU_RASR, rasr);
}

/// 使能/失能 MPU (区域配置保留)
///
/// 使能时同时置位:
/// - PRIVDEFENA: 默认内存映射作后台区域 (特权代码可访问未覆盖区,
///   正常功能不受区域影响, 区域仅叠加限制);
/// - HFNMIENA: NMI/HardFault 绕过 MPU (故障处理自身不受 MPU 卡死)。
fn set_enable(enable: bool) {
    if enable {
        write32(MPU_CTRL, CTRL_ENABLE | CTRL_HFNMIENA | CTRL_PRIVDEFENA);
    } else {
        write32(MPU_CTRL, 0);
    }
}

/// 初始化 MPU: 使能 fault 异常 + 配置静态保护区域 + 使能 MPU
///
/// 须在调度器启动前调用 (由 `hardware_init` 调用)。PRIVDEFENA=1:
/// 未覆盖内存特权代码按默认映射访问, 正常功能不受影响。
pub fn init() {
    // 使能 MemManage/BusFault/UsageFault 异常: MPU 违例直接进入
    // fault_handler (精确诊断), 而非升级为 HardFault
    write32(SHCSR, read32(SHCSR) | SHCSR_MEMFAULTENA | SHCSR_BUSFAULTENA | SHCSR_USGFAULTENA);

    // R0: FLASH 512KB 只读可执行 (防野指针写代码区)
    set_region(0, 0x0000_0000, 512 * 1024, AP_RO, false, 0, true);
    // R1: 外设 512MB 设备内存 RW + XN (含位带别名区)
    set_region(1, 0x4000_0000, 512 * 1024 * 1024, AP_FULL, true, 1, false);
    // R2: SRAM1/2/3 (0x20000000 起 256KB, 覆盖 SRAM12+SRAM3) RW + XN
    set_region(2, 0x2000_0000, 256 * 1024, AP_FULL, true, 0, true);
    // R3: SRAMH (32KB) RW + XN
    set_region(3, 0x1FFF_8000, 32 * 1024, AP_FULL, true, 0, true);
    // R4: RET_RAM (4KB) RW + XN
    set_region(4, 0x200F_0000, 4 * 1024, AP_FULL, true, 0, true);
    // R5: 线程栈守卫 (动态, 调度器启动后按当前线程配置)。
    // **初始必须禁用**: 若以基址 0 配置无访问区域, 会覆盖向量表
    // (0x0~0x20) 导致启动即 MemManage 故障。
    write32(MPU_RNR, 5);
    write32(MPU_RASR, 0); // ENABLE=0: 未配置

    set_enable(true);
}

/// 配置线程栈守卫: 把 R5 指向 `guard_base` 起的 32 字节无访问区域
///
/// 由调度器在上下文切换前调用 (目标线程的守卫区, 见 `sched` 模块)。
/// `guard_base` 必须 32 字节对齐 (线程创建时保证)。
#[inline]
pub fn set_thread_guard(guard_base: usize) {
    set_region(5, guard_base, STACK_GUARD_SIZE as u32, AP_NO_ACCESS, true, 0, false);
}

/// 失能线程栈守卫 (R5, 调试时临时关闭)
pub fn guard_off() {
    write32(MPU_RNR, 5);
    write32(MPU_RASR, 0); // ENABLE=0
}

/// 临时放开 FLASH 区域写权限 (EFM 触发写需要)
///
/// HC32F460 的 EFM 以"写 Flash 地址"触发编程/擦除 (见 efm 模块),
/// 与 R0 只读区域冲突 —— 擦写期间需把 R0 临时置为读写, 完成触发
/// 写后立即恢复只读。窗口极小 (单次 store), 期间中断不写 Flash。
/// MPU 未使能 (CFG_MPU_ENABLE=false) 时为无操作。
#[inline]
pub fn flash_writable(enable: bool) {
    if crate::config::MPU_ENABLE {
        let ap = if enable { AP_FULL } else { AP_RO };
        set_region(0, 0x0000_0000, 512 * 1024, ap, false, 0, true);
    }
}

/// 在 FLASH 可写窗口内执行 `f` (RAII 风格: 进入置读写, 退出恢复只读)
///
/// 供 [`crate::efm`] 的触发写使用 (program/sector_erase/swap)。
#[inline]
pub fn with_flash_writable<R>(f: impl FnOnce() -> R) -> R {
    flash_writable(true);
    let r = f();
    flash_writable(false);
    r
}
