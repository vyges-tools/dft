// SPDX-License-Identifier: Apache-2.0
//! Scan replacement, the library half: which library cells are scan cells, and for each non-scan
//! flop the scan cell that can take its place, with the port it maps each of its ports to.
//!
//! The design half (which instances are replaced, and the database edits) is separate; this one
//! reads only the Liberty library, in the order the database lists its masters.

use std::collections::{BTreeMap, BTreeSet};

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
}

/// What the scan cell choice reads besides the merged library: the library each cell came from
/// (index in read order; a cell with none is library 0) and the user's `set_dont_use` cells.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub origin: BTreeMap<String, usize>,
    pub dont_use: BTreeSet<String>,
}

impl Selection {
    fn library(&self, cell: &str) -> usize {
        self.origin.get(cell).copied().unwrap_or(0)
    }
}

/// Each cell name's library, in read order: the FIRST library that has it, as
/// [`merge_libraries`] resolves names.
pub fn library_origins(libs: &[Library]) -> BTreeMap<String, usize> {
    let mut origin = BTreeMap::new();
    for (i, lib) in libs.iter().enumerate() {
        for name in lib.cells.keys() {
            origin.entry(name.clone()).or_insert(i);
        }
    }
    origin
}

/// The cells of `lib` a `set_dont_use` pattern names (`*` and `?` as Tcl `string match`).
pub fn dont_use_cells(lib: &Library, patterns: &[String]) -> BTreeSet<String> {
    fn matches(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => matches(&p[1..], s) || (!s.is_empty() && matches(p, &s[1..])),
            (Some(b'?'), Some(_)) => matches(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => matches(&p[1..], &s[1..]),
            _ => false,
        }
    }
    let mut out = BTreeSet::new();
    for p in patterns {
        for name in lib.cells.keys() {
            if matches(p.as_bytes(), name.as_bytes()) {
                out.insert(name.clone());
            }
        }
    }
    out
}

/// A scan cell: its test cell has a scan-in port AND a scan-enable port (either polarity).
pub fn is_scan_cell(cell: &Cell) -> bool {
    let Some(test) = cell.test_cell.as_deref() else { return false };
    let has = |types: [ScanSignalType; 2]| test.top_level_ports().iter().any(|p| types.contains(&p.scan_signal_type));
    has([ScanSignalType::Input, ScanSignalType::InputInverted]) && has([ScanSignalType::Enable, ScanSignalType::EnableInverted])
}

/// The library cells a replacement can use or replace, from `masters` in the database's order:
/// those with a Liberty cell that is sequential, not `dont_use` and not a clock gate — the scan
/// cells the user has not `set_dont_use` (4155ac3 `collectScanCellAvailable`: the resizer's
/// dont-use set excludes SCAN cells only), and the rest (non-scan) in master order.
pub fn collect_scan_cells_available<'a>(lib: &'a Library, masters: &[String], sel: &Selection) -> (Vec<&'a Cell>, Vec<&'a Cell>) {
    let (mut scan, mut non_scan) = (Vec::new(), Vec::new());
    for m in masters {
        let Some(cell) = lib.cells.get(m) else { continue };
        if !cell.is_sequential() || cell.dont_use || cell.is_clock_gate {
            continue;
        }
        if is_scan_cell(cell) {
            if sel.dont_use.contains(&cell.name) {
                continue;
            }
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

/// `FindEquivalentPortInScanCell`: the FIRST signal port of `scan` (in its port order) with the
/// direction of `p` and — when neither has a function, the same name; otherwise alike functions.
fn find_equivalent_port<'a>(non_scan: &Cell, p: &Port, scan: &'a Cell) -> Result<Option<&'a str>, ReplaceError> {
    let pf = parse_function(non_scan, p)?;
    for q in scan.top_level_ports() {
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
            return Ok(Some(q.name.as_str()));
        }
    }
    Ok(None)
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
        let Some(q) = find_equivalent_port(non_scan, p, scan)? else { return Ok(None) };
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

/// `kDriveResistanceRelTolerance`: candidates within 1% relative drive of the best stay.
const DRIVE_RESISTANCE_REL_TOLERANCE: f64 = 0.01;
/// `kAreaRelTolerance`: then those within 0.01% of the smallest area (float, as the reference).
const AREA_REL_TOLERANCE: f32 = 1e-4;

/// `CandidateRank`: what one scan cell is ranked by against one non-scan cell.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateRank {
    pub footprint_match: bool,
    pub same_library: bool,
    pub rel_drive_diff: f64,
    pub area: f32,
    pub direct_name_match: bool,
    pub name: String,
}

