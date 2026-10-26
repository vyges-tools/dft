// SPDX-License-Identifier: Apache-2.0
//! `vyges-dft` — scan insertion and stitching, so a chip can be tested after fabrication.
//!
//! ## Exit codes
//!
//! ⛔ **"found something" and "could not run" never share a code.** A CI gate that cannot tell a
//! broken tool from a bad design will eventually read a missing engine as a clean run.
//!
//! | code | meaning |
//! | --- | --- |
//! | 0 | ok |
//! | 1 | a finding (the check ran and failed) |
//! | 2 | usage, or input that could not be read |
//! | 3 | the command is NOT IMPLEMENTED in this build |

use std::process::ExitCode;

const USAGE: &str = "\
vyges physical dft — scan insertion and stitching, so a chip can be tested after fabrication

USAGE:
  vyges physical dft replace <design.odb> --liberty FILE... -o OUT.odb
                                                  replace each flop by its closest scan equivalent
  vyges physical dft run     <design.odb> --liberty FILE... --sdc FILE -o OUT.odb [--max-length N]
                           [--max-chains N] [--clock-mixing no_mix|clock_mix] [--trace]
                                                  stitch the planned chains and record them
  vyges physical dft report  <design.odb> --liberty FILE... --sdc FILE [--max-length N]
                           [--max-chains N] [--clock-mixing no_mix|clock_mix] [--verbose]
                                                  the scan chains as planned
  vyges physical dft --describe
  vyges physical dft --help
  vyges physical dft --version

OPTIONS:
  -o FILE               write the report (or the output database) to FILE
  --liberty FILE        a Liberty library (repeatable, in read order; a cell resolves to the
                        first library that has it)
  --json                machine-readable output instead of the text report
  -q, --quiet           suppress non-essential output
  -v, --verbose         extra detail on stderr
  --describe            print a machine-readable JSON description of the command
  --bug-report          file a bug (central: vyges/community)
  --feature-request     request a feature (central)

EXIT STATUS:
  0  ok
  1  a finding — the check ran and failed
  2  usage or unreadable input
  3  refused — this build does not implement the command yet
";

/// The commands this engine will answer. One not yet built is refused (exit 3).
pub const COMMANDS: &[&str] = &["replace", "run", "report"];

const DESCRIBE: &str = r#"{
  "schema": "vyges-tool-descriptor/1.1",
  "name": "dft",
  "summary": "Scan insertion and stitching, so a chip can be tested after fabrication.",
  "maturity": "discovered",
  "provenance_limitations": [
    "`replace`, `report` and `run` are built; scan port name patterns are the defaults.",
    "`report` and `run` refuse a design with child blocks, and constraints whose clocks are not each on one port.",
    "`replace` refuses a library where two scan cells are equally close in drive to a flop, or where a pg_pin has no known pg_type."
  ],
  "invocation": { "args_template": ["replace"], "emits_json": false },
  "inputs": { "type": "object", "properties": {} },
  "artifacts": [],
  "assertion": { "id": "dft-ok", "field": "status", "pass_when": { "eq": "ok" } },
  "consumes": []
}"#;

fn refuse(what: &str) -> ExitCode {
    vyges_events::emit(
        &vyges_events::Event::new("vyges-dft", vyges_events::Severity::Error, format!("{what} is not implemented in this build"))
            .with_code("DFT-REFUSED"),
    );
    ExitCode::from(3)
}

