use core::str;

use crate::format::{
    BlockDevice, Crc32Mpeg2, Geometry, HEADER_SIZE, MAX_FILES, MAX_NAME_LEN, MAX_WEAR_BLOCKS,
    MAX_WEAR_TABLE_SIZE, PROGRAM_SIZE, RECORD_FLAG_DIRECTORY, RECORD_HEADER_SIZE, RecordHeader,
    SnapshotHeader, WEAR_BAD_BIT, WEAR_COUNT_MASK, checked_snapshot_span, commit_marker_bytes,
    crc32_mpeg2, decode_wear_table, encode_wear_table, generation_is_newer, wear_table_size,
};

use crate::{EntryKind, Error, FileInfo, FsInfo};

const COPY_BUFFER_SIZE: usize = 64;
const ZERO_WORD: [u8; PROGRAM_SIZE] = [0; PROGRAM_SIZE];

/// 磨损计数接近饱和时整体折半的触发阈值。
///
/// 折半保留计数相对顺序与坏块标记, 防止全部计数饱和后动态磨损均衡
/// 退化为"所有候选同分"的无差异选择 (旧设计 u16 饱和后不再区分块)。
/// 计数域为 15 位 (bit15 是坏块标记), 阈值取 30000 留足折半前余量。
const WEAR_RESCALE_AT: u16 = 30_000;

/// 单次提交 (mutate) 的坏块重试上限: 擦除/编程失败标记坏块后换位置
/// 重试; 超过上限返回 NoSpace (块全坏的设备无可用布局)。
const MAX_COMMIT_ATTEMPTS: u32 = 4;

/// 段写入失败分类。
enum CommitError<E> {
    /// 指定块擦除/编程失败: 旧快照完整无损 (候选段与旧快照不重叠),
    /// 可以安全地把该块标记为坏块并换位置重试。
    Block { block: u32, error: E },
    /// 其他失败 (含 commit 字阶段: 设备可能已完成提交, 状态不确定)。
    /// 必须放弃当前内存状态并重新挂载。
    Fatal(Error<E>),
}

impl<E> From<Error<E>> for CommitError<E> {
    fn from(error: Error<E>) -> Self {
        CommitError::Fatal(error)
    }
}

#[derive(Clone, Copy)]
struct Record {
    record_offset: u32,
    record_len: u32,
    data_offset: u32,
    data_len: u32,
    data_crc: u32,
    flags: u16,
    name_len: usize,
    name: [u8; MAX_NAME_LEN],
}

impl Record {
    fn name_eq(&self, name: &str) -> bool {
        self.name_len == name.len() && &self.name[..self.name_len] == name.as_bytes()
    }

    const fn kind(&self) -> EntryKind {
        if self.flags & RECORD_FLAG_DIRECTORY != 0 {
            EntryKind::Directory
        } else {
            EntryKind::File
        }
    }

    const fn is_directory(&self) -> bool {
        matches!(self.kind(), EntryKind::Directory)
    }

    const fn info(&self) -> FileInfo {
        FileInfo {
            kind: self.kind(),
            size: self.data_len,
            crc32: self.data_crc,
        }
    }
}

/// Mounted snapshot filesystem that exclusively owns its block device.
///
/// The in-RAM `wear` array mirrors the per-block erase counters of the active
/// snapshot. Placement of every new snapshot (see [`Self::mutate`]) chooses the
/// least-worn non-overlapping run, which is dynamic wear leveling; the explicit
/// [`Self::level`] performs static wear leveling by relocating an idle snapshot
/// to the least-worn area without changing any data.
pub struct FileSystem<D: BlockDevice> {
    device: D,
    geometry: Geometry,
    active: SnapshotHeader,
    recovery_required: bool,
    wear: [u16; MAX_WEAR_BLOCKS],
}

impl<D: BlockDevice> FileSystem<D> {
    /// Mount the newest completely valid snapshot without modifying the device.
    pub fn mount(mut device: D) -> Result<Self, Error<D::Error>> {
        let geometry = checked_geometry(&device)?;
        let active = scan_active(&mut device, geometry)?.ok_or(Error::NotFormatted)?;
        let wear = read_wear_table(&mut device, &active)?;
        Ok(Self {
            device,
            geometry,
            active,
            recovery_required: false,
            wear,
        })
    }

