use chrono::{DateTime, Utc};
use skywave_proto::*;
use uuid::Uuid;

fn session() -> SessionId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0001).into()
}
fn command() -> CommandId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0002).into()
}
fn device() -> DeviceId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0003).into()
}
fn approval() -> ApprovalId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0004).into()
}
fn blob_id() -> BlobId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0005).into()
}
fn server() -> ServerId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0006).into()
}
fn call() -> CallId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0007).into()
}
fn context() -> ContextId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0008).into()
}
fn moved_to() -> SessionId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0009).into()
}
fn blob() -> BlobRef {
    BlobRef {
        id: blob_id(),
        size: 48_213,
    }
}
fn at() -> DateTime<Utc> {
    "2026-10-29T12:00:00Z".parse().unwrap()
}

fn event(kind: EventKind) -> Event {
    Event {
        session_id: session(),
        at: at(),
        command_id: kind.logged_by_command().then(command),
        context: context(),
        kind,
    }
}

fn all_event_kinds() -> Vec<EventKind> {
    vec![
        EventKind::SessionCreated {
            workdir: "/srv/code/skywave".into(),
            context_name: "velvet walrus".into(),
            from: None,
        },
        EventKind::SessionCreated {
            workdir: "/srv/code/skywave".into(),
            context_name: "amber otter".into(),
            from: Some(SessionOrigin::Context(ContextRef {
                session: session(),
                context: context(),
            })),
        },
        EventKind::SessionCreated {
            workdir: "/srv/code/skywave".into(),
            context_name: "copper heron".into(),
            from: Some(SessionOrigin::Agent {
                conversation: "9b2c1e4a-5f3d-4c8e-a1b2-c3d4e5f6a7b8".into(),
            }),
        },
        EventKind::ContextStarted {
            by: device(),
            name: "amber otter".into(),
        },
        EventKind::ContextSwitched { by: device() },
        EventKind::ContextMoved {
            to: moved_to(),
            by: device(),
        },
        EventKind::UserMessage {
            device_id: device(),
            text: "run the tests".into(),
        },
        EventKind::AgentMessage {
            parent: None,
            text: "Running cargo test.".into(),
        },
        EventKind::AgentMessage {
            parent: Some(call()),
            text: "Found three call sites.".into(),
        },
        EventKind::ToolCall(ToolCall {
            parent: None,
            call: Some(call()),
            name: "exec".into(),
            display: ToolDisplay {
                kind: ToolKind::Shell,
                title: "cargo test".into(),
                detail: Some("run the test suite".into()),
                body: None,
            },
            input: None,
        }),
        EventKind::ToolCall(ToolCall {
            parent: Some(call()),
            call: Some(call()),
            name: "patch".into(),
            display: ToolDisplay {
                kind: ToolKind::FileEdit,
                title: "src/lib.rs".into(),
                detail: None,
                body: Some(Body {
                    format: BodyFormat::Diff,
                    preview: "@@ -1 +1 @@\n-old\n+new".into(),
                    lines: 120,
                    full: Some(blob()),
                }),
            },
            input: Some(blob()),
        }),
        EventKind::ToolCall(ToolCall {
            parent: None,
            call: None,
            name: "put".into(),
            display: ToolDisplay {
                kind: ToolKind::FileWrite,
                title: "notes.txt".into(),
                detail: None,
                body: Some(Body {
                    format: BodyFormat::Text,
                    preview: "hello".into(),
                    lines: 1,
                    full: None,
                }),
            },
            input: None,
        }),
        EventKind::ToolResult(ToolResult {
            call: Some(call()),
            is_error: false,
            status: Some("exit 0".into()),
            preview: "running 3 tests\ntest a ... ok".into(),
            lines: 40,
            output: Some(blob()),
        }),
        EventKind::ToolResult(ToolResult {
            call: Some(call()),
            is_error: true,
            status: Some("exit 1".into()),
            preview: "error: no such file".into(),
            lines: 1,
            output: None,
        }),
        EventKind::ToolResult(ToolResult {
            call: None,
            is_error: false,
            status: None,
            preview: String::new(),
            lines: 0,
            output: None,
        }),
        EventKind::ApprovalRequested {
            id: approval(),
            call: Some(call()),
            tool: "exec".into(),
            summary: "rm -rf target".into(),
            input: None,
            suggestions: vec![
                RuleSuggestion {
                    label: "allow exec(rm -rf target)".into(),
                    scope: RuleScope::Project,
                },
                RuleSuggestion {
                    label: "allow all edits".into(),
                    scope: RuleScope::Session,
                },
                RuleSuggestion {
                    label: "allow exec(cargo:*)".into(),
                    scope: RuleScope::User,
                },
            ],
        },
        EventKind::ApprovalRequested {
            id: approval(),
            call: None,
            tool: "put".into(),
            summary: "src/main.rs".into(),
            input: Some(blob()),
            suggestions: Vec::new(),
        },
        EventKind::ApprovalResolved {
            id: approval(),
            decision: Decision::Approve,
            by: device(),
            rule: None,
        },
        EventKind::ApprovalResolved {
            id: approval(),
            decision: Decision::Approve,
            by: device(),
            rule: Some(1),
        },
        EventKind::ApprovalResolved {
            id: approval(),
            decision: Decision::Deny,
            by: device(),
            rule: None,
        },
        EventKind::ApprovalCancelled {
            id: approval(),
            reason: "daemon restarted".into(),
        },
        EventKind::TurnInterrupted,
        EventKind::SessionStopped,
        EventKind::SessionRenamed {
            name: Some("fix the flaky test".into()),
            by: device(),
        },
        EventKind::SessionRenamed {
            name: None,
            by: device(),
        },
        EventKind::SessionArchived {
            archived: true,
            by: device(),
        },
        EventKind::SessionArchived {
            archived: false,
            by: device(),
        },
        EventKind::SessionStateChanged(SessionState::Idle),
        EventKind::SessionStateChanged(SessionState::AgentWorking),
        EventKind::SessionStateChanged(SessionState::Errored {
            message: "agent exited".into(),
        }),
        EventKind::TurnUsage(TurnUsage {
            duration_ms: Some(12_400),
            tokens_in: Some(3_100),
            tokens_out: Some(812),
            cost_usd: Some(0.04),
            model: Some("model-a".into()),
        }),
        EventKind::TurnUsage(TurnUsage {
            duration_ms: Some(2_590),
            ..TurnUsage::default()
        }),
        EventKind::PermissionModeRequested {
            mode: PermissionMode::Plan,
            by: device(),
        },
        EventKind::PermissionModeChanged {
            mode: PermissionMode::Plan,
            answers: Some(command()),
        },
        EventKind::PermissionModeChanged {
            mode: PermissionMode::Ask,
            answers: None,
        },
        EventKind::PermissionModeRefused {
            mode: PermissionMode::Bypass,
            reason: "not launched for it".into(),
            answers: Some(command()),
        },
        EventKind::ModelRequested {
            model: "sonnet".into(),
            by: device(),
        },
        EventKind::ModelChanged {
            model: "model-b".into(),
            answers: Some(command()),
        },
        EventKind::ModelRefused {
            model: "bogus".into(),
            reason: "not found".into(),
            answers: None,
        },
        EventKind::ModelsOffered {
            models: vec![ModelChoice {
                id: "sonnet".into(),
                label: "Sonnet".into(),
                detail: Some("Efficient".into()),
                resolved: Some("model-b".into()),
                efforts: vec![Effort::Low, Effort::High, Effort::XHigh],
            }],
        },
        EventKind::EffortRequested {
            effort: Effort::XHigh,
            by: device(),
        },
        EventKind::EffortChanged {
            effort: Some(Effort::Medium),
            answers: Some(command()),
        },
        EventKind::EffortChanged {
            effort: None,
            answers: None,
        },
        EventKind::EffortRefused {
            effort: Effort::Max,
            reason: "not on this model".into(),
            answers: Some(command()),
        },
        EventKind::BackgroundTasksChanged {
            tasks: vec![
                BackgroundTask {
                    call: Some(call()),
                    id: TaskId("a1b2c3".into()),
                    kind: TaskKind::Subagent,
                    title: "Explore the codebase".into(),
                    parent: None,
                    ambient: false,
                },
                BackgroundTask {
                    call: None,
                    id: TaskId("d4e5f6".into()),
                    kind: TaskKind::Shell,
                    title: "cargo test".into(),
                    parent: Some(TaskId("a1b2c3".into())),
                    ambient: true,
                },
                BackgroundTask {
                    call: None,
                    id: TaskId("g7".into()),
                    kind: TaskKind::Other,
                    title: "workflow".into(),
                    parent: None,
                    ambient: false,
                },
            ],
        },
        EventKind::BackgroundTasksChanged { tasks: vec![] },
        EventKind::SchedulesChanged {
            schedules: vec![
                Schedule {
                    id: ScheduleId("s1".into()),
                    when: "*/5 * * * *".into(),
                    recurring: true,
                    prompt: "check the deploy".into(),
                    outlives_agent: None,
                },
                Schedule {
                    id: ScheduleId("s2".into()),
                    when: "30 14 1 11 *".into(),
                    recurring: false,
                    prompt: "remind me".into(),
                    outlives_agent: Some(false),
                },
            ],
        },
        EventKind::TaskStopRequested {
            task: TaskId("a1b2c3".into()),
            by: device(),
        },
        EventKind::TaskStopRefused {
            task: TaskId("a1b2c3".into()),
            reason: "agent not running".into(),
            answers: Some(command()),
        },
        EventKind::CallsUnanswered {
            calls: vec![call()],
        },
        EventKind::ShellRequested {
            command: "cargo test".into(),
            when: RunWhen::Always,
            by: device(),
        },
        EventKind::ShellRequested {
            command: "git push".into(),
            when: RunWhen::OnSuccess,
            by: device(),
        },
        EventKind::ShellRequested {
            command: "git stash".into(),
            when: RunWhen::OnFailure,
            by: device(),
        },
        EventKind::ShellStopRequested {
            run: command(),
            by: device(),
        },
        EventKind::ShellStarted {
            run: command(),
            cwd: "/srv/code/skywave".into(),
        },
        EventKind::ShellOutput {
            run: command(),
            tail: "running 12 tests".into(),
            lines: 40,
            bytes: 2_048,
        },
    ]
    .into_iter()
    .chain(all_shell_ends().into_iter().map(shell_finished))
    .chain(
        all_imported_items()
            .into_iter()
            .enumerate()
            .map(|(i, item)| {
                EventKind::Imported {
                    // One item without a timestamp: a transcript line may lack one.
                    at: (i != 0).then(at),
                    item,
                }
            }),
    )
    .chain(
        all_import_outcomes()
            .into_iter()
            .map(|outcome| EventKind::ImportFinished { outcome }),
    )
    .collect()
}

