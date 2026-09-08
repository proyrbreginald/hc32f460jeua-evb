#![allow(dead_code)]

//! 集成测试共用的内存 NOR 模型与故障注入器。
//!
//! `RamNor` 强制真实 NOR 约束（只能 1 -> 0、编程粒度对齐、擦除整块），并按
//! 可配置事件序号或字节序号中断操作，用于枚举掉电切点。克隆实例共享介质状态，
//! 因而可模拟复位后用一个新 `FileSystem` 重新挂载同一片 Flash。

use std::cell::RefCell;
use std::rc::Rc;

use littlefs::BlockDevice;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RamError {
    OutOfBounds,
    Unaligned,
    NotErased,
    PowerLoss,
    /// A block that has been armed as permanently dead rejected an operation.
    DeadBlock,
}

#[derive(Clone)]
struct State {
    bytes: Vec<u8>,
    programmed_words: Vec<bool>,
    block_size: u32,
    block_count: u32,
    fault_budget: Option<u64>,
    events: u64,
    byte_order: [usize; 4],
    erase_counts: Vec<u32>,
    dead_blocks: Vec<bool>,
}

impl State {
    fn event(&mut self) -> Result<(), RamError> {
        self.events += 1;
        if let Some(remaining) = &mut self.fault_budget {
            if *remaining == 0 {
                return Err(RamError::PowerLoss);
            }
            *remaining -= 1;
        }
        Ok(())
    }

    fn range(&self, block: u32, offset: u32, length: usize) -> Result<usize, RamError> {
        if block >= self.block_count || offset > self.block_size {
            return Err(RamError::OutOfBounds);
        }
        let length = u32::try_from(length).map_err(|_| RamError::OutOfBounds)?;
        let end = offset.checked_add(length).ok_or(RamError::OutOfBounds)?;
        if end > self.block_size {
            return Err(RamError::OutOfBounds);
        }
        let absolute = block
            .checked_mul(self.block_size)
            .and_then(|base| base.checked_add(offset))
            .ok_or(RamError::OutOfBounds)?;
        Ok(absolute as usize)
    }
}

/// In-memory NOR model with byte-granular torn program/erase injection.
#[derive(Clone)]
pub struct RamNor {
    state: Rc<RefCell<State>>,
}

impl RamNor {
    pub fn new(block_size: u32, block_count: u32) -> Self {
        assert!(block_size >= 64 && block_size.is_multiple_of(4));
        assert!(block_count >= 2);
        let byte_count = (block_size * block_count) as usize;
        Self {
            state: Rc::new(RefCell::new(State {
                bytes: vec![0xff; byte_count],
                programmed_words: vec![false; byte_count / 4],
                block_size,
                block_count,
                fault_budget: None,
                events: 0,
                byte_order: [0, 1, 2, 3],
                erase_counts: vec![0; block_count as usize],
                dead_blocks: vec![false; block_count as usize],
            })),
        }
    }

    /// Arm a permanently dead block: every erase/program on it returns
    /// [`RamError::DeadBlock`] (classified as `permanent_block_failure`).
    pub fn arm_dead_block(&self, block: u32) {
        self.state.borrow_mut().dead_blocks[block as usize] = true;
    }

    /// Heal a previously armed dead block (test scaffolding).
    pub fn heal_dead_block(&self, block: u32) {
        self.state.borrow_mut().dead_blocks[block as usize] = false;
    }

    /// Create an independent persistent image with fault counters reset.
    pub fn fork(&self) -> Self {
        let mut state = self.state.borrow().clone();
        state.fault_budget = None;
        state.events = 0;
        Self {
            state: Rc::new(RefCell::new(state)),
        }
    }

    pub fn arm_power_loss(&self, after_events: u64, byte_order: [usize; 4]) {
        let mut state = self.state.borrow_mut();
        state.fault_budget = Some(after_events);
        state.events = 0;
        state.byte_order = byte_order;
    }

    pub fn power_cycle(&self) {
        self.state.borrow_mut().fault_budget = None;
    }

    pub fn reset_events(&self) {
        self.state.borrow_mut().events = 0;
    }

    pub fn events(&self) -> u64 {
        self.state.borrow().events
    }

    pub fn erase_counts(&self) -> Vec<u32> {
        self.state.borrow().erase_counts.clone()
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.state.borrow().bytes.clone()
    }

    pub fn overwrite_raw(&self, absolute: usize, bytes: &[u8]) {
        let mut state = self.state.borrow_mut();
        state.bytes[absolute..absolute + bytes.len()].copy_from_slice(bytes);
    }

    pub fn block_size_value(&self) -> u32 {
        self.state.borrow().block_size
    }

    fn geometry_values(&self) -> (u32, u32) {
        let state = self.state.borrow();
        (state.block_size, state.block_count)
    }
}

impl BlockDevice for RamNor {
    type Error = RamError;

    fn block_size(&self) -> u32 {
        self.geometry_values().0
    }

    fn block_count(&self) -> u32 {
        self.geometry_values().1
    }

    fn read(&mut self, block: u32, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        let state = self.state.borrow();
        let absolute = state.range(block, offset, bytes.len())?;
        bytes.copy_from_slice(&state.bytes[absolute..absolute + bytes.len()]);
        Ok(())
    }

    fn program(&mut self, block: u32, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        if !offset.is_multiple_of(4) || !bytes.len().is_multiple_of(4) {
            return Err(RamError::Unaligned);
        }

        let mut state = self.state.borrow_mut();
        if state.dead_blocks[block as usize] {
            return Err(RamError::DeadBlock);
        }
        let absolute = state.range(block, offset, bytes.len())?;
        for word in 0..bytes.len() / 4 {
            let word_index = absolute / 4 + word;
            if state.programmed_words[word_index]
                || state.bytes[word_index * 4..word_index * 4 + 4]
                    .iter()
                    .any(|byte| *byte != 0xff)
            {
                return Err(RamError::NotErased);
            }
        }

        for word in 0..bytes.len() / 4 {
            let word_index = absolute / 4 + word;
            state.programmed_words[word_index] = true;
            let target = &bytes[word * 4..word * 4 + 4];
            let order = state.byte_order;
            for byte_index in order {
                state.event()?;
                let address = word_index * 4 + byte_index;
                state.bytes[address] &= target[byte_index];
            }
        }
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), Self::Error> {
        let mut state = self.state.borrow_mut();
        if state.dead_blocks[block as usize] {
            return Err(RamError::DeadBlock);
        }
        let block_size = state.block_size as usize;
        let absolute = state.range(block, 0, block_size)?;
        state.erase_counts[block as usize] += 1;

        let first_word = absolute / 4;
        let word_count = block_size / 4;
        state.programmed_words[first_word..first_word + word_count].fill(false);

        // 73 is coprime to the test block sizes, producing a nonsequential
        // partial erase image when power is cut.
        for index in 0..block_size {
            state.event()?;
            let permuted = (index * 73) % block_size;
            state.bytes[absolute + permuted] = 0xff;
        }
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Self::Error> {
        self.state.borrow_mut().event()
    }

    fn permanent_block_failure(&self, error: &Self::Error) -> bool {
        matches!(error, RamError::DeadBlock)
    }
}

pub fn read_all(
    fs: &mut littlefs::FileSystem<RamNor>,
    name: &str,
) -> Result<Vec<u8>, littlefs::Error<RamError>> {
    let size = fs.stat(name)?.size as usize;
    let mut bytes = vec![0; size];
    let read = fs.read(name, 0, &mut bytes)?;
    assert_eq!(read, size);
    Ok(bytes)
}
