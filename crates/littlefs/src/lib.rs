#![no_std]
#![forbid(unsafe_code)]

//! A bounded, power-loss-safe snapshot filesystem for NOR flash.
//!
//! The disk format is intentionally incompatible with littlefs. See
//! `DESIGN.md` for the fault model and commit ordering.

pub mod format;
mod fs;

pub use format::{BlockDevice, Geometry, MAX_FILES, MAX_NAME_LEN};
pub use fs::FileSystem;

/// Filesystem operation failure.
#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    /// The block device rejected an operation.
    Device(E),
    /// A board-level singleton could not be acquired.
    DeviceBusy,
    /// The device geometry cannot support this format.
    InvalidGeometry,
    /// No valid committed snapshot exists.
    NotFormatted,
    /// A committed snapshot or file record failed validation.
    Corrupt,
    /// A filename is empty, too long, or contains a forbidden byte.
    InvalidName,
    /// The requested file does not exist.
    NotFound,
    /// The rename destination already exists.
    AlreadyExists,
    /// The file count or serialized snapshot exceeds its fixed bound.
    NoSpace,
    /// A file length cannot be represented by this format.
    FileTooLarge,
    /// A prior failed mutation has an uncertain durable outcome; remount first.
    RecoveryRequired,
}

/// Immutable file metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub size: u32,
    pub crc32: u32,
}

/// Current filesystem and capacity information.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsInfo {
    pub generation: u32,
    pub file_count: u32,
    pub serialized_bytes: u32,
    pub active_blocks: u32,
    pub capacity_bytes: u32,
}