fn all_imported_items() -> Vec<ImportedItem> {
    vec![
        ImportedItem::UserMessage {
            text: "Where is the event log?".into(),
        },
        ImportedItem::AgentMessage {
            text: "In crates/skywaved/src/event_log.rs.".into(),
        },
        ImportedItem::ToolCall(ToolCall {
            call: Some(call()),
            parent: None,
            name: "Bash".into(),
            display: ToolDisplay {
                kind: ToolKind::Shell,
                title: "ls crates".into(),
                detail: None,
                body: None,
            },
            input: Some(blob()),
        }),
        ImportedItem::ToolResult(ToolResult {
            call: Some(call()),
            is_error: false,
            status: Some("exit 0".into()),
            preview: "skywave-core\nskywaved".into(),
            lines: 2,
            output: None,
        }),
    ]
}

fn all_import_outcomes() -> Vec<ImportOutcome> {
    vec![
        ImportOutcome::Imported { items: 4 },
        ImportOutcome::NotFound,
        ImportOutcome::Failed {
            reason: "transcript unreadable".into(),
        },
        ImportOutcome::Unavailable,
    ]
}

fn all_conversations_results() -> Vec<ConversationsResult> {
    vec![
        ConversationsResult::Listed {
            conversations: vec![
                OutsideConversation {
                    id: "9b2c1e4a-5f3d-4c8e-a1b2-c3d4e5f6a7b8".into(),
                    workdir: "/srv/code/skywave".into(),
                    title: Some("Find the event log".into()),
                    first_prompt: Some("Where is the event log?".into()),
                    last_active: at(),
                    messages: 12,
                    session: None,
                },
                OutsideConversation {
                    id: "0d1e2f3a-4b5c-4d6e-8f70-81a2b3c4d5e6".into(),
                    workdir: "/srv/code/skywave".into(),
                    title: None,
                    first_prompt: None,
                    last_active: at(),
                    messages: 0,
                    session: Some(session()),
                },
            ],
            truncated: true,
        },
        ConversationsResult::Listed {
            conversations: vec![],
            truncated: false,
        },
        ConversationsResult::Unavailable,
    ]
}

