// SPDX-License-Identifier: Apache-2.0
//! The design database and the timer, as the stages read them.
//!
//! [`pin_clocks`] is `Sta::clocks(pin)` for every pin at once: the netlist as the database holds it,
//! the clocks of the constraints, arrivals found, and per pin the clocks of its CLOCK paths in
//! clock index order (`ClockSet` is ordered by index).

use std::collections::{BTreeSet, HashMap};

use vyges_opendb::{Db, DftScanChain, DftScanInst, DftTerm};
use vyges_sta::graph::Graph;
use vyges_sta::liberty::Library;
use vyges_sta::sdc::{user_to_sta, Clock, Sdc};
use vyges_sta::search::Search;

use crate::architect::{ClockEdge, ScanChain};
use crate::collect::CollectDesign;
use crate::stitch::{StitchDesign, StitchedChain, Term};

/// The constraints' clocks as the timer takes them, in the order they were made. Each must be on
/// one port: a virtual clock, a clock on an instance pin, or one on several ports is refused.
pub fn timer_clocks(sdc: &vyges_loom::sdc::Sdc, time_scale: f32) -> Result<Sdc, String> {
    let mut clocks = Vec::new();
    for c in &sdc.clocks {
        let [source] = c.sources.as_slice() else {
            return Err(format!("clock {} on {} sources: one source port is modelled", c.name, c.sources.len()));
        };
        if source.contains('/') {
            return Err(format!("clock {} on pin {source}: a clock on a port is modelled", c.name));
        }
        clocks.push(Clock::new(&c.name, user_to_sta(c.period, time_scale), source, false));
    }
    Ok(Sdc { clocks, input_delays: Vec::new(), output_delays: Vec::new(), path_delays: Vec::new() })
}

/// `Sta::clocks(pin)` for every pin a clock reaches, by pin name (`inst/pin`), in index order.
pub fn pin_clocks(libs: &[Library], netlist: &vyges_sta::netlist::Netlist, sdc: &Sdc) -> Result<HashMap<String, Vec<String>>, String> {
    let mut g = Graph::build(libs, netlist)?;
    g.find_delays(&HashMap::new(), None)?;
    let mut s = Search::in_graph_order(&g, sdc);
    s.find_arrivals()?;
    let mut at: HashMap<String, BTreeSet<usize>> = HashMap::new();
    // A pin's load vertex and its bidirect driver vertex share its name: both are read.
    for (v, vertex) in g.vertices.iter().enumerate() {
        for p in &s.paths[v] {
            if let (true, Some(e)) = (p.tag.is_clock, p.tag.clk_edge) {
                at.entry(vertex.name.clone()).or_default().insert(e / 2);
            }
        }
    }
    Ok(at.into_iter().map(|(pin, idx)| (pin, idx.into_iter().map(|i| sdc.clocks[i].name.clone()).collect())).collect())
}

/// The database's top block, with the clocks found by [`pin_clocks`].
pub struct OdbCollect<'a> {
    pub db: &'a Db,
    pub clocks: HashMap<String, Vec<String>>,
}

impl CollectDesign for OdbCollect<'_> {
    fn insts(&self) -> Vec<String> {
        self.db.inst_names()
    }
    fn inst_is_do_not_touch(&self, inst: &str) -> bool {
        self.db.inst_is_do_not_touch(inst)
    }
    fn inst_master(&self, inst: &str) -> String {
        self.db.inst_master(inst)
    }
    fn master_is_block(&self, master: &str) -> bool {
        self.db.master_is_block(master)
    }
    fn inst_iterm_sig_types(&self, inst: &str) -> Vec<(String, String)> {
        self.db.iterm_names(inst).into_iter().map(|t| {
            let sig = self.db.iterm_get_sig_type(inst, &t);
            (t, sig)
        }).collect()
    }
    fn inst_is_placed(&self, inst: &str) -> bool {
        self.db.inst_is_placed(inst)
    }
    fn inst_origin(&self, inst: &str) -> (i32, i32) {
        (self.db.inst_get_origin_x(inst), self.db.inst_get_origin_y(inst))
    }
    fn clocks(&self, inst: &str, pin: &str) -> Vec<String> {
        self.clocks.get(&format!("{inst}/{pin}")).cloned().unwrap_or_default()
    }
}

