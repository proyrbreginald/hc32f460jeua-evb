use core::str;

use crate::format::{
    BlockDevice, Crc32Mpeg2, Geometry, HEADER_SIZE, MAX_FILES, MAX_NAME_LEN, PROGRAM_SIZE,
    RECORD_HEADER_SIZE, RecordHeader, SnapshotHeader, commit_marker_bytes, crc32_mpeg2,
    generation_is_newer,
};

use crate::{Error, FileInfo, FsInfo};

const COPY_BUFFER_SIZE: usize = 64;
const ZERO_WORD: [u8; PROGRAM_SIZE] = [0; PROGRAM_SIZE];

#[derive(Clone, Copy)]
struct Record {
    record_offset: u32,
    record_len: u32,
    data_offset: u32,
    data_len: u32,
    data_crc: u32,
    name_len: usize,
    name: [u8; MAX_NAME_LEN],
}

impl Record {
    fn name_eq(&self, name: &str) -> bool {
        self.name_len == name.len() && &self.name[..self.name_len] == name.as_bytes()
    }

    const fn info(&self) -> FileInfo {
        FileInfo {
            size: self.data_len,
            crc32: self.data_crc,
        }
    }
}

/// Mounted snapshot filesystem that exclusively owns its block device.
pub struct FileSystem<D: BlockDevice> {
    device: D,
    geometry: Geometry,
    active: SnapshotHeader,
    recovery_required: bool,
}

impl<D: BlockDevice> FileSystem<D> {
    /// Mount the newest completely valid snapshot without modifying the device.
    pub fn mount(mut device: D) -> Result<Self, Error<D::Error>> {
        let geometry = checked_geometry(&device)?;
        let active = scan_active(&mut device, geometry)?.ok_or(Error::NotFormatted)?;
        Ok(Self {
            device,
            geometry,
            active,
            recovery_required: false,
        })
    }

    /// Atomically publish an empty filesystem.
    ///
    /// If a valid filesystem already exists, it remains mountable until the
    /// empty snapshot is completely written and committed.
    pub fn format(mut device: D) -> Result<Self, Error<D::Error>> {
        let geometry = checked_geometry(&device)?;
        let previous = scan_active(&mut device, geometry)?;
        let generation = previous
            .map(|header| header.generation.wrapping_add(1))
            .unwrap_or(0);
        let start_block = previous
            .map(|header| (header.start_block + header.block_span) % geometry.block_count)
            .unwrap_or(0);
        let header = SnapshotHeader::new(generation, start_block, 0, crc32_mpeg2(&[]), 0, geometry)
            .map_err(|_| Error::InvalidGeometry)?;

        write_candidate(&mut device, &header, previous.as_ref(), Change::Clear)?;
        Ok(Self {
            device,
            geometry,
            active: header,
            recovery_required: false,
        })
    }

    /// Return the owned device. Use this before remounting after an I/O error.
    pub fn into_device(self) -> D {
        self.device
    }

    /// Whether a failed mutation requires dropping this state and mounting
    /// again before another mutation.
    pub const fn recovery_required(&self) -> bool {
        self.recovery_required
    }

    pub fn info(&self) -> FsInfo {
        FsInfo {
            generation: self.active.generation,
            file_count: self.active.file_count,
            serialized_bytes: self.active.payload_len,
            active_blocks: self.active.block_span,
            capacity_bytes: snapshot_capacity(self.geometry).unwrap_or(0),
        }
    }

    /// Re-read and validate the complete active snapshot.
    pub fn verify(&mut self) -> Result<(), Error<D::Error>> {
        let mut encoded = [0u8; HEADER_SIZE];
        segment_read(&mut self.device, &self.active, 0, &mut encoded)?;
        let observed = SnapshotHeader::decode_at(&encoded, self.active.start_block, self.geometry)
            .map_err(|_| Error::Corrupt)?;
        if observed != self.active {
            return Err(Error::Corrupt);
        }
        validate_snapshot(&mut self.device, &self.active)
    }

    pub fn stat(&mut self, name: &str) -> Result<FileInfo, Error<D::Error>> {
        validate_name(name)?;
        find_record(&mut self.device, &self.active, name)?
            .map(|record| record.info())
            .ok_or(Error::NotFound)
    }

