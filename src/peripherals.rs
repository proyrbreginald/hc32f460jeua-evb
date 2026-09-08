//! 唯一的片内外设所有权入口。

use core::sync::atomic::{AtomicBool, Ordering};

use crate::can::Can;
use crate::crc::Crc;
use crate::dma::Dma;
use crate::gpio::Gpio;

type ConsoleUart = crate::config::ConsoleUart;
type TxDma = Dma<{ crate::config::DMA_TX_UNIT }, { crate::config::DMA_TX_CHANNEL }>;
type CopyDma = Dma<{ crate::config::DMA_COPY_UNIT }, { crate::config::DMA_COPY_CHANNEL }>;

static TAKEN: AtomicBool = AtomicBool::new(false);

/// 芯片级外设所有权集合。
pub struct Peripherals {
    pub(crate) gpio: Gpio,
    pub(crate) console: ConsoleUart,
    pub(crate) can: Can,
    pub(crate) dma_tx: Option<TxDma>,
    pub(crate) dma_copy: Option<CopyDma>,
    pub(crate) crc: Crc,
    pub(crate) clocks: crate::clk::ClockController,
}

impl Peripherals {
    /// 获取全部板级外设。整个复位周期只允许成功一次。
    pub fn take() -> Option<Self> {
        TAKEN
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self {
                gpio: Gpio::take(),
                console: ConsoleUart::take(),
                can: Can::new(),
                dma_tx: TxDma::take(),
                dma_copy: CopyDma::take(),
                crc: Crc::take(),
                clocks: crate::clk::ClockController::new(),
            })
    }
}
