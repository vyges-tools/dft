// SPDX-License-Identifier: Apache-2.0
//! Scan stitching: each planned chain wired from a scan-in port through its cells to a scan-out.
//!
//! One function per reference function, in call order:
//! - [`stitch`] — every chain in order, numbered from 0;
//! - [`stitch_chain`] — the shared scan enable and the chain's scan in ([`find_or_create_driver`]),
//!   every cell's scan enable, the cells chained, the first cell, then the scan out
//!   ([`find_or_create_scan_out`]);
//! - [`connect`] — one load to one driver, reusing or making a net.
//!
//! Every database edit goes through [`StitchDesign`] in the reference's order: net and port ids,
//! and the order of each net's terminals, are what the written netlist shows.

use vyges_sta::liberty::{Cell, Library, ScanSignalType};

use crate::architect::ScanChain;

/// A scan terminal: a block port, or an instance terminal `(instance, mterm)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Port(String),
    Pin(String, String),
}

impl Term {
    /// `GetTermName`: a port's name; an instance terminal's MTERM name.
    fn term_name(&self) -> &str {
        match self {
            Term::Port(p) => p,
            Term::Pin(_, t) => t,
        }
    }

    /// `getName()`: a port's name; an instance terminal's `instance/mterm`.
    fn full_name(&self) -> String {
        match self {
            Term::Port(p) => p.clone(),
            Term::Pin(i, t) => format!("{i}/{t}"),
        }
    }
}

/// What stitching reads and edits.
pub trait StitchDesign {
    fn inst_exists(&self, inst: &str) -> bool;
    fn iterm_exists(&self, inst: &str, mterm: &str) -> bool;
    /// `dbITerm::getIoType` as odb spells it (`INPUT`, `OUTPUT`, `INOUT`, `FEEDTHRU`).
    fn iterm_io_type(&self, inst: &str, mterm: &str) -> String;
    fn bterm_exists(&self, port: &str) -> bool;
    fn bterm_io_type(&self, port: &str) -> String;
    fn net_of(&self, t: &Term) -> Option<String>;
    /// The net's ports, in `getBTerms()` order.
    fn net_bterms(&self, net: &str) -> Vec<String>;
    /// `dbNet::create`; `false` where the name is taken.
    fn net_create(&mut self, name: &str) -> Result<bool, String>;
    fn net_set_sig_type(&mut self, net: &str, sig: &str) -> Result<(), String>;
    /// `dbBTerm::create(net, name)`; `false` where odb returns nullptr.
    fn bterm_create(&mut self, net: &str, name: &str) -> Result<bool, String>;
    fn bterm_set_sig_type(&mut self, port: &str, sig: &str) -> Result<(), String>;
    fn bterm_set_io_type(&mut self, port: &str, io: &str) -> Result<(), String>;
    /// `connect(net)`: the terminal moves onto `net`, off any net it was on.
    fn connect(&mut self, t: &Term, net: &str) -> Result<(), String>;
}

/// The port name patterns (`set_dft_config`'s; `{}` is the number).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StitchConfig {
    pub enable_name_pattern: String,
    pub in_name_pattern: String,
    pub out_name_pattern: String,
}

impl Default for StitchConfig {
    fn default() -> Self {
        StitchConfig {
            enable_name_pattern: "scan_enable_{}".into(),
            in_name_pattern: "scan_in_{}".into(),
            out_name_pattern: "scan_out_{}".into(),
        }
    }
}

/// `fmt::format(pattern, n)` for the one replacement field these patterns use.
fn format_pattern(pattern: &str, n: usize) -> Result<String, String> {
    match pattern.matches("{}").count() {
        1 if !pattern.replacen("{}", "", 1).contains(['{', '}']) => Ok(pattern.replacen("{}", &n.to_string(), 1)),
        _ => Err(format!("name pattern `{pattern}`: one `{{}}` field is modelled")),
    }
}

/// A chain as stitched: its own enable, in and out, and its cells' scan terminals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StitchedChain {
    pub enable: Term,
    pub scan_in: Term,
    pub scan_out: Term,
    /// Per cell: (enable, scan in, scan out) terminals.
    pub cells: Vec<(Term, Term, Term)>,
}

/// Why stitching stops: the reference's error line, or a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StitchError {
    /// `[ERROR DFT-nnnn] …`, as the reference raises it.
    Error(String),
    Refused(String),
}