    /// Atomically publish an empty filesystem.
    ///
    /// If a valid filesystem already exists, it remains mountable until the
    /// empty snapshot is completely written and committed. Erase counters are
    /// monotonic across formats: the previous table is carried over and only
    /// the destination blocks are incremented.
    ///
    /// 与 [`mutate`] 相同的**坏块重试**: 首格式 (virgin 介质) 遇到坏死块时
    /// 标记后换位重试 —— 否则坏块恰落在起始候选上时格式化永久失败
    /// (坏块不可能自愈, 而首格式没有旧快照可回退; 实测复现)。
    pub fn format(mut device: D) -> Result<Self, Error<D::Error>> {
        let geometry = checked_geometry(&device)?;
        let previous = scan_active(&mut device, geometry)?;
        let previous_wear = match &previous {
            Some(header) => read_wear_table(&mut device, header)?,
            None => [0u16; MAX_WEAR_BLOCKS],
        };
        let generation = previous
            .as_ref()
            .map(|header| header.generation.wrapping_add(1))
            .unwrap_or(0);
        let table_size = wear_table_size(geometry.block_count);
        let span =
            checked_snapshot_span(table_size, geometry.block_size).ok_or(Error::InvalidGeometry)?;
        let block_count = geometry.block_count;

        // 提交循环: 坏块重试 + 动态磨损均衡。有旧快照时经
        // `choose_start_block` 避开其占位并选最轻磨损位; 首格式时从首个
        // 不含坏块的候选起。
        let mut wear = rescaled_wear(&previous_wear, block_count);
        for _attempt in 0..MAX_COMMIT_ATTEMPTS {
            let start_block = match &previous {
                Some(active) => choose_start_block(active, span, &wear, block_count),
                None => choose_virgin_start(span, &wear, block_count),
            };
            let Some(start_block) = start_block else {
                break; // 坏块过多: 无可用布局
            };
            let incremented = incremented_wear(&wear, start_block, span, block_count);
            let mut table = [0u8; MAX_WEAR_TABLE_SIZE];
            encode_wear_table(&incremented, block_count, &mut table)
                .map_err(|_| Error::InvalidGeometry)?;
            let header = SnapshotHeader::new(
                generation,
                start_block,
                table_size,
                crc32_mpeg2(&table[..table_size as usize]),
                0,
                geometry,
            )
            .map_err(|_| Error::InvalidGeometry)?;

            match write_candidate(
                &mut device,
                &header,
                previous.as_ref(),
                Change::Clear,
                &table[..table_size as usize],
            ) {
                Ok(()) => {
                    return Ok(Self {
                        device,
                        geometry,
                        active: header,
                        recovery_required: false,
                        wear: incremented,
                    });
                }
                Err(CommitError::Block { block, .. }) => {
                    // 该块擦除/编程失败: 标记坏块 (RAM), 换位置重试。
                    // 未提交的候选不可见, 换位安全 (旧快照未动)。
                    wear = incremented;
                    mark_bad(&mut wear, block);
                }
                Err(CommitError::Fatal(error)) => return Err(error),
            }
        }
        Err(Error::NoSpace)
    }

    /// Return the owned device. Use this before remounting after an I/O error.
    pub fn into_device(self) -> D {
        self.device
    }

    /// 测试脚手架: 只读访问底层块设备 (集成测试注入坏块/检查擦除计数)。
    #[doc(hidden)]
    pub fn device(&self) -> &D {
        &self.device
    }

    /// Whether a failed mutation requires dropping this state and mounting
    /// again before another mutation.
    pub const fn recovery_required(&self) -> bool {
        self.recovery_required
    }

    pub fn info(&self) -> FsInfo {
        let (minimum, maximum) = self.wear_bounds();
        FsInfo {
            generation: self.active.generation,
            entry_count: self.active.file_count,
            file_count: self.active.file_count,
            serialized_bytes: self.active.payload_len,
            active_blocks: self.active.block_span,
            capacity_bytes: snapshot_capacity(self.geometry)
                .and_then(|capacity| {
                    capacity.checked_sub(wear_table_size(self.geometry.block_count))
                })
                .unwrap_or(0),
            min_erase_count: minimum,
            max_erase_count: maximum,
        }
    }

    /// Static wear leveling: relocate the active snapshot to the least-worn
    /// non-overlapping run without changing any data.
    ///
    /// This is a no-op when the per-block erase counts are already within one
    /// of each other. Relocation advances the generation exactly like a normal
    /// mutation and remains a single atomic commit; a torn relocation mounts
    /// back to the previous snapshot.
    pub fn level(&mut self) -> Result<(), Error<D::Error>> {
        if self.recovery_required {
            return Err(Error::RecoveryRequired);
        }
        let (minimum, maximum) = self.wear_bounds();
        if maximum.saturating_sub(minimum) <= 1 {
            return Ok(());
        }
        self.mutate(Change::Level)
    }