/// `IsSingleCharScanInsertion`: `scan` is `non_scan` with ONE `s` or `S` inserted, anywhere.
pub fn is_single_char_scan_insertion(non_scan: &str, scan: &str) -> bool {
    let (n, s) = (non_scan.as_bytes(), scan.as_bytes());
    if s.len() != n.len() + 1 {
        return false;
    }
    let prefix = n.iter().zip(s).take_while(|(a, b)| a == b).count();
    matches!(s[prefix], b's' | b'S') && s[prefix + 1..] == n[prefix..]
}

/// `IsMatchingFootprint`: both set, and equal or one `s`/`S` apart.
pub fn is_matching_footprint(non_scan: &str, scan: &str) -> bool {
    !non_scan.is_empty() && !scan.is_empty() && (scan == non_scan || is_single_char_scan_insertion(non_scan, scan))
}

/// `SelectBestCandidateRankIndex`: successive filters over the candidates (a flag filter keeps
/// those with the flag, when any has it): footprint match, same library, within 1% of the best
/// relative drive, within 0.01% of the smallest area, the `s`-insertion name; then the least
/// name. 0 when there are none.
pub fn select_best_candidate_rank_index(ranks: &[CandidateRank]) -> usize {
    if ranks.is_empty() {
        return 0;
    }
    let mut survivors: Vec<usize> = (0..ranks.len()).collect();
    fn filter_by_flag(survivors: &mut Vec<usize>, flag: impl Fn(usize) -> bool) {
        if survivors.iter().any(|&i| flag(i)) {
            survivors.retain(|&i| flag(i));
        }
    }
    filter_by_flag(&mut survivors, |i| ranks[i].footprint_match);
    filter_by_flag(&mut survivors, |i| ranks[i].same_library);
    let min_drive = survivors.iter().map(|&i| ranks[i].rel_drive_diff).fold(f64::INFINITY, f64::min);
    survivors.retain(|&i| ranks[i].rel_drive_diff - min_drive <= DRIVE_RESISTANCE_REL_TOLERANCE);
    let min_area = survivors.iter().map(|&i| ranks[i].area).fold(f32::INFINITY, f32::min);
    survivors.retain(|&i| {
        let max_area = ranks[i].area.abs().max(min_area.abs());
        ranks[i].area - min_area <= AREA_REL_TOLERANCE * max_area
    });
    filter_by_flag(&mut survivors, |i| ranks[i].direct_name_match);
    let best = survivors.iter().min_by(|&&a, &&b| ranks[a].name.cmp(&ranks[b].name));
    *best.expect("the drive and area filters keep their own minimum")
}

fn func_port(f: &FuncExpr) -> Option<&str> {
    match f {
        FuncExpr::Port(p) => Some(p),
        _ => None,
    }
}

fn func_has_port(f: &FuncExpr, name: &str) -> bool {
    match f {
        FuncExpr::Port(p) => p == name,
        FuncExpr::Not(x) => func_has_port(x, name),
        FuncExpr::And(a, b) | FuncExpr::Or(a, b) | FuncExpr::Xor(a, b) => func_has_port(a, name) || func_has_port(b, name),
        FuncExpr::One | FuncExpr::Zero => false,
    }
}

/// `FindPrimaryOutputPort`: in port order, the first output (any kind) that IS a register's
/// state (its function is the bare state variable); else the first that is the inverted state or
/// whose function reads either state; else the first output with a function; else `Q`.
fn find_primary_output_port(cell: &Cell) -> Result<Option<&Port>, ReplaceError> {
    let (mut seq_inv, mut func_port_found): (Option<&Port>, Option<&Port>) = (None, None);
    for p in cell.top_level_ports() {
        if !matches!(direction(p), Direction::Output | Direction::Tristate | Direction::Bidirect) {
            continue;
        }
        let f = parse_function(cell, p)?;
        if f.is_some() && func_port_found.is_none() {
            func_port_found = Some(p);
        }
        for seq in &cell.seqs {
            let out = seq.output.as_deref();
            let inv = seq.output_inv.as_deref();
            if out == Some(p.name.as_str()) || (out.is_some() && f.as_ref().and_then(func_port) == out) {
                return Ok(Some(p));
            }
            let reads = |v: Option<&str>| v.is_some_and(|v| f.as_ref().is_some_and(|f| func_has_port(f, v)));
            if seq_inv.is_none() && (inv == Some(p.name.as_str()) || reads(out) || reads(inv)) {
                seq_inv = Some(p);
            }
        }
    }
    Ok(seq_inv.or(func_port_found).or_else(|| cell.port("Q")))
}

