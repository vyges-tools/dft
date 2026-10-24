// SPDX-License-Identifier: Apache-2.0
//! `vyges_dft` — scan insertion and stitching, so a chip can be tested after fabrication.
//!
//! Stages are added here one at a time, each named after the step it performs and kept in the order
//! the command runs them.

pub mod replace;
pub mod collect;
pub mod architect;
pub mod stitch;
pub mod design;
