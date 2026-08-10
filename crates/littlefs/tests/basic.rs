mod common;

use common::{RamError, RamNor, read_all};
use littlefs::format::{COMMIT_OFFSET, HEADER_SIZE, SnapshotHeader, generation_is_newer};
use littlefs::{Error, FileSystem, Geometry, MAX_FILES};

#[test]
fn basic_file_workflow_survives_remount() {
    let device = RamNor::new(256, 8);
    let mut fs = FileSystem::format(device).unwrap();
    assert_eq!(fs.info().file_count, 0);

    fs.write("config", b"alpha").unwrap();
    fs.write("log", b"0123456789").unwrap();
    assert_eq!(fs.stat("config").unwrap().size, 5);

    let mut partial = [0u8; 4];
    assert_eq!(fs.read("log", 3, &mut partial).unwrap(), 4);
    assert_eq!(&partial, b"3456");
    assert_eq!(fs.read("log", 99, &mut partial).unwrap(), 0);

    let mut names = Vec::new();
    fs.list(|name, info| names.push((name.to_owned(), info.size)))
        .unwrap();
    assert_eq!(names, [("config".to_owned(), 5), ("log".to_owned(), 10)]);

    fs.write("config", b"beta-version").unwrap();
    fs.rename("log", "history").unwrap();
    fs.remove("config").unwrap();
    fs.verify().unwrap();

    let device = fs.into_device();
    let mut fs = FileSystem::mount(device).unwrap();
    assert_eq!(read_all(&mut fs, "history").unwrap(), b"0123456789");
    assert_eq!(fs.stat("config"), Err(Error::NotFound));
    assert_eq!(fs.info().file_count, 1);

    fs.clear().unwrap();
    assert_eq!(fs.info().file_count, 0);
    assert_eq!(
        FileSystem::mount(fs.into_device())
            .unwrap()
            .info()
            .file_count,
        0
    );
}

#[test]
fn names_and_namespace_errors_are_explicit() {
    let mut fs = FileSystem::format(RamNor::new(512, 8)).unwrap();
    for invalid in ["", "a/b", "\0", &"x".repeat(64)] {
        assert_eq!(fs.write(invalid, b"x"), Err(Error::InvalidName));
    }

    fs.write("source", b"value").unwrap();
    fs.write("target", b"other").unwrap();
    assert_eq!(fs.rename("missing", "new"), Err(Error::NotFound));
    assert_eq!(fs.rename("source", "target"), Err(Error::AlreadyExists));
    assert_eq!(fs.remove("missing"), Err(Error::NotFound));
    fs.rename("source", "source").unwrap();

    fs.write("配置", b"utf8-name").unwrap();
    assert_eq!(read_all(&mut fs, "配置").unwrap(), b"utf8-name");
}

#[test]
fn no_space_is_preflight_and_does_not_poison_mount() {
    let mut fs = FileSystem::format(RamNor::new(256, 8)).unwrap();
    let capacity = fs.info().capacity_bytes as usize;
    assert_eq!(
        fs.write("too-big", &vec![0x5a; capacity]),
        Err(Error::NoSpace)
    );
    assert!(!fs.recovery_required());

    fs.write("small", b"still writable").unwrap();
    assert_eq!(read_all(&mut fs, "small").unwrap(), b"still writable");
}

#[test]
fn file_count_is_bounded_without_allocation() {
    let mut fs = FileSystem::format(RamNor::new(512, 8)).unwrap();
    for index in 0..MAX_FILES {
        fs.write(&format!("f{index:02}"), b"").unwrap();
    }
    assert_eq!(fs.info().file_count, MAX_FILES);
    assert_eq!(fs.write("overflow", b""), Err(Error::NoSpace));
    assert!(!fs.recovery_required());
}

#[test]
fn rotating_single_block_snapshots_distribute_erases() {
    let device = RamNor::new(256, 8);
    let observer = device.clone();
    let mut fs = FileSystem::format(device).unwrap();
    for value in 0..63u8 {
        fs.write("counter", &[value]).unwrap();
    }

    let counts = observer.erase_counts();
    let minimum = *counts.iter().min().unwrap();
    let maximum = *counts.iter().max().unwrap();
    assert!(maximum - minimum <= 1, "erase distribution: {counts:?}");
    assert!(counts.iter().all(|count| *count > 0));
}

#[test]
fn multi_block_snapshot_wraps_across_partition_end() {
    let mut fs = FileSystem::format(RamNor::new(128, 8)).unwrap();
    let first = vec![0x11; 200];
    let second = vec![0x22; 200];
    let wrapped = (0..200).map(|index| index as u8).collect::<Vec<_>>();

    // Empty starts at block 0. These three-span snapshots start at blocks
    // 1, 4, and 7; the last one physically occupies 7, 0, and 1.
    fs.write("blob", &first).unwrap();
    fs.write("blob", &second).unwrap();
    fs.write("blob", &wrapped).unwrap();
    assert_eq!(fs.info().active_blocks, 3);
    assert_eq!(read_all(&mut fs, "blob").unwrap(), wrapped);

    let mut remounted = FileSystem::mount(fs.into_device()).unwrap();
    assert_eq!(read_all(&mut remounted, "blob").unwrap(), wrapped);
    remounted.verify().unwrap();
}

#[test]
fn corrupt_newest_snapshot_falls_back_to_previous_generation() {
    let device = RamNor::new(256, 8);
    let observer = device.clone();
    let mut fs = FileSystem::format(device).unwrap();
    fs.write("state", b"old").unwrap();
    fs.write("state", b"new").unwrap();
    drop(fs);

    let geometry = Geometry::new(256, 8);
    let image = observer.bytes();
    let mut newest: Option<SnapshotHeader> = None;
    for block in 0..geometry.block_count {
        let offset = (block * geometry.block_size) as usize;
        let Ok(header) =
            SnapshotHeader::decode_at(&image[offset..offset + HEADER_SIZE], block, geometry)
        else {
            continue;
        };
        if newest
            .map(|current| generation_is_newer(header.generation, current.generation))
            .unwrap_or(true)
        {
            newest = Some(header);
        }
    }
    let newest = newest.unwrap();
    let payload_byte = (newest.start_block * newest.block_size) as usize + HEADER_SIZE;
    observer.overwrite_raw(payload_byte, &[image[payload_byte] ^ 0x01]);

    let mut recovered = FileSystem::mount(observer).unwrap();
    assert_eq!(read_all(&mut recovered, "state").unwrap(), b"old");
}

#[test]
fn verify_rejects_corrupted_header_and_commit_word() {
    for offset in [0, COMMIT_OFFSET] {
        let device = RamNor::new(256, 8);
        let observer = device.clone();
        let mut fs = FileSystem::format(device).unwrap();
        let image = observer.bytes();

        observer.overwrite_raw(offset, &[image[offset] ^ 0x01]);

        assert_eq!(fs.verify(), Err(Error::Corrupt), "offset={offset}");
    }
}

#[test]
fn unformatted_media_is_reported_without_writes() {
    assert!(matches!(
        FileSystem::mount(RamNor::new(256, 8)),
        Err(Error::<RamError>::NotFormatted)
    ));
}
