// SPDX-License-Identifier: Apache-2.0
//! Scan cell collection: the instances a scan architecture may chain, each with its clock domain.
//!
//! One function per reference function, in call order:
//! - [`collect_scan_cells`] — the block's instances in database order (a `dont_touch` one skipped),
//!   each through [`scan_cell_factory`]; then all sorted by name;
//! - [`scan_cell_factory`] — [`identify_cell`], then [`create_one_bit_cell`] for a one-bit cell;
//! - [`create_one_bit_cell`] — [`find_one_bit_cell_clock_domain`], then [`get_test_cell`], then
//!   the warnings, in that order;
//! - [`find_one_bit_cell_clock_domain`] — the first CLOCK terminal, the first clock at it, then
//!   [`get_clock_domain_from_clock`].
//!
//! The design is read through [`CollectDesign`], so every rule here is testable without a database.

use vyges_sta::func_expr::FuncExpr;
use vyges_sta::liberty::{Cell, Library};

use crate::architect::{ClockDomain, ClockEdge, ScanCell};
use crate::replace::is_scan_cell;

/// What collection reads about the design.
pub trait CollectDesign {
    /// The block's instances, in `getInsts()` order.
    fn insts(&self) -> Vec<String>;
    fn inst_is_do_not_touch(&self, inst: &str) -> bool;
    fn inst_master(&self, inst: &str) -> String;
    /// `dbMaster::isBlock`.
    fn master_is_block(&self, master: &str) -> bool;
    /// The instance's terminals in `getITerms()` order, each with its signal type as odb names it.
    fn inst_iterm_sig_types(&self, inst: &str) -> Vec<(String, String)>;
    /// `dbInst::isPlaced`.
    fn inst_is_placed(&self, inst: &str) -> bool;
    /// `dbInst::getOrigin`.
    fn inst_origin(&self, inst: &str) -> (i32, i32);
    /// `Sta::clocks(pin)`: the clocks whose clock paths reach `inst/pin`, in clock INDEX order (the
    /// order they were created) — `ClockSet` is ordered by index, not by name.
    fn clocks(&self, inst: &str, pin: &str) -> Vec<String>;
}

/// Why collection refuses rather than answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectError {
    /// A sequential's clock expression that is missing or does not parse: the reference reads its
    /// operands without a check.
    BadClockExpr { cell: String, error: String },
}

/// The collected cells, sorted by name, and the warnings in the order they were logged.
pub fn collect_scan_cells(d: &dyn CollectDesign, lib: &Library) -> Result<(Vec<ScanCell>, Vec<String>), CollectError> {
    let mut cells = Vec::new();
    let mut log = Vec::new();
    for inst in d.insts() {
        if d.inst_is_do_not_touch(&inst) {
            continue;
        }
        if let Some(cell) = scan_cell_factory(d, lib, &inst, &mut log)? {
            cells.push(cell);
        }
    }
    // `std::ranges::sort` by name: unstable, but instance names in one block are unique.
    cells.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    Ok((cells, log))
}

/// `liberty_cell` of the master, if any (the network's: the first library that has the cell).
fn get_liberty_cell<'a>(lib: &'a Library, master: &str) -> Option<&'a Cell> {
    lib.cells.get(master)
}

/// A one-bit cell: a Liberty cell that is sequential, on a master that is not a block.
pub fn identify_cell(d: &dyn CollectDesign, lib: &Library, inst: &str) -> bool {
    let master = d.inst_master(inst);
    get_liberty_cell(lib, &master).is_some_and(Cell::is_sequential) && !d.master_is_block(&master)
}

pub fn scan_cell_factory(d: &dyn CollectDesign, lib: &Library, inst: &str, log: &mut Vec<String>) -> Result<Option<ScanCell>, CollectError> {
    if !identify_cell(d, lib, inst) {
        return Ok(None);
    }
    create_one_bit_cell(d, lib, inst, log)
}