    /// Read up to `buffer.len()` bytes starting at `offset`.
    pub fn read(
        &mut self,
        name: &str,
        offset: u32,
        buffer: &mut [u8],
    ) -> Result<usize, Error<D::Error>> {
        validate_name(name)?;
        let record = find_record(&mut self.device, &self.active, name)?.ok_or(Error::NotFound)?;
        if offset >= record.data_len || buffer.is_empty() {
            return Ok(0);
        }
        let remaining = (record.data_len - offset) as usize;
        let read_len = remaining.min(buffer.len());
        let logical = (HEADER_SIZE as u32)
            .checked_add(record.data_offset)
            .and_then(|value| value.checked_add(offset))
            .ok_or(Error::Corrupt)?;
        segment_read(
            &mut self.device,
            &self.active,
            logical,
            &mut buffer[..read_len],
        )?;
        Ok(read_len)
    }

    /// Visit every file. The borrowed name is valid only for the callback.
    pub fn list<F>(&mut self, mut visitor: F) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&str, FileInfo),
    {
        let mut offset = 0;
        let mut index = 0;
        while index < self.active.file_count {
            let record = read_record(&mut self.device, &self.active, offset)?;
            let name =
                str::from_utf8(&record.name[..record.name_len]).map_err(|_| Error::Corrupt)?;
            visitor(name, record.info());
            offset = offset
                .checked_add(record.record_len)
                .ok_or(Error::Corrupt)?;
            index += 1;
        }
        if offset != self.active.payload_len {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    /// Atomically create or replace a complete file.
    pub fn write(&mut self, name: &str, data: &[u8]) -> Result<(), Error<D::Error>> {
        validate_name(name)?;
        u32::try_from(data.len()).map_err(|_| Error::FileTooLarge)?;
        self.mutate(Change::Write { name, data })
    }

    /// Atomically remove a file.
    pub fn remove(&mut self, name: &str) -> Result<(), Error<D::Error>> {
        validate_name(name)?;
        if find_record(&mut self.device, &self.active, name)?.is_none() {
            return Err(Error::NotFound);
        }
        self.mutate(Change::Remove { name })
    }

    /// Atomically rename one file within the flat namespace.
    pub fn rename(&mut self, old_name: &str, new_name: &str) -> Result<(), Error<D::Error>> {
        validate_name(old_name)?;
        validate_name(new_name)?;
        if old_name == new_name {
            return if find_record(&mut self.device, &self.active, old_name)?.is_some() {
                Ok(())
            } else {
                Err(Error::NotFound)
            };
        }
        if find_record(&mut self.device, &self.active, old_name)?.is_none() {
            return Err(Error::NotFound);
        }
        if find_record(&mut self.device, &self.active, new_name)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        self.mutate(Change::Rename { old_name, new_name })
    }

    /// Atomically remove all files while preserving generation history.
    pub fn clear(&mut self) -> Result<(), Error<D::Error>> {
        self.mutate(Change::Clear)
    }

    fn mutate(&mut self, change: Change<'_>) -> Result<(), Error<D::Error>> {
        if self.recovery_required {
            return Err(Error::RecoveryRequired);
        }

        // Preflight is read-only. Invalid/corrupt input and NoSpace leave the
        // mounted state usable because no destination block has been erased.
        let stats = calculate_change(&mut self.device, &self.active, change)?;
        let generation = self.active.generation.wrapping_add(1);
        let start_block =
            (self.active.start_block + self.active.block_span) % self.geometry.block_count;
        let next = SnapshotHeader::new(
            generation,
            start_block,
            stats.payload_len,
            stats.payload_crc,
            stats.file_count,
            self.geometry,
        )
        .map_err(|_| Error::NoSpace)?;

        let result = write_candidate(&mut self.device, &next, Some(&self.active), change);
        if let Err(error) = result {
            // A device may report failure after physically completing the
            // commit word. Do not issue another erase/program from stale RAM.
            self.recovery_required = true;
            return Err(error);
        }
        self.active = next;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Change<'a> {
    Write {
        name: &'a str,
        data: &'a [u8],
    },
    Remove {
        name: &'a str,
    },
    Rename {
        old_name: &'a str,
        new_name: &'a str,
    },
    Clear,
}

#[derive(Clone, Copy)]
enum ExistingAction<'a> {
    Keep,
    Skip,
    Rename(&'a str),
}

impl<'a> Change<'a> {
    fn action_for(self, name: &str) -> ExistingAction<'a> {
        match self {
            Self::Write { name: replaced, .. } if name == replaced => ExistingAction::Skip,
            Self::Remove { name: removed } if name == removed => ExistingAction::Skip,
            Self::Rename { old_name, new_name } if name == old_name => {
                ExistingAction::Rename(new_name)
            }
            Self::Clear => ExistingAction::Skip,
            _ => ExistingAction::Keep,
        }
    }
}

fn validate_name<E>(name: &str) -> Result<(), Error<E>> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_NAME_LEN
        || bytes.iter().any(|byte| *byte == 0 || *byte == b'/')
    {
        Err(Error::InvalidName)
    } else {
        Ok(())
    }
}