fn all_shell_ends() -> Vec<ShellEnd> {
    vec![
        ShellEnd::Exited { code: 0 },
        ShellEnd::Exited { code: 101 },
        ShellEnd::Signalled { signal: 9 },
        ShellEnd::Stopped,
        ShellEnd::Skipped,
        ShellEnd::Lost,
        ShellEnd::NotStarted {
            reason: "no such directory".into(),
        },
    ]
}

/// A `ShellFinished` ending in `end`; one that ran has a start, a cwd and
/// output, one that never started has none.
fn shell_finished(end: ShellEnd) -> EventKind {
    let ran = matches!(
        end,
        ShellEnd::Exited { .. } | ShellEnd::Signalled { .. } | ShellEnd::Stopped
    );
    EventKind::ShellFinished {
        run: command(),
        by: device(),
        started_at: ran.then(at),
        command_line: "cargo test".into(),
        end,
        cwd: ran.then(|| "/srv/code/skywave".into()),
        preview: if ran {
            "test result: ok".into()
        } else {
            String::new()
        },
        lines: if ran { 40 } else { 0 },
        output: ran.then(blob),
        truncated: ran,
    }
}

fn all_command_kinds() -> Vec<CommandKind> {
    vec![
        CommandKind::CreateSession {
            workdir: "/srv/code/skywave".into(),
            conversation: None,
        },
        CommandKind::CreateSession {
            workdir: "/srv/code/skywave".into(),
            conversation: Some("9b2c1e4a-5f3d-4c8e-a1b2-c3d4e5f6a7b8".into()),
        },
        CommandKind::SendMessage {
            session_id: session(),
            context: context(),
            text: "hello".into(),
        },
        CommandKind::ResolveApproval {
            id: approval(),
            decision: Decision::Approve,
            rule: None,
        },
        CommandKind::ResolveApproval {
            id: approval(),
            decision: Decision::Approve,
            rule: Some(0),
        },
        CommandKind::ResolveApproval {
            id: approval(),
            decision: Decision::Deny,
            rule: None,
        },
        CommandKind::Interrupt {
            session_id: session(),
        },
        CommandKind::StopSession {
            session_id: session(),
        },
        CommandKind::RenameSession {
            session_id: session(),
            name: Some("fix the flaky test".into()),
        },
        CommandKind::RenameSession {
            session_id: session(),
            name: None,
        },
        CommandKind::SetSessionArchived {
            session_id: session(),
            archived: true,
        },
        CommandKind::SetSessionArchived {
            session_id: session(),
            archived: false,
        },
        CommandKind::SetPermissionMode {
            session_id: session(),
            mode: PermissionMode::AcceptEdits,
        },
        CommandKind::SetModel {
            session_id: session(),
            model: "sonnet".into(),
        },
        CommandKind::SetEffort {
            session_id: session(),
            effort: Effort::High,
        },
        CommandKind::ClearContext {
            session_id: session(),
            context: context(),
            name: "amber otter".into(),
        },
        CommandKind::SwitchContext {
            session_id: session(),
            context: context(),
        },
        CommandKind::MoveContext {
            session_id: session(),
            context: context(),
            name: "amber otter".into(),
        },
        CommandKind::StopTask {
            session_id: session(),
            task: TaskId("a1b2c3".into()),
        },
        CommandKind::RunShell {
            session_id: session(),
            command: "cargo test".into(),
            when: RunWhen::Always,
        },
        CommandKind::RunShell {
            session_id: session(),
            command: "git push".into(),
            when: RunWhen::OnSuccess,
        },
        CommandKind::RunShell {
            session_id: session(),
            command: "git stash".into(),
            when: RunWhen::OnFailure,
        },
        CommandKind::StopShell {
            session_id: session(),
            run: command(),
        },
    ]
}

fn all_command_results() -> Vec<CommandResult> {
    let mut results = vec![CommandResult::Accepted { seq: Seq(42) }];
    results.extend(
        [
            RejectReason::WorkdirNotAllowed,
            RejectReason::WorkdirNotFound,
            RejectReason::UnknownSession,
            RejectReason::UnknownApproval,
            RejectReason::ApprovalAlreadyResolved,
            RejectReason::ApprovalCancelled,
            RejectReason::NoActiveTurn,
            RejectReason::UnknownRule,
            RejectReason::UnknownModel,
            RejectReason::ContextExists,
            RejectReason::UnknownContext,
            RejectReason::ContextChanged,
            RejectReason::ContextMoved,
            RejectReason::ContextCurrent,
            RejectReason::NoConversation,
            RejectReason::BadConversation,
            RejectReason::UnknownTask,
            RejectReason::ShellDisabled,
            RejectReason::UnknownRun,
            RejectReason::RunFinished,
            RejectReason::BadShellCommand,
            RejectReason::BypassDisabled,
            RejectReason::MessageTooLong,
        ]
        .map(|reason| CommandResult::Rejected { reason }),
    );
    results
}