/// The clock domain first (it may warn), then the test cell; a cell with no clock domain is
/// warned about before one with no test cell.
pub fn create_one_bit_cell(d: &dyn CollectDesign, lib: &Library, inst: &str, log: &mut Vec<String>) -> Result<Option<ScanCell>, CollectError> {
    let domain = find_one_bit_cell_clock_domain(d, lib, inst, log)?;
    let has_test_cell = get_test_cell(d, lib, inst, log);
    let Some(domain) = domain else {
        log.push(format!("[WARNING DFT-0005] Cell '{inst}' doesn't have a valid clock connected. Can't create a scan cell"));
        return Ok(None);
    };
    if !has_test_cell {
        log.push(format!("[WARNING DFT-0007] Cell '{inst}' is not a scan cell. Can't use it for scan architect"));
        return Ok(None);
    }
    let origin = d.inst_is_placed(inst).then(|| d.inst_origin(inst));
    Ok(Some(ScanCell { name: inst.to_string(), domain, bits: 1, origin }))
}

/// The terminals whose signal type is CLOCK, in terminal order.
pub fn get_clock_pin(d: &dyn CollectDesign, inst: &str) -> Vec<String> {
    d.inst_iterm_sig_types(inst).into_iter().filter(|(_, sig)| sig == "CLOCK").map(|(t, _)| t).collect()
}

/// The FIRST clock terminal (only one is expected), and the first clock (lowest index) at it.
pub fn find_one_bit_cell_clock_domain(d: &dyn CollectDesign, lib: &Library, inst: &str, log: &mut Vec<String>) -> Result<Option<ClockDomain>, CollectError> {
    let Some(pin) = get_clock_pin(d, inst).into_iter().next() else {
        log.push(format!("[WARNING DFT-0049] Can't find a clock pin for cell '{inst}'"));
        return Ok(None);
    };
    let Some(clock) = d.clocks(inst, &pin).into_iter().next() else { return Ok(None) };
    let master = d.inst_master(inst);
    // Reached only for a one-bit cell, which has a Liberty cell.
    let cell = get_liberty_cell(lib, &master).expect("identify_cell found a Liberty cell");
    get_clock_domain_from_clock(cell, &clock).map(Some)
}

/// The edge is the LAST sequential's: falling when its clock expression has a left operand and no
/// right one (a NOT), rising otherwise — a plain port (neither operand) is rising.
pub fn get_clock_domain_from_clock(cell: &Cell, clock: &str) -> Result<ClockDomain, CollectError> {
    let mut edge = ClockEdge::Rising;
    for (_, _, clock_expr) in &cell.sequentials {
        let expr = FuncExpr::parse(clock_expr).map_err(|error| CollectError::BadClockExpr { cell: cell.name.clone(), error })?;
        edge = match expr {
            FuncExpr::Not(_) => ClockEdge::Falling,
            _ => ClockEdge::Rising,
        };
    }
    Ok(ClockDomain::new(clock, edge))
}

