#[path = "common/mod.rs"]
mod common;

use common::RamNor;
use littlefs::{FileSystem, Geometry};

/// 16 块几何 (板上 128KiB 分区: 16 × 8KiB) 的冒烟测试: 格式化 →
/// 目录 + 多次写 (覆盖轮转) → 重挂载后数据一致。
#[test]
fn sixteen_block_geometry_workflow() {
    let geometry = Geometry::new(8192, 16);
    let device = RamNor::new(geometry.block_size, geometry.block_count);
    let mut fs = FileSystem::format(device).unwrap();

    fs.mkdir("log").unwrap();
    // 多次小写 + 一次大文件 (跨快照块), 覆盖提交/轮转
    for i in 0..8 {
        let payload = vec![i as u8; 1024 + i * 7];
        fs.write(&format!("log/log{i}.log"), &payload).unwrap();
    }
    let big = vec![0x5Au8; 40 * 1024];
    fs.write("big.bin", &big).unwrap();
    assert_eq!(fs.stat("big.bin").unwrap().size, big.len() as u32);

    // 重挂载 (重新 scan, 挂载最新 generation)
    let device = fs.into_device();
    let mut fs = FileSystem::mount(device).unwrap();
    assert_eq!(fs.stat("big.bin").unwrap().size, big.len() as u32);
    let mut buf = vec![0u8; 64];
    let n = fs.read("big.bin", 20 * 1024, &mut buf).unwrap();
    assert_eq!(&buf[..n], &big[20 * 1024..20 * 1024 + n]);
    for i in 0..8 {
        let info = fs.stat(&format!("log/log{i}.log")).unwrap();
        assert_eq!(info.size, (1024 + i * 7) as u32);
    }
}