fn batch() -> EventBatch {
    EventBatch {
        from: Seq(1),
        events: all_event_kinds().into_iter().map(event).collect(),
    }
}

fn pair_request() -> PairRequest {
    PairRequest {
        code: "K7Q4-9M2X".into(),
        csr_pem: "-----BEGIN CERTIFICATE REQUEST-----\nMIIB\n-----END CERTIFICATE REQUEST-----\n"
            .into(),
        device_name: "laptop".into(),
    }
}

fn pair_response() -> PairResponse {
    PairResponse {
        device_id: device(),
        device_cert_pem: "-----BEGIN CERTIFICATE-----\nMIIC\n-----END CERTIFICATE-----\n".into(),
        ca_cert_pem: "-----BEGIN CERTIFICATE-----\nMIID\n-----END CERTIFICATE-----\n".into(),
    }
}

fn hello() -> HelloResponse {
    HelloResponse {
        protocol_version: PROTOCOL_VERSION,
        server_id: server(),
        device_id: device(),
        head: Seq(16),
        devices: vec![DeviceInfo {
            id: device(),
            name: "phone".into(),
        }],
        shell: true,
        bypass: true,
    }
}

fn workdir_query() -> WorkdirQuery {
    WorkdirQuery {
        session: session(),
        path: "src/agent".into(),
    }
}

fn changed_file(path: &str, change: FileChange, diff: DiffBody) -> ChangedFile {
    let text = !matches!(diff, DiffBody::Binary);
    ChangedFile {
        path: path.into(),
        from: (change == FileChange::Renamed).then(|| "old/name.rs".into()),
        change,
        added: text.then_some(12),
        removed: text.then_some(3),
        diff,
    }
}

fn all_changes_results() -> Vec<ChangesResult> {
    vec![
        ChangesResult::Changes(ChangeSet {
            at: at(),
            branch: Some("main".into()),
            head: Some("e3c8849".into()),
            files: vec![
                changed_file("a.rs", FileChange::Added, DiffBody::Blob(blob())),
                changed_file("b.rs", FileChange::Modified, DiffBody::Omitted),
                changed_file("c.rs", FileChange::Deleted, DiffBody::Blob(blob())),
                changed_file("d.rs", FileChange::Renamed, DiffBody::Blob(blob())),
                changed_file("e.png", FileChange::TypeChanged, DiffBody::Binary),
                changed_file(
                    "f.log",
                    FileChange::Untracked,
                    DiffBody::TooLarge { bytes: 2_097_152 },
                ),
                changed_file("g.rs", FileChange::Conflicted, DiffBody::Blob(blob())),
            ],
            total: 9,
        }),
        ChangesResult::Changes(ChangeSet {
            at: at(),
            branch: None,
            head: None,
            files: vec![],
            total: 0,
        }),
        ChangesResult::NotGit,
        ChangesResult::Failed {
            reason: "git: command not found".into(),
        },
        ChangesResult::UnknownSession,
        ChangesResult::WorkdirGone,
    ]
}

fn all_list_results() -> Vec<ListResult> {
    let entry = |name: &str, kind: EntryKind| DirEntry {
        name: name.into(),
        kind,
        size: (kind == EntryKind::File).then_some(1_234),
    };
    vec![
        ListResult::Listed {
            at: at(),
            path: "src".into(),
            entries: vec![
                entry("agent", EntryKind::Dir),
                entry("lib.rs", EntryKind::File),
                entry("pipe", EntryKind::Other),
                entry(
                    "to_dir",
                    EntryKind::Link {
                        to: LinkTarget::Dir,
                    },
                ),
                entry(
                    "to_file",
                    EntryKind::Link {
                        to: LinkTarget::File,
                    },
                ),
                entry(
                    "to_etc",
                    EntryKind::Link {
                        to: LinkTarget::Outside,
                    },
                ),
                entry(
                    "broken",
                    EntryKind::Link {
                        to: LinkTarget::Dangling,
                    },
                ),
            ],
            truncated: true,
        },
        ListResult::NotADirectory,
        ListResult::UnknownSession,
        ListResult::WorkdirGone,
        ListResult::NotAllowed,
        ListResult::NotFound,
    ]
}

fn all_file_results() -> Vec<FileResult> {
    let file = |content: FileBody| FileResult::File {
        at: at(),
        path: "src/lib.rs".into(),
        size: 48_213,
        content,
    };
    vec![
        file(FileBody::Blob(blob())),
        file(FileBody::Binary),
        file(FileBody::TooLarge),
        FileResult::IsADirectory,
        FileResult::NotAFile,
        FileResult::UnknownSession,
        FileResult::WorkdirGone,
        FileResult::NotAllowed,
        FileResult::NotFound,
    ]
}

fn round_trip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, value, "round trip changed value; json: {json}");
}

#[test]
fn every_event_kind_round_trips() {
    for kind in all_event_kinds() {
        round_trip(&event(kind));
    }
}

