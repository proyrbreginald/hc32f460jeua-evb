#[path = "common/mod.rs"]
mod common;

use common::{RamError, RamNor, read_all};
use littlefs::format::{
    HEADER_SIZE, MAX_WEAR_TABLE_SIZE, RecordHeader, SnapshotHeader, crc32_mpeg2,
    decode_wear_table, encode_wear_table,
};
use littlefs::{Error, FileSystem, Geometry};

const COLD_NAME: &str = "cold-config";

/// 64 次"大文件 + 小配置"交替写: 旧算法按 `start += span` 环形前移时,
/// 大/小快照的跨度差异会让部分块被擦除两倍于其余块; 动态磨损均衡必须把
/// 分布收敛到 ±2 以内, 且磨损表在重挂载后与设备实测一致。
#[test]
fn mixed_span_workload_distributes_erases_and_persists() {
    let geometry = Geometry::new(256, 8);
    let device = RamNor::new(geometry.block_size, geometry.block_count);
    let observer = device.clone();
    let mut fs = FileSystem::format(device).unwrap();

    for round in 0..64 {
        // span 1: 空文件集 (仅磨损表)
        fs.write("big", &[]).unwrap();
        // span 4: 恰好占用半分区的大文件
        let big = vec![round as u8; 4 * 256 - HEADER_SIZE - 16 - 25];
        fs.write("big", &big).unwrap();
    }

    let counts = observer.erase_counts();
    let minimum = *counts.iter().min().unwrap();
    let maximum = *counts.iter().max().unwrap();
    assert!(
        maximum - minimum <= 2,
        "mixed spans distribute erases: {counts:?}"
    );

    let device = fs.into_device();
    let mut remounted = FileSystem::mount(device).unwrap();
    let (min_ram, max_ram) = remounted.wear_bounds();
    assert_eq!((min_ram, max_ram), (minimum, maximum), "wear table persists");
    assert_eq!(remounted.info().min_erase_count, minimum);
    assert_eq!(remounted.info().max_erase_count, maximum);
    remounted.verify().unwrap();
}

/// 磨损表在原始字节层面对齐: 表位于每段快照的 payload 起点, 紧接 64B 头。
#[test]
fn wear_table_sits_after_the_header_on_disk() {
    let geometry = Geometry::new(512, 8);
    let mut fs = FileSystem::format(RamNor::new(512, 8)).unwrap();
    fs.write("state", b"value").unwrap();
    let image = fs.into_device().bytes();

    let mut newest: Option<SnapshotHeader> = None;
    for block in 0..geometry.block_count {
        let offset = (block * geometry.block_size) as usize;
        let Ok(header) =
            SnapshotHeader::decode_at(&image[offset..offset + HEADER_SIZE], block, geometry)
        else {
            continue;
        };
        if newest
            .map(|current| {
                littlefs::format::generation_is_newer(header.generation, current.generation)
            })
            .unwrap_or(true)
        {
            newest = Some(header);
        }
    }
    let newest = newest.expect("committed snapshot exists");
    let start = (newest.start_block * geometry.block_size) as usize + HEADER_SIZE;
    let mut table = [0u16; 64];
    decode_wear_table(&image[start..start + 16], geometry.block_count, &mut table).unwrap();
    assert_eq!(table[..geometry.block_count as usize], [1, 1, 0, 0, 0, 0, 0, 0]);
}

/// 手工构造一个"一半块磨损、一半块全新"的合法 v1.1 快照镜像。
fn imbalanced_device() -> (RamNor, RamNor) {
    let geometry = Geometry::new(512, 8);
    let data = b"cold-config";
    let record = RecordHeader::new(COLD_NAME.len() as u16, data.len() as u32, crc32_mpeg2(data), 0)
        .unwrap();
    let mut wear = [0u16; 64];
    wear[0] = 1;
    wear[1] = 1;
    wear[4..8].fill(100);
    let mut payload = vec![0u8; MAX_WEAR_TABLE_SIZE];
    let table_size = encode_wear_table(&wear, geometry.block_count, &mut payload).unwrap();
    payload.truncate(table_size);
    payload.extend_from_slice(&record.encode().unwrap());
    payload.extend_from_slice(COLD_NAME.as_bytes());
    payload.extend_from_slice(data);
    payload.resize(
        payload.len() + record.record_len as usize - 20 - COLD_NAME.len() - data.len(),
        0,
    );

    let header = SnapshotHeader::new(
        0,
        0,
        payload.len() as u32,
        crc32_mpeg2(&payload),
        1,
        geometry,
    )
    .unwrap();
    let device = RamNor::new(geometry.block_size, geometry.block_count);
    device.overwrite_raw(0, &header.encode_committed().unwrap());
    device.overwrite_raw(HEADER_SIZE, &payload);
    let observer = device.clone();
    (device, observer)
}

