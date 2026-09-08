//! Power-loss-safe filesystem integration for the HC32F460 internal Flash.
//!
//! The generic filesystem lives in `crates/littlefs`. This module owns the
//! board partition and narrows the permissive EFM driver to the filesystem's
//! NOR contract: relative addresses, one owner, aligned one-shot programming,
//! and mandatory erase/program readback.
//!
//! # 线程模型
//!
//! 挂载后的文件系统实例存放在全局 [`FILESYSTEM`] (`rtos::Mutex`, 优先级
//! 继承), 任意线程可经 [`mounted`] 获取独占守卫后使用 — shell 命令与
//! 日志落盘线程 (见 [`crate::logfile`]) 共享同一个实例。
//!
//! [`InternalFlash`] 原本以 `PhantomData<*mut ()>` 声明 `!Send + !Sync`
//! 把 Flash 访问限定在单线程; 现在经 `unsafe impl Send` 放宽, 安全契约:
//! - 所有 `FileSystem`/`InternalFlash` 访问都发生在 `rtos::Mutex` 保护内
//!   (持锁期间互斥, 锁有优先级继承, 高优先级等待者不会无界阻塞);
//! - 阻塞取锁在中断上下文会返回错误, 未挂载/ISR 访问被明确拒绝;
//! - 单核系统上临界区原语保证锁状态与 Flash 控制器状态一致。
#![allow(dead_code)]

use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::rtos::{Mutex, Timeout};
use littlefs::BlockDevice;

/// First filesystem sector (sector 46).
pub const PARTITION_START: u32 = 0x0005_C000;
/// Filesystem partition size: sectors 46 through 61 (128KiB).
pub const PARTITION_SIZE: u32 = 128 * 1024;
/// HC32F460 main-Flash erase sector size.
pub const BLOCK_SIZE: u32 = crate::efm::SECTOR_SIZE;
/// Number of sectors in the filesystem partition.
pub const BLOCK_COUNT: u32 = PARTITION_SIZE / BLOCK_SIZE;
/// First address not owned by the filesystem (sector 62 self-test area).
pub const PARTITION_END: u32 = PARTITION_START + PARTITION_SIZE;

const _: () = assert!(PARTITION_START.is_multiple_of(crate::efm::SECTOR_SIZE));
const _: () = assert!(PARTITION_END == 0x0007_C000);
const _: () = assert!(BLOCK_COUNT == 16);

static TAKEN: AtomicBool = AtomicBool::new(false);

/// 全局文件系统实例 (唯一所有者; 经 [`mounted`] 获取)
static FILESYSTEM: Mutex<Option<FileSystem>> = Mutex::new(None);

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
/// SAFETY (`unsafe impl Send`): 本类型原以 `PhantomData<*mut ()>` 声明
/// `!Send + !Sync` 以把 Flash 访问限定在单线程。放宽为 `Send` 的前提是
/// 模块级契约: 实例只能存放在 [`FILESYSTEM`] 中, 一切访问必须经由
/// `rtos::Mutex` 串行化 (见模块文档"线程模型"); 该契约由 [`mounted`]
/// 的守卫类型强制, `InternalFlash`/`FileSystem` 无任何脱离锁的访问路径。
pub struct InternalFlash {
    _not_send_or_sync: PhantomData<*mut ()>,
}

