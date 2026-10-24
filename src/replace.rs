// SPDX-License-Identifier: Apache-2.0
//! Scan replacement, the library half: which library cells are scan cells, and for each non-scan
//! flop the scan cell that can take its place, with the port it maps each of its ports to.
//!
//! The design half (which instances are replaced, and the database edits) is separate; this one
//! reads only the Liberty library, in the order the database lists its masters.

use std::collections::BTreeMap;

use vyges_sta::func_expr::FuncExpr;
use vyges_sta::liberty::{Cell, Direction, Library, Port, ScanSignalType};

/// A scan cell that can replace a non-scan cell: its name and, per non-scan port (power and
/// ground included), the scan port it maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanCandidate {
    pub scan_cell: String,
    pub port_mapping: BTreeMap<String, String>,
}

/// Why the library half refuses rather than answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceError {
    /// A `pg_pin` with no or an unknown `pg_type`: the reference counts it among the ordinary
    /// ports, in file order between the pins, which this model does not keep.
    UntypedPgPin { cell: String, pin: String },
    /// A port function that does not parse.
    BadFunction { cell: String, port: String, error: String },
    /// Two scan cells are equally close to a non-scan cell. The reference sorts its candidates
    /// with an UNSTABLE sort from a set ordered by address, so which one it takes is not
    /// determined by the library; refused rather than guessed.
    TiedCandidates { non_scan_cell: String, scan_cells: Vec<String> },
}

/// A scan cell: its test cell has a scan-in port AND a scan-enable port (either polarity).
pub fn is_scan_cell(cell: &Cell) -> bool {
    let Some(test) = cell.test_cell.as_deref() else { return false };
    let has = |types: [ScanSignalType; 2]| test.top_level_ports().iter().any(|p| types.contains(&p.scan_signal_type));
    has([ScanSignalType::Input, ScanSignalType::InputInverted]) && has([ScanSignalType::Enable, ScanSignalType::EnableInverted])
}

/// The library cells a replacement can use or replace, from `masters` in the database's order:
/// those with a Liberty cell that is sequential, not `dont_use` and not a clock gate — the scan
/// cells, and the rest (non-scan) in master order.
pub fn collect_scan_cells_available<'a>(lib: &'a Library, masters: &[String]) -> (Vec<&'a Cell>, Vec<&'a Cell>) {
    let (mut scan, mut non_scan) = (Vec::new(), Vec::new());
    for m in masters {
        let Some(cell) = lib.cells.get(m) else { continue };
        if !cell.is_sequential() || cell.dont_use || cell.is_clock_gate {
            continue;
        }
        if is_scan_cell(cell) {
            scan.push(cell);
        } else {
            non_scan.push(cell);
        }
    }
    (scan, non_scan)
}

/// A pin's direction as the reference keeps it: an output with a `three_state` is tristate.
fn direction(p: &Port) -> Direction {
    if p.direction == Direction::Output && p.is_any_tristate() {
        Direction::Tristate
    } else {
        p.direction
    }
}

/// What a name in a function refers to: a port (by its direction), or a register's state
/// variable (an internal port).
fn function_port_direction(cell: &Cell, name: &str) -> Option<Direction> {
    if let Some(p) = cell.top_level_ports().into_iter().find(|p| p.name == name) {
        return Some(direction(p));
    }
    let state = cell.seqs.iter().any(|s| s.output.as_deref() == Some(name) || s.output_inv.as_deref() == Some(name));
    state.then_some(Direction::Internal)
}

fn parse_function(cell: &Cell, p: &Port) -> Result<Option<FuncExpr>, ReplaceError> {
    p.function
        .as_deref()
        .filter(|f| !f.trim().is_empty())
        .map(FuncExpr::parse)
        .transpose()
        .map_err(|error| ReplaceError::BadFunction { cell: cell.name.clone(), port: p.name.clone(), error })
}