/// The database's top block, as stitching edits it.
pub struct OdbStitch<'a> {
    pub db: &'a mut Db,
}

fn some(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

impl StitchDesign for OdbStitch<'_> {
    fn inst_exists(&self, inst: &str) -> bool {
        !self.db.inst_master(inst).is_empty()
    }
    fn iterm_exists(&self, inst: &str, mterm: &str) -> bool {
        !self.db.iterm_get_io_type(inst, mterm).is_empty()
    }
    fn iterm_io_type(&self, inst: &str, mterm: &str) -> String {
        self.db.iterm_get_io_type(inst, mterm)
    }
    fn bterm_exists(&self, port: &str) -> bool {
        !self.db.bterm_get_io_type(port).is_empty()
    }
    fn bterm_io_type(&self, port: &str) -> String {
        self.db.bterm_get_io_type(port)
    }
    fn net_of(&self, t: &Term) -> Option<String> {
        match t {
            Term::Port(p) => some(self.db.bterm_get_net(p)),
            Term::Pin(i, m) => some(self.db.iterm_get_net(i, m)),
        }
    }
    fn net_bterms(&self, net: &str) -> Vec<String> {
        self.db.net_bterms(net)
    }
    fn net_create(&mut self, name: &str) -> Result<bool, String> {
        self.db.net_try_create(name).map_err(|e| e.to_string())
    }
    fn net_set_sig_type(&mut self, net: &str, sig: &str) -> Result<(), String> {
        self.db.net_set_sig_type(net, sig).map_err(|e| e.to_string())
    }
    fn bterm_create(&mut self, net: &str, name: &str) -> Result<bool, String> {
        self.db.bterm_try_create(net, name).map_err(|e| e.to_string())
    }
    fn bterm_set_sig_type(&mut self, port: &str, sig: &str) -> Result<(), String> {
        self.db.bterm_set_sig_type(port, sig).map_err(|e| e.to_string())
    }
    fn bterm_set_io_type(&mut self, port: &str, io: &str) -> Result<(), String> {
        self.db.bterm_set_io_type(port, io).map_err(|e| e.to_string())
    }
    fn connect(&mut self, t: &Term, net: &str) -> Result<(), String> {
        match t {
            Term::Port(p) => self.db.bterm_connect(p, net),
            Term::Pin(i, m) => self.db.connect(i, m, net),
        }
        .map_err(|e| e.to_string())
    }
}

fn odb_term(t: &Term) -> DftTerm {
    match t {
        Term::Port(p) => DftTerm::Port(p.clone()),
        Term::Pin(i, m) => DftTerm::Pin(i.clone(), m.clone()),
    }
}

/// The chains as `execute_dft_plan` records them: per cell its bits, enable, scan in and out, and
/// clock domain; per chain its enable, in and out.
pub fn scan_chain_records(chains: &[ScanChain], stitched: &[StitchedChain]) -> Vec<DftScanChain> {
    chains
        .iter()
        .zip(stitched)
        .map(|(c, s)| DftScanChain {
            name: c.name.clone(),
            cells: c
                .cells
                .iter()
                .zip(&s.cells)
                .map(|(cell, (enable, scan_in, scan_out))| DftScanInst {
                    inst: cell.name.clone(),
                    bits: cell.bits as u32,
                    enable: odb_term(enable),
                    scan_in: odb_term(scan_in),
                    scan_out: odb_term(scan_out),
                    clock: cell.domain.clock.clone(),
                    falling: cell.domain.edge == ClockEdge::Falling,
                })
                .collect(),
            enable: odb_term(&s.enable),
            scan_in: odb_term(&s.scan_in),
            scan_out: odb_term(&s.scan_out),
        })
        .collect()
}
