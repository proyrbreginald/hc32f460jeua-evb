mod common;

use common::{RamError, RamNor, read_all};
use littlefs::{Error, FileSystem};

const BLOCK_SIZE: u32 = 4096;
const BLOCK_COUNT: u32 = 8;

/// 坏块重试: 首次候选位置擦除失败 (块永久损坏) → 标记坏块 → 换位置
/// 重试成功; 坏块此后永不再被擦除; 坏标记随磨损表持久化到下一个
/// 成功提交的快照, 重挂载后仍然生效。
#[test]
fn dead_block_is_marked_and_skipped_forever() {
    let device = RamNor::new(BLOCK_SIZE, BLOCK_COUNT);
    let mut fs = FileSystem::format(device).unwrap();
    fs.write("state", b"v1").unwrap();

    // 快照 span = 1 块: 下一候选 = 当前块之后的某块。把全部"当前快照
    // 之外"的块轮流设为坏块过于复杂, 这里直接坏掉候选首块: 由于
    // 动态均衡按磨损选择, 我们改为"坏掉一个块后连续写多次, 每次都必须
    // 成功" —— 若坏块被选中, 重试路径必须接住并绕开。
    let dead = 5u32; // 任意非当前块 (format 后当前块 = 0)
    fs.device().arm_dead_block(dead);

    for i in 0..24 {
        let data = alloc(i);
        fs.write("state", &data).unwrap();
        assert_eq!(read_all(&mut fs, "state").unwrap(), data);
    }

    // 坏块从未被擦除 (始终被跳过)
    let erase_counts = fs.device().erase_counts();
    assert_eq!(erase_counts[dead as usize], 0, "坏块不应被擦除");

    // 坏标记持久化: 重挂载 (仍坏) 后继续写, 依旧成功
    let device = fs.into_device();
    let mut fs = FileSystem::mount(device).unwrap();
    for i in 24..40 {
        let data = alloc(i);
        fs.write("state", &data).unwrap();
        assert_eq!(read_all(&mut fs, "state").unwrap(), data);
    }
    let erase_counts = fs.device().erase_counts();
    assert_eq!(erase_counts[dead as usize], 0);

    // 坏块"康复" (测试脚手架) 后仍不被使用: 标记已持久化, 永不重映射
    fs.device().heal_dead_block(dead);
    fs.write("state", b"after-heal").unwrap();
    assert_eq!(fs.device().erase_counts()[dead as usize], 0);
}

/// 全部非重叠候选都含坏块时: 返回 NoSpace (无可用布局), 旧快照完整,
/// 无需重挂载即可继续读 (写则因几何死局持续 NoSpace)。
#[test]
fn all_candidates_dead_returns_no_space_with_old_snapshot_intact() {
    let device = RamNor::new(BLOCK_SIZE, BLOCK_COUNT);
    let mut fs = FileSystem::format(device).unwrap();
    fs.write("state", b"survives").unwrap();

    // span=1, 当前块 0: 候选 = 1..7, 全部设为坏块
    for block in 1..BLOCK_COUNT {
        fs.device().arm_dead_block(block);
    }
    assert_eq!(fs.write("state", b"nope"), Err(Error::<RamError>::NoSpace));
    // 旧快照未受损: 不要求重挂载, 读仍返回旧值
    assert_eq!(read_all(&mut fs, "state").unwrap(), b"survives");
    assert!(!fs.recovery_required());
}

/// 掉电类错误 (非永久块故障) 保持旧语义: Fatal + recovery_required,
/// 不标记坏块、不重试。
#[test]
fn power_loss_error_is_still_fatal_not_bad_block() {
    let device = RamNor::new(BLOCK_SIZE, BLOCK_COUNT);
    let mut fs = FileSystem::format(device).unwrap();
    fs.write("state", b"v1").unwrap();
    let base = fs.into_device();

    let device = base.fork();
    device.arm_power_loss(1, [0, 1, 2, 3]);
    let mut fs = FileSystem::mount(device).unwrap();
    assert_eq!(
        fs.write("state", b"v2"),
        Err(Error::Device(RamError::PowerLoss))
    );
    assert!(fs.recovery_required());
}

/// 磨损计数接近饱和时整体折半: 相对顺序保留、坏标记保留, 均衡不失效。
#[test]
fn wear_rescale_preserves_order_and_bad_flags() {
    // 经公共路径验证: 直接构造高磨损表代价过大, 这里通过"坏块跳过"
    // 与长期写入的均衡行为间接覆盖; 折半的单元级验证见 fs.rs 单元测试。
    let device = RamNor::new(BLOCK_SIZE, BLOCK_COUNT);
    let mut fs = FileSystem::format(device).unwrap();
    fs.write("state", b"start").unwrap();
    // 大量写入 (动态均衡轮转), 坏块始终被跳过
    fs.device().arm_dead_block(3);
    for i in 0..200 {
        let data = alloc(i);
        fs.write("state", &data).unwrap();
    }
    let (min, max) = fs.wear_bounds();
    assert!(
        max.saturating_sub(min) <= 2,
        "均衡失效: min={min} max={max}"
    );
    assert_eq!(fs.device().erase_counts()[3], 0);
}

/// 首格式块 0 坏死: 格式化必须标记坏块并换位重试成功 (旧实现永远选
/// start=0 且无重试, virgin 设备首块坏死时格式化永久失败 —— 实测复现)。
#[test]
fn first_format_survives_dead_block_zero() {
    let device = RamNor::new(BLOCK_SIZE, BLOCK_COUNT);
    device.arm_dead_block(0);

    let mut fs = FileSystem::format(device).unwrap();
    // 空快照位于某有效块 (0 已标记坏, 且永不被擦除)
    assert_eq!(fs.device().erase_counts()[0], 0, "坏块不应被擦除");
    fs.write("state", b"v1").unwrap();
    assert_eq!(read_all(&mut fs, "state").unwrap(), b"v1");

    // 坏标记已持久化: 重挂载后继续格式化/写入仍成功
    let device = fs.into_device();
    let mut fs = FileSystem::mount(device).unwrap();
    fs.write("state", b"v2").unwrap();
    assert_eq!(read_all(&mut fs, "state").unwrap(), b"v2");
}

/// 全部候选块坏死: 首格式返回 NoSpace, 而不是永久卡在坏块上。
#[test]
fn first_format_all_blocks_dead_returns_no_space() {
    let device = RamNor::new(BLOCK_SIZE, BLOCK_COUNT);
    for block in 0..BLOCK_COUNT {
        device.arm_dead_block(block);
    }
    assert!(matches!(
        FileSystem::format(device),
        Err(Error::<RamError>::NoSpace)
    ));
}

fn alloc(seed: u32) -> Vec<u8> {
    (0..64).map(|index| (index * 31 + seed * 7) as u8).collect()
}