/// Two functions alike: the same operator at every node, and at every port leaf the same name
/// and direction in its own cell (as two ports compare).
fn functions_equiv(a: &FuncExpr, ca: &Cell, b: &FuncExpr, cb: &Cell) -> bool {
    match (a, b) {
        (FuncExpr::Port(x), FuncExpr::Port(y)) => x == y && function_port_direction(ca, x) == function_port_direction(cb, y),
        (FuncExpr::Not(x), FuncExpr::Not(y)) => functions_equiv(x, ca, y, cb),
        (FuncExpr::And(x1, x2), FuncExpr::And(y1, y2)) | (FuncExpr::Or(x1, x2), FuncExpr::Or(y1, y2)) | (FuncExpr::Xor(x1, x2), FuncExpr::Xor(y1, y2)) => {
            functions_equiv(x1, ca, y1, cb) && functions_equiv(x2, ca, y2, cb)
        }
        (FuncExpr::One, FuncExpr::One) | (FuncExpr::Zero, FuncExpr::Zero) => true,
        _ => false,
    }
}

/// The power and ground pins with a known `pg_type`; any other `pg_pin` refuses the cell.
fn pg_pins(cell: &Cell) -> Result<&[(String, String)], ReplaceError> {
    const TYPES: [&str; 10] =
        ["primary_power", "primary_ground", "backup_power", "backup_ground", "internal_power", "internal_ground", "pwell", "nwell", "deeppwell", "deepnwell"];
    if let Some((pin, _)) = cell.pg_pins.iter().find(|(_, t)| !TYPES.contains(&t.as_str())) {
        return Err(ReplaceError::UntypedPgPin { cell: cell.name.clone(), pin: pin.clone() });
    }
    Ok(&cell.pg_pins)
}

/// Whether `scan` can replace `non_scan`, and the port mapping when it can:
///
/// - each signal port of the non-scan cell maps to the FIRST signal port of the scan cell (in its
///   port order) with the same direction and — when neither has a function, the same name;
///   otherwise alike functions; none: no;
/// - each power/ground pin maps to the first one of the same name and type; none: no;
/// - every scan signal port nothing mapped to must be a scan port of the scan cell's test cell
///   (a port of it with a scan signal type) — an extra preset, clear or enable says no.
pub fn is_scan_equivalent(non_scan: &Cell, scan: &Cell) -> Result<Option<BTreeMap<String, String>>, ReplaceError> {
    let mut mapping = BTreeMap::new();
    let mut seen: Vec<&str> = Vec::new();
    let scan_ports = scan.top_level_ports();
    for p in non_scan.top_level_ports() {
        let pf = parse_function(non_scan, p)?;
        let mut found = None;
        for q in &scan_ports {
            if direction(p) != direction(q) {
                continue;
            }
            let qf = parse_function(scan, q)?;
            let alike = match (&pf, &qf) {
                (None, None) => p.name == q.name,
                (Some(a), Some(b)) => functions_equiv(a, non_scan, b, scan),
                _ => false,
            };
            if alike {
                found = Some(q.name.as_str());
                break;
            }
        }
        let Some(q) = found else { return Ok(None) };
        mapping.insert(p.name.clone(), q.to_string());
        seen.push(q);
    }
    let scan_pg = pg_pins(scan)?;
    for (name, ty) in pg_pins(non_scan)? {
        let Some((q, _)) = scan_pg.iter().find(|(n, t)| n == name && t == ty) else { return Ok(None) };
        mapping.insert(name.clone(), q.clone());
    }
    for q in &scan_ports {
        if seen.contains(&q.name.as_str()) {
            continue;
        }
        let scan_port = scan.test_cell.as_deref().and_then(|t| t.top_level_ports().into_iter().find(|tp| tp.name == q.name));
        if !scan_port.is_some_and(|tp| tp.scan_signal_type != ScanSignalType::None) {
            return Ok(None);
        }
    }
    Ok(Some(mapping))
}

/// How far a scan cell's drive is from the non-scan cell's: |drive resistance of `Q`| apart,
/// or the largest value when either has no `Q`.
fn performance_difference(non_scan: &Cell, scan: &Cell) -> f64 {
    if non_scan.port("Q").is_none() || scan.port("Q").is_none() {
        return f64::MAX;
    }
    (f64::from(non_scan.drive_resistance("Q")) - f64::from(scan.drive_resistance("Q"))).abs()
}

