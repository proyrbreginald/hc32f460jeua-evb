//! Allocation-free on-disk primitives for the project's snapshot filesystem.
//!
//! This module deliberately contains no filesystem state machine. It defines the
//! block-device contract, CRC, fixed-width codecs, and checked layout arithmetic
//! used by the filesystem facade and its platform adapters.

/// A device program operation is aligned to this many bytes.
pub const PROGRAM_SIZE: usize = 4;
/// The erased value of every byte exposed by a [`BlockDevice`].
pub const ERASED_BYTE: u8 = 0xff;
/// The erased value of one program word.
pub const ERASED_WORD: u32 = 0xffff_ffff;

/// Snapshot magic (`RFS1` when viewed as bytes).
pub const MAGIC: u32 = 0x3153_4652;
/// Alias that makes the kind of [`MAGIC`] explicit at call sites.
pub const SNAPSHOT_MAGIC: u32 = MAGIC;
/// On-disk format version 1.0.
pub const DISK_VERSION: u32 = 0x0001_0000;
/// The word programmed last to make a snapshot visible to recovery.
pub const COMMIT_MARKER: u32 = 0xc35a_6f91;

/// Encoded snapshot-header size.
pub const HEADER_SIZE: usize = 64;
/// Number of bytes covered by the snapshot-header CRC.
pub const HEADER_CRC_OFFSET: usize = 56;
/// Offset of the header CRC in an encoded snapshot header.
pub const HEADER_CRC_FIELD_OFFSET: usize = HEADER_CRC_OFFSET;
/// Offset of the independently programmed commit word.
pub const COMMIT_OFFSET: usize = 60;
/// Number of bytes written before the independent commit word.
pub const SNAPSHOT_PREFIX_SIZE: usize = COMMIT_OFFSET;

/// Record magic (`FILE` when viewed as bytes).
pub const RECORD_MAGIC: u32 = 0x454c_4946;
/// Encoded record-header size.
pub const RECORD_HEADER_SIZE: usize = 20;
/// Maximum canonical root-relative UTF-8 path length in bytes.
pub const MAX_NAME_LEN: usize = 63;
/// Backward-compatible name for the maximum snapshot entry count.
pub const MAX_FILES: u32 = 32;
/// Maximum number of file and directory entries in one snapshot.
pub const MAX_ENTRIES: u32 = MAX_FILES;
/// Feature bits understood by this format version.
pub const SUPPORTED_FEATURES: u32 = 0;
/// Record flag identifying a directory entry.
pub const RECORD_FLAG_DIRECTORY: u16 = 1 << 0;
/// Record flag bits understood by this format version.
pub const SUPPORTED_RECORD_FLAGS: u16 = RECORD_FLAG_DIRECTORY;

/// Runtime geometry reported by a block device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
    pub block_size: u32,
    pub block_count: u32,
}

impl Geometry {
    pub const fn new(block_size: u32, block_count: u32) -> Self {
        Self {
            block_size,
            block_count,
        }
    }
}

/// Storage operations required by the filesystem facade.
///
/// Implementations must uphold all of the following rules:
///
/// - `block_size()` and `block_count()` are stable for the lifetime of a mount.
/// - A block is erased to all `0xff`; `erase` operates on exactly one block.
/// - `read` stays within one block but may use any byte offset and length.
/// - `program` stays within one block. Its offset and length are multiples of
///   [`PROGRAM_SIZE`]. It never performs an implicit read-modify-write.
/// - Each 4-byte word passed to `program` is still erased and is programmed at
///   most once between erases. Retrying a possibly completed word is forbidden.
/// - Returning `Ok(())` from `program` means every requested word is complete.
///   Power loss or an error may instead leave the current word torn: any subset
///   of the requested 1-to-0 transitions may have occurred. Such a word must
///   not be retried; its block must be erased before reuse. CRCs and the exact
///   final commit value make these interrupted writes invisible to recovery.
/// - Successful `sync` makes all preceding operations durable and visible to
///   subsequent reads.
///
/// Methods take `&mut self` so a flash controller, cache, or emulator can keep
/// exclusive operation state without interior mutability or dynamic dispatch.
pub trait BlockDevice {
    type Error;

    fn block_size(&self) -> u32;
    fn block_count(&self) -> u32;

