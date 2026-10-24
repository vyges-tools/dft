// SPDX-License-Identifier: Apache-2.0
//! Scan architecture: which scan cells go into which chain, and in what order.
//!
//! [`scan_architect`] is the sequence and nothing else; each stage below is one step of it, in
//! the order it runs:
//!
//! 1. [`ScanCellsBucket::init`] — cells grouped by clock-domain hash, each group stably sorted;
//! 2. [`infer_chain_count`] — per group, how many chains and how long;
//! 3. [`create_scan_chains`] — empty chains, numbered across groups in hash order;
//! 4. [`architect`] — each chain filled from the END of its group, then ordered;
//! 5. [`get_scan_chains`] — all chains, sorted by name.
//!
//! Everything here is pure: a cell is a name, a clock domain, a bit count and (when placed) an
//! origin, so the whole architecture is testable without a design database.

use std::collections::{BTreeMap, HashMap};

use vyges_drt::polygon90::Rect;
use vyges_drt::rtree::{DynRTree, Limits};

/// The clock edge a cell captures on. ⚠️ The DISCRIMINANT is part of a cell's domain id (its four
/// bytes are hashed), so these values are not free to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(i32)]
pub enum ClockEdge {
    Rising = 0,
    Falling = 1,
}

impl ClockEdge {
    pub fn name(self) -> &'static str {
        match self {
            ClockEdge::Rising => "rising",
            ClockEdge::Falling => "falling",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockDomain {
    pub clock: String,
    pub edge: ClockEdge,
}

/// 64-bit FNV-1a.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl ClockDomain {
    pub fn new(clock: &str, edge: ClockEdge) -> ClockDomain {
        ClockDomain { clock: clock.to_string(), edge }
    }

    /// 🔑 FNV-1a of the clock name XOR FNV-1a of the edge's four little-endian bytes. This id
    /// NUMBERS the chains (they are created in ascending id order), so it is transcribed, not
    /// replaced by any other hash.
    pub fn id(&self) -> u64 {
        fnv1a(self.clock.as_bytes()) ^ fnv1a(&(self.edge as i32).to_le_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanCell {
    pub name: String,
    pub domain: ClockDomain,
    pub bits: u64,
    /// The instance origin, when placed.
    pub origin: Option<(i32, i32)>,
}

/// How cells of different clock domains may share a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockMixing {
    /// Every clock domain (clock and edge) gets chains of its own.
    NoMix,
    /// All domains share one group; each chain still puts falling-edge cells first.
    ClockMix,
}

impl ClockMixing {
    pub fn name(self) -> &'static str {
        match self {
            ClockMixing::NoMix => "No Mix",
            ClockMixing::ClockMix => "Clock Mix",
        }
    }

    /// The group a domain falls in: its id with no mixing, one shared group (1) with mixing.
    fn hash(self, d: &ClockDomain) -> u64 {
        match self {
            ClockMixing::NoMix => d.id(),
            ClockMixing::ClockMix => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchitectConfig {
    pub clock_mixing: ClockMixing,
    /// The longest chain wanted, in bits; 200 when not given.
    pub max_length: Option<u64>,
    /// At most this many chains per group; lengthens chains rather than exceed it.
    pub max_chains: Option<u64>,
}

impl Default for ArchitectConfig {
    fn default() -> Self {
        ArchitectConfig { clock_mixing: ClockMixing::NoMix, max_length: None, max_chains: None }
    }
}

/// A chain as built: its cells in scan order (falling-edge cells first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanChain {
    pub name: String,
    pub cells: Vec<ScanCell>,
    pub bits: u64,
}

/// Per group: how many chains, and how many bits each may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashDomainLimits {
    pub chain_count: u64,
    pub max_length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchitectError {
    /// `max_chains` of 0 leaves nowhere to put a cell.
    NoChainsAllowed,
    /// The placed-cell walk found only the current cell among the 10 nearest (DFT-0010). The
    /// reference cannot reach it either: values are unique, so while another cell is in the tree
    /// one of the 10 nearest is not the current one.
    TooManyOverlappingCells,
}

/// The cells grouped by hash domain.
#[derive(Debug, Default)]
pub struct ScanCellsBucket {
    buckets: HashMap<u64, Vec<ScanCell>>,
}

/// The bucket order: by bits (fewest first); equal bits by clock name, and equal names by edge
/// (rising first). ⚠️ Equal clock AND edge compare equal, so the STABLE sort keeps the input
/// order there, which is name order.
fn compare_scan_cells(l: &ScanCell, r: &ScanCell) -> std::cmp::Ordering {
    if l.bits == r.bits {
        if l.domain.clock == r.domain.clock {
            return l.domain.edge.cmp(&r.domain.edge);
        }
        return l.domain.clock.as_bytes().cmp(r.domain.clock.as_bytes());
    }
    l.bits.cmp(&r.bits)
}

impl ScanCellsBucket {
    /// Each cell into its group, then each group stably sorted.
    pub fn init(config: &ArchitectConfig, cells: Vec<ScanCell>) -> ScanCellsBucket {
        let mut buckets: HashMap<u64, Vec<ScanCell>> = HashMap::new();
        for c in cells {
            buckets.entry(config.clock_mixing.hash(&c.domain)).or_default().push(c);
        }
        for v in buckets.values_mut() {
            v.sort_by(compare_scan_cells); // stable, as the reference's stable_sort
        }
        ScanCellsBucket { buckets }
    }

    pub fn total_bits_per_hash_domain(&self) -> HashMap<u64, u64> {
        self.buckets.iter().map(|(h, v)| (*h, v.iter().map(|c| c.bits).sum())).collect()
    }

    /// 🔑 Takes from the BACK: the last cell in bucket order goes into a chain first.
    fn pop(&mut self, hash: u64) -> Option<ScanCell> {
        self.buckets.get_mut(&hash)?.pop()
    }

    fn number_of_cells(&self, hash: u64) -> usize {
        self.buckets.get(&hash).map_or(0, Vec::len)
    }
}

/// `ceil(bits / max_length)` chains, capped by `max_chains`; then `ceil(bits / chains)` per chain.
pub fn infer_chain_count_from_max_length(
    total_bits: &HashMap<u64, u64>,
    max_length: u64,
    max_chains: Option<u64>,
) -> Result<BTreeMap<u64, HashDomainLimits>, ArchitectError> {
    let mut out = BTreeMap::new();
    for (&hash, &bits) in total_bits {
        let mut count = bits.div_ceil(max_length);
        if let Some(m) = max_chains {
            count = count.min(m);
        }
        if count == 0 {
            return Err(ArchitectError::NoChainsAllowed);
        }
        out.insert(hash, HashDomainLimits { chain_count: count, max_length: bits.div_ceil(count) });
    }
    Ok(out)
}

pub fn infer_chain_count(
    config: &ArchitectConfig,
    bucket: &ScanCellsBucket,
) -> Result<BTreeMap<u64, HashDomainLimits>, ArchitectError> {
    infer_chain_count_from_max_length(
        &bucket.total_bits_per_hash_domain(),
        config.max_length.unwrap_or(200),
        config.max_chains,
    )
}

/// Empty chains, named `chain_<n>` with `n` counting across groups IN ASCENDING HASH ORDER.
pub fn create_scan_chains(limits: &BTreeMap<u64, HashDomainLimits>) -> BTreeMap<u64, Vec<ScanChain>> {
    let mut n = 0u64;
    let mut out: BTreeMap<u64, Vec<ScanChain>> = BTreeMap::new();
    for (&hash, l) in limits {
        for _ in 0..l.chain_count {
            out.entry(hash).or_default().push(ScanChain { name: format!("chain_{n}"), cells: vec![], bits: 0 });
            n += 1;
        }
    }
    out
}

/// The heuristic: per group, fill each chain in turn while it is under the group's length and
/// cells remain; then order it — falling-edge cells first, each half by wirelength.
pub fn architect(
    chains: &mut BTreeMap<u64, Vec<ScanChain>>,
    limits: &BTreeMap<u64, HashDomainLimits>,
    bucket: &mut ScanCellsBucket,
) -> Result<(), ArchitectError> {
    for (&hash, group) in chains.iter_mut() {
        let max_length = limits[&hash].max_length;
        for chain in group.iter_mut() {
            let (mut falling, mut rising) = (Vec::new(), Vec::new());
            while chain.bits < max_length && bucket.number_of_cells(hash) > 0 {
                let cell = bucket.pop(hash).expect("counted");
                chain.bits += cell.bits;
                match cell.domain.edge {
                    ClockEdge::Rising => rising.push(cell),
                    ClockEdge::Falling => falling.push(cell),
                }
            }
            optimize_scan_wirelength(&mut falling)?;
            optimize_scan_wirelength(&mut rising)?;
            chain.cells = falling;
            chain.cells.append(&mut rising);
        }
    }
    Ok(())
}

/// The cells looked at for the next one in the walk.
const MAX_CELLS_TO_SEARCH: usize = 10;

/// Unplaced cells keep their order (the reference returns early unless EVERY cell is placed).
/// Placed cells are walked nearest to nearest:
///
/// - the start is the lower-leftmost — the lowest `x + y` of the origin (summed as 32-bit ints),
///   the first on a tie;
/// - the origins go into an R*-tree of 4 ([`Limits::rstar`]), bulk loaded in cell order;
/// - each step takes the first of the 10 nearest the current cell that is not it — the tree's
///   own order among equal distances, which decides most steps on placed rows — THEN removes the
///   current cell from the tree.
fn optimize_scan_wirelength(cells: &mut Vec<ScanCell>) -> Result<(), ArchitectError> {
    if cells.is_empty() || cells.iter().any(|c| c.origin.is_none()) {
        return Ok(());
    }
    let origin = |c: &ScanCell| c.origin.expect("placed");
    let (mut start, mut lowest) = (0usize, i64::MAX);
    for (i, c) in cells.iter().enumerate() {
        let (x, y) = origin(c);
        let dist = i64::from(x.wrapping_add(y));
        if dist < lowest {
            start = i;
            lowest = dist;
        }
    }
    let values: Vec<(Rect, usize)> = cells.iter().enumerate().map(|(i, c)| {
        let (x, y) = origin(c);
        (Rect { xl: x, yl: y, xh: x, yh: y }, i)
    }).collect();
    let mut tree = DynRTree::with_limits(values, Limits::rstar(4));
    let mut cursor = start;
    let mut order = vec![cursor];
    while order.len() < cells.len() {
        let next = tree.nearest(origin(&cells[cursor]), MAX_CELLS_TO_SEARCH).find(|&i| i != cursor);
        let Some(next) = next else {
            return Err(ArchitectError::TooManyOverlappingCells);
        };
        tree.remove(cursor);
        cursor = next;
        order.push(cursor);
    }
    let mut slots: Vec<Option<ScanCell>> = std::mem::take(cells).into_iter().map(Some).collect();
    *cells = order.into_iter().map(|i| slots[i].take().expect("visited once")).collect();
    Ok(())
}

/// Every chain, sorted by name AS A STRING: `chain_10` comes before `chain_2`.
pub fn get_scan_chains(chains: BTreeMap<u64, Vec<ScanChain>>) -> Vec<ScanChain> {
    let mut flat: Vec<ScanChain> = chains.into_values().flatten().collect();
    flat.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    flat
}

/// The architecture, as a sequence. `cells` come in sorted by name (the collector's order).
pub fn scan_architect(config: &ArchitectConfig, cells: Vec<ScanCell>) -> Result<Vec<ScanChain>, ArchitectError> {
    let mut bucket = ScanCellsBucket::init(config, cells);
    let limits = infer_chain_count(config, &bucket)?;
    let mut chains = create_scan_chains(&limits);
    architect(&mut chains, &limits, &mut bucket)?;
    Ok(get_scan_chains(chains))
}

/// The plan report, line for line.
pub fn report_dft_plan(config: &ArchitectConfig, chains: &[ScanChain], verbose: bool) -> String {
    let mut o = String::new();
    o.push_str("***************************\n");
    o.push_str("Report DFT Plan\n");
    o.push_str(&format!("Number of chains: {}\n", chains.len()));
    o.push_str(&format!("Clock domain: {}\n", config.clock_mixing.name()));
    o.push_str("***************************\n\n");
    for c in chains {
        report_chain(&mut o, c, verbose);
    }
    o.push('\n');
    o
}

/// The chain line; with `verbose`, one line per cell, naming the clock wherever the domain
/// changes, then a blank line.
fn report_chain(o: &mut String, c: &ScanChain, verbose: bool) {
    o.push_str(&format!("Scan chain '{}' has {} cells ({} bits)\n\n", c.name, c.cells.len(), c.bits));
    if !verbose {
        return;
    }
    // ⚠️ Starts at 0, not "none": a domain whose id were 0 would not be named on its first cell.
    let mut current = 0u64;
    for cell in &c.cells {
        let id = cell.domain.id();
        if id != current {
            o.push_str(&format!("  {} ({}, {})\n", cell.name, cell.domain.clock, cell.domain.edge.name()));
            current = id;
        } else {
            o.push_str(&format!("  {}\n", cell.name));
        }
    }
    o.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(name: &str, clock: &str, edge: ClockEdge) -> ScanCell {
        ScanCell { name: name.to_string(), domain: ClockDomain::new(clock, edge), bits: 1, origin: None }
    }

    fn no_mix(max_length: u64) -> ArchitectConfig {
        ArchitectConfig { clock_mixing: ClockMixing::NoMix, max_length: Some(max_length), max_chains: None }
    }

    fn sorted_by_name(mut v: Vec<ScanCell>) -> Vec<ScanCell> {
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    #[test]
    fn chain_count_and_length_per_hash_domain() {
        // Ported from the reference's unit test: ceil(bits/len) chains, then ceil(bits/chains).
        let lim = |c, m| HashDomainLimits { chain_count: c, max_length: m };
        let run = |pairs: &[(u64, u64)]| {
            infer_chain_count_from_max_length(&pairs.iter().copied().collect(), 5, None).unwrap()
        };
        assert_eq!(run(&[(1, 10)])[&1], lim(2, 5));
        assert_eq!(run(&[(1, 9)])[&1], lim(2, 5));
        assert_eq!(run(&[(1, 6)])[&1], lim(2, 3));
        let two = run(&[(1, 10), (2, 10)]);
        assert_eq!((two[&1], two[&2]), (lim(2, 5), lim(2, 5)));
        let two = run(&[(1, 10), (2, 9)]);
        assert_eq!((two[&1], two[&2]), (lim(2, 5), lim(2, 5)));
        let two = run(&[(1, 10), (2, 15)]);
        assert_eq!((two[&1], two[&2]), (lim(2, 5), lim(3, 5)));
        // A maximum of one chain takes every bit.
        let one = infer_chain_count_from_max_length(&[(1, 10)].into_iter().collect(), 5, Some(1)).unwrap();
        assert_eq!(one[&1], lim(1, 10));
    }

    #[test]
    fn max_chains_caps_the_count_and_lengthens_the_chains() {
        let l = infer_chain_count_from_max_length(&[(1, 10)].into_iter().collect(), 2, Some(3)).unwrap();
        assert_eq!(l[&1], HashDomainLimits { chain_count: 3, max_length: 4 });
        assert_eq!(
            infer_chain_count_from_max_length(&[(1, 10)].into_iter().collect(), 2, Some(0)),
            Err(ArchitectError::NoChainsAllowed)
        );
    }

    #[test]
    fn one_clock_domain_no_mix_fills_two_chains() {
        // Ported: 20 cells, max length 10 -> 2 chains, every cell placed, no chain empty.
        let cells = sorted_by_name((0..20).map(|i| cell(&format!("scan_cell{i}"), "clk1", ClockEdge::Rising)).collect());
        let chains = scan_architect(&no_mix(10), cells).unwrap();
        assert_eq!(chains.len(), 2);
        assert!(chains.iter().all(|c| !c.cells.is_empty()));
        assert_eq!(chains.iter().map(|c| c.cells.len()).sum::<usize>(), 20);
    }

    #[test]
    fn two_clocks_or_two_edges_no_mix_get_their_own_chains() {
        // Ported: 20 + 15 cells in two domains -> 4 chains, bits kept per edge.
        let mut v: Vec<ScanCell> = (0..20).map(|i| cell(&format!("scan_cell{i}"), "clk1", ClockEdge::Rising)).collect();
        v.extend((20..35).map(|i| cell(&format!("scan_cell{i}"), "clk2", ClockEdge::Rising)));
        assert_eq!(scan_architect(&no_mix(10), sorted_by_name(v)).unwrap().len(), 4);
        let mut v: Vec<ScanCell> = (0..20).map(|i| cell(&format!("scan_cell{i}"), "clk1", ClockEdge::Rising)).collect();
        v.extend((20..35).map(|i| cell(&format!("scan_cell{i}"), "clk1", ClockEdge::Falling)));
        let chains = scan_architect(&no_mix(10), sorted_by_name(v)).unwrap();
        assert_eq!(chains.len(), 4);
        let bits = |e| chains.iter().flat_map(|c| &c.cells).filter(|c| c.domain.edge == e).count();
        assert_eq!((bits(ClockEdge::Rising), bits(ClockEdge::Falling)), (20, 15));
    }

    #[test]
    fn the_no_mix_plan_report_matches_the_reference_golden() {
        // The reference's `scan_architect_no_mix_sky130` plan: 2 clocks x 2 edges x 5 flops,
        // max length 10. The chain NUMBERS come from ascending FNV domain ids (clock1/falling
        // is chain_0, clock1/rising chain_3); within a chain the cells come off the END of the
        // name-sorted bucket, so `ff10` (sorting before `ff2`) is last.
        let mut v = Vec::new();
        for (clk, tag) in [("clock1", "clk1"), ("clock2", "clk2")] {
            let idx: &[u32] = if tag == "clk1" { &[1, 3, 5, 7, 9] } else { &[2, 4, 6, 8, 10] };
            for &i in idx {
                for (e, en) in [(ClockEdge::Rising, "rising"), (ClockEdge::Falling, "falling")] {
                    v.push(cell(&format!("ff{i}_{tag}_{en}"), clk, e));
                }
            }
        }
        let config = no_mix(10);
        let chains = scan_architect(&config, sorted_by_name(v)).unwrap();
        let want = "***************************
Report DFT Plan
Number of chains: 4
Clock domain: No Mix
***************************

Scan chain 'chain_0' has 5 cells (5 bits)

  ff9_clk1_falling (clock1, falling)
  ff7_clk1_falling
  ff5_clk1_falling
  ff3_clk1_falling
  ff1_clk1_falling

Scan chain 'chain_1' has 5 cells (5 bits)

  ff8_clk2_falling (clock2, falling)
  ff6_clk2_falling
  ff4_clk2_falling
  ff2_clk2_falling
  ff10_clk2_falling

Scan chain 'chain_2' has 5 cells (5 bits)

  ff8_clk2_rising (clock2, rising)
  ff6_clk2_rising
  ff4_clk2_rising
  ff2_clk2_rising
  ff10_clk2_rising

Scan chain 'chain_3' has 5 cells (5 bits)

  ff9_clk1_rising (clock1, rising)
  ff7_clk1_rising
  ff5_clk1_rising
  ff3_clk1_rising
  ff1_clk1_rising

";
        let got = report_dft_plan(&config, &chains, true);
        assert!(got.starts_with(want), "{got}");
    }

    #[test]
    fn chain_names_sort_as_strings() {
        // Rule: the final sort is by name as a string, so chain_10 precedes chain_2.
        let cells = sorted_by_name((0..11).map(|i| cell(&format!("c{i:02}"), "clk", ClockEdge::Rising)).collect());
        let names: Vec<String> = scan_architect(&no_mix(1), cells).unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(&names[..3], &["chain_0", "chain_1", "chain_10"]);
    }

    #[test]
    fn falling_edge_cells_lead_a_mixed_chain() {
        let config = ArchitectConfig { clock_mixing: ClockMixing::ClockMix, max_length: Some(10), max_chains: None };
        let cells = sorted_by_name(vec![
            cell("a", "clk1", ClockEdge::Rising),
            cell("b", "clk2", ClockEdge::Falling),
            cell("c", "clk1", ClockEdge::Falling),
        ]);
        let chains = scan_architect(&config, cells).unwrap();
        let edges: Vec<ClockEdge> = chains[0].cells.iter().map(|c| c.domain.edge).collect();
        assert_eq!(edges, [ClockEdge::Falling, ClockEdge::Falling, ClockEdge::Rising]);
    }

    fn placed(origins: &[(i32, i32)]) -> Vec<ScanCell> {
        origins.iter().enumerate().map(|(i, &o)| ScanCell { origin: Some(o), ..cell(&i.to_string(), "clk", ClockEdge::Rising) }).collect()
    }

    fn names(cells: &[ScanCell]) -> Vec<usize> {
        cells.iter().map(|c| c.name.parse().unwrap()).collect()
    }

    // The walk from the lower-leftmost cell, each step to the first of the 10 nearest that is
    // not the current cell, in the R*-tree's order among equal distances: a 4 x 3 grid of pitch
    // 10 and two more cells on (10, 10). From (10, 0) the cells at (20, 0) and the three at
    // (10, 10) are equally near; the tree's order takes cell 13 — the reference's walk.
    #[test]
    fn placed_cells_walk_nearest_first_in_the_reference_order() {
        let mut cells = placed(&[(0, 0), (10, 0), (20, 0), (30, 0), (0, 10), (10, 10), (20, 10), (30, 10), (0, 20), (10, 20), (20, 20), (30, 20), (10, 10), (10, 10)]);
        optimize_scan_wirelength(&mut cells).unwrap();
        assert_eq!(names(&cells), [0, 1, 13, 5, 12, 9, 8, 4, 6, 7, 3, 2, 10, 11]);
    }

    // The start is the lowest x + y, the FIRST on a tie: (5, 0) and (0, 5) tie; cell 1 starts.
    #[test]
    fn the_walk_starts_at_the_first_lowest_x_plus_y() {
        let mut cells = placed(&[(50, 50), (5, 0), (0, 5)]);
        optimize_scan_wirelength(&mut cells).unwrap();
        assert_eq!(names(&cells), [1, 2, 0]);
    }

    // One unplaced cell and the reference returns before walking: the order stays as given.
    #[test]
    fn one_unplaced_cell_keeps_the_given_order() {
        let mut cells = placed(&[(50, 50), (5, 0), (0, 5)]);
        cells[2].origin = None;
        optimize_scan_wirelength(&mut cells).unwrap();
        assert_eq!(names(&cells), [0, 1, 2]);
    }
}
