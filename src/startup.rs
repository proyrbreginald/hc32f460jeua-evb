//! 复位启动流程
//!
//! 上电/复位后的执行序列:
//! 1. SRAMC 初始化 (SRAM3 等待周期等, 任何 RAM 使用之前);
//! 2. FPU 使能;
//! 3. `.data` / `.bss` 段初始化;
//! 4. 进入应用入口 [`crate::blink_loop`]。
//!
//! 对应的复位向量在 [`crate::vector_table::RESET_VECTOR`]。

// 声明链接脚本中定义的段边界符号
unsafe extern "C" {
    unsafe static _data_load_addr: u32;
    unsafe static mut _data_ram_start: u32;
    unsafe static mut _data_ram_end: u32;
    unsafe static mut _bss_ram_start: u32;
    unsafe static mut _bss_ram_end: u32;
}

/// 设置为复位处理函数
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reset_handler() -> ! {
    // ---- SRAMC 初始化 (对齐 DDL startup_hc32f460.S 的 ClrSramSR + SetSRAM3Wait) ----
    //
    // HC32F460 的 SRAM 分块管理 (SRAMH/SRAM12/SRAM3/SRAMR), 其中 SRAM3
    // (0x20020000~0x20026FFF, 本工程的栈区) 是慢速块, 需要 1 个读/写等待周期,
    // 否则访问数据会损坏。复位后 WTCR=0 (0 等待), 必须显式配置。
    //
    // 逻辑见 `sram` 模块 (set_wait_cycles/表 8-1); 此处**必须内联**:
    // 栈尚未建立 (栈指针在 RAM 顶, 本函数第一条指令即使用), 不能调用函数。
    // SRAMC 基址 0x4005_0800, 寄存器: WTCR(+0x0) WTPR(+0x4) CKCR(+0x8) CKPR(+0xC) CKSR(+0x10)
    // WTPR/CKPR 写保护键值: 0x77 解锁, 0x76 锁定 (SRAM_REG_UNLOCK_KEY/LOCK_KEY)
    unsafe {
        const SRAMC: usize = 0x4005_0800;

        // 清除 SRAM 校验错误标志 (CKSR: 1ERR/2ERR/PYERR)
        core::ptr::write_volatile((SRAMC + 0x10) as *mut u32, 0x1F);
        // 解锁 SRAMC 寄存器写保护
        core::ptr::write_volatile((SRAMC + 0x04) as *mut u32, 0x77);
        core::ptr::write_volatile((SRAMC + 0x0C) as *mut u32, 0x77);
        // SRAM3 读等待 1 周期 + 写等待 1 周期 (WTCR = 0x1100)
        core::ptr::write_volatile(SRAMC as *mut u32, 0x1100);
        // 恢复 SRAMC 寄存器写保护
        core::ptr::write_volatile((SRAMC + 0x04) as *mut u32, 0x76);
        core::ptr::write_volatile((SRAMC + 0x0C) as *mut u32, 0x76);
    }

    // ---- EFM 初始化: FLASH 读等待周期 (参考手册表 7-1) ----
    //
    // 复位后系统时钟为 MRC 8MHz → FLWT=0 (无等待)。
    // 逻辑见 `efm::set_wait_cycle` (表 7-1 全频段映射);
    // 切换外部晶振/更高时钟时由 `clk::switch_to_xtal` 在切换前重新配置。
    crate::efm::set_wait_cycle(crate::clk::MRC_HZ);

    // 开启 FPU (CPACR: 使能 CP10 和 CP11 的完全访问权限)
    unsafe {
        let cpacr = 0xE000ED88 as *mut u32;
        let value = core::ptr::read_volatile(cpacr);
        core::ptr::write_volatile(cpacr, value | (0b1111 << 20));
    }

    // 初始化 .data 段
    unsafe {
        let mut src = core::ptr::addr_of!(_data_load_addr);
        let mut dest = core::ptr::addr_of_mut!(_data_ram_start);
        let end = core::ptr::addr_of_mut!(_data_ram_end);

        while dest < end {
            core::ptr::write_volatile(dest, core::ptr::read_volatile(src));
            src = src.add(1);
            dest = dest.add(1);
        }
    }

    // 初始化 .bss 段
    unsafe {
        let mut dest = core::ptr::addr_of_mut!(_bss_ram_start);
        let end = core::ptr::addr_of_mut!(_bss_ram_end);

        while dest < end {
            core::ptr::write_volatile(dest, 0);
            dest = dest.add(1);
        }
    }

    // 进入应用入口
    crate::main();
}
