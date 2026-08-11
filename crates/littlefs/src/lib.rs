#![no_std]
#![forbid(unsafe_code)]

//! A bounded, power-loss-safe snapshot filesystem for NOR flash.
//!
//! The disk format is intentionally incompatible with littlefs. See
//! `DESIGN.md` for the fault model and commit ordering.

pub mod format;
mod fs;

pub use format::{BlockDevice, Geometry, MAX_ENTRIES, MAX_FILES, MAX_NAME_LEN};
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
    /// A canonical root-relative path is malformed or too long.
    InvalidName,
    /// The requested entry or one of its parents does not exist.
    NotFound,
    /// The destination entry already exists.
    AlreadyExists,
    /// A file operation was requested for a directory.
    IsDirectory,
    /// A directory operation was requested for a file.
    NotDirectory,
    /// A directory still contains entries.
    DirectoryNotEmpty,
    /// A directory cannot be moved into its own subtree.
    InvalidMove,
    /// The entry count or serialized snapshot exceeds its fixed bound.
    NoSpace,
    /// A file length cannot be represented by this format.
    FileTooLarge,
    /// A prior failed mutation has an uncertain durable outcome; remount first.
    RecoveryRequired,
}

/// Type of a persistent filesystem entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

/// Immutable entry metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryInfo {
    pub kind: EntryKind,
    pub size: u32,
    pub crc32: u32,
}

/// Backward-compatible name for entry metadata.
pub type FileInfo = EntryInfo;

/// Current filesystem and capacity information.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsInfo {
    pub generation: u32,
    /// Total file and directory record count.
    pub entry_count: u32,
    /// Backward-compatible alias of [`FsInfo::entry_count`].
    pub file_count: u32,
    pub serialized_bytes: u32,
    pub active_blocks: u32,
    pub capacity_bytes: u32,
}
