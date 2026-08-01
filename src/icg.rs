//! ICG (Intelligent Configuration Guide) 硬件配置段
//!
//! HC32F460 上电时硬件自动读取 flash `0x400~0x5FF` 区域的 ICG 配置数据,
//! 用于芯片级初始化。链接脚本将本段固定放置在 `0x400` (见 `link.ld` 的 `.icg`)。

/// ICG 配置数据 (全 0xFFFFFFFF, 与官方默认一致)
#[unsafe(link_section = ".icgs")]
#[unsafe(no_mangle)]
pub static ICGS: [u32; 8] = [
    0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF,
];
