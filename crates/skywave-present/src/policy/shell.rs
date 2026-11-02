//! What the one stop key does in a session's shell runs, the same in every
//! head.

use skywave_core::{Action, SessionView, ShellRun, ShellRunState};

/// The action one press of stop takes, or `None` when there is nothing to
/// stop. Each press does one thing, passing over runs already
/// [`stopping`](ShellRun::stopping), so the next press reaches the next run
/// instead of sending the same stop again:
///
/// 1. A running run (any device's): [`Action::StopShell`] for it.
/// 2. Else the most recently queued run still waiting, pending here or
///    queued on the server. A [`refused`](ShellRun::refused) run is not
///    waiting, as it will never run: it is passed over, and taking it back
///    is left to an explicit [`Action::Discard`]. This device's still
///    [`discardable`](ShellRun::discardable) one is taken back
///    ([`Action::Discard`], nothing reaches the server); any other gets
///    [`Action::StopShell`]. That covers another device's run and this
///    device's run that was sent and not answered yet: the outbox sends one
///    entry at a time, so the stop cannot overtake the run, and if the run
///    is refused the stop is refused too (`UnknownRun`).
pub fn shell_stop(view: &SessionView) -> Option<Action> {
    let runs = &view.shell.runs;
    let stop = |run: &ShellRun| Action::StopShell {
        session_id: view.id,
        run: run.run,
    };
    if let Some(running) = runs
        .iter()
        .find(|r| r.state == ShellRunState::Running && !r.stopping)
    {
        return Some(stop(running));
    }
    let waiting = runs.iter().rev().find(|r| {
        matches!(r.state, ShellRunState::Pending | ShellRunState::Queued)
            && !r.stopping
            && !r.refused()
    })?;
    Some(if waiting.by.me && waiting.discardable {
        Action::Discard(waiting.run)
    } else {
        stop(waiting)
    })
}

#[cfg(test)]
mod tests {
    use skywave_core::{
        CommandId, DeviceId, DeviceRef, FailReason, Outgoing, RejectReason, RunWhen, ShellEnd,
    };

    use super::*;
    use crate::policy::tests::session;

    fn run(state: ShellRunState, me: bool, outgoing: Option<Outgoing>) -> ShellRun {
        ShellRun {
            run: CommandId::new(),
            command: "make".into(),
            when: RunWhen::Always,
            by: DeviceRef {
                id: DeviceId::new(),
                name: None,
                me,
            },
            state,
            requested_at: None,
            started_at: None,
            finished_at: None,
            tail_or_preview: String::new(),
            lines: 0,
            output: None,
            truncated: false,
            discardable: matches!(outgoing, Some(Outgoing::Queued | Outgoing::Failed(_))),
            outgoing,
            stopping: false,
            delivery: None,
        }
    }

    fn view(runs: Vec<ShellRun>) -> SessionView {
        let mut view = session(Vec::new());
        view.shell.runs = runs;
        view
    }

    /// Presses stop as a head would: applies what the core would show
    /// after each action (a stop marks the run stopping, a discard takes it
    /// away) and returns which run each press named.
    fn presses(mut view: SessionView) -> Vec<Action> {
        let mut actions = Vec::new();
        while let Some(action) = shell_stop(&view) {
            let named = match &action {
                Action::StopShell { run, .. } | Action::Discard(run) => *run,
                other => panic!("unexpected {other:?}"),
            };
            match action {
                Action::Discard(_) => view.shell.runs.retain(|r| r.run != named),
                _ => {
                    for r in &mut view.shell.runs {
                        if r.run == named {
                            r.stopping = true;
                        }
                    }
                }
            }
            actions.push(action);
            assert!(actions.len() < 10, "stop must run out");
        }
        actions
    }

    fn stop_of(view: &SessionView, index: usize) -> Action {
        Action::StopShell {
            session_id: view.id,
            run: view.shell.runs[index].run,
        }
    }