/// A scan cell's scan terminals, from its test cell: the FIRST port of each scan signal type (either
/// polarity), in the test cell's port order.
pub struct CellPins {
    pub enable: String,
    pub scan_in: String,
    pub scan_out: String,
}

pub fn cell_pins(cell: &Cell) -> Option<CellPins> {
    let test = cell.test_cell.as_deref()?;
    let first = |types: [ScanSignalType; 2]| test.top_level_ports().iter().find(|p| types.contains(&p.scan_signal_type)).map(|p| p.name.clone());
    Some(CellPins {
        enable: first([ScanSignalType::Enable, ScanSignalType::EnableInverted])?,
        scan_in: first([ScanSignalType::Input, ScanSignalType::InputInverted])?,
        scan_out: first([ScanSignalType::Output, ScanSignalType::OutputInverted])?,
    })
}

/// One cell of a chain: its name and scan terminals.
struct Cellp {
    name: String,
    pins: CellPins,
}

impl Cellp {
    fn enable(&self) -> Term {
        Term::Pin(self.name.clone(), self.pins.enable.clone())
    }
    fn scan_in(&self) -> Term {
        Term::Pin(self.name.clone(), self.pins.scan_in.clone())
    }
    fn scan_out(&self) -> Term {
        Term::Pin(self.name.clone(), self.pins.scan_out.clone())
    }
}

/// Every chain in order, numbered from 0. `masters` names each cell's master.
pub fn stitch(
    d: &mut dyn StitchDesign,
    lib: &Library,
    masters: &dyn Fn(&str) -> String,
    config: &StitchConfig,
    chains: &[ScanChain],
    trace: &mut Vec<String>,
) -> Result<Vec<StitchedChain>, StitchError> {
    let mut out = Vec::new();
    for (ordinal, chain) in chains.iter().enumerate() {
        let mut cells = Vec::new();
        for c in &chain.cells {
            let master = masters(&c.name);
            let pins = lib.cells.get(&master).and_then(cell_pins).ok_or_else(|| StitchError::Refused(format!("{}: master {master} has no scan in, enable and out", c.name)))?;
            cells.push(Cellp { name: c.name.clone(), pins });
        }
        if cells.is_empty() {
            return Err(StitchError::Refused(format!("chain {} has no cells", chain.name)));
        }
        out.push(stitch_chain(d, config, &cells, ordinal, trace)?);
    }
    Ok(out)
}

fn stitch_chain(d: &mut dyn StitchDesign, config: &StitchConfig, cells: &[Cellp], ordinal: usize, trace: &mut Vec<String>) -> Result<StitchedChain, StitchError> {
    let refused = StitchError::Refused;
    let scan_enable_name = format_pattern(&config.enable_name_pattern, 0).map_err(refused)?;
    let scan_enable = find_or_create_driver(d, "scan-enable", &scan_enable_name)?;
    let scan_in_name = format_pattern(&config.in_name_pattern, ordinal).map_err(StitchError::Refused)?;
    let scan_in = find_or_create_driver(d, "scan-in", &scan_in_name)?;
    for c in cells {
        connect(d, &c.name, &c.enable(), &scan_enable, false, trace)?;
    }
    let first = &cells[0];
    let last = &cells[cells.len() - 1];
    // The cells between the first and the last.
    let middle = if cells.len() > 2 { &cells[1..cells.len() - 1] } else { &[][..] };
    for (i, current) in middle.iter().enumerate() {
        // ⚠️ The reference reads one past the end on the last middle cell; the slot it reads
        // still holds the last cell it popped, so the last middle cell feeds the last cell.
        let next = middle.get(i + 1).unwrap_or(last);
        connect(d, &next.name, &next.scan_in(), &current.scan_out(), false, trace)?;
    }
    connect(d, &first.name, &first.enable(), &scan_enable, false, trace)?;
    connect(d, &first.name, &first.scan_in(), &scan_in, false, trace)?;
    if let Some(second) = middle.first() {
        connect(d, &second.name, &second.scan_in(), &first.scan_out(), false, trace)?;
    } else if cells.len() > 1 {
        connect(d, &last.name, &last.scan_in(), &first.scan_out(), false, trace)?;
    }
    let scan_out_name = format_pattern(&config.out_name_pattern, ordinal).map_err(StitchError::Refused)?;
    let scan_out = find_or_create_scan_out(d, &last.scan_out(), &scan_out_name)?;
    connect(d, &last.name, &scan_out, &last.scan_out(), true, trace)?;
    Ok(StitchedChain { enable: scan_enable, scan_in, scan_out, cells: cells.iter().map(|c| (c.enable(), c.scan_in(), c.scan_out())).collect() })
}