/// 静态磨损均衡: 快照位于低磨损区而其余区域磨损不均时, `level()` 把快照
/// 搬到最磨损最轻的候选位置 (只擦除那些块), 数据与重挂载一致性不受影响。
#[test]
fn level_relocates_snapshot_to_least_worn_candidate() {
    let (device, observer) = imbalanced_device();
    let mut fs = FileSystem::mount(device).unwrap();
    assert_eq!(read_all(&mut fs, COLD_NAME).unwrap(), b"cold-config");
    assert_eq!(fs.wear_bounds(), (0, 100));

    let generation_before = fs.info().generation;
    fs.level().unwrap();

    // 只擦除了磨损最轻的候选块 (2); 已经磨损的 4..7 没有被碰 (镜像内
    // 的 100 只是模拟的历史磨损, 不计入 RamNor 的实时擦除计数)。
    let counts = observer.erase_counts();
    assert_eq!(&counts[..8], &[0, 0, 1, 0, 0, 0, 0, 0]);
    assert_eq!(fs.info().generation, generation_before + 1);
    assert_eq!(read_all(&mut fs, COLD_NAME).unwrap(), b"cold-config");
    fs.verify().unwrap();

    let mut remounted = FileSystem::mount(observer).unwrap();
    assert_eq!(read_all(&mut remounted, COLD_NAME).unwrap(), b"cold-config");
}

/// 分布已经均匀时 `level()` 是无操作: 不擦除任何块, 不推进 generation。
#[test]
fn level_is_a_noop_when_balanced() {
    let mut fs = FileSystem::format(RamNor::new(256, 8)).unwrap();
    for value in 0..20u8 {
        fs.write("counter", &[value]).unwrap();
    }
    let device = fs.into_device();
    let observer = device.clone();
    let mut mounted = FileSystem::mount(device).unwrap();
    let generation_before = mounted.info().generation;
    observer.reset_events();
    mounted.level().unwrap();
    assert_eq!(observer.events(), 0, "no-op level must not touch the device");
    assert_eq!(mounted.info().generation, generation_before);
}

/// 磨损表计数单调、只增不减: 重格式化保留旧计数并只增加目标块的计数。
#[test]
fn reformat_keeps_erase_counts_monotonic() {
    let device = RamNor::new(256, 8);
    let mut fs = FileSystem::format(device).unwrap();
    for value in 0..12u8 {
        fs.write("state", &[value]).unwrap();
    }
    let before = fs.info().max_erase_count;
    assert!(before > 0);
    let device = fs.into_device();

    let mut reformatted = FileSystem::format(device).unwrap();
    assert_eq!(reformatted.info().file_count, 0);
    assert!(
        reformatted.info().max_erase_count >= before,
        "counters never reset on reformat"
    );
    reformatted.verify().unwrap();
}

/// 挂载后 min/max 计数可直接用于磨损健康监控。
#[test]
fn wear_bounds_reflect_on_disk_table() {
    let device = RamNor::new(256, 8);
    let observer = device.clone();
    let mut fs = FileSystem::format(device).unwrap();
    for value in 0..30u8 {
        fs.write("hot", &[value]).unwrap();
    }
    let (min_ram, max_ram) = fs.wear_bounds();
    let counts = observer.erase_counts();
    assert_eq!(min_ram, *counts.iter().min().unwrap());
    assert_eq!(max_ram, *counts.iter().max().unwrap());

    let remounted = FileSystem::mount(fs.into_device()).unwrap();
    assert_eq!(remounted.wear_bounds(), (min_ram, max_ram));
    assert_eq!(remounted.info().min_erase_count, min_ram);
    assert_eq!(remounted.info().max_erase_count, max_ram);
}

/// level 在未均衡设备上断电恢复: 撕裂的搬迁要么整体可见 (数据不变),
/// 要么回退到旧快照; 与普通写一样满足原子性。
#[test]
fn level_has_atomic_power_loss_semantics() {
    let (base, _) = imbalanced_device();

    let event_count = {
        let fork = base.fork();
        let observer = fork.clone();
        let mut fs = FileSystem::mount(fork).unwrap();
        observer.reset_events();
        fs.level().unwrap();
        observer.events()
    };
    assert!(event_count > 0);

    for cut in 0..event_count {
        let fork = base.fork();
        fork.arm_power_loss(cut, [1, 0, 3, 2]);
        let mut fs = FileSystem::mount(fork).unwrap();
        assert!(fs.level().is_err());
        assert!(fs.recovery_required());

        let recovered_device = fs.into_device();
        recovered_device.power_cycle();
        let mut recovered = FileSystem::mount(recovered_device).unwrap();
        assert_eq!(
            read_all(&mut recovered, COLD_NAME).unwrap(),
            b"cold-config",
            "cut={cut}"
        );
        recovered.verify().unwrap();
    }
}

/// 满负载快照 (span = 半分区) 在两个互补候选位置间轮流, 依然均匀。
#[test]
fn half_partition_snapshots_alternate_evenly() {
    let geometry = Geometry::new(256, 8);
    let device = RamNor::new(geometry.block_size, geometry.block_count);
    let observer = device.clone();
    let mut fs = FileSystem::format(device).unwrap();
    let big = vec![0x5a; 4 * 256 - HEADER_SIZE - 16 - 25];
    for _ in 0..20 {
        fs.write("big", &big).unwrap();
    }
    let counts = observer.erase_counts();
    let minimum = *counts.iter().min().unwrap();
    let maximum = *counts.iter().max().unwrap();
    assert!(maximum - minimum <= 2, "half-partition alternation: {counts:?}");
}

/// level 在设备故障后同样要求 remount 才能继续。
#[test]
fn level_fault_requires_remount() {
    let (device, _) = imbalanced_device();
    let fork = device.fork();
    fork.arm_power_loss(0, [0, 1, 2, 3]);
    let mut fs = FileSystem::mount(fork).unwrap();
    assert_eq!(
        fs.level(),
        Err(Error::Device(RamError::PowerLoss)),
        "first erase fails with injected fault"
    );
    assert!(fs.recovery_required());
    assert_eq!(fs.level(), Err(Error::RecoveryRequired));
}