fn checked_geometry<D: BlockDevice>(device: &D) -> Result<Geometry, Error<D::Error>> {
    let geometry = device.geometry();
    if geometry.block_size < HEADER_SIZE as u32
        || !geometry.block_size.is_multiple_of(PROGRAM_SIZE as u32)
        || geometry.block_count < 2
        || geometry
            .block_size
            .checked_mul(geometry.block_count)
            .is_none()
        || snapshot_capacity(geometry).is_none()
    {
        return Err(Error::InvalidGeometry);
    }
    Ok(geometry)
}

fn snapshot_capacity(geometry: Geometry) -> Option<u32> {
    (geometry.block_count / 2)
        .checked_mul(geometry.block_size)?
        .checked_sub(HEADER_SIZE as u32)
}

fn scan_active<D: BlockDevice>(
    device: &mut D,
    geometry: Geometry,
) -> Result<Option<SnapshotHeader>, Error<D::Error>> {
    let mut newest: Option<SnapshotHeader> = None;
    let mut block = 0;
    while block < geometry.block_count {
        let mut encoded = [0u8; HEADER_SIZE];
        device.read(block, 0, &mut encoded).map_err(Error::Device)?;
        if let Ok(candidate) = SnapshotHeader::decode_at(&encoded, block, geometry) {
            match validate_snapshot(device, &candidate) {
                Ok(()) => {
                    let replace = newest
                        .map(|current| {
                            generation_is_newer(candidate.generation, current.generation)
                        })
                        .unwrap_or(true);
                    if replace {
                        newest = Some(candidate);
                    }
                }
                Err(Error::Corrupt) => {}
                Err(error) => return Err(error),
            }
        }
        block += 1;
    }
    Ok(newest)
}

fn segment_read<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
    logical_offset: u32,
    buffer: &mut [u8],
) -> Result<(), Error<D::Error>> {
    let length = u32::try_from(buffer.len()).map_err(|_| Error::Corrupt)?;
    let end = logical_offset.checked_add(length).ok_or(Error::Corrupt)?;
    let segment_len = snapshot
        .block_span
        .checked_mul(snapshot.block_size)
        .ok_or(Error::Corrupt)?;
    if end > segment_len {
        return Err(Error::Corrupt);
    }

    let mut logical = logical_offset;
    let mut output = buffer;
    while !output.is_empty() {
        let relative_block = logical / snapshot.block_size;
        let block = (snapshot.start_block + relative_block) % snapshot.block_count;
        let block_offset = logical % snapshot.block_size;
        let available = (snapshot.block_size - block_offset) as usize;
        let amount = output.len().min(available);
        let (current, rest) = output.split_at_mut(amount);
        device
            .read(block, block_offset, current)
            .map_err(Error::Device)?;
        logical = logical.checked_add(amount as u32).ok_or(Error::Corrupt)?;
        output = rest;
    }
    Ok(())
}

fn segment_program<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
    logical_offset: u32,
    data: &[u8],
) -> Result<(), Error<D::Error>> {
    if !logical_offset.is_multiple_of(PROGRAM_SIZE as u32)
        || !data.len().is_multiple_of(PROGRAM_SIZE)
    {
        return Err(Error::Corrupt);
    }
    let length = u32::try_from(data.len()).map_err(|_| Error::Corrupt)?;
    let end = logical_offset.checked_add(length).ok_or(Error::Corrupt)?;
    let segment_len = snapshot
        .block_span
        .checked_mul(snapshot.block_size)
        .ok_or(Error::Corrupt)?;
    if end > segment_len {
        return Err(Error::Corrupt);
    }

    let mut logical = logical_offset;
    let mut input = data;
    while !input.is_empty() {
        let relative_block = logical / snapshot.block_size;
        let block = (snapshot.start_block + relative_block) % snapshot.block_count;
        let block_offset = logical % snapshot.block_size;
        let available = (snapshot.block_size - block_offset) as usize;
        let amount = input.len().min(available);
        let amount = amount - amount % PROGRAM_SIZE;
        if amount == 0 {
            return Err(Error::Corrupt);
        }
        let (current, rest) = input.split_at(amount);
        device
            .program(block, block_offset, current)
            .map_err(Error::Device)?;
        logical = logical.checked_add(amount as u32).ok_or(Error::Corrupt)?;
        input = rest;
    }
    Ok(())
}

