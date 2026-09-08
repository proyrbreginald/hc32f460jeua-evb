//! 掉电安全穷举 (每个事务在全部事件切点 × 字节序下中断, 恢复后必须
//! 挂载到完整旧版本或完整新版本)。
//!
//! 覆盖操作: 文件替换写 / 跨尾部三块写 / 新文件创建 / remove / rename /
//! mkdir / rmdir / 目录树 rename / clear / 首格式 / 重格式化, 以及真实
//! 16×8KB 几何的写入冒烟 (前述用例多为 128B 块)。

mod common;

use common::{RamError, RamNor, read_all};
use littlefs::{EntryKind, Error, FileSystem};

const TEAR_ORDERS: [[usize; 4]; 4] = [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1], [1, 3, 0, 2]];

fn seeded(name: &str, data: &[u8]) -> RamNor {
    let mut fs = FileSystem::format(RamNor::new(128, 8)).unwrap();
    fs.write(name, data).unwrap();
    fs.into_device()
}

fn assert_power_loss<E>(result: Result<(), Error<E>>)
where
    E: core::fmt::Debug + PartialEq,
{
    assert!(result.is_err());
}

fn measured_write_events(base: &RamNor, name: &str, data: &[u8]) -> u64 {
    let device = base.fork();
    let observer = device.clone();
    let mut fs = FileSystem::mount(device).unwrap();
    observer.reset_events();
    fs.write(name, data).unwrap();
    observer.events()
}

/// 首格式 (virgin 介质) 断电: 恢复后要么无有效快照 (NotFormatted),
/// 要么是完整的空文件系统 —— 不能出现部分初始化的快照。
#[test]
fn every_first_format_cut_is_empty_or_not_formatted() {
    let base = RamNor::new(128, 8);
    let event_count = measured_format_events(&base);
    assert!(event_count > 0);

    for order in TEAR_ORDERS {
        for cut in 0..event_count {
            let device = base.fork();
            let observer = device.clone();
            device.arm_power_loss(cut, order);
            let result = FileSystem::format(device);
            assert!(result.is_err(), "cut={cut} order={order:?} 不应完成");

            observer.power_cycle();
            match FileSystem::mount(observer) {
                Ok(mut recovered) => {
                    assert_eq!(recovered.info().file_count, 0, "cut={cut} order={order:?}");
                    assert_eq!(
                        recovered.stat("anything"),
                        Err(Error::NotFound),
                        "cut={cut} order={order:?}"
                    );
                    recovered.verify().unwrap();
                }
                Err(Error::NotFormatted) => {}
                Err(error) => panic!("cut={cut} order={order:?}: {error:?}"),
            }
        }
    }
}

/// 新建文件 (非替换已有条目) 断电: 新文件要么完全不存在, 要么完整;
/// 已有条目始终不变。
#[test]
fn every_new_file_write_cut_is_absent_or_complete() {
    let value = b"brand-new-file-payload";
    let base = seeded("existing", b"untouched");

    let device = base.fork();
    let observer = device.clone();
    let mut fs = FileSystem::mount(device).unwrap();
    observer.reset_events();
    fs.write("added", value).unwrap();
    let event_count = observer.events();
    assert!(event_count > 0);

    for order in TEAR_ORDERS {
        for cut in 0..event_count {
            let device = base.fork();
            let observer = device.clone();
            device.arm_power_loss(cut, order);
            let mut fs = FileSystem::mount(device).unwrap();
            assert_power_loss(fs.write("added", value));

            observer.power_cycle();
            let mut recovered = FileSystem::mount(observer)
                .unwrap_or_else(|error| panic!("new-file cut={cut} order={order:?}: {error:?}"));
            assert_eq!(read_all(&mut recovered, "existing").unwrap(), b"untouched");
            match recovered.stat("added") {
                Ok(_) => assert_eq!(
                    read_all(&mut recovered, "added").unwrap(),
                    value,
                    "cut={cut} order={order:?}"
                ),
                Err(Error::NotFound) => {}
                other => panic!("cut={cut} order={order:?}: {other:?}"),
            }
            recovered.verify().unwrap();
        }
    }
}