/// `SplitTermIdentifier`: at the first `/` not preceded by a backslash (the backslash is kept).
fn split_term_identifier(input: &str) -> (String, Option<String>) {
    let b = input.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'/' && !(i != 0 && b[i - 1] == b'\\') {
            return (input[..i].to_string(), Some(input[i + 1..].to_string()));
        }
    }
    (input.to_string(), None)
}

fn error(code: u32, msg: String) -> StitchError {
    StitchError::Error(format!("[ERROR DFT-{code:04}] {msg}"))
}

/// An `instance/terminal` that must be an output, else an existing port, else a new input port.
pub fn find_or_create_driver(d: &mut dyn StitchDesign, kind: &str, with_name: &str) -> Result<Term, StitchError> {
    let (inst, term) = split_term_identifier(with_name);
    if let Some(term) = term {
        if !d.inst_exists(&inst) {
            return Err(error(34, format!("Instance {inst} not found for {kind} port")));
        }
        if !d.iterm_exists(&inst, &term) {
            return Err(error(35, format!("ITerm {inst}/{term} not found for {kind} port")));
        }
        let io = d.iterm_io_type(&inst, &term);
        if io != "OUTPUT" {
            return Err(error(36, format!("ITerm {inst}/{term} for {kind} port is a {io}")));
        }
        return Ok(Term::Pin(inst, term));
    }
    if d.bterm_exists(with_name) {
        // Its direction is not checked.
        return Ok(Term::Port(with_name.to_string()));
    }
    create_new_port(d, with_name, "INPUT")
}

/// An `instance/terminal` that must be an input, else an existing port that must be an output,
/// else an OUTPUT port already on the last cell's scan-out net, else a new output port.
pub fn find_or_create_scan_out(d: &mut dyn StitchDesign, cell_scan_out: &Term, with_name: &str) -> Result<Term, StitchError> {
    let (inst, term) = split_term_identifier(with_name);
    if let Some(term) = term {
        if !d.inst_exists(&inst) {
            return Err(error(37, format!("Instance {inst} not found for scan-out port")));
        }
        if !d.iterm_exists(&inst, &term) {
            return Err(error(38, format!("ITerm {inst}/{term} not found for scan-out port")));
        }
        let io = d.iterm_io_type(&inst, &term);
        if io != "INPUT" {
            return Err(error(39, format!("ITerm {inst}/{term} for scan-out port is a {io}")));
        }
        return Ok(Term::Pin(inst, term));
    }
    if d.bterm_exists(with_name) {
        if d.bterm_io_type(with_name) != "OUTPUT" {
            // (sic) the reference names the scan-enable kind here.
            return Err(error(40, format!("Top-level pin '{inst}' specified as scan-enable is not an output port")));
        }
        return Ok(Term::Port(with_name.to_string()));
    }
    // Only the top block is modelled, so the net is always the top block's.
    if let Some(net) = d.net_of(cell_scan_out) {
        if let Some(port) = d.net_bterms(&net).into_iter().find(|b| d.bterm_io_type(b) == "OUTPUT") {
            return Ok(Term::Port(port));
        }
    }
    create_new_port(d, with_name, "OUTPUT")
}

/// A new SCAN net and a SCAN port on it, of direction `io`.
fn create_new_port(d: &mut dyn StitchDesign, name: &str, io: &str) -> Result<Term, StitchError> {
    let refused = |e: String| StitchError::Refused(e);
    if !d.net_create(name).map_err(refused)? {
        return Err(error(31, format!("Error while attempting to create new port {name}: an unrelated net with the same name already exists")));
    }
    d.net_set_sig_type(name, "SCAN").map_err(refused)?;
    if !d.bterm_create(name, name).map_err(refused)? {
        return Err(error(18, format!("Failed to create port: a port named '{name}' already exists")));
    }
    d.bterm_set_sig_type(name, "SCAN").map_err(refused)?;
    d.bterm_set_io_type(name, io).map_err(refused)?;
    Ok(Term::Port(name.to_string()))
}