// SAFETY: 见结构体文档 —— 所有访问经 `FILESYSTEM` 互斥量串行化
unsafe impl Send for InternalFlash {}

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
        if !Self::region_erased(base, BLOCK_SIZE as usize)? {
            return Err(FlashError::VerifyFailed);
        }
        Ok(())
    }

    fn partition_is_erased(&self) -> Result<bool, FlashError> {
        Self::check_thread_context()?;
        Self::region_erased(PARTITION_START, PARTITION_SIZE as usize)
    }

    /// 校验 [address, address+len) 是否全部为擦除态 (0xFF)。
    ///
    /// 逐 1KiB 分块, 栈缓冲零分配; 块校验首次不一致会**重读一次**
    /// (见 [`Self::chunk_verify`]), 总线/缓存毛刺不会误判为数据损坏。
    fn region_erased(address: u32, len: usize) -> Result<bool, FlashError> {
        let mut scratch = [0u8; 1024];
        let mut offset = 0;
        while offset < len {
            let chunk = (len - offset).min(scratch.len());
            if !Self::chunk_verify(address + offset as u32, None, &mut scratch[..chunk])? {
                return Ok(false);
            }
            offset += chunk;
        }
        Ok(true)
    }

    /// 分块读取并校验: 大块读优先 DMA 整块拷贝 (Flash→RAM), 回退逐字。
    ///
    /// 校验不一致时**整个分块重读一次**: 单次读回毛刺 (总线竞争/缓存
    /// 一致性) 若直接判"非擦除态/编程失败", 会把整个 8KB 扇区永久标记
    /// 为坏块 (见 [`permanent_block_failure`]), 实际数据无害。
    fn chunk_verify(
        address: u32,
        expected: Option<&[u8]>,
        scratch: &mut [u8],
    ) -> Result<bool, FlashError> {
        for _attempt in 0..2 {
            Self::read_chunk(address, scratch)?;
            let matched = match expected {
                None => scratch.iter().all(|&b| b == 0xFF),
                Some(exp) => scratch == exp,
            };
            if matched {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 读取 [address, address+len) 到 `scratch` (长度 ≤1KiB 且为 4 的倍数;
    /// 调用方保证地址在分区内且内存映射)。大块读优先 DMA 整块拷贝。
    fn read_chunk(address: u32, scratch: &mut [u8]) -> Result<(), FlashError> {
        let flash = unsafe { core::slice::from_raw_parts(address as *const u8, scratch.len()) };
        if crate::dma::copy_try(flash, scratch) {
            return Ok(());
        }
        // 回退逐字 (Flash 字读, 4B 粒度; 长度恒为 4 的倍数)
        for (i, word) in scratch.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *word = crate::efm::read_word(address + (i * 4) as u32)
                .map_err(FlashError::Controller)?
                .to_le_bytes();
        }
        Ok(())
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

    /// 分类"块永久损坏"错误: 电源/总线完好的前提下, 擦写控制器的
    /// PEWERR (擦/写失败)、PGMISMTCH (回读不匹配) 与擦除后校验失败
    /// 表明该扇区单元已损坏 —— 文件系统可标记坏块并换位置重试。
    /// 掉电/超时/读冲突等模糊错误保持默认 false (Fatal, 需重挂载)。
    ///
    /// 校验类错误 (VerifyFailed/Mismatch) 在判永久前已经过一次
    /// **重读确认** (见 [`chunk_verify`]), 单次总线/缓存毛刺不会
    /// 把一个 8KB 扇区永久报废。
    fn permanent_block_failure(&self, error: &Self::Error) -> bool {
        matches!(
            error,
            FlashError::VerifyFailed
                | FlashError::Controller(crate::efm::EfmError::ProgramEraseError)
                | FlashError::Controller(crate::efm::EfmError::Mismatch)
        )
    }

    fn read(&mut self, block: u32, offset: u32, buffer: &mut [u8]) -> Result<(), Self::Error> {
        Self::check_thread_context()?;
        let address = Self::address(block, offset, buffer.len())?;
        // 大块读取走 DMA 整块拷贝 (Flash→RAM, 比逐字节循环快约一个数量级);
        // 未接管 (过短/通道忙) 时回退逐字节轮询。
        // `address` was range-checked by `Self::address`; Flash is memory mapped.
        let flash = unsafe { core::slice::from_raw_parts(address as *const u8, buffer.len()) };
        if crate::dma::copy_try(flash, buffer) {
            return Ok(());
        }
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

        // 写后回读校验: 分块校验不一致时重读一次 (见 [`Self::chunk_verify`]),
        // 单次读回毛刺不判永久坏块。优先 DMA 整块回读, 回退逐字。
        let mut scratch = [0u8; 1024];
        let mut offset = 0;
        while offset < data.len() {
            let chunk = (data.len() - offset).min(scratch.len());
            if !Self::chunk_verify(
                address + offset as u32,
                Some(&data[offset..offset + chunk]),
                &mut scratch[..chunk],
            )? {
                return Err(FlashError::VerifyFailed);
            }
            offset += chunk;
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

/// 挂载句柄: 持锁期间独占文件系统实例 (`Deref`/`DerefMut` → `FileSystem`)。
/// 守卫析构即释放互斥量; 在 shell 线程持有期间, 日志落盘线程会阻塞等待。
pub struct MountGuard {
    inner: crate::rtos::MutexGuard<'static, Option<FileSystem>>,
}

impl core::ops::Deref for MountGuard {
    type Target = FileSystem;

    fn deref(&self) -> &FileSystem {
        self.inner.as_ref().expect("MountGuard 仅来自已挂载状态")
    }
}

impl core::ops::DerefMut for MountGuard {
    fn deref_mut(&mut self) -> &mut FileSystem {
        self.inner.as_mut().expect("MountGuard 仅来自已挂载状态")
    }
}

/// 获取已挂载文件系统的独占访问守卫 (未挂载返回 `None`)。
///
/// 阻塞等待互斥量 (优先级继承); 仅可在线程上下文调用。
pub fn mounted() -> Option<MountGuard> {
    let guard = FILESYSTEM.lock(Timeout::Forever).ok()?;
    if guard.is_none() {
        return None;
    }
    Some(MountGuard { inner: guard })
}

/// 把新实例发布到全局槽位 (替换旧实例, 旧实例析构释放分区 token)
fn publish(filesystem: FileSystem) {
    let mut guard = FILESYSTEM
        .lock(Timeout::Forever)
        .expect("发布文件系统必须在线程上下文");
    drop(guard.replace(filesystem));
}

/// Result of starting the filesystem during boot.
pub enum Startup {
    /// An existing committed snapshot was mounted without writing Flash.
    Mounted,
    /// A completely erased partition was initialized with an empty snapshot.
    Formatted,
}

/// Starts the filesystem during boot.
///
/// Existing media is mounted read-only. A missing snapshot is formatted only
/// when every word in the partition is still erased; non-erased invalid media
/// is preserved for explicit diagnosis or recovery.
pub fn start(
    _resources: &crate::board::BoardResources,
) -> Result<Startup, littlefs::Error<FlashError>> {
    match mount() {
        Ok(filesystem) => {
            publish(filesystem);
            Ok(Startup::Mounted)
        }
        Err(littlefs::Error::NotFormatted) => {
            let device = InternalFlash::take().ok_or(littlefs::Error::DeviceBusy)?;
            if !device
                .partition_is_erased()
                .map_err(littlefs::Error::Device)?
            {
                return Err(littlefs::Error::NotFormatted);
            }
            let filesystem = littlefs::FileSystem::format(device)?;
            publish(filesystem);
            Ok(Startup::Formatted)
        }
        Err(error) => Err(error),
    }
}

/// Mounts the existing internal filesystem (独占分区 token, 替换全局实例)。
pub fn mount() -> Result<FileSystem, littlefs::Error<FlashError>> {
    let device = InternalFlash::take().ok_or(littlefs::Error::DeviceBusy)?;
    littlefs::FileSystem::mount(device)
}

/// Atomically formats the internal filesystem (独占分区 token)。
pub fn format() -> Result<FileSystem, littlefs::Error<FlashError>> {
    let device = InternalFlash::take().ok_or(littlefs::Error::DeviceBusy)?;
    littlefs::FileSystem::format(device)
}

/// 丢弃当前实例并重新挂载 (shell `mount` 命令与写错误恢复用)。
pub fn remount() -> Result<(), littlefs::Error<FlashError>> {
    let mut guard = FILESYSTEM
        .lock(Timeout::Forever)
        .map_err(|_| littlefs::Error::DeviceBusy)?;
    drop(guard.take()); // 释放旧实例的 InternalFlash token
    let device = InternalFlash::take().ok_or(littlefs::Error::DeviceBusy)?;
    let filesystem = littlefs::FileSystem::mount(device)?;
    *guard = Some(filesystem);
    Ok(())
}