    fn read(&mut self, block: u32, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error>;

    fn program(&mut self, block: u32, offset: u32, bytes: &[u8]) -> Result<(), Self::Error>;

    fn erase(&mut self, block: u32) -> Result<(), Self::Error>;

    fn sync(&mut self) -> Result<(), Self::Error>;

    fn geometry(&self) -> Geometry {
        Geometry::new(self.block_size(), self.block_count())
    }
}

/// CRC-32/MPEG-2 polynomial (`x^32 + ... + 1`).
pub const CRC32_MPEG2_POLYNOMIAL: u32 = 0x04c1_1db7;
/// CRC-32/MPEG-2 initial state.
pub const CRC32_MPEG2_INITIAL: u32 = 0xffff_ffff;

/// Incremental CRC-32/MPEG-2 calculator.
///
/// The algorithm is non-reflected and has no final XOR, matching the HC32F460
/// CRC peripheral's `crc32_mpeg2` configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Crc32Mpeg2 {
    state: u32,
}

impl Crc32Mpeg2 {
    pub const fn new() -> Self {
        Self {
            state: CRC32_MPEG2_INITIAL,
        }
    }

    /// Continue from a previously saved raw CRC state.
    pub const fn from_state(state: u32) -> Self {
        Self { state }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.state = crc32_mpeg2_update(self.state, bytes);
    }

    /// Return the current raw state. CRC-32/MPEG-2 has no final transform.
    pub const fn value(&self) -> u32 {
        self.state
    }

    pub const fn finalize(self) -> u32 {
        self.state
    }

    pub fn reset(&mut self) {
        self.state = CRC32_MPEG2_INITIAL;
    }
}

impl Default for Crc32Mpeg2 {
    fn default() -> Self {
        Self::new()
    }
}

/// Continue a CRC-32/MPEG-2 calculation from `state`.
pub fn crc32_mpeg2_update(mut state: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        state ^= (byte as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            state = if state & 0x8000_0000 != 0 {
                (state << 1) ^ CRC32_MPEG2_POLYNOMIAL
            } else {
                state << 1
            };
            bit += 1;
        }
    }
    state
}

/// Calculate CRC-32/MPEG-2 in one call.
pub fn crc32_mpeg2(bytes: &[u8]) -> u32 {
    crc32_mpeg2_update(CRC32_MPEG2_INITIAL, bytes)
}

/// Errors produced while encoding or validating on-disk structures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatError {
    BufferTooSmall { required: usize, actual: usize },
    BadSnapshotMagic { found: u32 },
    BadRecordMagic { found: u32 },
    UnsupportedVersion { found: u32 },
    NotCommitted { found: u32 },
    HeaderCrcMismatch { expected: u32, found: u32 },
    UnsupportedFeatures { found: u32 },
    NonZeroReserved,
    InvalidBlockSize { found: u32 },
    InvalidBlockCount { found: u32 },
    GeometryMismatch { expected: Geometry, found: Geometry },
    InvalidStart { expected: u32, found: u32 },
    InvalidSnapshotSpan { expected: u32, found: u32 },
    SnapshotOutOfBounds,
    SnapshotTooLarge,
    TooManyFiles { found: u32 },
    InvalidNameLength { found: u16 },
    InvalidRecordLength { expected: u32, found: u32 },
    UnsupportedRecordFlags { found: u16 },
    ArithmeticOverflow,
}

/// Fields stored in the 64-byte snapshot header.
///
/// Magic, version, reserved words, header CRC, and commit marker are managed by
/// the codec and are intentionally absent from this logical representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotHeader {
    pub generation: u32,
    pub start_block: u32,
    pub block_span: u32,
    pub payload_len: u32,
    pub payload_crc: u32,
    pub file_count: u32,
    pub block_size: u32,
    pub block_count: u32,
    pub features: u32,
}

impl SnapshotHeader {
    /// Construct a version-1 header and derive its canonical block span.
    pub fn new(
        generation: u32,
        start_block: u32,
        payload_len: u32,
        payload_crc: u32,
        file_count: u32,
        geometry: Geometry,
    ) -> Result<Self, FormatError> {
        validate_block_geometry(geometry)?;
        let block_span = checked_snapshot_span(payload_len, geometry.block_size)
            .ok_or(FormatError::ArithmeticOverflow)?;
        let header = Self {
            generation,
            start_block,
            block_span,
            payload_len,
            payload_crc,
            file_count,
            block_size: geometry.block_size,
            block_count: geometry.block_count,
            features: SUPPORTED_FEATURES,
        };
        header.validate()?;
        Ok(header)
    }