fn read_record<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
    payload_offset: u32,
) -> Result<Record, Error<D::Error>> {
    let header_end = payload_offset
        .checked_add(RECORD_HEADER_SIZE as u32)
        .ok_or(Error::Corrupt)?;
    if header_end > snapshot.payload_len {
        return Err(Error::Corrupt);
    }

    let mut encoded = [0u8; RECORD_HEADER_SIZE];
    let logical = (HEADER_SIZE as u32)
        .checked_add(payload_offset)
        .ok_or(Error::Corrupt)?;
    segment_read(device, snapshot, logical, &mut encoded)?;
    let header = RecordHeader::decode(&encoded).map_err(|_| Error::Corrupt)?;
    let record_end = payload_offset
        .checked_add(header.record_len)
        .ok_or(Error::Corrupt)?;
    if record_end > snapshot.payload_len {
        return Err(Error::Corrupt);
    }

    let name_len = header.name_len as usize;
    let mut name = [0u8; MAX_NAME_LEN];
    let name_offset = header_end;
    let name_logical = (HEADER_SIZE as u32)
        .checked_add(name_offset)
        .ok_or(Error::Corrupt)?;
    segment_read(device, snapshot, name_logical, &mut name[..name_len])?;
    let decoded_name = str::from_utf8(&name[..name_len]).map_err(|_| Error::Corrupt)?;
    validate_name::<D::Error>(decoded_name).map_err(|_| Error::Corrupt)?;

    let data_offset = name_offset
        .checked_add(header.name_len as u32)
        .ok_or(Error::Corrupt)?;
    let data_end = data_offset
        .checked_add(header.data_len)
        .ok_or(Error::Corrupt)?;
    if data_end > record_end {
        return Err(Error::Corrupt);
    }

    Ok(Record {
        record_offset: payload_offset,
        record_len: header.record_len,
        data_offset,
        data_len: header.data_len,
        data_crc: header.data_crc,
        name_len,
        name,
    })
}

fn find_record<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
    name: &str,
) -> Result<Option<Record>, Error<D::Error>> {
    let mut offset = 0;
    let mut index = 0;
    while index < snapshot.file_count {
        let record = read_record(device, snapshot, offset)?;
        if record.name_eq(name) {
            return Ok(Some(record));
        }
        offset = offset
            .checked_add(record.record_len)
            .ok_or(Error::Corrupt)?;
        index += 1;
    }
    if offset != snapshot.payload_len {
        return Err(Error::Corrupt);
    }
    Ok(None)
}

fn crc_segment_range<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
    logical_offset: u32,
    length: u32,
) -> Result<u32, Error<D::Error>> {
    let mut crc = Crc32Mpeg2::new();
    update_crc_from_segment(device, snapshot, logical_offset, length, &mut crc)?;
    Ok(crc.finalize())
}

fn update_crc_from_segment<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
    logical_offset: u32,
    length: u32,
    crc: &mut Crc32Mpeg2,
) -> Result<(), Error<D::Error>> {
    let mut buffer = [0u8; COPY_BUFFER_SIZE];
    let mut consumed = 0;
    while consumed < length {
        let amount = (length - consumed).min(COPY_BUFFER_SIZE as u32) as usize;
        let logical = logical_offset.checked_add(consumed).ok_or(Error::Corrupt)?;
        segment_read(device, snapshot, logical, &mut buffer[..amount])?;
        crc.update(&buffer[..amount]);
        consumed += amount as u32;
    }
    Ok(())
}

