//! Power-loss-safe filesystem integration for the HC32F460 internal Flash.
//!
//! The generic filesystem lives in `crates/littlefs`. This module owns the
//! board partition and narrows the permissive EFM driver to the filesystem's
//! NOR contract: relative addresses, one owner, aligned one-shot programming,
//! and mandatory erase/program readback.
#![allow(dead_code)]

use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering};

use littlefs::BlockDevice;

/// First filesystem sector (sector 54).
pub const PARTITION_START: u32 = 0x0006_C000;
/// Filesystem partition size: sectors 54 through 61.
pub const PARTITION_SIZE: u32 = 64 * 1024;
/// HC32F460 main-Flash erase sector size.
pub const BLOCK_SIZE: u32 = crate::efm::SECTOR_SIZE;
/// Number of sectors in the filesystem partition.
pub const BLOCK_COUNT: u32 = PARTITION_SIZE / BLOCK_SIZE;
/// First address not owned by the filesystem (sector 62 self-test area).
pub const PARTITION_END: u32 = PARTITION_START + PARTITION_SIZE;

const _: () = assert!(PARTITION_START.is_multiple_of(crate::efm::SECTOR_SIZE));
const _: () = assert!(PARTITION_END == 0x0007_C000);
const _: () = assert!(BLOCK_COUNT == 8);

static TAKEN: AtomicBool = AtomicBool::new(false);

/// Filesystem-specific internal-Flash failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashError {
    /// A block/offset/length escaped the reserved partition.
    OutOfBounds,
    /// Program offset or length was not four-byte aligned.
    Unaligned,
    /// The operation was attempted from an interrupt handler.
    InterruptContext,
    /// A program word was not completely erased.
    NotErased,
    /// Erase or program readback did not match the requested state.
    VerifyFailed,
    /// The EFM controller reported an operation failure.
    Controller(crate::efm::EfmError),
}

/// Unique owner of sectors 54 through 61.
///
/// `PhantomData<*mut ()>` intentionally keeps this token `!Send + !Sync`:
/// Flash operations stall the executing bus and are confined to one thread.
pub struct InternalFlash {
    _not_send_or_sync: PhantomData<*mut ()>,
}

impl InternalFlash {
    /// Acquires the filesystem partition once. Dropping the token releases it.
    fn take() -> Option<Self> {
        TAKEN
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self {
                _not_send_or_sync: PhantomData,
            })
    }

    fn check_thread_context() -> Result<(), FlashError> {
        if crate::critical_section::in_isr() {
            Err(FlashError::InterruptContext)
        } else {
            Ok(())
        }
    }

    fn address(block: u32, offset: u32, len: usize) -> Result<u32, FlashError> {
        if block >= BLOCK_COUNT || offset > BLOCK_SIZE {
            return Err(FlashError::OutOfBounds);
        }
        let len = u32::try_from(len).map_err(|_| FlashError::OutOfBounds)?;
        let end = offset.checked_add(len).ok_or(FlashError::OutOfBounds)?;
        if end > BLOCK_SIZE {
            return Err(FlashError::OutOfBounds);
        }
        PARTITION_START
            .checked_add(
                block
                    .checked_mul(BLOCK_SIZE)
                    .ok_or(FlashError::OutOfBounds)?,
            )
            .and_then(|base| base.checked_add(offset))
            .filter(|addr| *addr <= PARTITION_END)
            .ok_or(FlashError::OutOfBounds)
    }

    fn verify_erased(block: u32) -> Result<(), FlashError> {
        let base = Self::address(block, 0, BLOCK_SIZE as usize)?;
        let mut offset = 0;
        while offset < BLOCK_SIZE {
            if crate::efm::read_word(base + offset).map_err(FlashError::Controller)? != u32::MAX {
                return Err(FlashError::VerifyFailed);
            }
            offset += 4;
        }
        Ok(())
    }

    fn partition_is_erased(&self) -> Result<bool, FlashError> {
        Self::check_thread_context()?;
        let mut address = PARTITION_START;
        while address < PARTITION_END {
            if crate::efm::read_word(address).map_err(FlashError::Controller)? != u32::MAX {
                return Ok(false);
            }
            address += 4;
        }
        Ok(true)
    }
}