/// clear 断电: 恢复后要么是旧的完整目录树, 要么是完全空文件系统。
#[test]
fn every_clear_cut_is_old_files_or_empty() {
    let mut fs = FileSystem::format(RamNor::new(128, 8)).unwrap();
    fs.write("a", b"one").unwrap();
    fs.write("b", b"two").unwrap();
    fs.mkdir("d").unwrap();
    fs.write("d/c", b"three").unwrap();
    let base = fs.into_device();

    let device = base.fork();
    let observer = device.clone();
    let mut fs = FileSystem::mount(device).unwrap();
    observer.reset_events();
    fs.clear().unwrap();
    let event_count = observer.events();
    assert!(event_count > 0);

    for cut in 0..event_count {
        let device = base.fork();
        let observer = device.clone();
        device.arm_power_loss(cut, TEAR_ORDERS[3]);
        let mut fs = FileSystem::mount(device).unwrap();
        assert_power_loss(fs.clear());

        observer.power_cycle();
        let mut recovered = FileSystem::mount(observer).unwrap();
        if recovered.info().file_count == 0 {
            assert_eq!(
                recovered.stat("a"),
                Err(Error::NotFound),
                "cut={cut}: 空快照不得残留条目"
            );
            assert_eq!(
                recovered.stat("d/c"),
                Err(Error::NotFound),
                "cut={cut}: 空快照不得残留目录树"
            );
        } else {
            assert_eq!(read_all(&mut recovered, "a").unwrap(), b"one", "cut={cut}");
            assert_eq!(read_all(&mut recovered, "b").unwrap(), b"two", "cut={cut}");
            assert_eq!(
                read_all(&mut recovered, "d/c").unwrap(),
                b"three",
                "cut={cut}"
            );
        }
        recovered.verify().unwrap();
    }
}

/// 真实分区几何 (16 × 8KiB) 的写入断电冒烟: 此前全部 powerloss 用例
/// 使用 128B 块, 几何相关逻辑 (snapshot span / 候选搜索 / 磨损表宽度)
/// 未在此类掉电场景覆盖。
#[test]
fn geometry16_write_cut_recovers_one_complete_version() {
    let old = vec![0x21; 9_000];
    let new = vec![0x42; 9_000];
    let mut fs = FileSystem::format(RamNor::new(8192, 16)).unwrap();
    fs.write("blob", &old).unwrap();
    let base = fs.into_device();

    let event_count = measured_write_events(&base, "blob", &new);
    assert!(event_count > 0);
    for cut in 0..event_count {
        let device = base.fork();
        device.arm_power_loss(cut, TEAR_ORDERS[1]);
        let mut fs = FileSystem::mount(device).unwrap();
        assert_power_loss(fs.write("blob", &new));

        let recovered_device = fs.into_device();
        recovered_device.power_cycle();
        let mut recovered = FileSystem::mount(recovered_device)
            .unwrap_or_else(|error| panic!("geometry16 cut={cut}: {error:?}"));
        let value = read_all(&mut recovered, "blob").unwrap();
        assert!(value == old || value == new, "geometry16 cut={cut}");

        recovered.info(); // 其余 API 路径按需扩展
        recovered.verify().unwrap();
    }
}

#[test]
fn every_write_cut_recovers_one_complete_version() {
    let old = b"old-complete-value";
    let new = b"new-complete-value-with-more-data";
    let base = seeded("state", old);
    let event_count = measured_write_events(&base, "state", new);
    assert!(event_count > 0);

    for order in TEAR_ORDERS {
        for cut in 0..event_count {
            let device = base.fork();
            device.arm_power_loss(cut, order);
            let mut fs = FileSystem::mount(device).unwrap();
            let result = fs.write("state", new);
            assert_power_loss(result);
            assert!(fs.recovery_required(), "cut={cut} order={order:?}");
            assert_eq!(fs.write("must-not-run", b"x"), Err(Error::RecoveryRequired));

            let recovered_device = fs.into_device();
            recovered_device.power_cycle();
            let mut recovered = FileSystem::mount(recovered_device)
                .unwrap_or_else(|error| panic!("cut={cut} order={order:?}: {error:?}"));
            let value = read_all(&mut recovered, "state").unwrap();
            assert!(
                value == old || value == new,
                "cut={cut} order={order:?}: {value:?}"
            );
        }
    }
}

#[test]
fn every_cut_of_wrapped_three_block_write_is_recoverable() {
    let old = vec![0x41; 200];
    let new = (0..200).map(|index| (index * 3) as u8).collect::<Vec<_>>();

    let mut fs = FileSystem::format(RamNor::new(128, 8)).unwrap();
    fs.write("blob", &[0x20; 200]).unwrap(); // start 1, span 3
    fs.write("blob", &old).unwrap(); // start 4, span 3
    let base = fs.into_device();

    let event_count = measured_write_events(&base, "blob", &new);
    for cut in 0..event_count {
        let device = base.fork();
        device.arm_power_loss(cut, TEAR_ORDERS[3]);
        let mut fs = FileSystem::mount(device).unwrap();
        assert_power_loss(fs.write("blob", &new)); // destination wraps 7 -> 0 -> 1

        let recovered_device = fs.into_device();
        recovered_device.power_cycle();
        let mut recovered = FileSystem::mount(recovered_device)
            .unwrap_or_else(|error| panic!("wrapped cut={cut}: {error:?}"));
        let value = read_all(&mut recovered, "blob").unwrap();
        assert!(value == old || value == new, "wrapped cut={cut}");
    }
}

