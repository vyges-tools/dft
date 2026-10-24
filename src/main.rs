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
  vyges physical dft run     <design.odb> <job> -o OUT.odb  scan-replace and stitch the chains
  vyges physical dft report  <design.odb> <job>  the chains as planned or as built
  vyges physical dft --describe
  vyges physical dft --help
  vyges physical dft --version

OPTIONS:
  -o FILE               write the report (or the output database) to FILE
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

/// The commands this engine will answer. Each is refused (exit 3) until its stage is built.
pub const COMMANDS: &[&str] = &["run", "report"];

const DESCRIBE: &str = r#"{
  "schema": "vyges-tool-descriptor/1.1",
  "name": "dft",
  "summary": "Scan insertion and stitching, so a chip can be tested after fabrication.",
  "maturity": "discovered",
  "provenance_limitations": [
    "No command is implemented yet: every command refuses with exit 3."
  ],
  "invocation": { "args_template": ["run"], "emits_json": true },
  "inputs": { "type": "object", "properties": {} },
  "artifacts": [],
  "assertion": { "id": "dft-ok", "field": "status", "pass_when": { "equals": "ok" } },
  "consumes": []
}"#;

fn refuse(what: &str) -> ExitCode {
    vyges_events::emit(
        &vyges_events::Event::new("vyges-dft", vyges_events::Severity::Error, format!("{what} is not implemented in this build"))
            .with_code("DFT-REFUSED"),
    );
    ExitCode::from(3)
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
    }

    #[test]
    fn the_usage_spells_the_engine_under_its_group() {
        // The dispatcher REFUSES a wrong group prefix, so the help must spell the right one.
        for c in COMMANDS {
            assert!(USAGE.contains(&format!("vyges physical dft {c}")), "{c}");
        }
    }
}