fn input_error(msg: &str) -> ExitCode {
    vyges_events::emit(&vyges_events::Event::new("vyges-dft", vyges_events::Severity::Error, msg.to_string()).with_code("DFT-INPUT"));
    ExitCode::from(2)
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("vyges-dft: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

/// `replace`: the library half decides each master's class; the database walks its instances in
/// its own order and replaces them. Log lines are the reference's.
fn replace(args: &[String]) -> ExitCode {
    let (mut pos, mut out, mut liberty): (Vec<String>, _, Vec<String>) = (Vec::new(), None, Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => match it.next() {
                Some(f) => out = Some(f.clone()),
                None => return usage_error("-o needs a file"),
            },
            "--liberty" => match it.next() {
                Some(f) => liberty.push(f.clone()),
                None => return usage_error("--liberty needs a file"),
            },
            "-q" | "--quiet" | "-v" | "--verbose" => {}
            f if f.starts_with('-') => return usage_error(&format!("unknown option `{f}`")),
            f => pos.push(f.to_string()),
        }
    }
    let ([design], Some(out)) = (<[String; 1]>::try_from(pos).unwrap_or_default(), out) else {
        return usage_error("replace needs a design database and -o OUT.odb");
    };
    if design.is_empty() {
        return usage_error("replace needs a design database and -o OUT.odb");
    }
    let mut libs = Vec::new();
    for f in &liberty {
        let read = std::fs::read_to_string(f)
            .map_err(|e| e.to_string())
            .and_then(|text| vyges_sta::liberty_parse::parse(&text))
            .and_then(|g| vyges_sta::liberty::Library::read(&g));
        match read {
            Ok(lib) => libs.push(lib),
            Err(e) => return input_error(&format!("cannot read {f}: {e}")),
        }
    }
    let lib = vyges_dft::replace::merge_libraries(libs).unwrap_or_default();
    let mut db = match vyges_opendb::Db::open(&design) {
        Ok(db) => db,
        Err(e) => return input_error(&format!("cannot read {design}: {e}")),
    };
    let masters = match db.masters_in_lib_order() {
        Ok(m) => m,
        Err(e) => return input_error(&format!("cannot list the masters of {design}: {e}")),
    };
    let classes = match vyges_dft::replace::master_classes(&lib, &masters) {
        Ok(c) => c,
        Err(e) => {
            vyges_events::emit(&vyges_events::Event::new("vyges-dft", vyges_events::Severity::Error, format!("{e:?}; nothing written")).with_code("DFT-REFUSED"));
            return ExitCode::from(3);
        }
    };
    use vyges_dft::replace::MasterClass;
    use vyges_opendb::DftMasterClass;
    let table: Vec<(String, DftMasterClass)> = classes
        .into_iter()
        .map(|(m, c)| {
            let c = match c {
                MasterClass::Leave => DftMasterClass::Leave,
                MasterClass::AlreadyScan => DftMasterClass::AlreadyScan,
                MasterClass::NoEquivalent => DftMasterClass::NoEquivalent,
                MasterClass::Replace(r) => DftMasterClass::Replace { to: r.scan_cell, ports: r.port_mapping.into_iter().collect() },
            };
            (m, c)
        })
        .collect();
    let events = match db.dft_scan_replace(&table) {
        Ok(e) => e,
        Err(e) => return input_error(&format!("{design}: {e}")),
    };
    let mut replaced = 0;
    for (kind, inst, master) in &events {
        match kind.as_str() {
            "no-lib" => println!("[WARNING DFT-0012] Cell master '{master}' has no lib info. Can't create scan cell(s)"),
            "already-scan" => println!("[INFO DFT-0003] Cell '{inst}' is already an scan cell, we will not replace it"),
            "no-equivalent" => {
                println!("[WARNING DFT-0002] Can't scan replace cell '{inst}', that has lib cell '{master}'. No scan equivalent lib cell found")
            }
            _ => replaced += 1,
        }
    }
    if let Err(e) = db.write(&out) {
        return input_error(&format!("cannot write {out}: {e}"));
    }
    eprintln!("vyges-dft: {replaced} cell(s) scan-replaced; wrote {out}");
    ExitCode::SUCCESS
}

/// The libraries as read, in order.
fn read_libraries(files: &[String]) -> Result<Vec<vyges_sta::liberty::Library>, ExitCode> {
    let mut libs = Vec::new();
    for f in files {
        let read = std::fs::read_to_string(f)
            .map_err(|e| e.to_string())
            .and_then(|text| vyges_sta::liberty_parse::parse(&text))
            .and_then(|g| vyges_sta::liberty::Library::read(&g));
        match read {
            Ok(lib) => libs.push(lib),
            Err(e) => return Err(input_error(&format!("cannot read {f}: {e}"))),
        }
    }
    Ok(libs)
}

fn refused(why: String) -> ExitCode {
    vyges_events::emit(&vyges_events::Event::new("vyges-dft", vyges_events::Severity::Error, why).with_code("DFT-REFUSED"));
    ExitCode::from(3)
}

/// What `report` and `run` share: the options, the database, the libraries and the plan.
struct Plan {
    db: vyges_opendb::Db,
    lib: vyges_sta::liberty::Library,
    config: vyges_dft::architect::ArchitectConfig,
    chains: Vec<vyges_dft::architect::ScanChain>,
    verbose: bool,
    trace: bool,
    out: Option<String>,
}

/// `scanArchitect`: collect the scan cells (the clock at each from the timer), then architect the
/// chains. Collection's warnings are printed as they are logged.
fn plan(cmd: &str, args: &[String]) -> Result<Plan, ExitCode> {
    use vyges_dft::architect::{ArchitectConfig, ClockMixing};
    let (mut pos, mut liberty, mut sdc, mut verbose, mut trace, mut out): (Vec<String>, Vec<String>, Option<String>, bool, bool, Option<String>) =
        (Vec::new(), Vec::new(), None, false, false, None);
    let mut config = ArchitectConfig::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let flag = a.as_str();
        if matches!(flag, "--liberty" | "--sdc" | "--max-length" | "--max-chains" | "--clock-mixing" | "-o") {
            let Some(v) = it.next().cloned() else { return Err(usage_error(&format!("{flag} needs a value"))) };
            match flag {
                "--liberty" => liberty.push(v),
                "--sdc" => sdc = Some(v),
                "-o" => out = Some(v),
                "--clock-mixing" => {
                    config.clock_mixing = match v.as_str() {
                        "no_mix" => ClockMixing::NoMix,
                        "clock_mix" => ClockMixing::ClockMix,
                        _ => return Err(usage_error(&format!("--clock-mixing is no_mix or clock_mix, not `{v}`"))),
                    }
                }
                _ => {
                    let Ok(n) = v.parse::<u64>() else { return Err(usage_error(&format!("{flag} needs a number, not `{v}`"))) };
                    if flag == "--max-length" {
                        config.max_length = Some(n);
                    } else {
                        config.max_chains = Some(n);
                    }
                }
            }
            continue;
        }
        match flag {
            "--verbose" => verbose = true,
            "--trace" => trace = true,
            "-q" | "--quiet" | "-v" => {}
            f if f.starts_with('-') => return Err(usage_error(&format!("unknown option `{f}`"))),
            f => pos.push(f.to_string()),
        }
    }
    let ([design], Some(sdc)) = (<[String; 1]>::try_from(pos).unwrap_or_default(), sdc) else {
        return Err(usage_error(&format!("{cmd} needs a design database and --sdc FILE")));
    };
    let libs = read_libraries(&liberty)?;
    let Some(scale) = libs.first().map(|l| l.time_scale) else { return Err(usage_error(&format!("{cmd} needs --liberty"))) };
    let constraints = vyges_loom::sdc::Sdc::load(&sdc).map_err(|e| input_error(&format!("cannot read {sdc}: {e:?}")))?;
    let timer_sdc = vyges_dft::design::timer_clocks(&constraints, scale).map_err(refused)?;
    let db = vyges_opendb::Db::open(&design).map_err(|e| input_error(&format!("cannot read {design}: {e}")))?;
    if !db.block_get_children().is_empty() {
        return Err(refused(format!("{design}: child blocks are not modelled")));
    }
    let netlist = vyges_grt::timer::netlist(&db);
    let clocks = vyges_dft::design::pin_clocks(&libs, &netlist, &timer_sdc).map_err(|e| refused(format!("the timer: {e}")))?;
    let lib = vyges_dft::replace::merge_libraries(libs).unwrap_or_default();
    let d = vyges_dft::design::OdbCollect { db: &db, clocks };
    let (cells, log) = vyges_dft::collect::collect_scan_cells(&d, &lib).map_err(|e| refused(format!("{e:?}")))?;
    for l in &log {
        println!("{l}");
    }
    let chains = vyges_dft::architect::scan_architect(&config, cells).map_err(|e| refused(format!("{e:?}")))?;
    Ok(Plan { db, lib, config, chains, verbose, trace, out })
}

