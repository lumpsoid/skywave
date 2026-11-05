//! Checks, scenario outcomes and the printed report.

use std::collections::BTreeSet;
use std::time::Duration;

/// What a check's failure means.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    /// Behaviour the adapter relies on: a failure means the upgrade breaks
    /// skywaved.
    Contract,
    /// Behaviour we are waiting to change: a change is good news, and
    /// `revisit` names what to look at again.
    Watch { revisit: &'static str },
}

/// One check: `Ok` holds, `Err` says what was seen instead.
#[derive(Debug)]
pub struct Check {
    pub name: String,
    pub kind: Kind,
    pub outcome: Result<(), String>,
}

/// The checks of one scenario, in the order they ran.
#[derive(Debug, Default)]
pub struct Checks(pub Vec<Check>);

impl Checks {
    pub fn contract(&mut self, name: &str, outcome: Result<(), String>) {
        self.0.push(Check {
            name: name.to_owned(),
            kind: Kind::Contract,
            outcome,
        });
    }

    pub fn watch(&mut self, name: &str, revisit: &'static str, outcome: Result<(), String>) {
        self.0.push(Check {
            name: name.to_owned(),
            kind: Kind::Watch { revisit },
            outcome,
        });
    }
}

/// `Ok` when `holds`, else `Err` with `seen`.
pub fn ensure(holds: bool, seen: impl FnOnce() -> String) -> Result<(), String> {
    if holds { Ok(()) } else { Err(seen()) }
}

/// Why a scenario ended before its last step.
#[derive(Debug)]
pub enum Stop {
    /// A signal it waited for never came: the CLI broke a contract.
    Stalled(String),
    /// The model did not do what the prompt asked, so the scenario proves
    /// nothing either way; run it again.
    Inconclusive(String),
}

/// How a scenario came out, worst first.
#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Broken,
    Inconclusive,
    NewlyAvailable,
    Pass,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Broken => "BROKEN",
            Status::Inconclusive => "inconclusive",
            Status::NewlyAvailable => "NEWLY AVAILABLE",
            Status::Pass => "pass",
        }
    }
}

/// What one scenario found.
#[derive(Debug)]
pub struct ScenarioReport {
    pub name: &'static str,
    pub checks: Checks,
    pub stop: Option<Stop>,
    /// Message kinds the adapter has never seen (`type` or `type/subtype`).
    pub new_kinds: BTreeSet<String>,
    pub elapsed: Duration,
}

impl ScenarioReport {
    pub fn status(&self) -> Status {
        let failed = |contract: bool| {
            self.checks
                .0
                .iter()
                .any(|c| c.outcome.is_err() && matches!(c.kind, Kind::Contract) == contract)
        };
        if failed(true) || matches!(self.stop, Some(Stop::Stalled(_))) {
            Status::Broken
        } else if matches!(self.stop, Some(Stop::Inconclusive(_))) {
            Status::Inconclusive
        } else if failed(false) {
            Status::NewlyAvailable
        } else {
            Status::Pass
        }
    }

    /// Prints the scenario's line and, below it, every check that did not
    /// hold (every check when `verbose`).
    pub fn print(&self, verbose: bool) {
        println!(
            "{:<34} {:<16} {:>6.1}s",
            self.name,
            self.status().label(),
            self.elapsed.as_secs_f64()
        );
        for check in &self.checks.0 {
            let tag = match check.kind {
                Kind::Contract => "contract",
                Kind::Watch { .. } => "watch   ",
            };
            match (&check.outcome, check.kind) {
                (Ok(()), _) if verbose => println!("    ok   {tag}  {}", check.name),
                (Ok(()), _) => {}
                (Err(seen), Kind::Contract) => {
                    println!("    FAIL {tag}  {}\n           saw: {seen}", check.name);
                }
                (Err(seen), Kind::Watch { revisit }) => println!(
                    "    NEW  {tag}  {}\n           saw: {seen}\n           revisit: {revisit}",
                    check.name
                ),
            }
        }
        match &self.stop {
            Some(Stop::Stalled(what)) => println!("    FAIL stalled: {what}"),
            Some(Stop::Inconclusive(why)) => println!("    ?    inconclusive: {why}"),
            None => {}
        }
        for kind in &self.new_kinds {
            println!("    note new output kind, ignored by the adapter: {kind}");
        }
    }
}

/// Prints the totals; whether any scenario is broken.
pub fn summary(reports: &[ScenarioReport]) -> bool {
    let count = |status: Status| reports.iter().filter(|r| r.status() == status).count();
    let broken = count(Status::Broken);
    println!(
        "\n{} pass, {broken} broken, {} newly available, {} inconclusive",
        count(Status::Pass),
        count(Status::NewlyAvailable),
        count(Status::Inconclusive),
    );
    broken > 0
}