fn measured_remove_events(base: &RamNor) -> u64 {
    let device = base.fork();
    let observer = device.clone();
    let mut fs = FileSystem::mount(device).unwrap();
    observer.reset_events();
    fs.remove("obsolete").unwrap();
    observer.events()
}

#[test]
fn every_remove_cut_is_old_or_removed() {
    let old = b"must-remain-complete";
    let base = seeded("obsolete", old);
    let event_count = measured_remove_events(&base);

    for cut in 0..event_count {
        let device = base.fork();
        device.arm_power_loss(cut, TEAR_ORDERS[2]);
        let mut fs = FileSystem::mount(device).unwrap();
        assert_power_loss(fs.remove("obsolete"));

        let recovered_device = fs.into_device();
        recovered_device.power_cycle();
        let mut recovered = FileSystem::mount(recovered_device).unwrap();
        match recovered.stat("obsolete") {
            Ok(_) => assert_eq!(read_all(&mut recovered, "obsolete").unwrap(), old),
            Err(Error::NotFound) => {}
            other => panic!("cut={cut}: {other:?}"),
        }
    }
}

fn measured_rename_events(base: &RamNor) -> u64 {
    let device = base.fork();
    let observer = device.clone();
    let mut fs = FileSystem::mount(device).unwrap();
    observer.reset_events();
    fs.rename("before", "after").unwrap();
    observer.events()
}

#[test]
fn every_rename_cut_has_exactly_one_name() {
    let value = b"rename-payload";
    let base = seeded("before", value);
    let event_count = measured_rename_events(&base);

    for order in TEAR_ORDERS {
        for cut in 0..event_count {
            let device = base.fork();
            device.arm_power_loss(cut, order);
            let mut fs = FileSystem::mount(device).unwrap();
            assert_power_loss(fs.rename("before", "after"));

            let recovered_device = fs.into_device();
            recovered_device.power_cycle();
            let mut recovered = FileSystem::mount(recovered_device).unwrap();
            let before = recovered.stat("before").is_ok();
            let after = recovered.stat("after").is_ok();
            assert_ne!(before, after, "cut={cut} order={order:?}");
            let name = if before { "before" } else { "after" };
            assert_eq!(read_all(&mut recovered, name).unwrap(), value);
        }
    }
}

fn measured_mkdir_events(base: &RamNor) -> u64 {
    let device = base.fork();
    let observer = device.clone();
    let mut fs = FileSystem::mount(device).unwrap();
    observer.reset_events();
    fs.mkdir("created").unwrap();
    observer.events()
}

#[test]
fn every_mkdir_cut_is_absent_or_a_complete_directory() {
    let fs = FileSystem::format(RamNor::new(128, 8)).unwrap();
    let base = fs.into_device();
    let event_count = measured_mkdir_events(&base);

    for cut in 0..event_count {
        let device = base.fork();
        device.arm_power_loss(cut, TEAR_ORDERS[0]);
        let mut fs = FileSystem::mount(device).unwrap();
        assert_power_loss(fs.mkdir("created"));

        let recovered_device = fs.into_device();
        recovered_device.power_cycle();
        let mut recovered = FileSystem::mount(recovered_device).unwrap();
        match recovered.stat("created") {
            Ok(info) => assert_eq!(info.kind, EntryKind::Directory, "cut={cut}"),
            Err(Error::NotFound) => {}
            other => panic!("cut={cut}: {other:?}"),
        }
        recovered.verify().unwrap();
    }
}

fn measured_rmdir_events(base: &RamNor) -> u64 {
    let device = base.fork();
    let observer = device.clone();
    let mut fs = FileSystem::mount(device).unwrap();
    observer.reset_events();
    fs.rmdir("empty").unwrap();
    observer.events()
}