fn range_is_zero<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
    logical_offset: u32,
    length: u32,
) -> Result<bool, Error<D::Error>> {
    let mut buffer = [0u8; COPY_BUFFER_SIZE];
    let mut consumed = 0;
    while consumed < length {
        let amount = (length - consumed).min(COPY_BUFFER_SIZE as u32) as usize;
        let logical = logical_offset.checked_add(consumed).ok_or(Error::Corrupt)?;
        segment_read(device, snapshot, logical, &mut buffer[..amount])?;
        if buffer[..amount].iter().any(|byte| *byte != 0) {
            return Ok(false);
        }
        consumed += amount as u32;
    }
    Ok(true)
}

fn validate_snapshot<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
) -> Result<(), Error<D::Error>> {
    snapshot.validate().map_err(|_| Error::Corrupt)?;
    let payload_crc =
        crc_segment_range(device, snapshot, HEADER_SIZE as u32, snapshot.payload_len)?;
    if payload_crc != snapshot.payload_crc {
        return Err(Error::Corrupt);
    }

    let mut offsets = [0u32; MAX_FILES as usize];
    let mut name_hashes = [0u32; MAX_FILES as usize];
    let mut offset = 0;
    let mut index = 0usize;
    while index < snapshot.file_count as usize {
        let record = read_record(device, snapshot, offset)?;
        let name_hash = crc32_mpeg2(&record.name[..record.name_len]);

        let mut previous = 0;
        while previous < index {
            if name_hashes[previous] == name_hash {
                let prior = read_record(device, snapshot, offsets[previous])?;
                if prior.name_len == record.name_len
                    && prior.name[..prior.name_len] == record.name[..record.name_len]
                {
                    return Err(Error::Corrupt);
                }
            }
            previous += 1;
        }

        let data_logical = (HEADER_SIZE as u32)
            .checked_add(record.data_offset)
            .ok_or(Error::Corrupt)?;
        let data_crc = crc_segment_range(device, snapshot, data_logical, record.data_len)?;
        if data_crc != record.data_crc {
            return Err(Error::Corrupt);
        }

        let data_end = record
            .data_offset
            .checked_add(record.data_len)
            .ok_or(Error::Corrupt)?;
        let record_end = record
            .record_offset
            .checked_add(record.record_len)
            .ok_or(Error::Corrupt)?;
        let padding = record_end.checked_sub(data_end).ok_or(Error::Corrupt)?;
        if !range_is_zero(
            device,
            snapshot,
            (HEADER_SIZE as u32)
                .checked_add(data_end)
                .ok_or(Error::Corrupt)?,
            padding,
        )? {
            return Err(Error::Corrupt);
        }

        offsets[index] = offset;
        name_hashes[index] = name_hash;
        offset = record_end;
        index += 1;
    }
    if offset != snapshot.payload_len {
        return Err(Error::Corrupt);
    }
    Ok(())
}

struct PlanStats {
    payload_len: u32,
    payload_crc: u32,
    file_count: u32,
}

struct StatsBuilder {
    payload_len: u32,
    crc: Crc32Mpeg2,
    file_count: u32,
}

impl StatsBuilder {
    fn new() -> Self {
        Self {
            payload_len: 0,
            crc: Crc32Mpeg2::new(),
            file_count: 0,
        }
    }

    fn bytes<E>(&mut self, bytes: &[u8]) -> Result<(), Error<E>> {
        self.payload_len = self
            .payload_len
            .checked_add(u32::try_from(bytes.len()).map_err(|_| Error::NoSpace)?)
            .ok_or(Error::NoSpace)?;
        self.crc.update(bytes);
        Ok(())
    }

    fn zeros<E>(&mut self, mut length: u32) -> Result<(), Error<E>> {
        while length >= PROGRAM_SIZE as u32 {
            self.bytes(&ZERO_WORD)?;
            length -= PROGRAM_SIZE as u32;
        }
        if length != 0 {
            self.bytes(&ZERO_WORD[..length as usize])?;
        }
        Ok(())
    }

    fn add_file<E>(&mut self) -> Result<(), Error<E>> {
        self.file_count = self.file_count.checked_add(1).ok_or(Error::NoSpace)?;
        if self.file_count > MAX_FILES {
            return Err(Error::NoSpace);
        }
        Ok(())
    }