    pub const fn geometry(&self) -> Geometry {
        Geometry::new(self.block_size, self.block_count)
    }

    /// Validate fields that can be checked without consulting a live device.
    pub fn validate(&self) -> Result<(), FormatError> {
        validate_block_geometry(self.geometry())?;

        if self.features != SUPPORTED_FEATURES {
            return Err(FormatError::UnsupportedFeatures {
                found: self.features,
            });
        }
        if self.file_count > MAX_FILES {
            return Err(FormatError::TooManyFiles {
                found: self.file_count,
            });
        }

        let expected_span = checked_snapshot_span(self.payload_len, self.block_size)
            .ok_or(FormatError::ArithmeticOverflow)?;
        if self.block_span != expected_span {
            return Err(FormatError::InvalidSnapshotSpan {
                expected: expected_span,
                found: self.block_span,
            });
        }
        if self.block_span > self.block_count / 2 {
            return Err(FormatError::SnapshotTooLarge);
        }
        if self.start_block >= self.block_count {
            return Err(FormatError::SnapshotOutOfBounds);
        }
        Ok(())
    }

    /// Validate the location and geometry against the values used by a mount.
    pub fn validate_at(
        &self,
        scanned_start_block: u32,
        device_geometry: Geometry,
    ) -> Result<(), FormatError> {
        self.validate()?;
        validate_block_geometry(device_geometry)?;
        if self.geometry() != device_geometry {
            return Err(FormatError::GeometryMismatch {
                expected: device_geometry,
                found: self.geometry(),
            });
        }
        if self.start_block != scanned_start_block {
            return Err(FormatError::InvalidStart {
                expected: scanned_start_block,
                found: self.start_block,
            });
        }
        Ok(())
    }

    /// Encode bytes 0..60, which must be written before the commit word.
    ///
    /// Programming this prefix and [`commit_marker_bytes`] in separate calls is
    /// important: the erased marker word must not be included in the first
    /// program operation because a word may only be programmed once per erase.
    pub fn encode_prefix(&self) -> Result<[u8; SNAPSHOT_PREFIX_SIZE], FormatError> {
        let encoded = self.encode_with_marker(ERASED_WORD)?;
        let mut prefix = [0u8; SNAPSHOT_PREFIX_SIZE];
        prefix.copy_from_slice(&encoded[..SNAPSHOT_PREFIX_SIZE]);
        Ok(prefix)
    }

    /// Encode a diagnostic full image with an erased commit word.
    ///
    /// Do not program all 64 bytes from this result in one operation. Use
    /// [`SnapshotHeader::encode_prefix`] and then program
    /// [`commit_marker_bytes`] independently.
    pub fn encode_uncommitted(&self) -> Result<[u8; HEADER_SIZE], FormatError> {
        self.encode_with_marker(ERASED_WORD)
    }

    /// Encode a complete committed image for tests and offline tooling.
    ///
    /// A live device must still program the prefix first and the commit word
    /// last rather than programming this array as one operation.
    pub fn encode_committed(&self) -> Result<[u8; HEADER_SIZE], FormatError> {
        self.encode_with_marker(COMMIT_MARKER)
    }

    fn encode_with_marker(&self, marker: u32) -> Result<[u8; HEADER_SIZE], FormatError> {
        self.validate()?;
        let mut bytes = [0u8; HEADER_SIZE];
        write_u32(&mut bytes, 0, MAGIC);
        write_u32(&mut bytes, 4, DISK_VERSION);
        write_u32(&mut bytes, 8, self.generation);
        write_u32(&mut bytes, 12, self.start_block);
        write_u32(&mut bytes, 16, self.block_span);
        write_u32(&mut bytes, 20, self.payload_len);
        write_u32(&mut bytes, 24, self.payload_crc);
        write_u32(&mut bytes, 28, self.file_count);
        write_u32(&mut bytes, 32, self.block_size);
        write_u32(&mut bytes, 36, self.block_count);
        write_u32(&mut bytes, 40, self.features);
        // Bytes 44..56 are reserved and remain zero.
        let header_crc = crc32_mpeg2(&bytes[..HEADER_CRC_OFFSET]);
        write_u32(&mut bytes, HEADER_CRC_FIELD_OFFSET, header_crc);
        write_u32(&mut bytes, COMMIT_OFFSET, marker);
        Ok(bytes)
    }