    /// Smallest and largest erase count over all non-bad blocks.
    ///
    /// Bad blocks are excluded from wear statistics: their counters no longer
    /// participate in placement decisions.
    pub fn wear_bounds(&self) -> (u32, u32) {
        let mut minimum = u32::MAX;
        let mut maximum = 0;
        for entry in self.wear[..self.geometry.block_count as usize].iter() {
            if entry & WEAR_BAD_BIT != 0 {
                continue;
            }
            let count = (entry & WEAR_COUNT_MASK) as u32;
            minimum = minimum.min(count);
            maximum = maximum.max(count);
        }
        if minimum == u32::MAX {
            return (0, 0); // 全部块均为坏块
        }
        (minimum, maximum)
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

    /// Return metadata for a canonical root-relative path.
    ///
    /// The empty path denotes the implicit root directory. Persistent paths do
    /// not begin or end with `/`, and contain no empty, `.` or `..` component.
    pub fn stat(&mut self, name: &str) -> Result<FileInfo, Error<D::Error>> {
        if name.is_empty() {
            return Ok(root_info());
        }
        validate_name(name)?;
        find_record(&mut self.device, &self.active, name)?
            .map(|record| record.info())
            .ok_or(Error::NotFound)
    }

    /// Maximum data length that a whole-file write to `name` can currently fit.
    ///
    /// This is a read-only preflight. Replacing a file reclaims its current
    /// record, while creating a file also accounts for the namespace limit.
    pub fn max_write_size(&mut self, name: &str) -> Result<u32, Error<D::Error>> {
        validate_name(name)?;
        ensure_parent_directory(&mut self.device, &self.active, name)?;
        let existing = find_record(&mut self.device, &self.active, name)?;
        if existing.is_some_and(|record| record.is_directory()) {
            return Err(Error::IsDirectory);
        }
        if existing.is_none() && self.active.file_count >= MAX_FILES {
            return Err(Error::NoSpace);
        }

        let replaced_len = existing.map_or(0, |record| record.record_len);
        let retained_len = self
            .active
            .payload_len
            .checked_sub(replaced_len)
            .ok_or(Error::Corrupt)?;
        let available = snapshot_capacity(self.geometry)
            .ok_or(Error::InvalidGeometry)?
            .checked_sub(retained_len)
            .ok_or(Error::Corrupt)?;
        let fixed_len = (RECORD_HEADER_SIZE as u32)
            .checked_add(name.len() as u32)
            .ok_or(Error::FileTooLarge)?;
        available.checked_sub(fixed_len).ok_or(Error::NoSpace)
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
        if record.is_directory() {
            return Err(Error::IsDirectory);
        }
        if offset >= record.data_len || buffer.is_empty() {
            return Ok(0);
        }
        let remaining = (record.data_len - offset) as usize;
        let read_len = remaining.min(buffer.len());
        let logical = self
            .active
            .records_base()
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

    /// Visit every persistent entry recursively.
    ///
    /// Names are canonical root-relative paths. The borrowed name is valid only
    /// for the callback. Prefer [`FileSystem::read_dir`] for shell-style listing.
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
        if offset != self.active.records_len() {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    /// Visit the direct children of a directory.
    ///
    /// The empty path denotes the implicit root directory. Child names contain
    /// only the final path component and are valid only for the callback.
    pub fn read_dir<F>(&mut self, directory: &str, mut visitor: F) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&str, FileInfo),
    {
        if !directory.is_empty() {
            validate_name(directory)?;
            let record =
                find_record(&mut self.device, &self.active, directory)?.ok_or(Error::NotFound)?;
            if !record.is_directory() {
                return Err(Error::NotDirectory);
            }
        }

        let mut offset = 0;
        let mut index = 0;
        while index < self.active.file_count {
            let record = read_record(&mut self.device, &self.active, offset)?;
            let name =
                str::from_utf8(&record.name[..record.name_len]).map_err(|_| Error::Corrupt)?;
            if parent_name(name) == non_root(directory) {
                visitor(base_name(name), record.info());
            }
            offset = offset
                .checked_add(record.record_len)
                .ok_or(Error::Corrupt)?;
            index += 1;
        }
        if offset != self.active.records_len() {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    /// Atomically create or replace a complete file.
    pub fn write(&mut self, name: &str, data: &[u8]) -> Result<(), Error<D::Error>> {
        validate_name(name)?;
        u32::try_from(data.len()).map_err(|_| Error::FileTooLarge)?;
        ensure_parent_directory(&mut self.device, &self.active, name)?;
        if find_record(&mut self.device, &self.active, name)?
            .is_some_and(|record| record.is_directory())
        {
            return Err(Error::IsDirectory);
        }
        self.mutate(Change::Write { name, data })
    }

    /// Atomically remove a file.
    pub fn remove(&mut self, name: &str) -> Result<(), Error<D::Error>> {
        validate_name(name)?;
        match find_record(&mut self.device, &self.active, name)? {
            None => return Err(Error::NotFound),
            Some(record) if record.is_directory() => return Err(Error::IsDirectory),
            Some(_) => {}
        }
        self.mutate(Change::Remove { name })
    }

    /// Atomically create an empty directory.
    pub fn mkdir(&mut self, name: &str) -> Result<(), Error<D::Error>> {
        validate_name(name)?;
        ensure_parent_directory(&mut self.device, &self.active, name)?;
        if find_record(&mut self.device, &self.active, name)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        self.mutate(Change::Mkdir { name })
    }

    /// Atomically remove an empty directory.
    pub fn rmdir(&mut self, name: &str) -> Result<(), Error<D::Error>> {
        validate_name(name)?;
        let record = find_record(&mut self.device, &self.active, name)?.ok_or(Error::NotFound)?;
        if !record.is_directory() {
            return Err(Error::NotDirectory);
        }
        if snapshot_has_descendant(&mut self.device, &self.active, name)? {
            return Err(Error::DirectoryNotEmpty);
        }
        self.mutate(Change::Remove { name })
    }

    /// Atomically rename a file or a complete directory subtree.
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
        let source =
            find_record(&mut self.device, &self.active, old_name)?.ok_or(Error::NotFound)?;
        if find_record(&mut self.device, &self.active, new_name)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        if source.is_directory() && is_descendant(new_name, old_name) {
            return Err(Error::InvalidMove);
        }
        ensure_parent_directory(&mut self.device, &self.active, new_name)?;
        preflight_rename(
            &mut self.device,
            &self.active,
            old_name,
            new_name,
            source.is_directory(),
        )?;
        self.mutate(Change::Rename {
            old_name,
            new_name,
            recursive: source.is_directory(),
        })
    }

    /// Atomically remove all entries while preserving generation history.
    pub fn clear(&mut self) -> Result<(), Error<D::Error>> {
        self.mutate(Change::Clear)
    }

    fn mutate(&mut self, change: Change<'_>) -> Result<(), Error<D::Error>> {
        if self.recovery_required {
            return Err(Error::RecoveryRequired);
        }

        // Preflight is read-only. Invalid/corrupt input and NoSpace leave the
        // mounted state usable because no destination block has been erased.
        let stats = calculate_change(&mut self.device, &self.active, change, Crc32Mpeg2::new())?;

        // The wear table is the first bytes of the payload, so the snapshot
        // span (and therefore the placement freedom) depends on it.
        let table_size = wear_table_size(self.geometry.block_count);
        let payload_len = table_size
            .checked_add(stats.payload_len)
            .ok_or(Error::NoSpace)?;
        let span =
            checked_snapshot_span(payload_len, self.geometry.block_size).ok_or(Error::Corrupt)?;
        let block_count = self.geometry.block_count;

        // 提交循环: 动态磨损均衡 (跳过坏块) + 坏块重试。
        // 失败候选与旧快照不重叠, 未提交即不可见 —— 换位置重试安全;
        // 每次重试前把失败块标记为坏 (随下一次成功提交持久化)。
        let generation = self.active.generation.wrapping_add(1);
        let mut wear = self.wear;
        for _attempt in 0..MAX_COMMIT_ATTEMPTS {
            // 饱和预防: 计数接近 u15 上限时整体折半 (见 rescaled_wear)
            wear = rescaled_wear(&wear, block_count);
            let Some(start_block) = choose_start_block(&self.active, span, &wear, block_count)
            else {
                break;
            };
            let incremented = incremented_wear(&wear, start_block, span, block_count);
            let mut table = [0u8; MAX_WEAR_TABLE_SIZE];
            encode_wear_table(&incremented, block_count, &mut table).map_err(|_| Error::Corrupt)?;

            // The payload CRC covers the wear table followed by the records; the
            // record pass continues from the table's CRC state.
            let stats = calculate_change(
                &mut self.device,
                &self.active,
                change,
                Crc32Mpeg2::from_state(crc32_mpeg2(&table[..table_size as usize])),
            )?;
            let next = SnapshotHeader::new(
                generation,
                start_block,
                payload_len,
                stats.payload_crc,
                stats.file_count,
                self.geometry,
            )
            .map_err(|_| Error::NoSpace)?;

            match write_candidate(
                &mut self.device,
                &next,
                Some(&self.active),
                change,
                &table[..table_size as usize],
            ) {
                Ok(()) => {
                    self.active = next;
                    self.wear = incremented;
                    return Ok(());
                }
                Err(CommitError::Block { block, .. }) => {
                    // 该块擦除/编程失败: 标记坏块 (RAM), 保留本次已发生的
                    // 擦除计数, 换位置重试
                    wear = incremented;
                    mark_bad(&mut wear, block);
                }
                Err(CommitError::Fatal(error)) => {
                    // 设备可能已完成提交: 放弃当前内存状态, 要求重挂载
                    self.recovery_required = true;
                    return Err(error);
                }
            }
        }
        // 无可用布局 (坏块过多) 或重试耗尽: 旧快照仍完整可挂载
        Err(Error::NoSpace)
    }
}

#[derive(Clone, Copy)]
enum Change<'a> {
    Write {
        name: &'a str,
        data: &'a [u8],
    },
    Mkdir {
        name: &'a str,
    },
    Remove {
        name: &'a str,
    },
    Rename {
        old_name: &'a str,
        new_name: &'a str,
        recursive: bool,
    },
    Clear,
    /// Static wear leveling: rewrite every record unchanged at a less-worn
    /// location. Never carries an added or removed record.
    Level,
}

#[derive(Clone, Copy)]
enum ExistingAction<'a> {
    Keep,
    Skip,
    Rename(&'a str),
}

impl<'a> Change<'a> {
    fn action_for<'buffer, E>(
        self,
        name: &str,
        renamed: &'buffer mut NameBuffer,
    ) -> Result<ExistingAction<'buffer>, Error<E>> {
        let action = match self {
            Self::Write { name: replaced, .. } if name == replaced => ExistingAction::Skip,
            Self::Remove { name: removed } if name == removed => ExistingAction::Skip,
            Self::Rename {
                old_name,
                new_name,
                recursive,
            } if name == old_name || (recursive && is_descendant(name, old_name)) => {
                ExistingAction::Rename(renamed.replace_prefix(name, old_name, new_name)?)
            }
            Self::Clear => ExistingAction::Skip,
            // Level rewrites every record unchanged at the new location.
            Self::Level => ExistingAction::Keep,
            // Mkdir and unrelated Write/Rename records are copied verbatim.
            _ => ExistingAction::Keep,
        };
        Ok(action)
    }
}