    fn finish(self, geometry: Geometry) -> Result<PlanStats, Error<core::convert::Infallible>> {
        if self.payload_len > snapshot_capacity(geometry).ok_or(Error::NoSpace)? {
            return Err(Error::NoSpace);
        }
        Ok(PlanStats {
            payload_len: self.payload_len,
            payload_crc: self.crc.finalize(),
            file_count: self.file_count,
        })
    }
}

fn stats_segment<D: BlockDevice>(
    builder: &mut StatsBuilder,
    device: &mut D,
    snapshot: &SnapshotHeader,
    payload_offset: u32,
    length: u32,
) -> Result<(), Error<D::Error>> {
    let mut buffer = [0u8; COPY_BUFFER_SIZE];
    let mut copied = 0;
    while copied < length {
        let amount = (length - copied).min(COPY_BUFFER_SIZE as u32) as usize;
        let logical = (HEADER_SIZE as u32)
            .checked_add(payload_offset)
            .and_then(|value| value.checked_add(copied))
            .ok_or(Error::Corrupt)?;
        segment_read(device, snapshot, logical, &mut buffer[..amount])?;
        builder.bytes(&buffer[..amount])?;
        copied += amount as u32;
    }
    Ok(())
}

fn record_encoding(
    name: &str,
    data_len: u32,
    data_crc: u32,
) -> Result<(RecordHeader, [u8; RECORD_HEADER_SIZE]), Error<core::convert::Infallible>> {
    let name_len = u16::try_from(name.len()).map_err(|_| Error::InvalidName)?;
    let header = RecordHeader::new(name_len, data_len, data_crc, 0).map_err(|_| Error::NoSpace)?;
    let encoded = header.encode().map_err(|_| Error::NoSpace)?;
    Ok((header, encoded))
}

fn stats_new_record<E>(
    builder: &mut StatsBuilder,
    name: &str,
    data: &[u8],
) -> Result<(), Error<E>> {
    let data_len = u32::try_from(data.len()).map_err(|_| Error::FileTooLarge)?;
    let (header, encoded) =
        record_encoding(name, data_len, crc32_mpeg2(data)).map_err(|error| match error {
            Error::InvalidName => Error::InvalidName,
            _ => Error::NoSpace,
        })?;
    builder.bytes(&encoded)?;
    builder.bytes(name.as_bytes())?;
    builder.bytes(data)?;
    let used = (RECORD_HEADER_SIZE as u32)
        .checked_add(name.len() as u32)
        .and_then(|value| value.checked_add(data_len))
        .ok_or(Error::NoSpace)?;
    builder.zeros(header.record_len - used)?;
    builder.add_file()
}

fn stats_renamed_record<D: BlockDevice>(
    builder: &mut StatsBuilder,
    device: &mut D,
    snapshot: &SnapshotHeader,
    record: &Record,
    new_name: &str,
) -> Result<(), Error<D::Error>> {
    let (header, encoded) =
        record_encoding(new_name, record.data_len, record.data_crc).map_err(|_| Error::NoSpace)?;
    builder.bytes(&encoded)?;
    builder.bytes(new_name.as_bytes())?;
    stats_segment(
        builder,
        device,
        snapshot,
        record.data_offset,
        record.data_len,
    )?;
    let used = (RECORD_HEADER_SIZE as u32)
        .checked_add(new_name.len() as u32)
        .and_then(|value| value.checked_add(record.data_len))
        .ok_or(Error::NoSpace)?;
    builder.zeros(header.record_len - used)?;
    builder.add_file()
}

fn calculate_change<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
    change: Change<'_>,
) -> Result<PlanStats, Error<D::Error>> {
    let mut builder = StatsBuilder::new();
    let mut offset = 0;
    let mut index = 0;
    while index < snapshot.file_count {
        let record = read_record(device, snapshot, offset)?;
        let name = str::from_utf8(&record.name[..record.name_len]).map_err(|_| Error::Corrupt)?;
        match change.action_for(name) {
            ExistingAction::Keep => {
                stats_segment(
                    &mut builder,
                    device,
                    snapshot,
                    record.record_offset,
                    record.record_len,
                )?;
                builder.add_file()?;
            }
            ExistingAction::Skip => {}
            ExistingAction::Rename(new_name) => {
                stats_renamed_record(&mut builder, device, snapshot, &record, new_name)?;
            }
        }
        offset = offset
            .checked_add(record.record_len)
            .ok_or(Error::Corrupt)?;
        index += 1;
    }
    if offset != snapshot.payload_len {
        return Err(Error::Corrupt);
    }

    if let Change::Write { name, data } = change {
        stats_new_record(&mut builder, name, data)?;
    }

    builder
        .finish(snapshot.geometry())
        .map_err(|error| match error {
            Error::NoSpace => Error::NoSpace,
            _ => Error::Corrupt,
        })
}