/// `RelativeDriveResistanceDiff`: |R_non_scan - R_scan| / max(|R_non_scan|, |R_scan|) of the
/// primary output and the scan port it maps to (by the mapping, else the first equivalent port,
/// else by name); the largest value when there is none, 0 when both are 0.
fn relative_drive_resistance_diff(non_scan: &Cell, non_scan_out: Option<&Port>, scan: &Cell, mapping: &BTreeMap<String, String>) -> Result<f64, ReplaceError> {
    let Some(out) = non_scan_out else { return Ok(f64::MAX) };
    let mut scan_out = mapping.get(&out.name).and_then(|m| scan.port(m)).map(|p| p.name.as_str());
    if scan_out.is_none() {
        scan_out = find_equivalent_port(non_scan, out, scan)?;
    }
    if scan_out.is_none() {
        scan_out = scan.port(&out.name).map(|p| p.name.as_str());
    }
    let Some(scan_out) = scan_out else { return Ok(f64::MAX) };
    let r_non_scan = f64::from(non_scan.drive_resistance(&out.name));
    let r_scan = f64::from(scan.drive_resistance(scan_out));
    let max_r = r_non_scan.abs().max(r_scan.abs());
    Ok(if max_r == 0.0 { 0.0 } else { (r_non_scan - r_scan).abs() / max_r })
}

/// `ComputeCandidateRank`.
fn candidate_rank(non_scan: &Cell, non_scan_out: Option<&Port>, scan: &Cell, mapping: &BTreeMap<String, String>, sel: &Selection) -> Result<CandidateRank, ReplaceError> {
    Ok(CandidateRank {
        footprint_match: is_matching_footprint(&non_scan.footprint, &scan.footprint),
        same_library: sel.library(&scan.name) == sel.library(&non_scan.name),
        rel_drive_diff: relative_drive_resistance_diff(non_scan, non_scan_out, scan, mapping)?,
        area: scan.area,
        direct_name_match: is_single_char_scan_insertion(&non_scan.name, &scan.name),
        name: scan.name.clone(),
    })
}

/// `SelectBestScanCell` (4155ac3): rank each candidate, take the best. `None` with no candidate.
fn select_best_scan_cell(non_scan: &Cell, candidates: Vec<(&Cell, ScanCandidate)>, sel: &Selection) -> Result<Option<ScanCandidate>, ReplaceError> {
    if candidates.is_empty() {
        return Ok(None);
    }
    let non_scan_out = find_primary_output_port(non_scan)?;
    let mut ranks = Vec::with_capacity(candidates.len());
    for (scan, c) in &candidates {
        ranks.push(candidate_rank(non_scan, non_scan_out, scan, &c.port_mapping, sel)?);
    }
    let best = select_best_candidate_rank_index(&ranks);
    Ok(candidates.into_iter().nth(best).map(|(_, c)| c))
}