/// The library half, per non-scan cell with any candidate: the scan cell closest in drive.
/// Keyed by non-scan cell name.
pub fn scan_equivalents(lib: &Library, masters: &[String]) -> Result<BTreeMap<String, ScanCandidate>, ReplaceError> {
    let (scan_cells, non_scan_cells) = collect_scan_cells_available(lib, masters);
    let mut out = BTreeMap::new();
    for non_scan in non_scan_cells {
        let mut candidates = Vec::new();
        for scan in &scan_cells {
            if let Some(port_mapping) = is_scan_equivalent(non_scan, scan)? {
                candidates.push((performance_difference(non_scan, scan), ScanCandidate { scan_cell: scan.name.clone(), port_mapping }));
            }
        }
        let Some(best) = candidates.iter().map(|c| c.0).min_by(f64::total_cmp) else { continue };
        let closest: Vec<&ScanCandidate> = candidates.iter().filter(|c| c.0 == best).map(|c| &c.1).collect();
        if closest.len() > 1 {
            return Err(ReplaceError::TiedCandidates { non_scan_cell: non_scan.name.clone(), scan_cells: closest.iter().map(|c| c.scan_cell.clone()).collect() });
        }
        out.insert(non_scan.name.clone(), closest[0].clone());
    }
    Ok(out)
}

/// What the instance walk does with a master's instances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MasterClass {
    /// Not sequential, or a clock gate: nothing to replace.
    Leave,
    /// A usable scan cell already (reported as such).
    AlreadyScan,
    /// Replaced by this scan cell.
    Replace(ScanCandidate),
    /// Sequential but nothing can replace it (reported) — a `dont_use` scan cell included, as it is
    /// no usable scan cell.
    NoEquivalent,
}

/// Per master with a Liberty cell, in `masters` order, its class; a master with none is left out
/// (the walk reports it once per CORE master).
pub fn master_classes(lib: &Library, masters: &[String]) -> Result<Vec<(String, MasterClass)>, ReplaceError> {
    let (scan_cells, _) = collect_scan_cells_available(lib, masters);
    let equivalents = scan_equivalents(lib, masters)?;
    let mut out = Vec::new();
    for m in masters {
        let Some(cell) = lib.cells.get(m) else { continue };
        let class = if !cell.is_sequential() || cell.is_clock_gate {
            MasterClass::Leave
        } else if scan_cells.iter().any(|c| c.name == *m) {
            MasterClass::AlreadyScan
        } else if let Some(c) = equivalents.get(m) {
            MasterClass::Replace(c.clone())
        } else {
            MasterClass::NoEquivalent
        };
        out.push((m.clone(), class));
    }
    Ok(out)
}