/// A test cell with a scan-in and a scan-enable port. (The reference warns DFT-0011 for a master
/// with no Liberty cell here; collection reaches this only for a cell that has one.)
pub fn get_test_cell(d: &dyn CollectDesign, lib: &Library, inst: &str, log: &mut Vec<String>) -> bool {
    let master = d.inst_master(inst);
    match get_liberty_cell(lib, &master) {
        None => {
            log.push(format!("[WARNING DFT-0011] Cell master '{master}' has no lib info. Can't find scan cell"));
            false
        }
        Some(cell) => is_scan_cell(cell),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use vyges_sta::liberty_parse::parse;

    const LIB: &str = r#"library (t) { time_unit : "1ns"; capacitive_load_unit (1, pf);
      cell (dff) { ff (IQ, IQN) { clocked_on : "CLK"; next_state : "D"; }
        pin (CLK) { direction : input; clock : true; } pin (D) { direction : input; }
        pin (Q) { direction : output; function : "IQ"; } }
      cell (sdff) { ff (IQ, IQN) { clocked_on : "CLK"; next_state : "(D&!SCE)|(SCD&SCE)"; }
        pin (CLK) { direction : input; clock : true; } pin (D) { direction : input; }
        pin (SCD) { direction : input; } pin (SCE) { direction : input; }
        pin (Q) { direction : output; function : "IQ"; }
        test_cell () { ff (IQ, IQN) { clocked_on : "CLK"; next_state : "D"; }
          pin (CLK) { direction : input; } pin (D) { direction : input; }
          pin (SCD) { direction : input; signal_type : test_scan_in; }
          pin (SCE) { direction : input; signal_type : test_scan_enable; }
          pin (Q) { direction : output; function : "IQ"; signal_type : test_scan_out; } } }
      cell (sdffn) { ff (IQ, IQN) { clocked_on : "!CLK_N"; next_state : "(D&!SCE)|(SCD&SCE)"; }
        pin (CLK_N) { direction : input; clock : true; } pin (D) { direction : input; }
        pin (SCD) { direction : input; } pin (SCE) { direction : input; }
        pin (Q) { direction : output; function : "IQ"; }
        test_cell () { ff (IQ, IQN) { clocked_on : "!CLK_N"; next_state : "D"; }
          pin (CLK_N) { direction : input; } pin (D) { direction : input; }
          pin (SCD) { direction : input; signal_type : test_scan_in; }
          pin (SCE) { direction : input; signal_type : test_scan_enable; }
          pin (Q) { direction : output; function : "IQ"; signal_type : test_scan_out; } } }
      cell (buf) { pin (A) { direction : input; } pin (X) { direction : output; function : "A"; } }
    }"#;

    fn lib() -> Library {
        Library::read(&parse(LIB).unwrap()).unwrap()
    }

    #[derive(Default)]
    struct D {
        insts: Vec<(String, String)>,
        sig: HashMap<String, Vec<(String, String)>>,
        clocks: HashMap<String, Vec<String>>,
        dont_touch: Vec<String>,
        blocks: Vec<String>,
        placed: HashMap<String, (i32, i32)>,
    }

    impl D {
        fn inst(mut self, name: &str, master: &str, clock_pin: Option<&str>, clocks: &[&str]) -> D {
            self.insts.push((name.into(), master.into()));
            let mut terms = vec![("D".to_string(), "SIGNAL".to_string())];
            if let Some(p) = clock_pin {
                terms.insert(0, (p.to_string(), "CLOCK".to_string()));
                self.clocks.insert(format!("{name}/{p}"), clocks.iter().map(|c| c.to_string()).collect());
            }
            self.sig.insert(name.into(), terms);
            self
        }
    }

    impl CollectDesign for D {
        fn insts(&self) -> Vec<String> {
            self.insts.iter().map(|(n, _)| n.clone()).collect()
        }
        fn inst_is_do_not_touch(&self, inst: &str) -> bool {
            self.dont_touch.iter().any(|i| i == inst)
        }
        fn inst_master(&self, inst: &str) -> String {
            self.insts.iter().find(|(n, _)| n == inst).unwrap().1.clone()
        }
        fn master_is_block(&self, master: &str) -> bool {
            self.blocks.iter().any(|m| m == master)
        }
        fn inst_iterm_sig_types(&self, inst: &str) -> Vec<(String, String)> {
            self.sig[inst].clone()
        }
        fn inst_is_placed(&self, inst: &str) -> bool {
            self.placed.contains_key(inst)
        }
        fn inst_origin(&self, inst: &str) -> (i32, i32) {
            self.placed[inst]
        }
        fn clocks(&self, inst: &str, pin: &str) -> Vec<String> {
            self.clocks.get(&format!("{inst}/{pin}")).cloned().unwrap_or_default()
        }
    }

    fn names(cells: &[ScanCell]) -> Vec<(&str, &str, ClockEdge)> {
        cells.iter().map(|c| (c.name.as_str(), c.domain.clock.as_str(), c.domain.edge)).collect()
    }

    #[test]
    fn cells_are_sorted_by_name_with_the_first_clock_and_the_edge_of_the_clock_expression() {
        // Rules (CollectScanCells / GetClock / GetClockDomainFromClock): database order is
        // collected, then sorted by name bytewise ('B' < 'a'); the clock is the FIRST of the set at
        // the clock pin (index order, so "z_first" though it sorts last by name); a NOT clock
        // expression is falling.
        let d = D::default()
            .inst("b", "sdff", Some("CLK"), &["z_first", "a_second"])
            .inst("a", "sdffn", Some("CLK_N"), &["c1"])
            .inst("B", "sdff", Some("CLK"), &["c1"])
            .inst("u", "buf", None, &[]);
        let (cells, log) = collect_scan_cells(&d, &lib()).unwrap();
        assert_eq!(names(&cells), vec![("B", "c1", ClockEdge::Rising), ("a", "c1", ClockEdge::Falling), ("b", "z_first", ClockEdge::Rising)]);
        assert!(log.is_empty());
        assert!(cells.iter().all(|c| c.bits == 1 && c.origin.is_none()));
    }

    #[test]
    fn warnings_come_in_reference_order_and_skip_the_cell() {
        // Rules (CreateOneBitCell): no clock pin warns 49 THEN 5; a clock pin with no clock warns 5
        // only; a non-scan flop with a clock warns 7; with neither, 5 and not 7. dont_touch and
        // block masters are skipped silently.
        let mut d = D::default()
            .inst("no_pin", "sdff", None, &[])
            .inst("no_clock", "sdff", Some("CLK"), &[])
            .inst("plain", "dff", Some("CLK"), &["c"])
            .inst("plain_no_clock", "dff", Some("CLK"), &[])
            .inst("touched", "sdff", None, &[])
            .inst("macro", "sdff", None, &[]);
        d.dont_touch.push("touched".into());
        d.blocks.push("sdff".into());
        // `macro` shares the master `sdff`, now a block: every sdff instance is skipped.
        let (cells, log) = collect_scan_cells(&d, &lib()).unwrap();
        assert!(cells.is_empty());
        assert_eq!(
            log,
            vec![
                "[WARNING DFT-0007] Cell 'plain' is not a scan cell. Can't use it for scan architect",
                "[WARNING DFT-0005] Cell 'plain_no_clock' doesn't have a valid clock connected. Can't create a scan cell",
            ]
        );
        d.blocks.clear();
        let (cells, log) = collect_scan_cells(&d, &lib()).unwrap();
        assert!(cells.is_empty());
        assert_eq!(
            log,
            vec![
                "[WARNING DFT-0049] Can't find a clock pin for cell 'no_pin'",
                "[WARNING DFT-0005] Cell 'no_pin' doesn't have a valid clock connected. Can't create a scan cell",
                "[WARNING DFT-0005] Cell 'no_clock' doesn't have a valid clock connected. Can't create a scan cell",
                "[WARNING DFT-0007] Cell 'plain' is not a scan cell. Can't use it for scan architect",
                "[WARNING DFT-0005] Cell 'plain_no_clock' doesn't have a valid clock connected. Can't create a scan cell",
                "[WARNING DFT-0049] Can't find a clock pin for cell 'macro'",
                "[WARNING DFT-0005] Cell 'macro' doesn't have a valid clock connected. Can't create a scan cell",
            ]
        );
    }

    #[test]
    fn a_placed_cell_carries_its_origin() {
        let mut d = D::default().inst("p", "sdff", Some("CLK"), &["c"]).inst("q", "sdff", Some("CLK"), &["c"]);
        d.placed.insert("p".into(), (5, 7));
        let (cells, _) = collect_scan_cells(&d, &lib()).unwrap();
        assert_eq!(cells.iter().map(|c| c.origin).collect::<Vec<_>>(), vec![Some((5, 7)), None]);
    }

    #[test]
    fn the_last_sequential_decides_the_edge() {
        // Rule: the loop over sequentials keeps overwriting the edge; the last one wins.
        let lib = Library::read(
            &parse(
                r#"library (t) { cell (two) { ff (A, AN) { clocked_on : "!C"; next_state : "D"; }
                   latch (B, BN) { enable : "C"; data_in : "D"; }
                   pin (C) { direction : input; } pin (D) { direction : input; } } }"#,
            )
            .unwrap(),
        )
        .unwrap();
        let cell = &lib.cells["two"];
        assert_eq!(get_clock_domain_from_clock(cell, "k").unwrap().edge, ClockEdge::Rising);
    }
}