struct SegmentWriter<'a, D: BlockDevice> {
    device: &'a mut D,
    snapshot: &'a SnapshotHeader,
    cursor: u32,
    pending: [u8; PROGRAM_SIZE],
    pending_len: usize,
}

impl<'a, D: BlockDevice> SegmentWriter<'a, D> {
    fn new(device: &'a mut D, snapshot: &'a SnapshotHeader) -> Self {
        Self {
            device,
            snapshot,
            cursor: HEADER_SIZE as u32,
            pending: [0; PROGRAM_SIZE],
            pending_len: 0,
        }
    }

    fn bytes(&mut self, mut bytes: &[u8]) -> Result<(), Error<D::Error>> {
        while !bytes.is_empty() {
            let available = PROGRAM_SIZE - self.pending_len;
            let amount = bytes.len().min(available);
            self.pending[self.pending_len..self.pending_len + amount]
                .copy_from_slice(&bytes[..amount]);
            self.pending_len += amount;
            self.cursor = self
                .cursor
                .checked_add(amount as u32)
                .ok_or(Error::NoSpace)?;
            bytes = &bytes[amount..];

            if self.pending_len == PROGRAM_SIZE {
                let word_offset = self.cursor - PROGRAM_SIZE as u32;
                let word = self.pending;
                segment_program(self.device, self.snapshot, word_offset, &word)?;
                self.pending = [0; PROGRAM_SIZE];
                self.pending_len = 0;
            }
        }
        Ok(())
    }

    fn zeros(&mut self, mut length: u32) -> Result<(), Error<D::Error>> {
        while length >= PROGRAM_SIZE as u32 {
            self.bytes(&ZERO_WORD)?;
            length -= PROGRAM_SIZE as u32;
        }
        if length != 0 {
            self.bytes(&ZERO_WORD[..length as usize])?;
        }
        Ok(())
    }

    fn copy_payload(
        &mut self,
        source: &SnapshotHeader,
        payload_offset: u32,
        length: u32,
    ) -> Result<(), Error<D::Error>> {
        let mut buffer = [0u8; COPY_BUFFER_SIZE];
        let mut copied = 0;
        while copied < length {
            let amount = (length - copied).min(COPY_BUFFER_SIZE as u32) as usize;
            let logical = (HEADER_SIZE as u32)
                .checked_add(payload_offset)
                .and_then(|value| value.checked_add(copied))
                .ok_or(Error::Corrupt)?;
            segment_read(self.device, source, logical, &mut buffer[..amount])?;
            self.bytes(&buffer[..amount])?;
            copied += amount as u32;
        }
        Ok(())
    }

    fn finish(self, expected_payload_len: u32) -> Result<(), Error<D::Error>> {
        if self.pending_len != 0
            || self.cursor
                != (HEADER_SIZE as u32)
                    .checked_add(expected_payload_len)
                    .ok_or(Error::NoSpace)?
        {
            return Err(Error::Corrupt);
        }
        Ok(())
    }
}

fn emit_new_record<D: BlockDevice>(
    writer: &mut SegmentWriter<'_, D>,
    name: &str,
    data: &[u8],
) -> Result<(), Error<D::Error>> {
    let data_len = u32::try_from(data.len()).map_err(|_| Error::FileTooLarge)?;
    let (header, encoded) =
        record_encoding(name, data_len, crc32_mpeg2(data)).map_err(|_| Error::NoSpace)?;
    writer.bytes(&encoded)?;
    writer.bytes(name.as_bytes())?;
    writer.bytes(data)?;
    let used = (RECORD_HEADER_SIZE as u32)
        .checked_add(name.len() as u32)
        .and_then(|value| value.checked_add(data_len))
        .ok_or(Error::NoSpace)?;
    writer.zeros(header.record_len - used)
}