#[test]
fn every_rmdir_cut_is_present_or_completely_removed() {
    let mut fs = FileSystem::format(RamNor::new(128, 8)).unwrap();
    fs.mkdir("empty").unwrap();
    let base = fs.into_device();
    let event_count = measured_rmdir_events(&base);

    for cut in 0..event_count {
        let device = base.fork();
        device.arm_power_loss(cut, TEAR_ORDERS[1]);
        let mut fs = FileSystem::mount(device).unwrap();
        assert_power_loss(fs.rmdir("empty"));

        let recovered_device = fs.into_device();
        recovered_device.power_cycle();
        let mut recovered = FileSystem::mount(recovered_device).unwrap();
        match recovered.stat("empty") {
            Ok(info) => assert_eq!(info.kind, EntryKind::Directory, "cut={cut}"),
            Err(Error::NotFound) => {}
            other => panic!("cut={cut}: {other:?}"),
        }
        recovered.verify().unwrap();
    }
}

fn directory_tree() -> RamNor {
    let mut fs = FileSystem::format(RamNor::new(128, 8)).unwrap();
    fs.mkdir("tree").unwrap();
    fs.mkdir("tree/sub").unwrap();
    fs.write("tree/root", b"root-value").unwrap();
    fs.write("tree/sub/leaf", b"leaf-value").unwrap();
    fs.write("treehouse", b"prefix-neighbor").unwrap();
    fs.into_device()
}

fn measured_directory_rename_events(base: &RamNor) -> u64 {
    let device = base.fork();
    let observer = device.clone();
    let mut fs = FileSystem::mount(device).unwrap();
    observer.reset_events();
    fs.rename("tree", "moved").unwrap();
    observer.events()
}

#[test]
fn every_directory_rename_cut_recovers_one_complete_tree() {
    let base = directory_tree();
    let event_count = measured_directory_rename_events(&base);

    for order in TEAR_ORDERS {
        for cut in 0..event_count {
            let device = base.fork();
            device.arm_power_loss(cut, order);
            let mut fs = FileSystem::mount(device).unwrap();
            assert_power_loss(fs.rename("tree", "moved"));

            let recovered_device = fs.into_device();
            recovered_device.power_cycle();
            let mut recovered = FileSystem::mount(recovered_device).unwrap_or_else(|error| {
                panic!("directory rename cut={cut} order={order:?}: {error:?}")
            });
            let old = recovered.stat("tree").is_ok();
            let new = recovered.stat("moved").is_ok();
            assert_ne!(old, new, "cut={cut} order={order:?}");

            let root = if old { "tree" } else { "moved" };
            let other = if old { "moved" } else { "tree" };
            assert_eq!(
                recovered.stat(root).unwrap().kind,
                EntryKind::Directory,
                "cut={cut} order={order:?}"
            );
            assert_eq!(
                recovered.stat(&format!("{root}/sub")).unwrap().kind,
                EntryKind::Directory
            );
            assert_eq!(
                read_all(&mut recovered, &format!("{root}/root")).unwrap(),
                b"root-value"
            );
            assert_eq!(
                read_all(&mut recovered, &format!("{root}/sub/leaf")).unwrap(),
                b"leaf-value"
            );
            assert_eq!(recovered.stat(other), Err(Error::NotFound));
            assert_eq!(
                recovered.stat(&format!("{other}/sub/leaf")),
                Err(Error::NotFound)
            );
            assert_eq!(
                read_all(&mut recovered, "treehouse").unwrap(),
                b"prefix-neighbor"
            );
            recovered.verify().unwrap();
        }
    }
}

fn measured_format_events(base: &RamNor) -> u64 {
    let device = base.fork();
    let observer = device.clone();
    observer.reset_events();
    let _fs = FileSystem::format(device).unwrap();
    observer.events()
}

#[test]
fn every_reformat_cut_is_old_or_empty() {
    let value = b"preserved-until-empty-commit";
    let base = seeded("state", value);
    let event_count = measured_format_events(&base);

    for cut in 0..event_count {
        let device = base.fork();
        let observer = device.clone();
        observer.arm_power_loss(cut, TEAR_ORDERS[1]);
        let result = FileSystem::format(device);
        assert!(result.is_err(), "cut {cut} unexpectedly completed");

        observer.power_cycle();
        let mut recovered = FileSystem::mount(observer).unwrap();
        match recovered.stat("state") {
            Ok(_) => assert_eq!(read_all(&mut recovered, "state").unwrap(), value),
            Err(Error::NotFound) => assert_eq!(recovered.info().file_count, 0),
            other => panic!("cut={cut}: {other:?}"),
        }
    }
}

#[test]
fn injected_device_error_requires_remount() {
    let base = seeded("state", b"old");
    let device = base.fork();
    device.arm_power_loss(0, TEAR_ORDERS[0]);
    let mut fs = FileSystem::mount(device).unwrap();
    assert_eq!(
        fs.write("state", b"new"),
        Err(Error::Device(RamError::PowerLoss))
    );
    assert_eq!(fs.remove("state"), Err(Error::RecoveryRequired));
}
