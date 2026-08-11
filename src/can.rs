//! HC32F460 classic CAN 2.0B controller driver.
//!
//! The implementation follows DDL v3.3.0 `hc32_ll_can.c/h` and RM Rev1.71
//! chapter 30. CAN communication is clocked directly by XTAL; EXCLK only
//! clocks the controller logic and must be at least 1.5 times CANCLK.
//!
//! This module intentionally implements classic CAN only. The controller's
//! optional TTCAN extension is not configured here.

#![allow(dead_code)]

use core::sync::atomic::{AtomicU8, Ordering};

use crate::can_timing::BitTiming;

const CAN_BASE: usize = 0x4007_0400;
const PWC_BASE: usize = 0x4004_8000;

// Register offsets, checked against CM_CAN_TypeDef and the project SVD.
const RBUF: usize = 0x00;
const TBUF: usize = 0x50;
const CFG_STAT: usize = 0xA0;
const TCMD: usize = 0xA1;
const TCTRL: usize = 0xA2;
const RCTRL: usize = 0xA3;
const RTIE: usize = 0xA4;
const RTIF: usize = 0xA5;
const ERRINT: usize = 0xA6;
const LIMIT: usize = 0xA7;
const SBT: usize = 0xA8;
const EALCAP: usize = 0xB0;
const RECNT: usize = 0xB2;
const TECNT: usize = 0xB3;
const ACFCTRL: usize = 0xB4;
const ACFEN: usize = 0xB6;
const ACF: usize = 0xB8;

const CFG_STAT_RESET: u8 = 1 << 7;
const CFG_STAT_LBME: u8 = 1 << 6;
const CFG_STAT_LBMI: u8 = 1 << 5;
const CFG_STAT_TPSS: u8 = 1 << 4;
const CFG_STAT_TSSS: u8 = 1 << 3;

const TCMD_TBSEL: u8 = 1 << 7;
const TCMD_LOM: u8 = 1 << 6;
const TCMD_TPE: u8 = 1 << 4;
const TCMD_TPA: u8 = 1 << 3;
const TCMD_TSONE: u8 = 1 << 2;
const TCMD_TSALL: u8 = 1 << 1;
const TCMD_TSA: u8 = 1 << 0;

const TCTRL_TSNEXT: u8 = 1 << 6;
const TCTRL_TSMODE: u8 = 1 << 5;
const TCTRL_TSSTAT: u8 = 0x03;

const RCTRL_SACK: u8 = 1 << 7;
const RCTRL_ROM: u8 = 1 << 6;
const RCTRL_RREL: u8 = 1 << 4;
const RCTRL_RBALL: u8 = 1 << 3;
const RCTRL_RSTAT: u8 = 0x03;

const ERRINT_ENABLE_MASK: u8 = 0x2A;
const ERRINT_FLAG_MASK: u8 = 0x15;
const ERRINT_STATUS_MASK: u8 = 0xD5;
const ACFCTRL_SELMASK: u8 = 1 << 5;
const EXT_ID_MASK: u32 = 0x1FFF_FFFF;
const ACF_AIDE: u32 = 1 << 29;
const ACF_AIDEE: u32 = 1 << 30;
const IRQ_UNREGISTERED: u8 = u8::MAX;

/// CAN identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Id {
    Standard(u16),
    Extended(u32),
}

impl Id {
    pub const fn raw(self) -> u32 {
        match self {
            Self::Standard(id) => id as u32,
            Self::Extended(id) => id,
        }
    }

    pub const fn is_extended(self) -> bool {
        matches!(self, Self::Extended(_))
    }

    const fn valid(self) -> bool {
        match self {
            Self::Standard(id) => id <= 0x7FF,
            Self::Extended(id) => id <= EXT_ID_MASK,
        }
    }
}

/// A classic CAN transmit frame. Only the first `dlc` bytes are transmitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxFrame {
    pub id: Id,
    pub rtr: bool,
    pub dlc: u8,
    pub data: [u8; 8],
}

