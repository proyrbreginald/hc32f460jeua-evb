#![no_std]
#![allow(dead_code)]

//! Host-testable, hardware-independent firmware algorithms.

extern crate alloc;

pub mod can_timing;
pub mod heap_layout;
pub mod logfile_core;
pub mod logring;
pub mod soak_report_core;
pub mod zmodem;