#[test]
fn every_tool_kind_and_body_format_round_trips() {
    let kinds = [
        ToolKind::FileWrite,
        ToolKind::FileEdit,
        ToolKind::FileRead,
        ToolKind::Shell,
        ToolKind::Search,
        ToolKind::Fetch,
        ToolKind::Subagent,
        ToolKind::Other,
    ];
    for kind in kinds {
        round_trip(&kind);
    }
    let names: Vec<String> = kinds
        .iter()
        .map(|k| serde_json::to_string(k).unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "\"file_write\"",
            "\"file_edit\"",
            "\"file_read\"",
            "\"shell\"",
            "\"search\"",
            "\"fetch\"",
            "\"subagent\"",
            "\"other\""
        ]
    );
    round_trip(&BodyFormat::Text);
    round_trip(&BodyFormat::Diff);
}

#[test]
fn unknown_tool_kind_decodes_as_other() {
    let kind: ToolKind = serde_json::from_str("\"teleport\"").unwrap();
    assert_eq!(kind, ToolKind::Other);
}

fn decode(json: &str) -> EventKind {
    serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}: {json}"))
}

#[test]
fn v1_approval_requested_decodes_without_a_call() {
    let kind = decode(
        r#"{"type":"approval_requested","id":"01920000-0000-7000-8000-000000000004","tool":"exec","summary":"rm -rf target","input":null}"#,
    );
    assert_eq!(
        kind,
        EventKind::ApprovalRequested {
            id: approval(),
            call: None,
            tool: "exec".into(),
            summary: "rm -rf target".into(),
            input: None,
            suggestions: Vec::new(),
        }
    );
}

#[test]
fn v3_approval_events_decode_without_suggestions_or_rule() {
    let kind = decode(
        r#"{"type":"approval_requested","id":"01920000-0000-7000-8000-000000000004","call":"01920000-0000-7000-8000-000000000007","tool":"exec","summary":"rm -rf target","input":null}"#,
    );
    assert!(
        matches!(&kind, EventKind::ApprovalRequested { call: Some(c), suggestions, .. } if *c == call() && suggestions.is_empty()),
        "{kind:?}"
    );
    let kind = decode(
        r#"{"type":"approval_resolved","id":"01920000-0000-7000-8000-000000000004","decision":"approve","by":"01920000-0000-7000-8000-000000000003"}"#,
    );
    assert_eq!(
        kind,
        EventKind::ApprovalResolved {
            id: approval(),
            decision: Decision::Approve,
            by: device(),
            rule: None,
        }
    );
    let command: CommandKind = serde_json::from_str(
        r#"{"type":"resolve_approval","id":"01920000-0000-7000-8000-000000000004","decision":"deny"}"#,
    )
    .unwrap();
    assert_eq!(
        command,
        CommandKind::ResolveApproval {
            id: approval(),
            decision: Decision::Deny,
            rule: None,
        }
    );
}

#[test]
fn turn_usage_decodes_with_figures_left_out() {
    let kind = decode(r#"{"type":"turn_usage","duration_ms":1648}"#);
    assert_eq!(
        kind,
        EventKind::TurnUsage(TurnUsage {
            duration_ms: Some(1648),
            ..TurnUsage::default()
        })
    );
    assert!(TurnUsage::default().is_empty());
}

#[test]
fn v7_model_choice_decodes_without_efforts() {
    let kind = decode(
        r#"{"type":"models_offered","models":[{"id":"sonnet","label":"Sonnet","detail":null,"resolved":null}]}"#,
    );
    let EventKind::ModelsOffered { models } = kind else {
        panic!("not ModelsOffered: {kind:?}");
    };
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].efforts, Vec::<Effort>::new());
    // The same choice with its levels keeps them, by their wire names.
    let kind = decode(
        r#"{"type":"models_offered","models":[{"id":"sonnet","label":"Sonnet","detail":null,"resolved":null,"efforts":["low","xhigh"]}]}"#,
    );
    let EventKind::ModelsOffered { models } = kind else {
        panic!("not ModelsOffered: {kind:?}");
    };
    assert_eq!(models[0].efforts, vec![Effort::Low, Effort::XHigh]);
}