impl TxFrame {
    pub const fn data(id: Id, dlc: u8, data: [u8; 8]) -> Self {
        Self {
            id,
            rtr: false,
            dlc,
            data,
        }
    }

    pub const fn remote(id: Id, dlc: u8) -> Self {
        Self {
            id,
            rtr: true,
            dlc,
            data: [0; 8],
        }
    }

    fn validate(&self) -> Result<(), CanError> {
        if !self.id.valid() {
            return Err(match self.id {
                Id::Standard(_) => CanError::InvalidStandardId,
                Id::Extended(_) => CanError::InvalidExtendedId,
            });
        }
        if self.dlc > 8 {
            return Err(CanError::InvalidDlc);
        }
        Ok(())
    }
}

/// A frame read from the ten-slot receive FIFO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxFrame {
    pub id: Id,
    pub rtr: bool,
    pub dlc: u8,
    pub data: [u8; 8],
    pub self_tx: bool,
    pub error: ErrorKind,
    pub cycle_time: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterType {
    StandardAndExtended,
    StandardOnly,
    ExtendedOnly,
}

impl FilterType {
    const fn bits(self) -> u32 {
        match self {
            Self::StandardAndExtended => 0,
            Self::StandardOnly => ACF_AIDEE,
            Self::ExtendedOnly => ACF_AIDEE | ACF_AIDE,
        }
    }
}

/// One of the controller's eight acceptance filters.
///
/// A mask bit of one ignores the corresponding identifier bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Filter {
    pub id: u32,
    pub mask: u32,
    pub kind: FilterType,
}

impl Filter {
    pub const ACCEPT_ALL: Self = Self {
        id: 0,
        mask: EXT_ID_MASK,
        kind: FilterType::StandardAndExtended,
    };

    const fn valid(self) -> bool {
        self.id <= EXT_ID_MASK
            && self.mask <= EXT_ID_MASK
            && (!matches!(self.kind, FilterType::StandardOnly) || self.id <= 0x7FF)
    }
}