impl Drop for InternalFlash {
    fn drop(&mut self) {
        TAKEN.store(false, Ordering::Release);
    }
}

impl BlockDevice for InternalFlash {
    type Error = FlashError;

    fn block_size(&self) -> u32 {
        BLOCK_SIZE
    }

    fn block_count(&self) -> u32 {
        BLOCK_COUNT
    }

    fn read(&mut self, block: u32, offset: u32, buffer: &mut [u8]) -> Result<(), Self::Error> {
        Self::check_thread_context()?;
        let address = Self::address(block, offset, buffer.len())?;
        for (index, byte) in buffer.iter_mut().enumerate() {
            *byte =
                crate::efm::read_byte(address + index as u32).map_err(FlashError::Controller)?;
        }
        Ok(())
    }

    fn program(&mut self, block: u32, offset: u32, data: &[u8]) -> Result<(), Self::Error> {
        Self::check_thread_context()?;
        if !offset.is_multiple_of(4) || !data.len().is_multiple_of(4) {
            return Err(FlashError::Unaligned);
        }
        let address = Self::address(block, offset, data.len())?;

        // HC32F460 does not guarantee repeated programming, even for 1 -> 0.
        // Every destination word must still be in its pristine erased state.
        for index in (0..data.len()).step_by(4) {
            if crate::efm::read_word(address + index as u32).map_err(FlashError::Controller)?
                != u32::MAX
            {
                return Err(FlashError::NotErased);
            }
        }

        crate::efm::program(address, data).map_err(FlashError::Controller)?;

        for (index, expected) in data.chunks_exact(4).enumerate() {
            let actual = crate::efm::read_word(address + (index * 4) as u32)
                .map_err(FlashError::Controller)?;
            if actual.to_le_bytes() != expected {
                return Err(FlashError::VerifyFailed);
            }
        }
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), Self::Error> {
        Self::check_thread_context()?;
        let address = Self::address(block, 0, BLOCK_SIZE as usize)?;
        crate::efm::sector_erase(address).map_err(FlashError::Controller)?;
        Self::verify_erased(block)
    }

    fn sync(&mut self) -> Result<(), Self::Error> {
        Self::check_thread_context()?;
        if !crate::efm::wait_ready() {
            return Err(FlashError::Controller(crate::efm::EfmError::Timeout));
        }
        crate::arch::data_sync_barrier();
        Ok(())
    }
}

/// Mounted filesystem over the board's fixed internal-Flash partition.
pub type FileSystem = littlefs::FileSystem<InternalFlash>;

/// Result of starting the filesystem during boot.
pub enum Startup {
    /// An existing committed snapshot was mounted without writing Flash.
    Mounted(FileSystem),
    /// A completely erased partition was initialized with an empty snapshot.
    Formatted(FileSystem),
}

/// Starts the filesystem during boot.
///
/// Existing media is mounted read-only. A missing snapshot is formatted only
/// when every word in the partition is still erased; non-erased invalid media
/// is preserved for explicit diagnosis or recovery.
pub fn start(
    resources: &crate::board::BoardResources,
) -> Result<Startup, littlefs::Error<FlashError>> {
    match mount(resources) {
        Ok(filesystem) => Ok(Startup::Mounted(filesystem)),
        Err(littlefs::Error::NotFormatted) => {
            let device = InternalFlash::take().ok_or(littlefs::Error::DeviceBusy)?;
            if !device
                .partition_is_erased()
                .map_err(littlefs::Error::Device)?
            {
                return Err(littlefs::Error::NotFormatted);
            }
            littlefs::FileSystem::format(device).map(Startup::Formatted)
        }
        Err(error) => Err(error),
    }
}

/// Mounts the existing internal filesystem after board initialization.
pub fn mount(
    _resources: &crate::board::BoardResources,
) -> Result<FileSystem, littlefs::Error<FlashError>> {
    let device = InternalFlash::take().ok_or(littlefs::Error::DeviceBusy)?;
    littlefs::FileSystem::mount(device)
}

/// Atomically formats the internal filesystem after board initialization.
pub fn format(
    _resources: &crate::board::BoardResources,
) -> Result<FileSystem, littlefs::Error<FlashError>> {
    let device = InternalFlash::take().ok_or(littlefs::Error::DeviceBusy)?;
    littlefs::FileSystem::format(device)
}
