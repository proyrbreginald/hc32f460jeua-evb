mod common;

use common::{RamError, RamNor, read_all};
use littlefs::{Error, FileSystem};

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