fn emit_renamed_record<D: BlockDevice>(
    writer: &mut SegmentWriter<'_, D>,
    source: &SnapshotHeader,
    record: &Record,
    new_name: &str,
) -> Result<(), Error<D::Error>> {
    let (header, encoded) =
        record_encoding(new_name, record.data_len, record.data_crc).map_err(|_| Error::NoSpace)?;
    writer.bytes(&encoded)?;
    writer.bytes(new_name.as_bytes())?;
    writer.copy_payload(source, record.data_offset, record.data_len)?;
    let used = (RECORD_HEADER_SIZE as u32)
        .checked_add(new_name.len() as u32)
        .and_then(|value| value.checked_add(record.data_len))
        .ok_or(Error::NoSpace)?;
    writer.zeros(header.record_len - used)
}

fn emit_change<D: BlockDevice>(
    writer: &mut SegmentWriter<'_, D>,
    source: Option<&SnapshotHeader>,
    change: Change<'_>,
) -> Result<(), Error<D::Error>> {
    if let Some(source) = source {
        let mut offset = 0;
        let mut index = 0;
        while index < source.file_count {
            let record = read_record(writer.device, source, offset)?;
            let name =
                str::from_utf8(&record.name[..record.name_len]).map_err(|_| Error::Corrupt)?;
            match change.action_for(name) {
                ExistingAction::Keep => {
                    writer.copy_payload(source, record.record_offset, record.record_len)?;
                }
                ExistingAction::Skip => {}
                ExistingAction::Rename(new_name) => {
                    emit_renamed_record(writer, source, &record, new_name)?;
                }
            }
            offset = offset
                .checked_add(record.record_len)
                .ok_or(Error::Corrupt)?;
            index += 1;
        }
        if offset != source.payload_len {
            return Err(Error::Corrupt);
        }
    } else if !matches!(change, Change::Clear) {
        return Err(Error::Corrupt);
    }

    if let Change::Write { name, data } = change {
        emit_new_record(writer, name, data)?;
    }
    Ok(())
}

fn segments_overlap(left: &SnapshotHeader, right: &SnapshotHeader) -> bool {
    let mut left_index = 0;
    while left_index < left.block_span {
        let left_block = (left.start_block + left_index) % left.block_count;
        let mut right_index = 0;
        while right_index < right.block_span {
            let right_block = (right.start_block + right_index) % right.block_count;
            if left_block == right_block {
                return true;
            }
            right_index += 1;
        }
        left_index += 1;
    }
    false
}

fn write_candidate<D: BlockDevice>(
    device: &mut D,
    candidate: &SnapshotHeader,
    source: Option<&SnapshotHeader>,
    change: Change<'_>,
) -> Result<(), Error<D::Error>> {
    if source
        .map(|active| segments_overlap(active, candidate))
        .unwrap_or(false)
    {
        return Err(Error::NoSpace);
    }

    let mut relative = 0;
    while relative < candidate.block_span {
        let block = (candidate.start_block + relative) % candidate.block_count;
        device.erase(block).map_err(Error::Device)?;
        relative += 1;
    }

    // The commit word is a pristine program unit and is not part of this call.
    let prefix = candidate.encode_prefix().map_err(|_| Error::Corrupt)?;
    segment_program(device, candidate, 0, &prefix)?;

    let mut writer = SegmentWriter::new(device, candidate);
    emit_change(&mut writer, source, change)?;
    writer.finish(candidate.payload_len)?;

    device.sync().map_err(Error::Device)?;

    let mut observed = [0u8; HEADER_SIZE];
    segment_read(device, candidate, 0, &mut observed)?;
    if observed[..crate::format::SNAPSHOT_PREFIX_SIZE] != prefix
        || observed[crate::format::COMMIT_OFFSET..] != [0xff; PROGRAM_SIZE]
    {
        return Err(Error::Corrupt);
    }
    validate_snapshot(device, candidate)?;

    // Publish only after complete readback. A torn word is rejected at mount
    // unless it happens to equal this exact value; even then both CRCs and the
    // full record structure above must also be valid.
    segment_program(
        device,
        candidate,
        crate::format::COMMIT_OFFSET as u32,
        &commit_marker_bytes(),
    )?;
    device.sync().map_err(Error::Device)?;

    segment_read(device, candidate, 0, &mut observed)?;
    let decoded = SnapshotHeader::decode_at(&observed, candidate.start_block, candidate.geometry())
        .map_err(|_| Error::Corrupt)?;
    if decoded != *candidate {
        return Err(Error::Corrupt);
    }
    validate_snapshot(device, candidate)
}