/// A `SessionCreated` stored before v10 has no `from`: it created a session
/// of its own, logged by its `CreateSession`.
#[test]
fn v9_session_created_decodes_without_from() {
    let kind =
        decode(r#"{"type":"session_created","workdir":"/w","context_name":"velvet walrus"}"#);
    assert_eq!(
        kind,
        EventKind::SessionCreated {
            workdir: "/w".into(),
            context_name: "velvet walrus".into(),
            from: None,
        }
    );
    assert!(kind.logged_by_command());
    // Positive control: a `from` on the wire is read, not dropped.
    let kind = decode(
        r#"{"type":"session_created","workdir":"/w","context_name":"velvet walrus","from":{"kind":"context","session":"01920000-0000-7000-8000-000000000001","context":"01920000-0000-7000-8000-000000000008"}}"#,
    );
    let EventKind::SessionCreated { from, .. } = kind else {
        panic!("not SessionCreated: {kind:?}");
    };
    assert_eq!(
        from,
        Some(SessionOrigin::Context(ContextRef {
            session: session(),
            context: context(),
        }))
    );
}

/// The v10/v11 `from`, a bare [`ContextRef`], no longer decodes: v12 tags
/// it with what the session continues. Accepted break (alpha): databases
/// holding a moved session from before v12 are deleted, not migrated.
#[test]
fn a_bare_context_ref_from_is_refused() {
    let json = r#"{"type":"session_created","workdir":"/w","context_name":"velvet walrus","from":{"session":"01920000-0000-7000-8000-000000000001","context":"01920000-0000-7000-8000-000000000008"}}"#;
    assert!(serde_json::from_str::<EventKind>(json).is_err());
}

#[test]
fn session_origin_is_tagged_by_kind() {
    let moved = SessionOrigin::Context(ContextRef {
        session: session(),
        context: context(),
    });
    assert_eq!(
        serde_json::to_value(&moved).unwrap(),
        serde_json::json!({
            "kind": "context",
            "session": "01920000-0000-7000-8000-000000000001",
            "context": "01920000-0000-7000-8000-000000000008",
        })
    );
    let adopted = SessionOrigin::Agent {
        conversation: "abc".into(),
    };
    assert_eq!(
        serde_json::to_value(&adopted).unwrap(),
        serde_json::json!({ "kind": "agent", "conversation": "abc" })
    );
    round_trip(&moved);
    round_trip(&adopted);
}

/// Only a fresh `SessionCreated` is its `CreateSession`'s own event: a moved
/// context's `MoveContext` names itself on the `ContextMoved` after it, and
/// an adopting `CreateSession` on the `ImportFinished` after its imported
/// history, so a device that settles either from the log has the session.
#[test]
fn session_created_is_logged_by_command_only_when_fresh() {
    let created = |from| EventKind::SessionCreated {
        workdir: "/w".into(),
        context_name: "start".into(),
        from,
    };
    assert!(created(None).logged_by_command());
    assert!(
        !created(Some(SessionOrigin::Agent {
            conversation: "abc".into(),
        }))
        .logged_by_command()
    );
    assert!(
        EventKind::ImportFinished {
            outcome: ImportOutcome::NotFound
        }
        .logged_by_command()
    );
    assert!(
        !created(Some(SessionOrigin::Context(ContextRef {
            session: session(),
            context: context(),
        })))
        .logged_by_command()
    );
}

#[test]
fn a_conversation_id_is_trimmed_and_otherwise_kept_or_refused() {
    let uuid = "9b2c1e4a-5f3d-4c8e-a1b2-c3d4e5f6a7b8";
    assert_eq!(check_conversation_id(uuid), Some(uuid));
    assert_eq!(check_conversation_id(&format!("  {uuid}\t ")), Some(uuid));
    let longest = "a".repeat(limits::CONVERSATION_ID_MAX_CHARS);
    assert_eq!(check_conversation_id(&longest), Some(longest.as_str()));
    // Refused, never cut or cleaned: a changed id names another conversation.
    let too_long = "a".repeat(limits::CONVERSATION_ID_MAX_CHARS + 1);
    for id in ["", "   ", "ab cd", "ab\ncd", "ab\u{7}cd", too_long.as_str()] {
        assert_eq!(check_conversation_id(id), None, "{id:?}");
    }
}

#[test]
fn run_shell_decodes_without_when_as_always() {
    let command: CommandKind = serde_json::from_str(
        r#"{"type":"run_shell","session_id":"01920000-0000-7000-8000-000000000001","command":"make"}"#,
    )
    .unwrap();
    assert_eq!(
        command,
        CommandKind::RunShell {
            session_id: session(),
            command: "make".into(),
            when: RunWhen::Always,
        }
    );
    // The default is `Always`, not whatever comes first.
    assert_eq!(RunWhen::default(), RunWhen::Always);
    let on_failure: CommandKind = serde_json::from_str(
        r#"{"type":"run_shell","session_id":"01920000-0000-7000-8000-000000000001","command":"make","when":"on_failure"}"#,
    )
    .unwrap();
    assert!(
        matches!(
            on_failure,
            CommandKind::RunShell {
                when: RunWhen::OnFailure,
                ..
            }
        ),
        "{on_failure:?}"
    );
}

#[test]
fn every_shell_end_round_trips() {
    for end in all_shell_ends() {
        round_trip(&end);
    }
}

#[test]
fn every_workdir_body_round_trips() {
    round_trip(&workdir_query());
    for result in all_changes_results() {
        round_trip(&result);
    }
    for result in all_list_results() {
        round_trip(&result);
    }
    for result in all_file_results() {
        round_trip(&result);
    }
}

/// `path` may be left out of the query string (`changes` ignores it).
#[test]
fn a_workdir_query_without_a_path_names_the_workdir() {
    let json = serde_json::json!({ "session": session() });
    let query: WorkdirQuery = serde_json::from_value(json).unwrap();
    assert_eq!(query.path, "");
}

/// The workdir fixtures hold every variant of every workdir enum. The
/// matches are exhaustive, so a new variant fails to compile here until it
/// is named, and the counts fail until a fixture carries it.
#[test]
fn workdir_fixtures_cover_every_variant() {
    use std::collections::BTreeSet;
    fn diff(d: &DiffBody) -> &'static str {
        match d {
            DiffBody::Blob(_) => "blob",
            DiffBody::Binary => "binary",
            DiffBody::TooLarge { .. } => "too_large",
            DiffBody::Omitted => "omitted",
        }
    }
    fn change(c: FileChange) -> &'static str {
        match c {
            FileChange::Added => "added",
            FileChange::Modified => "modified",
            FileChange::Deleted => "deleted",
            FileChange::Renamed => "renamed",
            FileChange::TypeChanged => "type_changed",
            FileChange::Untracked => "untracked",
            FileChange::Conflicted => "conflicted",
        }
    }
    fn kind(k: EntryKind) -> &'static str {
        match k {
            EntryKind::Dir => "dir",
            EntryKind::File => "file",
            EntryKind::Other => "other",
            EntryKind::Link { to } => match to {
                LinkTarget::Dir => "link_dir",
                LinkTarget::File => "link_file",
                LinkTarget::Outside => "link_outside",
                LinkTarget::Dangling => "link_dangling",
            },
        }
    }
    let mut changes = BTreeSet::new();
    let mut diffs = BTreeSet::new();
    let mut file_changes = BTreeSet::new();
    for result in all_changes_results() {
        changes.insert(match &result {
            ChangesResult::Changes(set) => {
                for file in &set.files {
                    diffs.insert(diff(&file.diff));
                    file_changes.insert(change(file.change));
                }
                "changes"
            }
            ChangesResult::NotGit => "not_git",
            ChangesResult::Failed { .. } => "failed",
            ChangesResult::UnknownSession => "unknown_session",
            ChangesResult::WorkdirGone => "workdir_gone",
        });
    }
    let mut lists = BTreeSet::new();
    let mut kinds = BTreeSet::new();
    for result in all_list_results() {
        lists.insert(match &result {
            ListResult::Listed { entries, .. } => {
                kinds.extend(entries.iter().map(|e| kind(e.kind)));
                "listed"
            }
            ListResult::NotADirectory => "not_a_directory",
            ListResult::UnknownSession => "unknown_session",
            ListResult::WorkdirGone => "workdir_gone",
            ListResult::NotAllowed => "not_allowed",
            ListResult::NotFound => "not_found",
        });
    }
    let mut files = BTreeSet::new();
    let mut bodies = BTreeSet::new();
    for result in all_file_results() {
        files.insert(match &result {
            FileResult::File { content, .. } => {
                bodies.insert(match content {
                    FileBody::Blob(_) => "blob",
                    FileBody::Binary => "binary",
                    FileBody::TooLarge => "too_large",
                });
                "file"
            }
            FileResult::IsADirectory => "is_a_directory",
            FileResult::NotAFile => "not_a_file",
            FileResult::UnknownSession => "unknown_session",
            FileResult::WorkdirGone => "workdir_gone",
            FileResult::NotAllowed => "not_allowed",
            FileResult::NotFound => "not_found",
        });
    }
    assert_eq!(changes.len(), 5, "{changes:?}");
    assert_eq!(diffs.len(), 4, "{diffs:?}");
    assert_eq!(file_changes.len(), 7, "{file_changes:?}");
    assert_eq!(lists.len(), 6, "{lists:?}");
    assert_eq!(kinds.len(), 7, "{kinds:?}");
    assert_eq!(files.len(), 7, "{files:?}");
    assert_eq!(bodies.len(), 3, "{bodies:?}");
}