struct NameBuffer {
    bytes: [u8; MAX_NAME_LEN],
    len: usize,
}

impl NameBuffer {
    const fn new() -> Self {
        Self {
            bytes: [0; MAX_NAME_LEN],
            len: 0,
        }
    }

    fn replace_prefix<E>(
        &mut self,
        name: &str,
        old_prefix: &str,
        new_prefix: &str,
    ) -> Result<&str, Error<E>> {
        let suffix = name.get(old_prefix.len()..).ok_or(Error::Corrupt)?;
        let len = new_prefix
            .len()
            .checked_add(suffix.len())
            .filter(|length| *length <= MAX_NAME_LEN)
            .ok_or(Error::InvalidName)?;
        self.bytes[..new_prefix.len()].copy_from_slice(new_prefix.as_bytes());
        self.bytes[new_prefix.len()..len].copy_from_slice(suffix.as_bytes());
        self.len = len;
        str::from_utf8(&self.bytes[..self.len]).map_err(|_| Error::Corrupt)
    }
}

fn validate_name<E>(name: &str) -> Result<(), Error<E>> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_NAME_LEN
        || bytes.first() == Some(&b'/')
        || bytes.last() == Some(&b'/')
        || bytes.contains(&0)
    {
        return Err(Error::InvalidName);
    }
    for component in name.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(Error::InvalidName);
        }
    }
    Ok(())
}

const fn root_info() -> FileInfo {
    FileInfo {
        kind: EntryKind::Directory,
        size: 0,
        crc32: crate::format::CRC32_MPEG2_INITIAL,
    }
}

fn non_root(path: &str) -> Option<&str> {
    if path.is_empty() { None } else { Some(path) }
}

