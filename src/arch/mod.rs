//! CPU architecture primitives used by the runtime and platform services.
//!
//! The active backend is selected at compile time. Keeping these operations
//! behind this module prevents the RTOS and application layers from depending
//! directly on Cortex-M registers and inline assembly.

#[cfg(target_arch = "arm")]
mod cortex_m;

#[cfg(target_arch = "arm")]
pub(crate) use cortex_m::*;

#[cfg(not(target_arch = "arm"))]
compile_error!("no architecture backend is available for this target");
