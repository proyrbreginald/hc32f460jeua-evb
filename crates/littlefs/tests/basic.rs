mod common;

use common::{RamError, RamNor, read_all};
use littlefs::format::{
    COMMIT_OFFSET, HEADER_SIZE, MAX_WEAR_TABLE_SIZE, RECORD_FLAG_DIRECTORY, RECORD_HEADER_SIZE,
    RecordHeader, SnapshotHeader, crc32_mpeg2, encode_wear_table, generation_is_newer,
};
use littlefs::{EntryKind, Error, FileSystem, Geometry, MAX_FILES};

fn raw_snapshot(records: &[(&str, u16, &[u8])]) -> RamNor {
    let geometry = Geometry::new(512, 8);
    let mut payload = Vec::new();
    let mut table = [0u8; MAX_WEAR_TABLE_SIZE];
    let table_size = encode_wear_table(&[0; 64], geometry.block_count, &mut table).unwrap();
    payload.extend_from_slice(&table[..table_size]);
    for &(name, flags, data) in records {
        let header = RecordHeader::new(
            name.len() as u16,
            data.len() as u32,
            crc32_mpeg2(data),
            flags,
        )
        .unwrap();
        payload.extend_from_slice(&header.encode().unwrap());
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(data);
        payload.resize(
            payload.len() + header.record_len as usize
                - RECORD_HEADER_SIZE
                - name.len()
                - data.len(),
            0,
        );
    }

    let header = SnapshotHeader::new(
        0,
        0,
        payload.len() as u32,
        crc32_mpeg2(&payload),
        records.len() as u32,
        geometry,
    )
    .unwrap();
    let device = RamNor::new(geometry.block_size, geometry.block_count);
    device.overwrite_raw(0, &header.encode_committed().unwrap());
    device.overwrite_raw(HEADER_SIZE, &payload);
    device
}

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
    for invalid in [
        "",
        "/absolute",
        "trailing/",
        "a//b",
        "a/./b",
        "a/../b",
        "\0",
        &"x".repeat(64),
    ] {
        assert_eq!(fs.write(invalid, b"x"), Err(Error::InvalidName));
    }
    assert_eq!(fs.write("missing/file", b"x"), Err(Error::NotFound));

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
fn directory_workflow_is_typed_and_survives_remount() {
    let mut fs = FileSystem::format(RamNor::new(512, 8)).unwrap();
    assert_eq!(fs.stat("").unwrap().kind, EntryKind::Directory);

    fs.mkdir("etc").unwrap();
    fs.mkdir("etc/network").unwrap();
    fs.write("etc/config", b"mode=normal").unwrap();
    fs.write("root-file", b"root").unwrap();

    assert_eq!(fs.stat("etc").unwrap().kind, EntryKind::Directory);
    assert_eq!(fs.stat("etc/config").unwrap().kind, EntryKind::File);
    assert_eq!(fs.info().entry_count, 4);
    assert_eq!(fs.info().entry_count, fs.info().file_count);

    let mut root = Vec::new();
    fs.read_dir("", |name, info| {
        root.push((name.to_owned(), info.kind, info.size))
    })
    .unwrap();
    assert_eq!(
        root,
        [
            ("etc".to_owned(), EntryKind::Directory, 0),
            ("root-file".to_owned(), EntryKind::File, 4),
        ]
    );

    let mut etc = Vec::new();
    fs.read_dir("etc", |name, info| etc.push((name.to_owned(), info.kind)))
        .unwrap();
    assert_eq!(
        etc,
        [
            ("network".to_owned(), EntryKind::Directory),
            ("config".to_owned(), EntryKind::File),
        ]
    );

    assert_eq!(fs.read("etc", 0, &mut [0; 1]), Err(Error::IsDirectory));
    assert_eq!(fs.max_write_size("etc"), Err(Error::IsDirectory));
    assert_eq!(fs.write("etc", b"file"), Err(Error::IsDirectory));
    assert_eq!(fs.remove("etc"), Err(Error::IsDirectory));
    assert_eq!(fs.rmdir("etc/config"), Err(Error::NotDirectory));
    assert_eq!(fs.rmdir("etc"), Err(Error::DirectoryNotEmpty));
    assert_eq!(fs.mkdir("etc"), Err(Error::AlreadyExists));

    fs.write("plain", b"file").unwrap();
    assert_eq!(fs.mkdir("plain/child"), Err(Error::NotDirectory));
    assert_eq!(fs.write("plain/child", b"x"), Err(Error::NotDirectory));

    let mut fs = FileSystem::mount(fs.into_device()).unwrap();
    fs.verify().unwrap();
    assert_eq!(read_all(&mut fs, "etc/config").unwrap(), b"mode=normal");
    fs.remove("etc/config").unwrap();
    fs.rmdir("etc/network").unwrap();
    fs.rmdir("etc").unwrap();
    assert_eq!(fs.stat("etc"), Err(Error::NotFound));
}

