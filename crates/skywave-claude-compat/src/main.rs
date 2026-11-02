//! Checks a Claude Code CLI build against what skywaved's adapter relies on.
//! Run it after every CLI upgrade (docs/agents/claude-code.md, "Checking a
//! new CLI").
//!
//! Each scenario drives the real CLI through the adapter's own protocol
//! state machine, in a scratch directory, on the cheap model, and reports
//! pass, broken (a contract the adapter relies on no longer holds) or newly
//! available (something we wait for changed; the report names the decision
//! to revisit). It runs the real CLI, so it is manual and never part of
//! `cargo test`.

mod report;
mod scenarios;
mod scrub;
mod session;
mod shape;

use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Instant;

use clap::Parser;
use serde_json::json;
use skywaved::agent::claude_code::protocol::PINNED_VERSION;

use crate::report::{Checks, ScenarioReport, ensure, summary};
use crate::scenarios::{ALL, Ctx, Scenario, Tier};
use crate::session::Shared;

#[derive(Parser)]
#[command(
    name = "skywave-claude-compat",
    about = "Checks a Claude Code CLI build against what skywaved's adapter relies on"
)]
struct Cli {
    /// Control requests only: seconds, no tokens. The default.
    #[arg(long, conflicts_with = "full")]
    quick: bool,
    /// Every scenario, real turns included: minutes, a few cents on haiku.
    #[arg(long)]
    full: bool,
    /// Run only this scenario, whatever its tier (repeatable).
    #[arg(long, value_name = "SCENARIO")]
    only: Vec<String>,
    /// Write each scenario's scrubbed recording to DIR/<scenario>.ndjson,
    /// in the fixture format.
    #[arg(long, value_name = "DIR")]
    record: Option<PathBuf>,
    /// The CLI to check.
    #[arg(long, default_value = "claude")]
    claude: String,
    /// Print every check, not only those that did not hold.
    #[arg(short, long)]
    verbose: bool,
    /// List the scenarios and exit.
    #[arg(long)]
    list: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.list {
        for scenario in ALL {
            let tier = match scenario.tier {
                Tier::Quick => "quick",
                Tier::Full => "full",
            };
            println!("{:<26} {tier:<6} {}", scenario.name, scenario.about);
        }
        return ExitCode::SUCCESS;
    }
    if let Some(unknown) = cli.only.iter().find(|n| !ALL.iter().any(|s| s.name == **n)) {
        eprintln!("unknown scenario {unknown}; see --list");
        return ExitCode::from(2);
    }
    let chosen: Vec<&Scenario> = ALL
        .iter()
        .filter(|s| {
            if cli.only.is_empty() {
                cli.full || s.tier == Tier::Quick
            } else {
                cli.only.iter().any(|n| n == s.name)
            }
        })
        .collect();

    let version = match Command::new(&cli.claude).arg("--version").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(out) => {
            eprintln!(
                "{} --version failed: {}",
                cli.claude,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("cannot run {}: {e}", cli.claude);
            return ExitCode::from(2);
        }
    };
    let pinned = if version.starts_with(PINNED_VERSION) {
        "the version the adapter is pinned to".to_owned()
    } else {
        format!("the adapter is pinned to {PINNED_VERSION}")
    };
    println!("claude {version} ({pinned})\n");
    if let Some(dir) = &cli.record
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        eprintln!("cannot create {}: {e}", dir.display());
        return ExitCode::from(2);
    }
    let home = std::env::var("HOME").unwrap_or_default();

    let mut reports = Vec::new();
    for scenario in chosen {
        let recorder = Shared::default();
        recorder.borrow_mut().records.push(json!({
            "dir": "note",
            "text": format!("skywave-claude-compat scenario {}, claude {version}", scenario.name),
        }));
        let mut ctx = Ctx {
            claude: cli.claude.clone(),
            recorder: recorder.clone(),
            checks: Checks::default(),
        };
        let started = Instant::now();
        // Every process of the scenario has ended once `run` returns.
        let stop = (scenario.run)(&mut ctx).err();
        let recorded = recorder.take();
        ctx.checks.contract(
            "every line decodes as the adapter reads it",
            ensure(recorded.broken.is_empty(), || recorded.broken.join("; ")),
        );
        let report = ScenarioReport {
            name: scenario.name,
            checks: ctx.checks,
            stop,
            new_kinds: recorded.new_kinds,
            elapsed: started.elapsed(),
        };
        report.print(cli.verbose);
        if let Some(dir) = &cli.record {
            let path = dir.join(format!("{}.ndjson", scenario.name));
            let lines: String = scrub::scrub(&recorded.records, &home)
                .iter()
                .map(|r| format!("{r}\n"))
                .collect();
            if let Err(e) = std::fs::write(&path, lines) {
                eprintln!("cannot write {}: {e}", path.display());
            }
        }
        reports.push(report);
    }
    if summary(&reports) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