/// The library half (`collectScanCellAvailable`), per non-scan cell with any candidate: the
/// scan cell [`select_best_scan_cell`] ranks first. Keyed by non-scan cell name.
pub fn scan_equivalents(lib: &Library, masters: &[String], sel: &Selection) -> Result<BTreeMap<String, ScanCandidate>, ReplaceError> {
    let (scan_cells, non_scan_cells) = collect_scan_cells_available(lib, masters, sel);
    let mut out = BTreeMap::new();
    for non_scan in non_scan_cells {
        let mut candidates = Vec::new();
        for scan in &scan_cells {
            if let Some(port_mapping) = is_scan_equivalent(non_scan, scan)? {
                candidates.push((*scan, ScanCandidate { scan_cell: scan.name.clone(), port_mapping }));
            }
        }
        if let Some(best) = select_best_scan_cell(non_scan, candidates, sel)? {
            out.insert(non_scan.name.clone(), best);
        }
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
pub fn master_classes(lib: &Library, masters: &[String], sel: &Selection) -> Result<Vec<(String, MasterClass)>, ReplaceError> {
    let (scan_cells, _) = collect_scan_cells_available(lib, masters, sel);
    let equivalents = scan_equivalents(lib, masters, sel)?;
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
        let (scan, non_scan) = collect_scan_cells_available(&l, &masters(), &Selection::default());
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
        let eq = scan_equivalents(&l, &masters(), &Selection::default()).unwrap();
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
        let (scan, non_scan) = collect_scan_cells_available(&l, &masters(), &Selection::default());
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

    // Rule (4155ac3): the drive compared is the primary output's and the scan port it MAPS to,
    // so a scan cell naming its output otherwise ties on drive; the `s`-insertion name decides.
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
        assert_eq!(scan_equivalents(&l, &m, &Selection::default()).unwrap()["dff"].scan_cell, "sdff");
    }

    // Rule: per master with a Liberty cell, in master order: combinational and clock gates are
    // left, a usable scan cell is reported as one, a flop with an equivalent is replaced, a flop
    // without one — or a dont_use scan cell, which is no usable scan cell — has no equivalent.
    #[test]
    fn every_master_with_a_liberty_cell_is_classed() {
        let l = lib();
        let got = master_classes(&l, &masters(), &Selection::default()).unwrap();
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

    // Upstream gtest `IsSingleCharScanInsertion` (4155ac3): one `s`/`S` inserted, leading,
    // infix (after a library prefix) or trailing; never two characters, another letter, or none.
    #[test]
    fn single_char_scan_insertion_as_upstream() {
        for (a, b) in [("DFF_X1", "SDFF_X1"), ("dfxtp", "sdfxtp"), ("dfxtp_1", "sdfxtp_1"), ("sky130_fd_sc_hd__dfxtp_1", "sky130_fd_sc_hd__sdfxtp_1"), ("DFF_X1", "DFF_X1S")] {
            assert!(is_single_char_scan_insertion(a, b), "{a} -> {b}");
        }
        for (a, b) in [("dfxtp", "sdfbbp"), ("sky130_fd_sc_hd__dfxtp_1", "sky130_fd_sc_hd__sdfxtp_1_lp"), ("DFF_X1", "XDFF_X1"), ("DFF_X1", "DFF_X1")] {
            assert!(!is_single_char_scan_insertion(a, b), "{a} -> {b}");
        }
    }

    // Upstream gtest `IsMatchingFootprint`: equal or one `s` apart, both set.
    #[test]
    fn matching_footprint_as_upstream() {
        assert!(is_matching_footprint("dfxtp", "dfxtp") && is_matching_footprint("dfxtp", "sdfxtp") && is_matching_footprint("DFF_X1", "SDFF_X1"));
        assert!(!is_matching_footprint("", "") && !is_matching_footprint("dfxtp", "") && !is_matching_footprint("", "sdfxtp") && !is_matching_footprint("dfxtp", "sdfbbp"));
    }

    fn rank(footprint_match: bool, same_library: bool, rel_drive_diff: f64, area: f32, direct_name_match: bool, name: &str) -> CandidateRank {
        CandidateRank { footprint_match, same_library, rel_drive_diff, area, direct_name_match, name: name.into() }
    }

    fn better(a: &CandidateRank, b: &CandidateRank) -> bool {
        select_best_candidate_rank_index(&[a.clone(), b.clone()]) == 0 && select_best_candidate_rank_index(&[b.clone(), a.clone()]) == 1
    }

    // Upstream gtests `IsBetterCandidateRank*`: footprint first, then the same library; a drive
    // more than 1% apart wins, within 1% the smaller area; then the `s`-insertion name, then the
    // least name.
    #[test]
    fn candidate_rank_tiers_as_upstream() {
        let fp = rank(true, true, 0.02, 20.0, true, "sky130_fd_sc_hd__sdfxtp_1");
        let no_fp = rank(false, true, 0.001, 19.0, false, "sky130_fd_sc_hd__sdfbbp_1");
        let cross = rank(true, false, 0.001, 18.0, true, "sky130_fd_sc_hs__sdfxtp_1");
        assert!(better(&fp, &no_fp) && !better(&no_fp, &fp) && better(&fp, &cross) && !better(&cross, &fp));
        let compact = rank(true, true, 0.004, 20.0, true, "sky130_fd_sc_hd__sdfxtp_1");
        let wider = rank(true, true, 0.002, 21.25, false, "sky130_fd_sc_hd__sdfxtp_1_lp");
        let wrong = rank(true, true, 0.40, 17.5, false, "sky130_fd_sc_hd__sdfxtp_2");
        assert!(better(&compact, &wider) && !better(&wider, &compact) && better(&compact, &wrong) && !better(&wrong, &compact));
        let direct = rank(true, true, 0.002, 20.0, true, "sky130_fd_sc_hd__sdfxtp_1_b");
        let indirect = rank(true, true, 0.002, 20.0, false, "sky130_fd_sc_hd__sdfxtp_1_a");
        assert!(better(&direct, &indirect) && !better(&indirect, &direct));
        let (first, second) = (rank(true, true, 0.002, 20.0, false, "x_a"), rank(true, true, 0.002, 20.0, false, "x_b"));
        assert!(better(&first, &second) && !better(&second, &first));
        assert_eq!(select_best_candidate_rank_index(&[]), 0);
    }

    // Upstream gtest `SelectBestCandidateRankIndexIsOrderInvariant`: every permutation picks the
    // one within 1% of the best drive with the smaller area.
    #[test]
    fn candidate_rank_choice_is_order_invariant() {
        let base = [
            rank(true, true, 0.001, 21.25, false, "sky130_fd_sc_hd__sdfxtp_1_lp"),
            rank(true, true, 0.008, 20.0, true, "sky130_fd_sc_hd__sdfxtp_1"),
            rank(true, true, 0.015, 18.75, false, "sky130_fd_sc_hd__sdfxtp_2"),
            rank(false, true, 0.0, 15.0, false, "sky130_fd_sc_hd__sdfbbp_1"),
        ];
        fn perms(n: usize) -> Vec<Vec<usize>> {
            if n == 0 {
                return vec![vec![]];
            }
            let mut out = Vec::new();
            for p in perms(n - 1) {
                for at in 0..=p.len() {
                    let mut q = p.clone();
                    q.insert(at, n - 1);
                    out.push(q);
                }
            }
            out
        }
        for p in perms(base.len()) {
            let permuted: Vec<CandidateRank> = p.iter().map(|&i| base[i].clone()).collect();
            assert_eq!(permuted[select_best_candidate_rank_index(&permuted)].name, "sky130_fd_sc_hd__sdfxtp_1", "{p:?}");
        }
    }

    // Rule (4155ac3 `collectScanCellAvailable`): a scan cell the user `set_dont_use` is no
    // candidate, so the next one is taken; a non-scan cell named so is still replaced.
    #[test]
    fn a_user_dont_use_scan_cell_is_skipped() {
        let text = LIB.replace("cell (sdff_unused) { dont_use : true;", "cell (sdff_unused) {");
        let twin = LIB[LIB.find("cell (sdff) {").unwrap()..LIB.find("cell (sdff_unused)").unwrap()].replace("cell (sdff)", "cell (sdff_twin)");
        let text = text.replace("cell (buf)", &format!("{twin} cell (buf)"));
        let l = Library::read(&parse(&text).unwrap()).unwrap();
        let mut m = masters();
        m.push("sdff_twin".into());
        // equal in every tier but name: `sdff` is the `s`-insertion of `dff`
        assert_eq!(scan_equivalents(&l, &m, &Selection::default()).unwrap()["dff"].scan_cell, "sdff");
        let sel = Selection { dont_use: dont_use_cells(&l, &["sdff".into(), "dff".into()]), ..Default::default() };
        assert_eq!(scan_equivalents(&l, &m, &sel).unwrap()["dff"].scan_cell, "sdff_twin");
        assert_eq!(dont_use_cells(&l, &["sdff*".into()]).into_iter().collect::<Vec<_>>(), ["sdff", "sdff_twin", "sdff_unused"]);
    }
}