    /// Decode and structurally validate a committed snapshot header.
    ///
    /// Mount code should normally use [`SnapshotHeader::decode_at`] so the
    /// CRC-protected, on-disk location and geometry are also compared with the
    /// device being scanned.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        if bytes.len() < HEADER_SIZE {
            return Err(FormatError::BufferTooSmall {
                required: HEADER_SIZE,
                actual: bytes.len(),
            });
        }

        let magic = read_u32(bytes, 0);
        if magic != MAGIC {
            return Err(FormatError::BadSnapshotMagic { found: magic });
        }
        let version = read_u32(bytes, 4);
        if version != DISK_VERSION {
            return Err(FormatError::UnsupportedVersion { found: version });
        }
        let marker = read_u32(bytes, COMMIT_OFFSET);
        if marker != COMMIT_MARKER {
            return Err(FormatError::NotCommitted { found: marker });
        }

        let found_crc = read_u32(bytes, HEADER_CRC_FIELD_OFFSET);
        let expected_crc = crc32_mpeg2(&bytes[..HEADER_CRC_OFFSET]);
        if found_crc != expected_crc {
            return Err(FormatError::HeaderCrcMismatch {
                expected: expected_crc,
                found: found_crc,
            });
        }

        let features = read_u32(bytes, 40);
        if features != SUPPORTED_FEATURES {
            return Err(FormatError::UnsupportedFeatures { found: features });
        }
        if bytes[44..56].iter().any(|&byte| byte != 0) {
            return Err(FormatError::NonZeroReserved);
        }

        let header = Self {
            generation: read_u32(bytes, 8),
            start_block: read_u32(bytes, 12),
            block_span: read_u32(bytes, 16),
            payload_len: read_u32(bytes, 20),
            payload_crc: read_u32(bytes, 24),
            file_count: read_u32(bytes, 28),
            block_size: read_u32(bytes, 32),
            block_count: read_u32(bytes, 36),
            features,
        };
        header.validate()?;
        Ok(header)
    }

    /// Decode a committed header and bind it to its physical scan location.
    pub fn decode_at(
        bytes: &[u8],
        scanned_start_block: u32,
        device_geometry: Geometry,
    ) -> Result<Self, FormatError> {
        let header = Self::decode(bytes)?;
        header.validate_at(scanned_start_block, device_geometry)?;
        Ok(header)
    }
}

/// Bytes programmed in a separate, final 4-byte operation to commit a snapshot.
pub const fn commit_marker_bytes() -> [u8; PROGRAM_SIZE] {
    COMMIT_MARKER.to_le_bytes()
}

/// Fields stored in each 20-byte entry-record header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordHeader {
    pub record_len: u32,
    pub data_len: u32,
    pub data_crc: u32,
    pub name_len: u16,
    pub flags: u16,
}

impl RecordHeader {
    pub fn new(
        name_len: u16,
        data_len: u32,
        data_crc: u32,
        flags: u16,
    ) -> Result<Self, FormatError> {
        let record_len =
            checked_record_len(name_len, data_len).ok_or(FormatError::ArithmeticOverflow)?;
        let header = Self {
            record_len,
            data_len,
            data_crc,
            name_len,
            flags,
        };
        header.validate()?;
        Ok(header)
    }

