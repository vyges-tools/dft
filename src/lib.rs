// SPDX-License-Identifier: Apache-2.0
//! `vyges_dft` — scan insertion and stitching, so a chip can be tested after fabrication.
//!
//! Stages are added here one at a time, each named after the step it performs and kept in the order
//! the command runs them.

// The crate's own name inside it: the command-line entry ([`cli_main`]) is built into the
// library too, and its `vyges_dft::…` paths resolve here.
extern crate self as vyges_dft;

pub mod replace;
pub mod collect;
pub mod architect;
pub mod stitch;
pub mod design;

/// The `vyges-dft` command-line entry (the binary's `main`), built into the library too so a
/// suite binary can link every engine and run the one it is invoked as.
#[cfg(feature = "suite")]
#[doc(hidden)]
#[path = "main.rs"]
pub mod cli_main;