/// `Connect(load, driver, preserve)` for cell `cell`. Preserving: the driver's net if it has one,
/// else the load's net taken by the driver. Otherwise the load joins the driver's net, made (SCAN,
/// named after the driver) when the driver has none.
pub fn connect(d: &mut dyn StitchDesign, cell: &str, load: &Term, driver: &Term, preserve: bool, trace: &mut Vec<String>) -> Result<(), StitchError> {
    let refused = |e: String| StitchError::Refused(e);
    let (dn, ln) = (driver.term_name(), load.term_name());
    trace.push(format!("[DEBUG DFT-scan_cell_connect] Cell \"{cell}\", driver: \"{dn}\" -> load: \"{ln}\""));
    if preserve {
        if let Some(net) = d.net_of(driver) {
            trace.push(format!("[DEBUG DFT-scan_cell_connect]     Preserving connections 1: Connecting driver net \"{net}\" -> load pin \"{ln}\""));
            return d.connect(load, &net).map_err(refused);
        }
        if let Some(net) = d.net_of(load) {
            trace.push(format!("[DEBUG DFT-scan_cell_connect]     Preserving connections 2: Connecting driver pin \"{dn}\" -> load net \"{net}\""));
            return d.connect(driver, &net).map_err(refused);
        }
    }
    let net = match d.net_of(driver) {
        Some(net) => net,
        None => {
            let name = driver.full_name();
            if !d.net_create(&name).map_err(refused)? {
                return Err(error(30, format!("Failed to create driver net named '{name}'")));
            }
            d.net_set_sig_type(&name, "SCAN").map_err(refused)?;
            d.connect(driver, &name).map_err(refused)?;
            name
        }
    };
    trace.push(format!("[DEBUG DFT-scan_cell_connect]     New connection: Connecting driver net \"{net}\" -> load pin \"{ln}\""));
    d.connect(load, &net).map_err(refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A design that records every edit, in order.
    #[derive(Default)]
    struct D {
        nets: BTreeMap<String, Vec<Term>>,
        ports: BTreeMap<String, String>,
        insts: Vec<String>,
        ops: Vec<String>,
    }

    impl D {
        fn net_of_term(&self, t: &Term) -> Option<String> {
            self.nets.iter().find(|(_, ts)| ts.contains(t)).map(|(n, _)| n.clone())
        }
    }

    impl StitchDesign for D {
        fn inst_exists(&self, inst: &str) -> bool {
            self.insts.iter().any(|i| i == inst)
        }
        fn iterm_exists(&self, inst: &str, _: &str) -> bool {
            self.inst_exists(inst)
        }
        fn iterm_io_type(&self, _: &str, mterm: &str) -> String {
            if mterm == "Q" { "OUTPUT" } else { "INPUT" }.into()
        }
        fn bterm_exists(&self, port: &str) -> bool {
            self.ports.contains_key(port)
        }
        fn bterm_io_type(&self, port: &str) -> String {
            self.ports[port].clone()
        }
        fn net_of(&self, t: &Term) -> Option<String> {
            self.net_of_term(t)
        }
        fn net_bterms(&self, net: &str) -> Vec<String> {
            self.nets[net].iter().filter_map(|t| if let Term::Port(p) = t { Some(p.clone()) } else { None }).collect()
        }
        fn net_create(&mut self, name: &str) -> Result<bool, String> {
            self.ops.push(format!("net {name}"));
            Ok(self.nets.insert(name.into(), Vec::new()).is_none())
        }
        fn net_set_sig_type(&mut self, net: &str, sig: &str) -> Result<(), String> {
            self.ops.push(format!("sig {net} {sig}"));
            Ok(())
        }
        fn bterm_create(&mut self, net: &str, name: &str) -> Result<bool, String> {
            self.ops.push(format!("port {name}"));
            self.ports.insert(name.into(), String::new());
            self.nets.get_mut(net).unwrap().push(Term::Port(name.into()));
            Ok(true)
        }
        fn bterm_set_sig_type(&mut self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn bterm_set_io_type(&mut self, port: &str, io: &str) -> Result<(), String> {
            self.ops.push(format!("io {port} {io}"));
            self.ports.insert(port.into(), io.into());
            Ok(())
        }
        fn connect(&mut self, t: &Term, net: &str) -> Result<(), String> {
            for ts in self.nets.values_mut() {
                ts.retain(|x| x != t);
            }
            self.nets.get_mut(net).unwrap().push(t.clone());
            self.ops.push(format!("connect {} {net}", t.full_name()));
            Ok(())
        }
    }

    fn cells(names: &[&str]) -> Vec<Cellp> {
        names.iter().map(|n| Cellp { name: n.to_string(), pins: CellPins { enable: "SCE".into(), scan_in: "SCD".into(), scan_out: "Q".into() } }).collect()
    }

    fn design(names: &[&str]) -> D {
        let mut d = D { insts: names.iter().map(|n| n.to_string()).collect(), ..D::default() };
        for n in names {
            // Each flop drives its own functional net.
            d.nets.insert(format!("{n}_q"), vec![Term::Pin(n.to_string(), "Q".into())]);
        }
        d
    }

    fn scan_in_sources(d: &D, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .map(|n| {
                let net = d.net_of_term(&Term::Pin(n.to_string(), "SCD".into())).unwrap();
                d.nets[&net].iter().find(|t| !matches!(t, Term::Pin(_, p) if p == "SCD")).unwrap().full_name()
            })
            .collect()
    }

    #[test]
    fn a_chain_runs_from_the_scan_in_through_each_cell_in_order() {
        // Rules (ScanStitch::Stitch): first <- scan_in; each later cell <- the previous Q — the
        // last one through the past-the-end read, which yields the popped last cell.
        for names in [vec!["a"], vec!["a", "b"], vec!["a", "b", "c"], vec!["a", "b", "c", "d", "e"]] {
            let mut d = design(&names);
            let mut trace = Vec::new();
            let r = stitch_chain(&mut d, &StitchConfig::default(), &cells(&names), 3, &mut trace).unwrap();
            let mut want = vec!["scan_in_3".to_string()];
            want.extend(names.iter().take(names.len() - 1).map(|n| format!("{n}/Q")));
            assert_eq!(scan_in_sources(&d, &names), want, "{names:?}");
            assert_eq!((r.enable.clone(), r.scan_in.clone()), (Term::Port("scan_enable_0".into()), Term::Port("scan_in_3".into())));
            // The last cell's Q net has no output port: a new one, named for the chain.
            assert_eq!(r.scan_out, Term::Port("scan_out_3".into()));
        }
    }

    #[test]
    fn the_connect_sequence_is_the_references() {
        // Every cell's enable first, then the middle cells (each by the NEXT cell), then the first
        // cell's enable again and its scan in, then the second's, then the scan out preserving.
        let names = ["a", "b", "c", "d"];
        let mut d = design(&names);
        let mut trace = Vec::new();
        stitch_chain(&mut d, &StitchConfig::default(), &cells(&names), 0, &mut trace).unwrap();
        let heads: Vec<&str> = trace.iter().filter(|l| l.contains("Cell \"")).map(|l| l.split("] ").nth(1).unwrap()).collect();
        assert_eq!(
            heads,
            vec![
                "Cell \"a\", driver: \"scan_enable_0\" -> load: \"SCE\"",
                "Cell \"b\", driver: \"scan_enable_0\" -> load: \"SCE\"",
                "Cell \"c\", driver: \"scan_enable_0\" -> load: \"SCE\"",
                "Cell \"d\", driver: \"scan_enable_0\" -> load: \"SCE\"",
                "Cell \"c\", driver: \"Q\" -> load: \"SCD\"",
                "Cell \"d\", driver: \"Q\" -> load: \"SCD\"",
                "Cell \"a\", driver: \"scan_enable_0\" -> load: \"SCE\"",
                "Cell \"a\", driver: \"scan_in_0\" -> load: \"SCD\"",
                "Cell \"b\", driver: \"Q\" -> load: \"SCD\"",
                "Cell \"d\", driver: \"Q\" -> load: \"scan_out_0\"",
            ]
        );
        // The scan out preserves the last cell's Q net: the new port joins it.
        assert_eq!(trace.last().unwrap(), "[DEBUG DFT-scan_cell_connect]     Preserving connections 1: Connecting driver net \"d_q\" -> load pin \"scan_out_0\"");
        // Ports are made in order: enable, in, then out; the out net too.
        let made: Vec<&String> = d.ops.iter().filter(|o| o.starts_with("net ") || o.starts_with("port ")).collect();
        assert_eq!(made, vec!["net scan_enable_0", "port scan_enable_0", "net scan_in_0", "port scan_in_0", "net scan_out_0", "port scan_out_0"]);
        // Both drivers are made before any cell is connected; drivers are inputs, the scan out an output.
        assert_eq!(
            d.ops[..9],
            ["net scan_enable_0", "sig scan_enable_0 SCAN", "port scan_enable_0", "io scan_enable_0 INPUT", "net scan_in_0", "sig scan_in_0 SCAN", "port scan_in_0", "io scan_in_0 INPUT", "connect a/SCE scan_enable_0"]
        );
        assert_eq!(d.ports["scan_out_0"], "OUTPUT");
    }

    #[test]
    fn an_output_port_on_the_last_q_net_is_the_scan_out() {
        let names = ["a", "b"];
        let mut d = design(&names);
        d.ports.insert("out1".into(), "OUTPUT".into());
        d.ports.insert("in1".into(), "INPUT".into());
        d.nets.get_mut("b_q").unwrap().extend([Term::Port("in1".into()), Term::Port("out1".into())]);
        let mut trace = Vec::new();
        let r = stitch_chain(&mut d, &StitchConfig::default(), &cells(&names), 0, &mut trace).unwrap();
        assert_eq!(r.scan_out, Term::Port("out1".into()));
        assert!(!d.ports.contains_key("scan_out_0"));
    }

    #[test]
    fn a_driver_with_no_net_gets_a_scan_net_named_after_it() {
        // Rule (Connect): no driver net -> dbNet::create(driver->getName()) = "inst/mterm", SCAN.
        let names = ["a", "b"];
        let mut d = design(&names);
        d.nets.remove("a_q");
        let mut trace = Vec::new();
        stitch_chain(&mut d, &StitchConfig::default(), &cells(&names), 0, &mut trace).unwrap();
        assert!(d.ops.contains(&"net a/Q".to_string()) && d.ops.contains(&"sig a/Q SCAN".to_string()));
        assert!(trace.iter().any(|l| l.ends_with("New connection: Connecting driver net \"a/Q\" -> load pin \"SCD\"")));
    }

    #[test]
    fn an_existing_port_is_reused_and_named_terms_are_checked() {
        let names = ["a"];
        let mut d = design(&names);
        d.ports.insert("scan_in_0".into(), "OUTPUT".into());
        d.nets.insert("si".into(), vec![Term::Port("scan_in_0".into())]);
        let mut trace = Vec::new();
        let r = stitch_chain(&mut d, &StitchConfig::default(), &cells(&names), 0, &mut trace).unwrap();
        // A driver port's direction is not checked.
        assert_eq!(r.scan_in, Term::Port("scan_in_0".into()));
        // Names with a slash are instance terminals; a backslash-escaped slash is not a separator.
        assert_eq!(split_term_identifier("u1/Q"), ("u1".into(), Some("Q".into())));
        assert_eq!(split_term_identifier("a\\/b/Q"), ("a\\/b".into(), Some("Q".into())));
        assert_eq!(split_term_identifier("p"), ("p".into(), None));
        let mut d = design(&names);
        assert_eq!(find_or_create_driver(&mut d, "scan-in", "a/SCD"), Err(StitchError::Error("[ERROR DFT-0036] ITerm a/SCD for scan-in port is a INPUT".into())));
        assert_eq!(find_or_create_driver(&mut d, "scan-in", "z/Q"), Err(StitchError::Error("[ERROR DFT-0034] Instance z not found for scan-in port".into())));
        let q = Term::Pin("a".into(), "Q".into());
        assert_eq!(find_or_create_scan_out(&mut d, &q, "a/Q"), Err(StitchError::Error("[ERROR DFT-0039] ITerm a/Q for scan-out port is a OUTPUT".into())));
        d.ports.insert("so".into(), "INPUT".into());
        assert_eq!(find_or_create_scan_out(&mut d, &q, "so"), Err(StitchError::Error("[ERROR DFT-0040] Top-level pin 'so' specified as scan-enable is not an output port".into())));
    }

    #[test]
    fn patterns_take_one_number() {
        assert_eq!(format_pattern("scan_in_{}", 7).unwrap(), "scan_in_7");
        assert!(format_pattern("scan_in_{:02}", 7).is_err());
        assert!(format_pattern("scan_in", 7).is_err());
    }
}