    pub fn validate(&self) -> Result<(), FormatError> {
        if self.name_len == 0 || self.name_len as usize > MAX_NAME_LEN {
            return Err(FormatError::InvalidNameLength {
                found: self.name_len,
            });
        }
        if self.flags & !SUPPORTED_RECORD_FLAGS != 0 {
            return Err(FormatError::UnsupportedRecordFlags { found: self.flags });
        }
        let expected = checked_record_len(self.name_len, self.data_len)
            .ok_or(FormatError::ArithmeticOverflow)?;
        if self.record_len != expected {
            return Err(FormatError::InvalidRecordLength {
                expected,
                found: self.record_len,
            });
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<[u8; RECORD_HEADER_SIZE], FormatError> {
        self.validate()?;
        let mut bytes = [0u8; RECORD_HEADER_SIZE];
        write_u32(&mut bytes, 0, RECORD_MAGIC);
        write_u32(&mut bytes, 4, self.record_len);
        write_u32(&mut bytes, 8, self.data_len);
        write_u32(&mut bytes, 12, self.data_crc);
        write_u16(&mut bytes, 16, self.name_len);
        write_u16(&mut bytes, 18, self.flags);
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        if bytes.len() < RECORD_HEADER_SIZE {
            return Err(FormatError::BufferTooSmall {
                required: RECORD_HEADER_SIZE,
                actual: bytes.len(),
            });
        }
        let magic = read_u32(bytes, 0);
        if magic != RECORD_MAGIC {
            return Err(FormatError::BadRecordMagic { found: magic });
        }
        let header = Self {
            record_len: read_u32(bytes, 4),
            data_len: read_u32(bytes, 8),
            data_crc: read_u32(bytes, 12),
            name_len: read_u16(bytes, 16),
            flags: read_u16(bytes, 18),
        };
        header.validate()?;
        Ok(header)
    }
}

/// Return whether `value` is aligned to the device program size.
pub const fn is_aligned_4(value: u32) -> bool {
    value & ((PROGRAM_SIZE as u32) - 1) == 0
}

/// Round `value` up to a 4-byte boundary without wrapping.
pub const fn checked_align_4(value: u32) -> Option<u32> {
    match value.checked_add((PROGRAM_SIZE as u32) - 1) {
        Some(value) => Some(value & !((PROGRAM_SIZE as u32) - 1)),
        None => None,
    }
}

/// Calculate the padded record length using checked arithmetic.
pub const fn checked_record_len(name_len: u16, data_len: u32) -> Option<u32> {
    let with_name = match (RECORD_HEADER_SIZE as u32).checked_add(name_len as u32) {
        Some(value) => value,
        None => return None,
    };
    let unaligned = match with_name.checked_add(data_len) {
        Some(value) => value,
        None => return None,
    };
    checked_align_4(unaligned)
}

/// Calculate `ceil((HEADER_SIZE + payload_len) / block_size)` safely.
pub const fn checked_snapshot_span(payload_len: u32, block_size: u32) -> Option<u32> {
    if block_size == 0 {
        return None;
    }
    let total = match (HEADER_SIZE as u32).checked_add(payload_len) {
        Some(value) => value,
        None => return None,
    };
    let whole = total / block_size;
    let extra = if total % block_size == 0 { 0 } else { 1 };
    whole.checked_add(extra)
}

/// Sequence-number comparison that remains valid across one `u32` wrap.
///
/// The two inputs must be less than `2^31` generations apart. Exactly half the
/// sequence space is intentionally treated as ambiguous (`false` both ways).
pub const fn generation_is_newer(candidate: u32, reference: u32) -> bool {
    let distance = candidate.wrapping_sub(reference);
    distance != 0 && distance < 0x8000_0000
}

fn validate_block_geometry(geometry: Geometry) -> Result<(), FormatError> {
    if geometry.block_size < HEADER_SIZE as u32 || !is_aligned_4(geometry.block_size) {
        return Err(FormatError::InvalidBlockSize {
            found: geometry.block_size,
        });
    }
    if geometry.block_count < 2 {
        return Err(FormatError::InvalidBlockCount {
            found: geometry.block_count,
        });
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEOMETRY: Geometry = Geometry::new(64, 8);

    fn sample_header() -> SnapshotHeader {
        SnapshotHeader::new(7, 2, 64, 0x1122_3344, 2, GEOMETRY).unwrap()
    }

    fn refresh_header_crc(bytes: &mut [u8; HEADER_SIZE]) {
        let crc = crc32_mpeg2(&bytes[..HEADER_CRC_OFFSET]);
        write_u32(bytes, HEADER_CRC_FIELD_OFFSET, crc);
    }

    #[test]
    fn crc_matches_standard_vector() {
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_e6e7);
        assert_eq!(crc32_mpeg2(b""), CRC32_MPEG2_INITIAL);
    }

    #[test]
    fn crc_is_incremental_and_resettable() {
        let mut crc = Crc32Mpeg2::new();
        crc.update(b"1234");
        let saved = crc.value();
        crc.update(b"56789");
        assert_eq!(crc.finalize(), 0x0376_e6e7);

        let mut resumed = Crc32Mpeg2::from_state(saved);
        resumed.update(b"56789");
        assert_eq!(resumed.value(), 0x0376_e6e7);
        resumed.reset();
        assert_eq!(resumed.value(), CRC32_MPEG2_INITIAL);
    }

    #[test]
    fn snapshot_codec_is_little_endian_and_round_trips() {
        let header = sample_header();
        let encoded = header.encode_committed().unwrap();

        assert_eq!(&encoded[0..4], b"RFS1");
        assert_eq!(&encoded[4..8], &DISK_VERSION.to_le_bytes());
        assert_eq!(&encoded[8..12], &7u32.to_le_bytes());
        assert_eq!(&encoded[12..16], &2u32.to_le_bytes());
        assert_eq!(&encoded[44..56], &[0u8; 12]);
        assert_eq!(
            read_u32(&encoded, HEADER_CRC_FIELD_OFFSET),
            crc32_mpeg2(&encoded[..HEADER_CRC_OFFSET])
        );
        assert_eq!(&encoded[COMMIT_OFFSET..], &COMMIT_MARKER.to_le_bytes());
        assert_eq!(SnapshotHeader::decode_at(&encoded, 2, GEOMETRY), Ok(header));
    }

    #[test]
    fn uncommitted_header_is_never_accepted() {
        let header = sample_header();
        let encoded = header.encode_uncommitted().unwrap();
        assert_eq!(&encoded[COMMIT_OFFSET..], &ERASED_WORD.to_le_bytes());
        assert_eq!(
            SnapshotHeader::decode(&encoded),
            Err(FormatError::NotCommitted { found: ERASED_WORD })
        );
        assert_eq!(header.encode_prefix().unwrap(), encoded[..COMMIT_OFFSET]);
        assert_eq!(commit_marker_bytes(), COMMIT_MARKER.to_le_bytes());
    }

    #[test]
    fn snapshot_crc_rejects_corruption() {
        let mut encoded = sample_header().encode_committed().unwrap();
        let stored = read_u32(&encoded, HEADER_CRC_FIELD_OFFSET);
        encoded[20] ^= 1;
        assert!(matches!(
            SnapshotHeader::decode(&encoded),
            Err(FormatError::HeaderCrcMismatch { found, .. }) if found == stored
        ));
    }

    #[test]
    fn snapshot_rejects_unknown_and_noncanonical_fields() {
        let mut encoded = sample_header().encode_committed().unwrap();
        write_u32(&mut encoded, 40, 1);
        refresh_header_crc(&mut encoded);
        assert_eq!(
            SnapshotHeader::decode(&encoded),
            Err(FormatError::UnsupportedFeatures { found: 1 })
        );

        let mut encoded = sample_header().encode_committed().unwrap();
        encoded[44] = 1;
        refresh_header_crc(&mut encoded);
        assert_eq!(
            SnapshotHeader::decode(&encoded),
            Err(FormatError::NonZeroReserved)
        );

        let mut encoded = sample_header().encode_committed().unwrap();
        write_u32(&mut encoded, 16, 3);
        refresh_header_crc(&mut encoded);
        assert_eq!(
            SnapshotHeader::decode(&encoded),
            Err(FormatError::InvalidSnapshotSpan {
                expected: 2,
                found: 3
            })
        );
    }

    #[test]
    fn snapshot_is_bound_to_scan_location_and_device_geometry() {
        let encoded = sample_header().encode_committed().unwrap();
        assert_eq!(
            SnapshotHeader::decode_at(&encoded, 3, GEOMETRY),
            Err(FormatError::InvalidStart {
                expected: 3,
                found: 2
            })
        );
        assert_eq!(
            SnapshotHeader::decode_at(&encoded, 2, Geometry::new(128, 8)),
            Err(FormatError::GeometryMismatch {
                expected: Geometry::new(128, 8),
                found: GEOMETRY
            })
        );
    }

    #[test]
    fn snapshot_limits_files_extent_and_half_device() {
        assert_eq!(
            SnapshotHeader::new(0, 0, 0, 0, MAX_FILES + 1, GEOMETRY),
            Err(FormatError::TooManyFiles {
                found: MAX_FILES + 1
            })
        );
        assert_eq!(
            SnapshotHeader::new(0, 8, 0, 0, 0, GEOMETRY),
            Err(FormatError::SnapshotOutOfBounds)
        );
        let wrapped = SnapshotHeader::new(0, 7, 64, 0, 0, GEOMETRY).unwrap();
        assert_eq!(wrapped.block_span, 2);
        assert_eq!(wrapped.start_block, 7);
        assert_eq!(
            SnapshotHeader::new(0, 0, 64 * 4, 0, 0, GEOMETRY),
            Err(FormatError::SnapshotTooLarge)
        );
    }

    #[test]
    fn snapshot_constructor_reports_invalid_geometry() {
        assert_eq!(
            SnapshotHeader::new(0, 0, 0, 0, 0, Geometry::new(0, 8)),
            Err(FormatError::InvalidBlockSize { found: 0 })
        );
        assert_eq!(
            SnapshotHeader::new(0, 0, 0, 0, 0, Geometry::new(64, 1)),
            Err(FormatError::InvalidBlockCount { found: 1 })
        );
    }

    #[test]
    fn record_codec_round_trips_and_calculates_padding() {
        let header = RecordHeader::new(3, 6, 0xaabb_ccdd, 0).unwrap();
        assert_eq!(header.record_len, 32);
        let encoded = header.encode().unwrap();
        assert_eq!(&encoded[..4], b"FILE");
        assert_eq!(&encoded[16..18], &3u16.to_le_bytes());
        assert_eq!(RecordHeader::decode(&encoded), Ok(header));
        assert!(is_aligned_4(header.record_len));

        let directory =
            RecordHeader::new(3, 0, CRC32_MPEG2_INITIAL, RECORD_FLAG_DIRECTORY).unwrap();
        assert_eq!(
            RecordHeader::decode(&directory.encode().unwrap()),
            Ok(directory)
        );
    }

    #[test]
    fn record_rejects_bad_lengths_flags_and_magic() {
        assert_eq!(
            RecordHeader::new(0, 0, 0, 0),
            Err(FormatError::InvalidNameLength { found: 0 })
        );
        assert_eq!(
            RecordHeader::new((MAX_NAME_LEN + 1) as u16, 0, 0, 0),
            Err(FormatError::InvalidNameLength {
                found: (MAX_NAME_LEN + 1) as u16
            })
        );
        assert_eq!(
            RecordHeader::new(1, 0, 0, 2),
            Err(FormatError::UnsupportedRecordFlags { found: 2 })
        );

        let header = RecordHeader::new(1, 0, 0, 0).unwrap();
        let mut encoded = header.encode().unwrap();
        write_u32(&mut encoded, 4, header.record_len + 4);
        assert!(matches!(
            RecordHeader::decode(&encoded),
            Err(FormatError::InvalidRecordLength { .. })
        ));
        encoded[..4].copy_from_slice(b"NOPE");
        assert!(matches!(
            RecordHeader::decode(&encoded),
            Err(FormatError::BadRecordMagic { .. })
        ));
    }

    #[test]
    fn checked_layout_math_never_wraps() {
        assert_eq!(checked_align_4(0), Some(0));
        assert_eq!(checked_align_4(1), Some(4));
        assert_eq!(checked_align_4(u32::MAX), None);
        assert_eq!(checked_record_len(1, u32::MAX), None);
        assert_eq!(checked_snapshot_span(u32::MAX, 64), None);
        assert_eq!(checked_snapshot_span(0, 0), None);
    }

    #[test]
    fn generation_comparison_handles_wrap_and_ambiguity() {
        assert!(!generation_is_newer(10, 10));
        assert!(generation_is_newer(11, 10));
        assert!(!generation_is_newer(10, 11));
        assert!(generation_is_newer(0, u32::MAX));
        assert!(!generation_is_newer(u32::MAX, 0));
        assert!(!generation_is_newer(0x8000_0000, 0));
        assert!(!generation_is_newer(0, 0x8000_0000));
    }
}