/// Several libraries as the design sees them: a cell name resolves to the FIRST library (in
/// read order) that has it.
pub fn merge_libraries(libs: Vec<Library>) -> Option<Library> {
    let mut it = libs.into_iter();
    let mut first = it.next()?;
    for lib in it {
        for (name, cell) in lib.cells {
            first.cells.entry(name).or_insert(cell);
        }
    }
    Some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vyges_sta::liberty_parse::parse;

    const LIB: &str = r#"library (t) { time_unit : "1ns"; capacitive_load_unit (1, pf);
      cell (dff) { pg_pin (VPWR) { pg_type : primary_power; } pg_pin (VGND) { pg_type : primary_ground; }
        ff (IQ, IQN) { clocked_on : "CLK"; next_state : "D"; }
        pin (CLK) { direction : input; clock : true; } pin (D) { direction : input; }
        pin (Q) { direction : output; function : "IQ"; } }
      cell (dffr) { pg_pin (VPWR) { pg_type : primary_power; } pg_pin (VGND) { pg_type : primary_ground; }
        ff (IQ, IQN) { clocked_on : "CLK"; next_state : "D"; clear : "!RESET_B"; }
        pin (CLK) { direction : input; clock : true; } pin (D) { direction : input; }
        pin (RESET_B) { direction : input; } pin (Q) { direction : output; function : "IQ"; } }
      cell (sdff) { pg_pin (VPWR) { pg_type : primary_power; } pg_pin (VGND) { pg_type : primary_ground; }
        ff (IQ, IQN) { clocked_on : "CLK"; next_state : "(D&!SCE)|(SCD&SCE)"; }
        pin (CLK) { direction : input; clock : true; } pin (D) { direction : input; }
        pin (SCD) { direction : input; } pin (SCE) { direction : input; }
        pin (Q) { direction : output; function : "IQ"; }
        test_cell () { ff (IQ, IQN) { clocked_on : "CLK"; next_state : "D"; }
          pin (CLK) { direction : input; } pin (D) { direction : input; }
          pin (SCD) { direction : input; signal_type : test_scan_in; }
          pin (SCE) { direction : input; signal_type : test_scan_enable; }
          pin (Q) { direction : output; function : "IQ"; signal_type : test_scan_out; } } }
      cell (sdff_unused) { dont_use : true;
        ff (IQ, IQN) { clocked_on : "CLK"; next_state : "D"; }
        pin (CLK) { direction : input; } pin (D) { direction : input; } pin (Q) { direction : output; function : "IQ"; }
        test_cell () { pin (D) { direction : input; signal_type : test_scan_in; } pin (CLK) { direction : input; signal_type : test_scan_enable; } } }
      cell (buf) { pin (A) { direction : input; } pin (X) { direction : output; function : "A"; } } }"#;

    fn lib() -> Library {
        Library::read(&parse(LIB).unwrap()).unwrap()
    }

    fn masters() -> Vec<String> {
        ["buf", "dff", "dffr", "sdff", "sdff_unused", "not_in_liberty"].map(String::from).to_vec()
    }

    // Rule: only sequential cells that are usable (not dont_use, not a clock gate) are sorted, in
    // master order; a scan cell's test cell names a scan-in AND a scan-enable port.
    #[test]
    fn scan_and_non_scan_cells_in_master_order() {
        let l = lib();
        let (scan, non_scan) = collect_scan_cells_available(&l, &masters());
        assert_eq!(scan.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["sdff"]);
        assert_eq!(non_scan.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["dff", "dffr"]);
    }

    // Rule: inputs without a function map by NAME, outputs by function (`IQ`, the register's
    // state, on both), PG pins by name and type; the scan cell's extra SCD and SCE are scan ports
    // of its test cell, so they may stay unmapped. A non-scan port with no scan counterpart
    // (dffr's RESET_B) says no.
    #[test]
    fn a_flop_maps_to_the_scan_flop_port_by_port() {
        let l = lib();
        let m = is_scan_equivalent(&l.cells["dff"], &l.cells["sdff"]).unwrap().unwrap();
        let want: BTreeMap<String, String> =
            [("CLK", "CLK"), ("D", "D"), ("Q", "Q"), ("VPWR", "VPWR"), ("VGND", "VGND")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        assert_eq!(m, want);
        assert_eq!(is_scan_equivalent(&l.cells["dffr"], &l.cells["sdff"]).unwrap(), None);
        let eq = scan_equivalents(&l, &masters()).unwrap();
        assert_eq!(eq.keys().collect::<Vec<_>>(), ["dff"]);
        assert_eq!(eq["dff"].scan_cell, "sdff");
    }

    // Rule: an unmapped scan port that is NOT a scan port of the test cell (an extra enable or
    // clear) says no.
    #[test]
    fn an_extra_functional_port_on_the_scan_cell_says_no() {
        let text = LIB.replace("pin (SCE) { direction : input; signal_type : test_scan_enable; }", "pin (SCE) { direction : input; signal_type : test_scan_enable; } pin (EN) { direction : input; }")
            .replace("pin (SCD) { direction : input; } pin (SCE) { direction : input; }", "pin (SCD) { direction : input; } pin (SCE) { direction : input; } pin (EN) { direction : input; }");
        let l = Library::read(&parse(&text).unwrap()).unwrap();
        assert_eq!(is_scan_equivalent(&l.cells["dff"], &l.cells["sdff"]).unwrap(), None);
    }

    // Rule: a test cell with a scan-in but NO scan-enable is no scan cell (it is non-scan).
    #[test]
    fn a_scan_in_without_an_enable_is_no_scan_cell() {
        let text = LIB.replace("pin (CLK) { direction : input; signal_type : test_scan_enable; }", "pin (CLK) { direction : input; }")
            .replace("cell (sdff_unused) { dont_use : true;", "cell (sdff_unused) {");
        let l = Library::read(&parse(&text).unwrap()).unwrap();
        let (scan, non_scan) = collect_scan_cells_available(&l, &masters());
        assert_eq!(scan.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["sdff"]);
        assert_eq!(non_scan.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["dff", "dffr", "sdff_unused"]);
    }

    // Rule: power and ground pins match by name AND type: a scan cell whose VGND is typed as power
    // is no replacement.
    #[test]
    fn a_power_pin_of_another_type_says_no() {
        let at = LIB.find("cell (sdff) {").unwrap();
        let text = format!("{}{}", &LIB[..at], LIB[at..].replacen("pg_pin (VGND) { pg_type : primary_ground; }", "pg_pin (VGND) { pg_type : primary_power; }", 1));
        let l = Library::read(&parse(&text).unwrap()).unwrap();
        assert_eq!(is_scan_equivalent(&l.cells["dff"], &l.cells["sdff"]).unwrap(), None);
    }

    // Rule: ports match only in the same direction: the scan cell's CLK as an inout does not take
    // the flop's input CLK, though the names agree.
    #[test]
    fn a_port_of_another_direction_says_no() {
        let at = LIB.find("cell (sdff) {").unwrap();
        let text = format!("{}{}", &LIB[..at], LIB[at..].replacen("pin (CLK) { direction : input; clock : true; }", "pin (CLK) { direction : inout; clock : true; }", 1));
        let l = Library::read(&parse(&text).unwrap()).unwrap();
        assert_eq!(is_scan_equivalent(&l.cells["dff"], &l.cells["sdff"]).unwrap(), None);
    }

    // Rule: the CLOSEST drive wins; a scan cell with no `Q` port (its output named otherwise but
    // alike) is as far as can be, so the one with `Q` is taken.
    #[test]
    fn the_closest_drive_wins() {
        let at = LIB.find("cell (sdff) {").unwrap();
        let end = LIB.find("cell (sdff_unused)").unwrap();
        let far = LIB[at..end].replace("cell (sdff)", "cell (sdff_qo)").replace("pin (Q) {", "pin (QO) {");
        let text = LIB.replace("cell (buf)", &format!("{far} cell (buf)"));
        let l = Library::read(&parse(&text).unwrap()).unwrap();
        let mut m = masters();
        m.insert(0, "sdff_qo".into());
        assert!(is_scan_equivalent(&l.cells["dff"], &l.cells["sdff_qo"]).unwrap().is_some());
        assert_eq!(scan_equivalents(&l, &m).unwrap()["dff"].scan_cell, "sdff");
    }

    // Rule: per master with a Liberty cell, in master order: combinational and clock gates are
    // left, a usable scan cell is reported as one, a flop with an equivalent is replaced, a flop
    // without one — or a dont_use scan cell, which is no usable scan cell — has no equivalent.
    #[test]
    fn every_master_with_a_liberty_cell_is_classed() {
        let l = lib();
        let got = master_classes(&l, &masters()).unwrap();
        let names: Vec<&str> = got.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(names, ["buf", "dff", "dffr", "sdff", "sdff_unused"]);
        assert_eq!(got[0].1, MasterClass::Leave);
        assert!(matches!(&got[1].1, MasterClass::Replace(c) if c.scan_cell == "sdff"));
        assert_eq!(got[2].1, MasterClass::NoEquivalent);
        assert_eq!(got[3].1, MasterClass::AlreadyScan);
        assert_eq!(got[4].1, MasterClass::NoEquivalent);
    }

    // Rule: a name resolves to the first library that has it.
    #[test]
    fn the_first_library_with_a_cell_wins() {
        let a = Library::read(&parse(r#"library (a) { cell (x) { area : 1; } }"#).unwrap()).unwrap();
        let b = Library::read(&parse(r#"library (b) { cell (x) { area : 2; } cell (y) { area : 3; } }"#).unwrap()).unwrap();
        let m = merge_libraries(vec![a, b]).unwrap();
        assert_eq!((m.cells["x"].area, m.cells["y"].area), (1.0, 3.0));
    }

    // Rule: two scan cells as close in drive are a tie the reference settles by address: refused.
    #[test]
    fn equally_close_scan_cells_are_refused() {
        let text = LIB.replace("cell (sdff_unused) { dont_use : true;", "cell (sdff_unused) {");
        let twin = LIB[LIB.find("cell (sdff) {").unwrap()..LIB.find("cell (sdff_unused)").unwrap()].replace("cell (sdff)", "cell (sdff_twin)");
        let text = text.replace("cell (buf)", &format!("{twin} cell (buf)"));
        let l = Library::read(&parse(&text).unwrap()).unwrap();
        let mut m = masters();
        m.push("sdff_twin".into());
        assert!(matches!(scan_equivalents(&l, &m), Err(ReplaceError::TiedCandidates { .. })));
    }
}