pub static ACCEPT_ALL_FILTER: [Filter; 1] = [Filter::ACCEPT_ALL];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkMode {
    Normal,
    Silent,
    InternalLoopback,
    ExternalLoopback,
    ExternalLoopbackSilent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxOverflowMode {
    OverwriteOldest,
    DiscardNewest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StbPriority {
    Fifo,
    LowestIdFirst,
}

/// Events whose flags and optional aggregate interrupt signal are enabled.
/// The hardware only sets many status flags when the matching enable is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interrupts(u16);

impl Interrupts {
    pub const NONE: Self = Self(0);
    pub const ERROR_INTERRUPT: Self = Self(1 << 1);
    pub const STB_TX: Self = Self(1 << 2);
    pub const PTB_TX: Self = Self(1 << 3);
    pub const RX_WARN: Self = Self(1 << 4);
    pub const RX_FULL: Self = Self(1 << 5);
    pub const RX_OVERRUN: Self = Self(1 << 6);
    pub const RX: Self = Self(1 << 7);
    pub const BUS_ERROR: Self = Self(1 << 9);
    pub const ARBITRATION_LOST: Self = Self(1 << 11);
    pub const ERROR_PASSIVE: Self = Self(1 << 13);
    pub const ALL: Self = Self(0x2AFE);

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    const fn rtie(self) -> u8 {
        self.0 as u8
    }

    const fn errint(self) -> u8 {
        (self.0 >> 8) as u8 & ERRINT_ENABLE_MASK
    }
}

/// Snapshot flags in the same packed layout as DDL `CAN_GetStatusValue`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(u32);

impl Status {
    pub const NONE: Self = Self(0);
    pub const BUS_OFF: Self = Self(1 << 0);
    pub const TX_ACTIVE: Self = Self(1 << 1);
    pub const RX_ACTIVE: Self = Self(1 << 2);
    pub const RX_OVERFLOW: Self = Self(1 << 5);
    pub const TX_BUFFER_FULL: Self = Self(1 << 8);
    pub const TX_ABORTED: Self = Self(1 << 16);
    pub const ERROR_INTERRUPT: Self = Self(1 << 17);
    pub const STB_TX: Self = Self(1 << 18);
    pub const PTB_TX: Self = Self(1 << 19);
    pub const RX_WARN: Self = Self(1 << 20);
    pub const RX_FULL: Self = Self(1 << 21);
    pub const RX_OVERRUN: Self = Self(1 << 22);
    pub const RX: Self = Self(1 << 23);
    pub const BUS_ERROR: Self = Self(1 << 24);
    pub const ARBITRATION_LOST: Self = Self(1 << 26);
    pub const ERROR_PASSIVE_CHANGED: Self = Self(1 << 28);
    pub const ERROR_PASSIVE_NODE: Self = Self(1 << 30);
    pub const ERROR_COUNT_WARNING: Self = Self(1 << 31);
    pub const TX_ERRORS: Self = Self(
        Self::BUS_OFF.0 | Self::ERROR_INTERRUPT.0 | Self::BUS_ERROR.0 | Self::ARBITRATION_LOST.0,
    );

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    None,
    Bit,
    Form,
    Stuff,
    Acknowledge,
    Crc,
    Other,
    Reserved,
}

impl ErrorKind {
    const fn from_raw(value: u8) -> Self {
        match value & 7 {
            0 => Self::None,
            1 => Self::Bit,
            2 => Self::Form,
            3 => Self::Stuff,
            4 => Self::Acknowledge,
            5 => Self::Crc,
            6 => Self::Other,
            _ => Self::Reserved,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorInfo {
    pub arbitration_lost_position: u8,
    pub kind: ErrorKind,
    pub rx_count: u8,
    pub tx_count: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferStatus {
    Empty,
    Low,
    High,
    Full,
}

impl BufferStatus {
    const fn from_raw(value: u8) -> Self {
        match value & 3 {
            0 => Self::Empty,
            1 => Self::Low,
            2 => Self::High,
            _ => Self::Full,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxBuffer {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StbTransmit {
    One,
    All,
}

/// Controller initialization parameters corresponding to `stc_can_init_t`.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub mode: WorkMode,
    pub bitrate: u32,
    pub sample_point_permille: u16,
    pub sjw: u8,
    pub max_bitrate_error_ppm: u32,
    pub filters: &'static [Filter],
    pub ptb_single_shot: bool,
    pub stb_single_shot: bool,
    pub stb_priority: StbPriority,
    pub rx_warn_limit: u8,
    pub error_warn_limit: u8,
    pub rx_all_frames: bool,
    pub rx_overflow: RxOverflowMode,
    /// Self-ACK only affects external loopback. Internal loopback ACKs itself.
    pub self_ack: bool,
    pub interrupts: Interrupts,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: WorkMode::Normal,
            bitrate: 500_000,
            sample_point_permille: 750,
            sjw: 2,
            max_bitrate_error_ppm: 10_000,
            filters: &ACCEPT_ALL_FILTER,
            ptb_single_shot: false,
            stb_single_shot: false,
            stb_priority: StbPriority::Fifo,
            rx_warn_limit: 10,
            error_warn_limit: 7,
            rx_all_frames: false,
            rx_overflow: RxOverflowMode::DiscardNewest,
            self_ack: false,
            interrupts: Interrupts::ALL,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanError {
    BitTimingUnsupported,
    BitrateTooHigh,
    XtalNotReady,
    ClockConstraint,
    InvalidFilterCount,
    InvalidFilter,
    InvalidRxWarnLimit,
    InvalidErrorWarnLimit,
    InvalidStandardId,
    InvalidExtendedId,
    InvalidDlc,
    InLocalReset,
    SilentMode,
    TxBusy,
    TxBufferFull,
}

/// The board-owned handle for the chip's single CAN instance.
pub struct Can {
    irq_line: AtomicU8,
}

impl Can {
    pub(crate) const fn new() -> Self {
        Self {
            irq_line: AtomicU8::new(IRQ_UNREGISTERED),
        }
    }

    /// Initialize classic CAN and return the selected, effective bit timing.
    ///
    /// Initialization enters local reset, which clears the hardware RX and STB
    /// FIFOs. The caller must own the controller lifecycle and quiesce any
    /// application/IRQ consumer before calling this method.
    pub fn init(&self, cfg: Config) -> Result<BitTiming, CanError> {
        validate_config(&cfg)?;
        let timing = crate::can_timing::calculate(
            crate::clk::XTAL_HZ,
            cfg.bitrate,
            cfg.sample_point_permille as u32,
            cfg.sjw as u32,
            cfg.max_bitrate_error_ppm,
        )
        .ok_or(CanError::BitTimingUnsupported)?;

        if u64::from(crate::clk::exclk_hz()) * 2 < u64::from(crate::clk::XTAL_HZ) * 3 {
            return Err(CanError::ClockConstraint);
        }
        if !crate::clk::xtal_stable() {
            let xtal_was_enabled = crate::clk::xtal_enabled();
            if crate::clk::xtal_init().is_err() {
                if !xtal_was_enabled {
                    let _ = crate::clk::xtal_cmd(false);
                }
                return Err(CanError::XtalNotReady);
            }
        }

        // RESET does not clear interrupt enables. Keep the complete register
        // transition atomic with respect to an already-routed CAN ISR.
        crate::critical_section::with(|_| {
            clock_cmd(true);
            modify8(CFG_STAT, |v| v | CFG_STAT_RESET);
            write32(SBT, timing.register_value());

            modify8(TCTRL, |v| match cfg.stb_priority {
                StbPriority::Fifo => v & !TCTRL_TSMODE,
                StbPriority::LowestIdFirst => v | TCTRL_TSMODE,
            });

            for (index, filter) in cfg.filters.iter().enumerate() {
                write8(ACFCTRL, index as u8);
                write32(ACF, filter.id);
                write8(ACFCTRL, index as u8 | ACFCTRL_SELMASK);
                write32(ACF, filter.mask | filter.kind.bits());
            }

            // Work mode fields are only changed with RESET clear. Replace every
            // mode bit on each init so switching from silent/loopback cannot leak.
            modify8(CFG_STAT, |v| v & !CFG_STAT_RESET);
            let loopback = match cfg.mode {
                WorkMode::InternalLoopback => CFG_STAT_LBMI,
                WorkMode::ExternalLoopback | WorkMode::ExternalLoopbackSilent => CFG_STAT_LBME,
                WorkMode::Normal | WorkMode::Silent => 0,
            };
            modify8(CFG_STAT, |v| {
                (v & !(CFG_STAT_LBMI | CFG_STAT_LBME)) | loopback
            });
            let listen_only = matches!(
                cfg.mode,
                WorkMode::Silent | WorkMode::ExternalLoopbackSilent
            );
            modify8(TCMD, |v| {
                if listen_only {
                    v | TCMD_LOM
                } else {
                    v & !TCMD_LOM
                }
            });

            let single_shot = (u8::from(cfg.ptb_single_shot) * CFG_STAT_TPSS)
                | (u8::from(cfg.stb_single_shot) * CFG_STAT_TSSS);
            modify8(CFG_STAT, |v| {
                (v & !(CFG_STAT_TPSS | CFG_STAT_TSSS)) | single_shot
            });
            write8(LIMIT, (cfg.rx_warn_limit << 4) | cfg.error_warn_limit);

            let mut rctrl = 0;
            if cfg.rx_all_frames {
                rctrl |= RCTRL_RBALL;
            }
            if cfg.rx_overflow == RxOverflowMode::DiscardNewest {
                rctrl |= RCTRL_ROM;
            }
            if cfg.self_ack {
                rctrl |= RCTRL_SACK;
            }
            write8(RCTRL, rctrl);
            write8(ACFEN, ((1u16 << cfg.filters.len()) - 1) as u8);

            // W1C stale flags while publishing exactly the requested enables.
            write8(RTIF, 0xFF);
            write8(ERRINT, cfg.interrupts.errint() | ERRINT_FLAG_MASK);
            write8(RTIE, cfg.interrupts.rtie());
        });
        Ok(timing)
    }

    /// Stop CAN, clear event enables and gate its controller clock.
    ///
    /// This is a lifecycle operation with the same exclusive-access
    /// requirement as [`Self::init`]. It does not unregister an INTC route.
    pub fn deinit(&self) {
        crate::critical_section::with(|_| {
            clock_cmd(true);
            modify8(CFG_STAT, |v| v | CFG_STAT_RESET);
            write8(RTIE, 0);
            write8(ERRINT, ERRINT_FLAG_MASK);
            write8(RTIF, 0xFF);
            write8(ACFEN, 0);
            let irq_line = self.irq_line.load(Ordering::Relaxed);
            if irq_line != IRQ_UNREGISTERED {
                crate::intc::clear_pend(crate::intc::Line::new(irq_line));
            }
            clock_cmd(false);
        });
    }

    pub fn enter_local_reset(&self) {
        crate::critical_section::with(|_| modify8(CFG_STAT, |v| v | CFG_STAT_RESET));
    }

    pub fn exit_local_reset(&self) {
        crate::critical_section::with(|_| modify8(CFG_STAT, |v| v & !CFG_STAT_RESET));
    }

    pub fn in_local_reset(&self) -> bool {
        read8(CFG_STAT) & CFG_STAT_RESET != 0
    }

    /// Fill and request the primary transmit buffer without blocking.
    pub fn try_transmit_ptb(&self, frame: &TxFrame) -> Result<(), CanError> {
        frame.validate()?;
        crate::critical_section::with(|_| {
            self.ensure_can_transmit()?;
            if read8(TCMD) & TCMD_TPE != 0 {
                return Err(CanError::TxBusy);
            }
            self.clear_status(Status::PTB_TX);
            modify8(TCMD, |v| v & !TCMD_TBSEL);
            write_tx_frame(frame);
            modify8(TCMD, |v| v | TCMD_TPE);
            Ok(())
        })
    }

    /// Add one frame to the four-slot secondary transmit FIFO.
    pub fn enqueue_stb(&self, frame: &TxFrame) -> Result<(), CanError> {
        frame.validate()?;
        crate::critical_section::with(|_| {
            self.ensure_can_transmit()?;
            if read8(TCMD) & (TCMD_TSONE | TCMD_TSALL) != 0 {
                return Err(CanError::TxBusy);
            }
            if read8(RTIE) & 1 != 0 || read8(TCTRL) & TCTRL_TSSTAT == TCTRL_TSSTAT {
                return Err(CanError::TxBufferFull);
            }
            modify8(TCMD, |v| v | TCMD_TBSEL);
            write_tx_frame(frame);
            modify8(TCTRL, |v| v | TCTRL_TSNEXT);
            Ok(())
        })
    }

    pub fn start_stb(&self, request: StbTransmit) -> Result<(), CanError> {
        crate::critical_section::with(|_| {
            self.ensure_can_transmit()?;
            if read8(TCMD) & (TCMD_TSONE | TCMD_TSALL) != 0 {
                return Err(CanError::TxBusy);
            }
            let bit = match request {
                StbTransmit::One => TCMD_TSONE,
                StbTransmit::All => TCMD_TSALL,
            };
            self.clear_status(Status::STB_TX);
            modify8(TCMD, |v| v | bit);
            Ok(())
        })
    }

    pub fn abort(&self, buffer: TxBuffer) {
        let bit = match buffer {
            TxBuffer::Primary => TCMD_TPA,
            TxBuffer::Secondary => TCMD_TSA,
        };
        crate::critical_section::with(|_| modify8(TCMD, |v| v | bit));
    }

    pub fn tx_pending(&self, buffer: TxBuffer) -> bool {
        let bits = match buffer {
            TxBuffer::Primary => TCMD_TPE,
            TxBuffer::Secondary => TCMD_TSONE | TCMD_TSALL,
        };
        read8(TCMD) & bits != 0
    }

    pub fn tx_buffer_status(&self) -> BufferStatus {
        BufferStatus::from_raw(read8(TCTRL) & TCTRL_TSSTAT)
    }

    /// Read and release the oldest receive FIFO slot.
    pub fn try_receive(&self) -> Option<RxFrame> {
        crate::critical_section::with(|_| {
            if read8(RCTRL) & RCTRL_RSTAT == 0 {
                return None;
            }
            let raw_id = read32(RBUF);
            let ctrl = read32(RBUF + 4);
            let extended = ctrl & (1 << 7) != 0;
            let rtr = ctrl & (1 << 6) != 0;
            let dlc = (ctrl as u8 & 0x0F).min(8);
            let id = if extended {
                Id::Extended(raw_id & EXT_ID_MASK)
            } else {
                Id::Standard((raw_id & 0x7FF) as u16)
            };

            let mut data = [0; 8];
            if dlc != 0 {
                let word = read32(RBUF + 8).to_le_bytes();
                let count = usize::from(dlc.min(4));
                data[..count].copy_from_slice(&word[..count]);
            }
            if dlc > 4 {
                let word = read32(RBUF + 12).to_le_bytes();
                data[4..usize::from(dlc)].copy_from_slice(&word[..usize::from(dlc - 4)]);
            }
            modify8(RCTRL, |v| v | RCTRL_RREL);

            Some(RxFrame {
                id,
                rtr,
                dlc,
                data,
                self_tx: ctrl & (1 << 12) != 0,
                error: ErrorKind::from_raw((ctrl >> 13) as u8),
                cycle_time: (ctrl >> 16) as u16,
            })
        })
    }

    pub fn rx_buffer_status(&self) -> BufferStatus {
        BufferStatus::from_raw(read8(RCTRL) & RCTRL_RSTAT)
    }

    pub fn status(&self) -> Status {
        crate::critical_section::with(|_| {
            let cfg = u32::from(read8(CFG_STAT)) & 0x07;
            let overflow = u32::from(read8(RCTRL)) & 0x20;
            let tx_full = u32::from(read8(RTIE)) << 8 & Status::TX_BUFFER_FULL.0;
            let events = u32::from(read8(RTIF)) << 16;
            let errors = u32::from(read8(ERRINT) & ERRINT_STATUS_MASK) << 24;
            Status(cfg | overflow | tx_full | events | errors)
        })
    }

    /// Clear W1C event/error flags while preserving ERRINT enable bits.
    pub fn clear_status(&self, flags: Status) {
        crate::critical_section::with(|_| {
            let rtif = (flags.0 >> 16) as u8;
            if rtif != 0 {
                write8(RTIF, rtif);
            }
            let err_flags = (flags.0 >> 24) as u8 & ERRINT_FLAG_MASK;
            if err_flags != 0 {
                let enables = read8(ERRINT) & ERRINT_ENABLE_MASK;
                write8(ERRINT, enables | err_flags);
            }
        });
    }

    pub fn error_info(&self) -> ErrorInfo {
        crate::critical_section::with(|_| {
            let captured = read8(EALCAP);
            ErrorInfo {
                arbitration_lost_position: captured & 0x1F,
                kind: ErrorKind::from_raw(captured >> 5),
                rx_count: read8(RECNT),
                tx_count: read8(TECNT),
            }
        })
    }

    pub fn enable_interrupts(&self, interrupts: Interrupts) {
        crate::critical_section::with(|_| {
            write8(RTIE, read8(RTIE) | interrupts.rtie());
            let enables = read8(ERRINT) & ERRINT_ENABLE_MASK;
            write8(ERRINT, enables | interrupts.errint());
        });
    }

    pub fn disable_interrupts(&self, interrupts: Interrupts) {
        crate::critical_section::with(|_| {
            write8(RTIE, read8(RTIE) & !interrupts.rtie());
            let enables = read8(ERRINT) & ERRINT_ENABLE_MASK & !interrupts.errint();
            write8(ERRINT, enables);
        });
    }

    /// Route the controller's aggregate event to one NVIC line.
    pub fn register_irq(
        &self,
        line: crate::intc::Line,
        priority: u8,
        handler: crate::intc::Handler,
    ) -> Result<(), crate::intc::IrqError> {
        crate::critical_section::with(|_| {
            let registered = self.irq_line.load(Ordering::Relaxed);
            if registered != IRQ_UNREGISTERED && registered != line.n() {
                return Err(crate::intc::IrqError::LineTaken);
            }
            crate::intc::register(crate::intc::src::CAN_INT, line, priority, handler)?;
            self.irq_line.store(line.n(), Ordering::Relaxed);
            Ok(())
        })
    }

    /// Disable and remove a CAN aggregate interrupt route registered on `line`.
    pub fn unregister_irq(&self, line: crate::intc::Line) {
        crate::critical_section::with(|_| {
            if self.irq_line.load(Ordering::Relaxed) == line.n() {
                crate::intc::unregister(line);
                self.irq_line.store(IRQ_UNREGISTERED, Ordering::Relaxed);
            }
        });
    }

    /// Whether this handle currently owns an aggregate CAN interrupt route.
    pub fn irq_registered(&self) -> bool {
        self.irq_line.load(Ordering::Relaxed) != IRQ_UNREGISTERED
    }

    fn ensure_can_transmit(&self) -> Result<(), CanError> {
        if self.in_local_reset() {
            Err(CanError::InLocalReset)
        } else if read8(TCMD) & TCMD_LOM != 0 && read8(CFG_STAT) & CFG_STAT_LBME == 0 {
            Err(CanError::SilentMode)
        } else {
            Ok(())
        }
    }
}

fn validate_config(cfg: &Config) -> Result<(), CanError> {
    if cfg.bitrate > crate::can_timing::MAX_CLASSIC_BITRATE {
        return Err(CanError::BitrateTooHigh);
    }
    if cfg.filters.is_empty() || cfg.filters.len() > 8 {
        return Err(CanError::InvalidFilterCount);
    }
    if cfg.filters.iter().any(|filter| !filter.valid()) {
        return Err(CanError::InvalidFilter);
    }
    if !(1..=10).contains(&cfg.rx_warn_limit) {
        return Err(CanError::InvalidRxWarnLimit);
    }
    if cfg.error_warn_limit > 15 {
        return Err(CanError::InvalidErrorWarnLimit);
    }
    Ok(())
}

fn write_tx_frame(frame: &TxFrame) {
    let ctrl =
        u32::from(frame.dlc) | u32::from(frame.rtr) << 6 | u32::from(frame.id.is_extended()) << 7;
    write32(TBUF, frame.id.raw());
    write32(TBUF + 4, ctrl);
    if !frame.rtr {
        write32(
            TBUF + 8,
            u32::from_le_bytes([frame.data[0], frame.data[1], frame.data[2], frame.data[3]]),
        );
        write32(
            TBUF + 12,
            u32::from_le_bytes([frame.data[4], frame.data[5], frame.data[6], frame.data[7]]),
        );
    }
}

fn clock_cmd(enable: bool) {
    crate::critical_section::with(|_| {
        let address = (PWC_BASE + 0x04) as *mut u32;
        let value = unsafe { core::ptr::read_volatile(address) };
        let next = if enable { value & !1 } else { value | 1 };
        unsafe { core::ptr::write_volatile(address, next) };
    });
}

fn read8(offset: usize) -> u8 {
    unsafe { core::ptr::read_volatile((CAN_BASE + offset) as *const u8) }
}

fn write8(offset: usize, value: u8) {
    unsafe { core::ptr::write_volatile((CAN_BASE + offset) as *mut u8, value) };
}

fn modify8(offset: usize, f: impl FnOnce(u8) -> u8) {
    write8(offset, f(read8(offset)));
}

fn read32(offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((CAN_BASE + offset) as *const u32) }
}

fn write32(offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile((CAN_BASE + offset) as *mut u32, value) };
}