    #[test]
    fn the_running_run_then_each_queued_one_newest_first() {
        let view = view(vec![
            run(
                ShellRunState::Finished(ShellEnd::Exited { code: 0 }),
                true,
                None,
            ),
            run(ShellRunState::Running, false, None),
            run(ShellRunState::Queued, false, None),
            run(
                ShellRunState::Queued,
                true,
                Some(Outgoing::Accepted(skywave_core::Seq(9))),
            ),
        ]);
        assert_eq!(
            presses(view.clone()),
            [stop_of(&view, 1), stop_of(&view, 3), stop_of(&view, 2)]
        );
    }

    #[test]
    fn unsent_runs_are_discarded_newest_first() {
        let view = view(vec![
            run(ShellRunState::Pending, true, Some(Outgoing::Queued)),
            run(ShellRunState::Pending, true, Some(Outgoing::Queued)),
            run(ShellRunState::Pending, true, Some(Outgoing::Queued)),
        ]);
        let ids: Vec<CommandId> = view.shell.runs.iter().map(|r| r.run).collect();
        assert_eq!(
            presses(view),
            [
                Action::Discard(ids[2]),
                Action::Discard(ids[1]),
                Action::Discard(ids[0])
            ]
        );
    }

    #[test]
    fn a_logged_run_not_started_gets_a_stop() {
        let view = view(vec![run(
            ShellRunState::Queued,
            true,
            Some(Outgoing::Accepted(skywave_core::Seq(3))),
        )]);
        assert_eq!(shell_stop(&view), Some(stop_of(&view, 0)));
    }

    #[test]
    fn a_sent_run_without_an_answer_gets_a_stop_not_a_discard() {
        let view = view(vec![run(
            ShellRunState::Pending,
            true,
            Some(Outgoing::Sending),
        )]);
        assert!(!view.shell.runs[0].discardable);
        assert_eq!(shell_stop(&view), Some(stop_of(&view, 0)));
    }

    #[test]
    fn a_refused_run_is_not_waiting() {
        let refused = Outgoing::Failed(FailReason::Rejected(RejectReason::BadShellCommand));
        let alone = view(vec![run(ShellRunState::Pending, true, Some(refused))]);
        assert!(alone.shell.runs[0].discardable);
        assert_eq!(shell_stop(&alone), None, "it never runs: nothing to stop");

        // Refused long ago, it still sorts after every logged run: the
        // second press must reach the queued run, not take the refused one.
        let view = view(vec![
            run(ShellRunState::Running, false, None),
            run(
                ShellRunState::Queued,
                true,
                Some(Outgoing::Accepted(skywave_core::Seq(7))),
            ),
            run(ShellRunState::Pending, true, Some(refused)),
        ]);
        assert_eq!(
            presses(view.clone()),
            [stop_of(&view, 0), stop_of(&view, 1)]
        );
    }

    #[test]
    fn stopping_runs_are_passed_over() {
        let mut runs = vec![
            run(ShellRunState::Queued, false, None),
            run(ShellRunState::Running, false, None),
            run(ShellRunState::Pending, true, Some(Outgoing::Sending)),
        ];
        runs[1].stopping = true;
        runs[2].stopping = true;
        let view = view(runs);
        assert_eq!(shell_stop(&view), Some(stop_of(&view, 0)));

        let mut all = view.clone();
        all.shell.runs[0].stopping = true;
        assert_eq!(shell_stop(&all), None, "every one is stopping");
    }

    #[test]
    fn nothing_to_stop_once_everything_finished() {
        let view = view(vec![
            run(ShellRunState::Finished(ShellEnd::Stopped), true, None),
            run(ShellRunState::Finished(ShellEnd::Skipped), false, None),
        ]);
        assert_eq!(shell_stop(&view), None);
        assert_eq!(shell_stop(&session(Vec::new())), None);
    }
}
