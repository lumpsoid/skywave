//! The Dart contract: one file per feature, DTOs shaped for the GUI.
//! Run `rinf gen` in `gui/` after changing anything here.
//!
//! `rinf gen` reads every signal type in the crate into one flat namespace,
//! so names are unique across files and avoid Flutter's own.
//!
//! - [`boot`]: `Boot` in, `Booted`/`BootFailed` out, `HubFailed` on a
//!   panic.
//! - [`intents`]: every GUI intent after boot, as one ordered `GuiIntent`.
//! - [`link`]: the banner (`LinkUpdate`).
//! - [`screen`]: the screen and its data but the timeline (`ScreenUpdate`).
//! - [`timeline`]: the open session's timeline, snapshot then keyed diffs.
//! - [`search`]: the timeline search (`SearchUpdate`).
//! - [`shell`]: the open session's shell runs, snapshot then keyed diffs.
//! - [`workdir`]: the workdir screen's data (`ScreenView::Workdir`).
//! - [`effects`]: one-shot platform effects (`PlatformEffect`).

mod boot;
mod effects;
mod intents;
mod link;
mod screen;
mod search;
mod shell;
mod timeline;
mod workdir;

pub use boot::{Boot, BootFailed, Booted, HubFailed};
pub use effects::PlatformEffect;
pub use intents::{ActionIntent, BlobPolicyChoice, CopyWhat, DecisionView, GuiIntent, RouteView};
pub use link::{LinkState, LinkUpdate};
pub use screen::{
    AgentSettingsView, AgentState, ApprovalPanel, BlobPolicyView, CandidateView, CompletionPopup,
    ComposerModeView, ComposerView, ContextRow, ConversationRowView, ConversationsView,
    CreatePrompt, CreatingView, EffortOption, EffortView, ListKindView, ModeOption, ModelOption,
    PairForm, PairingState, PermissionModeView, RenamePrompt, RepairNotice, RuleOption,
    ScheduleRow, ScreenUpdate, ScreenView, SessionRowView, SessionScreen, ShellPromptView,
    SnippetView, StopHeadings, StorageGroupingView, StorageRowView, StorageTotalView,
    SubagentHeaderView, TaskRow, UsageGroupingView, UsageRowView, UsageSpanView,
};
pub use search::{LineHit, SearchHit, SearchUpdate, Utf16Range};
pub use shell::{ShellRunKind, ShellRunRow, ShellUpdate};
pub use timeline::{
    BlobFacts, BlobStateKind, LineStyle, OutgoingState, OutgoingView, RowKind, SectionFormat,
    SectionLine, SectionView, TimelineRow, TimelineUpdate, ToolFacts, ToolKindView, ToolState,
};
pub use workdir::{
    WorkdirChangeKind, WorkdirChangeRow, WorkdirContent, WorkdirEntryRow, WorkdirLine,
    WorkdirLineKind, WorkdirPaneKind, WorkdirScreen,
};