fn parent_name(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

fn base_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn is_descendant(path: &str, directory: &str) -> bool {
    path.len() > directory.len()
        && path.as_bytes().get(directory.len()) == Some(&b'/')
        && path.starts_with(directory)
}

fn checked_geometry<D: BlockDevice>(device: &D) -> Result<Geometry, Error<D::Error>> {
    let geometry = device.geometry();
    if geometry.block_size < HEADER_SIZE as u32
        || !geometry.block_size.is_multiple_of(PROGRAM_SIZE as u32)
        || geometry.block_size < wear_table_size(geometry.block_count)
        || geometry.block_count < 2
        || geometry.block_count > MAX_WEAR_BLOCKS as u32
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
) -> Result<(), CommitError<D::Error>> {
    if !logical_offset.is_multiple_of(PROGRAM_SIZE as u32)
        || !data.len().is_multiple_of(PROGRAM_SIZE)
    {
        return Err(CommitError::Fatal(Error::Corrupt));
    }
    let length = u32::try_from(data.len()).map_err(|_| Error::Corrupt)?;
    let end = logical_offset.checked_add(length).ok_or(Error::Corrupt)?;
    let segment_len = snapshot
        .block_span
        .checked_mul(snapshot.block_size)
        .ok_or(Error::Corrupt)?;
    if end > segment_len {
        return Err(CommitError::Fatal(Error::Corrupt));
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
            return Err(CommitError::Fatal(Error::Corrupt));
        }
        let (current, rest) = input.split_at(amount);
        if let Err(error) = device.program(block, block_offset, current) {
            // 仅"电源完好且该块确已损坏"的错误可标记坏块并重试; 其余
            // (掉电/超时/总线撕裂) 一律 Fatal, 由调用方重挂载
            if device.permanent_block_failure(&error) {
                return Err(CommitError::Block { block, error });
            }
            Err(Error::Device(error))?;
        }
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
    let logical = snapshot
        .records_base()
        .checked_add(payload_offset)
        .ok_or(Error::Corrupt)?;
    segment_read(device, snapshot, logical, &mut encoded)?;
    let header = RecordHeader::decode(&encoded).map_err(|_| Error::Corrupt)?;
    if header.flags == RECORD_FLAG_DIRECTORY
        && (header.data_len != 0 || header.data_crc != crc32_mpeg2(&[]))
    {
        return Err(Error::Corrupt);
    }
    let record_end = payload_offset
        .checked_add(header.record_len)
        .ok_or(Error::Corrupt)?;
    if record_end > snapshot.payload_len {
        return Err(Error::Corrupt);
    }

    let name_len = header.name_len as usize;
    let mut name = [0u8; MAX_NAME_LEN];
    let name_offset = header_end;
    let name_logical = snapshot
        .records_base()
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
        flags: header.flags,
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
    if offset != snapshot.records_len() {
        return Err(Error::Corrupt);
    }
    Ok(None)
}

fn ensure_parent_directory<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
    name: &str,
) -> Result<(), Error<D::Error>> {
    let Some(parent) = parent_name(name) else {
        return Ok(());
    };
    let record = find_record(device, snapshot, parent)?.ok_or(Error::NotFound)?;
    if !record.is_directory() {
        return Err(Error::NotDirectory);
    }
    Ok(())
}

fn snapshot_has_descendant<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
    directory: &str,
) -> Result<bool, Error<D::Error>> {
    let mut offset = 0;
    let mut index = 0;
    while index < snapshot.file_count {
        let record = read_record(device, snapshot, offset)?;
        let name = str::from_utf8(&record.name[..record.name_len]).map_err(|_| Error::Corrupt)?;
        if is_descendant(name, directory) {
            return Ok(true);
        }
        offset = offset
            .checked_add(record.record_len)
            .ok_or(Error::Corrupt)?;
        index += 1;
    }
    if offset != snapshot.records_len() {
        return Err(Error::Corrupt);
    }
    Ok(false)
}

