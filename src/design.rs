// SPDX-License-Identifier: Apache-2.0
//! The design database and the timer, as the stages read them.
//!
//! [`pin_clocks`] is `Sta::clocks(pin)` for every pin at once: the netlist as the database holds it,
//! the clocks of the constraints, arrivals found, and per pin the clocks of its CLOCK paths in
//! clock index order (`ClockSet` is ordered by index).

use std::collections::{BTreeSet, HashMap};

use vyges_opendb::Db;
use vyges_sta::graph::Graph;
use vyges_sta::liberty::Library;
use vyges_sta::sdc::{user_to_sta, Clock, Sdc};
use vyges_sta::search::Search;

use crate::collect::CollectDesign;

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