#[test]
fn directory_rename_moves_one_complete_subtree() {
    let mut fs = FileSystem::format(RamNor::new(512, 8)).unwrap();
    fs.mkdir("a").unwrap();
    fs.mkdir("a/sub").unwrap();
    fs.write("a/sub/file", b"payload").unwrap();
    fs.write("ab", b"prefix-neighbor").unwrap();

    fs.rename("a", "moved").unwrap();
    assert_eq!(fs.stat("a"), Err(Error::NotFound));
    assert_eq!(fs.stat("a/sub"), Err(Error::NotFound));
    assert_eq!(fs.stat("moved").unwrap().kind, EntryKind::Directory);
    assert_eq!(fs.stat("moved/sub").unwrap().kind, EntryKind::Directory);
    assert_eq!(read_all(&mut fs, "moved/sub/file").unwrap(), b"payload");
    assert_eq!(read_all(&mut fs, "ab").unwrap(), b"prefix-neighbor");

    assert_eq!(
        fs.rename("moved", "moved/sub/deeper"),
        Err(Error::InvalidMove)
    );
    fs.write("target", b"occupied").unwrap();
    assert_eq!(fs.rename("moved", "target"), Err(Error::AlreadyExists));

    let mut remounted = FileSystem::mount(fs.into_device()).unwrap();
    remounted.verify().unwrap();
    assert_eq!(
        read_all(&mut remounted, "moved/sub/file").unwrap(),
        b"payload"
    );
}

#[test]
fn directory_rename_preflights_every_descendant_length() {
    let mut fs = FileSystem::format(RamNor::new(512, 8)).unwrap();
    fs.mkdir("d").unwrap();
    let child = format!("d/{}", "x".repeat(60));
    assert_eq!(child.len(), 62);
    fs.write(&child, b"value").unwrap();

    assert_eq!(fs.rename("d", "long"), Err(Error::InvalidName));
    assert!(!fs.recovery_required());
    assert_eq!(read_all(&mut fs, &child).unwrap(), b"value");
    assert_eq!(fs.stat("long"), Err(Error::NotFound));
}

#[test]
fn mount_accepts_legacy_files_and_rejects_invalid_trees() {
    let legacy = raw_snapshot(&[("legacy", 0, b"old-format-root-file")]);
    let mut fs = FileSystem::mount(legacy).unwrap();
    assert_eq!(fs.stat("legacy").unwrap().kind, EntryKind::File);
    assert_eq!(
        read_all(&mut fs, "legacy").unwrap(),
        b"old-format-root-file"
    );

    let orphan = raw_snapshot(&[("missing/child", 0, b"orphan")]);
    assert!(matches!(
        FileSystem::mount(orphan),
        Err(Error::<RamError>::NotFormatted)
    ));

    let file_parent = raw_snapshot(&[("parent", 0, b"file"), ("parent/child", 0, b"child")]);
    assert!(matches!(
        FileSystem::mount(file_parent),
        Err(Error::<RamError>::NotFormatted)
    ));

    let nonempty_directory = raw_snapshot(&[("dir", RECORD_FLAG_DIRECTORY, b"not-empty")]);
    assert!(matches!(
        FileSystem::mount(nonempty_directory),
        Err(Error::<RamError>::NotFormatted)
    ));
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
fn max_write_size_accounts_for_records_and_namespace_limit() {
    let mut fs = FileSystem::format(RamNor::new(256, 8)).unwrap();
    let empty_capacity = fs.max_write_size("a").unwrap();
    fs.write("other", &[0x41; 100]).unwrap();
    assert!(fs.max_write_size("a").unwrap() < empty_capacity);
    assert!(fs.max_write_size("other").unwrap() > fs.max_write_size("a").unwrap());

    let replacement_capacity = fs.max_write_size("other").unwrap() as usize;
    fs.write("other", &vec![0x42; replacement_capacity])
        .unwrap();
    assert_eq!(
        fs.write("other", &vec![0x43; replacement_capacity + 1]),
        Err(Error::NoSpace)
    );

    let mut full = FileSystem::format(RamNor::new(512, 8)).unwrap();
    for index in 0..MAX_FILES {
        full.write(&format!("f{index:02}"), b"").unwrap();
    }
    assert_eq!(full.max_write_size("overflow"), Err(Error::NoSpace));
    assert!(full.max_write_size("f00").is_ok());
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