fn preflight_rename<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
    old_name: &str,
    new_name: &str,
    recursive: bool,
) -> Result<(), Error<D::Error>> {
    let mut offset = 0;
    let mut index = 0;
    let mut renamed = NameBuffer::new();
    while index < snapshot.file_count {
        let record = read_record(device, snapshot, offset)?;
        let name = str::from_utf8(&record.name[..record.name_len]).map_err(|_| Error::Corrupt)?;
        if name == old_name || (recursive && is_descendant(name, old_name)) {
            renamed.replace_prefix::<D::Error>(name, old_name, new_name)?;
        }
        offset = offset
            .checked_add(record.record_len)
            .ok_or(Error::Corrupt)?;
        index += 1;
    }
    if offset != snapshot.records_len() {
        return Err(Error::Corrupt);
    }

    Ok(())
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

        let data_logical = snapshot
            .records_base()
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
            snapshot
                .records_base()
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
    if offset != snapshot.records_len() {
        return Err(Error::Corrupt);
    }

    // Every non-root parent is explicit and must itself be a directory. Run
    // this after the boundary/CRC pass so arbitrary record offsets are never
    // followed before the whole payload has been structurally validated.
    let mut child = 0usize;
    while child < snapshot.file_count as usize {
        let record = read_record(device, snapshot, offsets[child])?;
        let name = str::from_utf8(&record.name[..record.name_len]).map_err(|_| Error::Corrupt)?;
        if let Some(parent) = parent_name(name) {
            let parent = find_record(device, snapshot, parent)?.ok_or(Error::Corrupt)?;
            if !parent.is_directory() {
                return Err(Error::Corrupt);
            }
        }
        child += 1;
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
    /// `seed` is the CRC state of every payload byte written before the
    /// records (the wear table); the finished value covers table + records.
    fn new(seed: Crc32Mpeg2) -> Self {
        Self {
            payload_len: 0,
            crc: seed,
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
        let capacity = snapshot_capacity(geometry).ok_or(Error::NoSpace)?;
        let total = self
            .payload_len
            .checked_add(wear_table_size(geometry.block_count))
            .ok_or(Error::NoSpace)?;
        if total > capacity {
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
        let logical = snapshot
            .records_base()
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
    flags: u16,
) -> Result<(RecordHeader, [u8; RECORD_HEADER_SIZE]), Error<core::convert::Infallible>> {
    let name_len = u16::try_from(name.len()).map_err(|_| Error::InvalidName)?;
    let header =
        RecordHeader::new(name_len, data_len, data_crc, flags).map_err(|_| Error::NoSpace)?;
    let encoded = header.encode().map_err(|_| Error::NoSpace)?;
    Ok((header, encoded))
}

fn stats_new_record<E>(
    builder: &mut StatsBuilder,
    name: &str,
    data: &[u8],
    flags: u16,
) -> Result<(), Error<E>> {
    let data_len = u32::try_from(data.len()).map_err(|_| Error::FileTooLarge)?;
    let (header, encoded) =
        record_encoding(name, data_len, crc32_mpeg2(data), flags).map_err(|error| match error {
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
        record_encoding(new_name, record.data_len, record.data_crc, record.flags)
            .map_err(|_| Error::NoSpace)?;
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
    crc_seed: Crc32Mpeg2,
) -> Result<PlanStats, Error<D::Error>> {
    let mut builder = StatsBuilder::new(crc_seed);
    let mut renamed = NameBuffer::new();
    let mut offset = 0;
    let mut index = 0;
    while index < snapshot.file_count {
        let record = read_record(device, snapshot, offset)?;
        let name = str::from_utf8(&record.name[..record.name_len]).map_err(|_| Error::Corrupt)?;
        match change.action_for(name, &mut renamed)? {
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
    if offset != snapshot.records_len() {
        return Err(Error::Corrupt);
    }

    match change {
        Change::Write { name, data } => stats_new_record(&mut builder, name, data, 0)?,
        Change::Mkdir { name } => stats_new_record(&mut builder, name, &[], RECORD_FLAG_DIRECTORY)?,
        _ => {}
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

    fn bytes(&mut self, mut bytes: &[u8]) -> Result<(), CommitError<D::Error>> {
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

    fn zeros(&mut self, mut length: u32) -> Result<(), CommitError<D::Error>> {
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
    ) -> Result<(), CommitError<D::Error>> {
        let mut buffer = [0u8; COPY_BUFFER_SIZE];
        let mut copied = 0;
        while copied < length {
            let amount = (length - copied).min(COPY_BUFFER_SIZE as u32) as usize;
            let logical = source
                .records_base()
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
    flags: u16,
) -> Result<(), CommitError<D::Error>> {
    let data_len = u32::try_from(data.len()).map_err(|_| Error::FileTooLarge)?;
    let (header, encoded) =
        record_encoding(name, data_len, crc32_mpeg2(data), flags).map_err(|_| Error::NoSpace)?;
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
) -> Result<(), CommitError<D::Error>> {
    let (header, encoded) =
        record_encoding(new_name, record.data_len, record.data_crc, record.flags)
            .map_err(|_| Error::NoSpace)?;
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
) -> Result<(), CommitError<D::Error>> {
    if let Some(source) = source {
        let mut offset = 0;
        let mut index = 0;
        let mut renamed = NameBuffer::new();
        while index < source.file_count {
            let record = read_record(writer.device, source, offset)?;
            let name =
                str::from_utf8(&record.name[..record.name_len]).map_err(|_| Error::Corrupt)?;
            match change.action_for(name, &mut renamed)? {
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
        if offset != source.records_len() {
            Err(Error::Corrupt)?;
        }
    } else if !matches!(change, Change::Clear) {
        Err(Error::Corrupt)?;
    }

    match change {
        Change::Write { name, data } => emit_new_record(writer, name, data, 0)?,
        Change::Mkdir { name } => emit_new_record(writer, name, &[], RECORD_FLAG_DIRECTORY)?,
        _ => {}
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

fn spans_overlap(
    start: u32,
    span: u32,
    other_start: u32,
    other_span: u32,
    block_count: u32,
) -> bool {
    let mut index = 0;
    while index < span {
        let block = (start + index) % block_count;
        let mut other = 0;
        while other < other_span {
            if (other_start + other) % block_count == block {
                return true;
            }
            other += 1;
        }
        index += 1;
    }
    false
}

/// Choose the next segment start with dynamic wear leveling.
///
/// Among every run of `span` consecutive blocks that does not overlap the
/// active snapshot and contains **no bad blocks**, pick the one with the
/// lowest maximum erase count (lowest total as tie-break). The remaining
/// tie-break is the smallest forward cyclic distance from the old successor
/// position, which reproduces the sequential sweep for uniformly worn devices.
/// `span <= block_count/2` guarantees at least one non-overlapping candidate;
/// bad blocks may remove all of them, in which case `None` is returned
/// (the device has no viable layout for this snapshot).
fn choose_start_block(
    active: &SnapshotHeader,
    span: u32,
    wear: &[u16; MAX_WEAR_BLOCKS],
    block_count: u32,
) -> Option<u32> {
    let successor = (active.start_block + active.block_span) % block_count;
    let mut best: Option<(u32, u32, u32, u32)> = None;
    let mut start = 0;
    while start < block_count {
        if !spans_overlap(
            start,
            span,
            active.start_block,
            active.block_span,
            block_count,
        ) {
            let (mut maximum, mut sum) = (0u32, 0u32);
            let mut bad = false;
            let mut index = 0;
            while index < span {
                let entry = wear[((start + index) % block_count) as usize];
                if entry & WEAR_BAD_BIT != 0 {
                    bad = true;
                    break;
                }
                let count = (entry & WEAR_COUNT_MASK) as u32;
                maximum = maximum.max(count);
                sum = sum.saturating_add(count);
                index += 1;
            }
            if bad {
                start += 1;
                continue;
            }
            let distance = (start + block_count - successor) % block_count;
            let replace = best
                .map(|(best_max, best_sum, best_distance, _)| {
                    (maximum, sum, distance) < (best_max, best_sum, best_distance)
                })
                .unwrap_or(true);
            if replace {
                best = Some((maximum, sum, distance, start));
            }
        }
        start += 1;
    }
    best.map(|(_, _, _, start)| start)
}

/// Return `wear` with every block of the destination segment incremented,
/// saturating at the 15-bit counter width (bad flags preserved).
fn incremented_wear(
    wear: &[u16; MAX_WEAR_BLOCKS],
    start_block: u32,
    span: u32,
    block_count: u32,
) -> [u16; MAX_WEAR_BLOCKS] {
    let mut next = *wear;
    let mut index = 0;
    while index < span {
        let block = ((start_block + index) % block_count) as usize;
        let entry = next[block];
        // 计数域 15 位: +1 后钳位到掩码上限 (32767+1 不再进位到坏位)
        let count = ((entry & WEAR_COUNT_MASK) + 1).min(WEAR_COUNT_MASK);
        next[block] = (entry & WEAR_BAD_BIT) | count;
        index += 1;
    }
    next
}

/// 磨损计数接近饱和时整体折半 (坏块标记保留)。
///
/// u15 计数饱和后所有块同分, 动态磨损均衡退化为无差异选择; 折半在
/// 计数逼近饱和前触发, 保留相对顺序, 均衡永不失效。折半发生在选择
/// 新位置**之前**, 折半后的计数随下一次成功提交持久化。
fn rescaled_wear(wear: &[u16; MAX_WEAR_BLOCKS], block_count: u32) -> [u16; MAX_WEAR_BLOCKS] {
    let mut next = *wear;
    let saturated = next[..block_count as usize]
        .iter()
        .any(|&entry| entry & WEAR_COUNT_MASK >= WEAR_RESCALE_AT);
    if saturated {
        for entry in &mut next[..block_count as usize] {
            *entry = (*entry & WEAR_BAD_BIT) | ((*entry & WEAR_COUNT_MASK) / 2);
        }
    }
    next
}

/// 无旧快照 (首格式) 时的首个可行候选起点: 依次找第一个完全不含
/// 坏块的连续 `span` 块运行。全为坏块 (或坏块过多) 时返回 `None`。
fn choose_virgin_start(span: u32, wear: &[u16; MAX_WEAR_BLOCKS], block_count: u32) -> Option<u32> {
    let mut start = 0;
    while start < block_count {
        let mut bad = false;
        let mut index = 0;
        while index < span {
            if wear[((start + index) % block_count) as usize] & WEAR_BAD_BIT != 0 {
                bad = true;
                break;
            }
            index += 1;
        }
        if !bad {
            return Some(start);
        }
        start += 1;
    }
    None
}

/// 标记坏块 (仅内存; 随下一次成功提交的磨损表持久化)。
///
/// 被标记块不再作为快照放置候选; 无重映射 —— 跳过即"管理"。
fn mark_bad(wear: &mut [u16; MAX_WEAR_BLOCKS], block: u32) {
    wear[block as usize] |= WEAR_BAD_BIT;
}

/// Read the active snapshot's per-block erase table.
///
/// The table is the first bytes of the payload, so its integrity is covered
/// by the snapshot payload CRC validated during mount.
fn read_wear_table<D: BlockDevice>(
    device: &mut D,
    snapshot: &SnapshotHeader,
) -> Result<[u16; MAX_WEAR_BLOCKS], Error<D::Error>> {
    let size = wear_table_size(snapshot.block_count);
    let mut bytes = [0u8; MAX_WEAR_TABLE_SIZE];
    segment_read(
        device,
        snapshot,
        HEADER_SIZE as u32,
        &mut bytes[..size as usize],
    )?;
    let mut wear = [0u16; MAX_WEAR_BLOCKS];
    decode_wear_table(&bytes, snapshot.block_count, &mut wear).map_err(|_| Error::Corrupt)?;
    Ok(wear)
}

fn write_candidate<D: BlockDevice>(
    device: &mut D,
    candidate: &SnapshotHeader,
    source: Option<&SnapshotHeader>,
    change: Change<'_>,
    wear_bytes: &[u8],
) -> Result<(), CommitError<D::Error>> {
    if source
        .map(|active| segments_overlap(active, candidate))
        .unwrap_or(false)
    {
        Err(Error::NoSpace)?;
    }

    let mut relative = 0;
    while relative < candidate.block_span {
        let block = (candidate.start_block + relative) % candidate.block_count;
        if let Err(error) = device.erase(block) {
            // 仅"电源完好且该块确已损坏"的错误可标记坏块并重试;
            // 掉电等模糊错误一律 Fatal (候选未写入任何内容, 旧快照无损)
            if device.permanent_block_failure(&error) {
                return Err(CommitError::Block { block, error });
            }
            Err(Error::Device(error))?;
        }
        relative += 1;
    }

    // The commit word is a pristine program unit and is not part of this call.
    let prefix = candidate.encode_prefix().map_err(|_| Error::Corrupt)?;
    segment_program(device, candidate, 0, &prefix)?;

    let mut writer = SegmentWriter::new(device, candidate);
    // The wear table is the first bytes of the payload, before the records.
    writer.bytes(wear_bytes)?;
    emit_change(&mut writer, source, change)?;
    writer.finish(candidate.payload_len)?;

    device.sync().map_err(Error::Device)?;

    let mut observed = [0u8; HEADER_SIZE];
    segment_read(device, candidate, 0, &mut observed)?;
    if observed[..crate::format::SNAPSHOT_PREFIX_SIZE] != prefix
        || observed[crate::format::COMMIT_OFFSET..] != [0xff; PROGRAM_SIZE]
    {
        Err(Error::Corrupt)?;
    }
    validate_snapshot(device, candidate)?;

    // Publish only after complete readback. A torn word is rejected at mount
    // unless it happens to equal this exact value; even then both CRCs and the
    // full record structure above must also be valid.
    //
    // commit 字失败是不可重试的: 设备可能已物理完成提交 (块级错误按
    // Fatal 处理, 调用方必须重挂载, 不得从旧内存状态继续写)。
    segment_program(
        device,
        candidate,
        crate::format::COMMIT_OFFSET as u32,
        &commit_marker_bytes(),
    )
    .map_err(|error| match error {
        CommitError::Block { error, .. } => CommitError::Fatal(Error::Device(error)),
        fatal => fatal,
    })?;
    device.sync().map_err(Error::Device)?;

    segment_read(device, candidate, 0, &mut observed)?;
    let decoded = SnapshotHeader::decode_at(&observed, candidate.start_block, candidate.geometry())
        .map_err(|_| Error::Corrupt)?;
    if decoded != *candidate {
        Err(Error::Corrupt)?;
    }
    validate_snapshot(device, candidate).map_err(CommitError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_geometry() -> Geometry {
        Geometry::new(4096, 8)
    }

    fn sample_active() -> SnapshotHeader {
        // span=1 的空快照 (起始块 0)
        SnapshotHeader::new(1, 0, wear_table_size(8), 0, 0, sample_geometry()).unwrap()
    }

    #[test]
    fn rescale_halves_counts_and_preserves_bad_flags() {
        let mut wear = [0u16; MAX_WEAR_BLOCKS];
        wear[0] = 10;
        wear[1] = WEAR_RESCALE_AT; // 触发折半
        wear[2] = WEAR_BAD_BIT | 20_000; // 坏块, 计数 20000
        wear[3] = WEAR_BAD_BIT | 29_999; // 坏块, 计数接近饱和

        let rescaled = rescaled_wear(&wear, 8);
        assert_eq!(rescaled[0], 5);
        assert_eq!(rescaled[1], WEAR_RESCALE_AT / 2);
        // 坏块标记保留, 计数折半
        assert_eq!(rescaled[2], WEAR_BAD_BIT | 10_000);
        assert_eq!(rescaled[3], WEAR_BAD_BIT | 14_999);
        // 范围外条目原样保留
        assert_eq!(rescaled[8], 0);
    }

    #[test]
    fn rescale_is_idempotent_below_threshold() {
        let mut wear = [0u16; MAX_WEAR_BLOCKS];
        wear[0] = WEAR_RESCALE_AT - 1;
        let rescaled = rescaled_wear(&wear, 8);
        assert_eq!(rescaled, wear);
    }

    #[test]
    fn choose_skips_runs_containing_bad_blocks() {
        let active = sample_active(); // 当前 [0,1)
        let mut wear = [0u16; MAX_WEAR_BLOCKS];
        // 均匀磨损: 顺序轮转本应选块 1 (后继); 坏掉 1 后应选 2
        let start = choose_start_block(&active, 1, &wear, 8).unwrap();
        assert_eq!(start, 1);
        wear[1] = WEAR_BAD_BIT;
        let start = choose_start_block(&active, 1, &wear, 8).unwrap();
        assert_eq!(start, 2);
    }

    #[test]
    fn choose_returns_none_when_all_candidates_contain_bad_blocks() {
        let active = sample_active(); // 当前 [0,1), 候选 = 1..7
        let mut wear = [0u16; MAX_WEAR_BLOCKS];
        for block in 1..8 {
            wear[block] = WEAR_BAD_BIT;
        }
        assert_eq!(choose_start_block(&active, 1, &wear, 8), None);
    }

    #[test]
    fn incremented_wear_saturates_at_mask_and_keeps_bad() {
        let mut wear = [0u16; MAX_WEAR_BLOCKS];
        wear[0] = WEAR_COUNT_MASK;
        wear[1] = WEAR_BAD_BIT | 5;
        let next = incremented_wear(&wear, 0, 2, 8);
        assert_eq!(next[0], WEAR_COUNT_MASK); // 饱和
        assert_eq!(next[1], WEAR_BAD_BIT | 6); // 坏标记保留, 计数递增
    }

    #[test]
    fn mark_bad_sets_flag_without_touching_count() {
        let mut wear = [0u16; MAX_WEAR_BLOCKS];
        wear[4] = 123;
        mark_bad(&mut wear, 4);
        assert_eq!(wear[4], WEAR_BAD_BIT | 123);
    }

    #[test]
    fn wear_bounds_skip_bad_blocks() {
        let mut wear = [7u16; MAX_WEAR_BLOCKS];
        wear[0] = 3;
        wear[1] = 9;
        wear[2] = WEAR_BAD_BIT | 20_000; // 坏块, 不参与统计
        let fs = FileSystem::<DummyDevice> {
            device: DummyDevice,
            geometry: sample_geometry(),
            active: sample_active(),
            recovery_required: false,
            wear,
        };
        assert_eq!(fs.wear_bounds(), (3, 9));
    }

    /// 仅用于构造 FileSystem 的哑设备 (不执行任何 I/O)
    struct DummyDevice;

    impl BlockDevice for DummyDevice {
        type Error = ();
        fn block_size(&self) -> u32 {
            4096
        }
        fn block_count(&self) -> u32 {
            8
        }
        fn read(&mut self, _b: u32, _o: u32, _buf: &mut [u8]) -> Result<(), ()> {
            Ok(())
        }
        fn program(&mut self, _b: u32, _o: u32, _d: &[u8]) -> Result<(), ()> {
            Ok(())
        }
        fn erase(&mut self, _b: u32) -> Result<(), ()> {
            Ok(())
        }
        fn sync(&mut self) -> Result<(), ()> {
            Ok(())
        }
    }
}