#[test]
fn workdir_caps() {
    assert_eq!(limits::WORKDIR_CHANGES_MAX, 500);
    assert_eq!(limits::WORKDIR_DIFF_MAX_BYTES, 1024 * 1024);
    assert_eq!(limits::WORKDIR_DIFFS_MAX_BYTES, 8 * 1024 * 1024);
    assert_eq!(limits::WORKDIR_FILE_MAX_BYTES, 4 * 1024 * 1024);
    assert_eq!(limits::WORKDIR_LIST_MAX, 1000);
}

#[test]
fn every_conversations_result_round_trips() {
    for result in all_conversations_results() {
        round_trip(&result);
    }
}

/// The imported item stays nested under `item`: the event's top-level
/// `type` is `imported` whatever it carries, so nothing that selects on the
/// top-level `type` (the server's open calls) mistakes it for a live one.
#[test]
fn an_imported_item_is_nested_under_item() {
    for item in all_imported_items() {
        let json = serde_json::to_value(EventKind::Imported { at: None, item }).unwrap();
        assert_eq!(json["type"], "imported", "{json}");
        assert!(json["item"]["type"].is_string(), "{json}");
        assert!(json.get("call").is_none(), "{json}");
    }
    let finished = serde_json::to_value(EventKind::ImportFinished {
        outcome: ImportOutcome::NotFound,
    })
    .unwrap();
    assert_eq!(
        finished,
        serde_json::json!({"type": "import_finished", "outcome": {"kind": "not_found"}})
    );
}

#[test]
fn conversation_caps() {
    assert_eq!(limits::CONVERSATIONS_MAX, 100);
}

#[test]
fn protocol_is_v18() {
    assert_eq!(PROTOCOL_VERSION, 18);
    assert_eq!(limits::CONTEXT_NAME_MAX_CHARS, 40);
    assert_eq!(limits::CONVERSATION_ID_MAX_CHARS, 128);
    assert_eq!(limits::SHELL_COMMAND_MAX_BYTES, 8192);
    assert_eq!(limits::MESSAGE_MAX_BYTES, 256 * 1024);
    assert_eq!(limits::FIRST_PROMPT_MAX_CHARS, 200);
    // A session's first context has the session's own id.
    assert_eq!(ContextId::first_of(session()).0, session().0);
}

/// Events must carry their context: no default for a v8 shape.
#[test]
fn an_event_without_a_context_is_rejected() {
    let mut json = serde_json::to_value(event(EventKind::TurnInterrupted)).unwrap();
    assert!(serde_json::from_value::<Event>(json.clone()).is_ok());
    json.as_object_mut().unwrap().remove("context");
    assert!(serde_json::from_value::<Event>(json).is_err());
}

#[test]
fn display_caps() {
    assert_eq!(limits::TITLE_MAX_CHARS, 120);
    assert_eq!(limits::DETAIL_MAX_CHARS, 120);
    assert_eq!(limits::STATUS_MAX_CHARS, 60);
    assert_eq!(limits::PREVIEW_MAX_LINES, 8);
    assert_eq!(limits::PREVIEW_MAX_BYTES, 512);
    assert_eq!(limits::RULE_LABEL_MAX_CHARS, 120);
    assert_eq!(limits::RULE_SUGGESTIONS_MAX, 8);
    assert_eq!(limits::TASKS_MAX, 32);
    assert_eq!(limits::SCHEDULES_MAX, 16);
    assert_eq!(limits::TASK_ID_MAX_CHARS, 64);
    assert_eq!(limits::SCHEDULE_WHEN_MAX_CHARS, 60);
}