/// `report_dft_plan`: the plan, printed. Log lines are the reference's.
fn report(args: &[String]) -> ExitCode {
    match plan("report", args) {
        Ok(p) => {
            print!("{}", vyges_dft::architect::report_dft_plan(&p.config, &p.chains, p.verbose));
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

/// `execute_dft_plan`: the plan stitched into the database, then the chains recorded in it.
/// `--trace` prints the reference's `scan_cell_connect` debug lines.
fn run(args: &[String]) -> ExitCode {
    let mut p = match plan("run", args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let Some(out) = p.out.clone() else { return usage_error("run needs -o OUT.odb") };
    let masters: std::collections::HashMap<String, String> = p.chains.iter().flat_map(|c| &c.cells).map(|c| (c.name.clone(), p.db.inst_master(&c.name))).collect();
    let mut trace = Vec::new();
    let stitched = {
        let mut d = vyges_dft::design::OdbStitch { db: &mut p.db };
        vyges_dft::stitch::stitch(&mut d, &p.lib, &|i: &str| masters[i].clone(), &vyges_dft::stitch::StitchConfig::default(), &p.chains, &mut trace)
    };
    if p.trace {
        for l in &trace {
            println!("{l}");
        }
    }
    let stitched = match stitched {
        Ok(s) => s,
        Err(vyges_dft::stitch::StitchError::Error(line)) => {
            println!("{line}");
            return ExitCode::from(1);
        }
        Err(vyges_dft::stitch::StitchError::Refused(why)) => return refused(why),
    };
    let records = vyges_dft::design::scan_chain_records(&p.chains, &stitched);
    if let Err(e) = p.db.dft_write_scan_chains(&records) {
        return input_error(&format!("cannot record the scan chains: {e}"));
    }
    if let Err(e) = p.db.write(&out) {
        return input_error(&format!("cannot write {out}: {e}"));
    }
    eprintln!("vyges-dft: {} chain(s) stitched; wrote {out}", stitched.len());
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("-V") | Some("--version") => {
            println!("vyges-dft {} ({})", env!("CARGO_PKG_VERSION"), env!("VYGES_GIT_SHA"));
            println!("Copyright 2026 Vyges. Apache-2.0.");
            ExitCode::SUCCESS
        }
        Some("--describe") => {
            println!("{DESCRIBE}");
            ExitCode::SUCCESS
        }
        Some("replace") => replace(&args[1..]),
        Some("report") => report(&args[1..]),
        Some("run") => run(&args[1..]),
        Some(cmd) if COMMANDS.contains(&cmd) => refuse(cmd),
        Some(other) => {
            eprintln!("vyges-dft: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_is_valid_json_with_a_legal_maturity() {
        let d: serde_json::Value = serde_json::from_str(DESCRIBE).expect("descriptor is valid JSON");
        assert_eq!(d["schema"], "vyges-tool-descriptor/1.1");
        assert_eq!(d["name"], "dft");
        // Anything else is read as `discovered` and every verdict becomes `unknown`.
        let m = d["maturity"].as_str().unwrap();
        assert!(["discovered", "structured", "workflow-validated"].contains(&m), "{m}");
        // `eq` is the schema's predicate; `equals` is dropped at runtime and the verdict is unknown.
        assert_eq!(d["assertion"]["pass_when"]["eq"], "ok");
    }

    #[test]
    fn the_usage_spells_the_engine_under_its_group() {
        // The dispatcher REFUSES a wrong group prefix, so the help must spell the right one.
        for c in COMMANDS {
            assert!(USAGE.contains(&format!("vyges physical dft {c}")), "{c}");
        }
    }
}