/// Each set replaces the last one whole and leaves the other set alone;
/// nothing else touches the work.
#[test]
fn agent_work_keeps_the_last_set_of_each() {
    let task = |id: &str| BackgroundTask {
        call: None,
        id: TaskId(id.into()),
        kind: TaskKind::Shell,
        title: id.into(),
        parent: None,
        ambient: false,
    };
    let schedule = Schedule {
        id: ScheduleId("s".into()),
        when: "* * * * *".into(),
        recurring: true,
        prompt: "p".into(),
        outlives_agent: None,
    };
    let mut work = AgentWork::default();
    for kind in [
        EventKind::BackgroundTasksChanged {
            tasks: vec![task("a"), task("b")],
        },
        EventKind::SchedulesChanged {
            schedules: vec![schedule.clone()],
        },
        EventKind::BackgroundTasksChanged {
            tasks: vec![task("c")],
        },
        EventKind::AgentMessage {
            parent: None,
            text: "x".into(),
        },
    ] {
        work.apply(&kind);
    }
    assert_eq!(
        work,
        AgentWork {
            tasks: vec![task("c")],
            schedules: vec![schedule],
        }
    );
    let quiet = EventKind::BackgroundTasksChanged { tasks: vec![] };
    assert!(AgentWork::folds(&quiet));
    assert!(!AgentWork::folds(&EventKind::TurnInterrupted));
    assert_eq!(quiet.device(), None);
    assert!(!quiet.logged_by_command());
    assert_eq!(quiet.archived_effect(), None);
}

#[test]
fn every_command_kind_round_trips() {
    for kind in all_command_kinds() {
        round_trip(&Command {
            id: command(),
            kind,
        });
    }
}

#[test]
fn every_command_result_round_trips() {
    for result in all_command_results() {
        round_trip(&result);
    }
}

#[test]
fn api_bodies_round_trip() {
    round_trip(&batch());
    round_trip(&EventBatch {
        from: Seq(17),
        events: vec![],
    });
    round_trip(&pair_request());
    round_trip(&pair_response());
    round_trip(&hello());
    round_trip(&EventsQuery {
        after: Seq(16),
        wait: Some(30),
        limit: Some(500),
    });
    round_trip(&EventsQuery {
        after: Seq::ZERO,
        wait: None,
        limit: None,
    });
}

#[test]
fn events_carry_no_seq_on_the_wire() {
    let json = serde_json::to_value(event(EventKind::TurnInterrupted)).unwrap();
    assert!(json.get("seq").is_none(), "{json}");
}

#[test]
fn batch_assigns_contiguous_seqs() {
    let batch = EventBatch {
        from: Seq(7),
        events: vec![
            event(EventKind::TurnInterrupted),
            event(EventKind::SessionStateChanged(SessionState::Idle)),
            event(EventKind::AgentMessage {
                parent: None,
                text: "ok".into(),
            }),
        ],
    };
    let seqs: Vec<Seq> = batch.iter().map(|(seq, _)| seq).collect();
    assert_eq!(seqs, [Seq(7), Seq(8), Seq(9)]);
    assert_eq!(batch.last(), Some(Seq(9)));

    let empty = EventBatch {
        from: Seq(10),
        events: vec![],
    };
    assert_eq!(empty.iter().count(), 0);
    assert_eq!(empty.last(), None);
    assert_eq!(Seq::ZERO.next(), Seq(1));
}

#[test]
fn api_paths() {
    assert_eq!(path::PAIR, "/v1/pair");
    assert_eq!(path::HELLO, "/v1/hello");
    assert_eq!(path::COMMANDS, "/v1/commands");
    assert_eq!(path::EVENTS, "/v1/events");
    assert_eq!(path::CONVERSATIONS, "/v1/conversations");
    assert_eq!(path::WORKDIR_CHANGES, "/v1/workdir/changes");
    assert_eq!(path::WORKDIR_LIST, "/v1/workdir/list");
    assert_eq!(path::WORKDIR_FILE, "/v1/workdir/file");
    assert_eq!(path::BLOB_ROUTE, "/v1/blobs/{id}");
    assert_eq!(
        path::blob(blob_id()),
        "/v1/blobs/01920000-0000-7000-8000-000000000005"
    );
}

#[test]
fn fresh_ids_are_uuid_v7() {
    assert_eq!(SessionId::new().0.get_version_num(), 7);
    assert_eq!(CommandId::new().0.get_version_num(), 7);
}

/// Pins the JSON wire format. If this fails, the format changed: update the
/// expectation deliberately (`UPDATE_SNAPSHOTS=1 cargo test`) and consider
/// bumping `PROTOCOL_VERSION`.
#[test]
fn wire_format_snapshot() {
    let commands: Vec<Command> = all_command_kinds()
        .into_iter()
        .map(|kind| Command {
            id: command(),
            kind,
        })
        .collect();
    // A struct, not `json!`, so keys keep their real wire order.
    #[derive(serde::Serialize)]
    struct Snapshot {
        protocol_version: u32,
        hello: HelloResponse,
        pair_request: PairRequest,
        pair_response: PairResponse,
        events_query: EventsQuery,
        event_batch: EventBatch,
        commands: Vec<Command>,
        command_results: Vec<CommandResult>,
        workdir_query: WorkdirQuery,
        workdir_changes: Vec<ChangesResult>,
        workdir_list: Vec<ListResult>,
        workdir_file: Vec<FileResult>,
        conversations: Vec<ConversationsResult>,
    }
    let actual = serde_json::to_string_pretty(&Snapshot {
        protocol_version: PROTOCOL_VERSION,
        hello: hello(),
        pair_request: pair_request(),
        pair_response: pair_response(),
        events_query: EventsQuery {
            after: Seq(16),
            wait: Some(30),
            limit: Some(500),
        },
        event_batch: batch(),
        commands,
        command_results: all_command_results(),
        workdir_query: workdir_query(),
        workdir_changes: all_changes_results(),
        workdir_list: all_list_results(),
        workdir_file: all_file_results(),
        conversations: all_conversations_results(),
    })
    .unwrap();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/wire.json");
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(path, format!("{actual}\n")).unwrap();
    }
    let expected = std::fs::read_to_string(path).unwrap();
    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "\nactual:\n{actual}\n"
    );
}
