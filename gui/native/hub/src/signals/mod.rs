//! The Dart contract: one file per feature, DTOs shaped for the GUI.
//! Run `rinf gen` in `gui/` after changing anything here.
//!
//! `rinf gen` reads every signal type in the crate into one flat namespace,
//! so names are unique across files and avoid Flutter's own.
//!
//! - [`boot`]: `Boot` in, `Booted`/`BootFailed` out, `HubFailed` on a
//!   panic.
//! - [`intents`]: every GUI intent after boot, as one ordered `GuiIntent`.
//! - [`link`]: the link's status line, banner and approvals elsewhere (`LinkUpdate`).
//! - [`screen`]: the screen and its data but the timeline (`ScreenUpdate`).
//! - [`timeline`]: the open session's timeline, snapshot then keyed diffs.
//! - [`search`]: the timeline search (`SearchUpdate`).
//! - [`shell`]: the open session's shell runs, snapshot then keyed diffs.
//! - [`workdir`]: the workdir screen's data (`ScreenView::Workdir`).
//! - [`crop`]: the crop page (`ScreenView::Crop`), its line windows
//!   (`CropLinesUpdate`), and the blob a delete or crop names
//!   (`BlobTargetView`).
//! - [`effects`]: one-shot platform effects (`PlatformEffect`).

mod boot;
mod crop;
mod effects;
mod intents;
mod link;
mod screen;
mod search;
mod shell;
mod timeline;
mod workdir;

pub use boot::{Boot, BootFailed, Booted, HubFailed};
pub use crop::{
    BlobTargetView, BlobUseView, CropContentKind, CropLine, CropLinesUpdate, CropPresetView,
    CropScreen, LineSpanView,
};
pub use effects::PlatformEffect;
pub use intents::{
    ActionIntent, BlobPolicyChoice, CopyWhat, DecisionView, GuiIntent, PastedCode,
    QuestionAnswerView, RouteView,
};
pub use link::{LinkState, LinkUpdate};
pub use screen::{
    AgentLoginView, AgentSettingsView, AgentState, ApprovalPanel, BlobPolicyView, CandidateView,
    CompletionPopup, ComposerImage, ComposerModeView, ComposerView, ContextRow,
    ConversationRowView, ConversationsView, CreatePrompt, CreatingView, EffortOption, EffortView,
    InfoFieldView, InfoSectionView, LimitRowView, LimitsView, ListKindView, LoginFlowState,
    ModeOption, ModelOption, PairForm, PairingState, PermissionModeView, QuestionOptionView,
    QuestionPanel, QuestionView, RenamePrompt, RepairNotice, RuleOption, ScheduleRow, ScreenUpdate,
    ScreenView, SessionRowView, SessionScreen, ShellPromptView, SnippetView, StopHeadings,
    StorageGroupingView, StorageRowView, StorageTotalView, SubagentHeaderView, SubagentRowView,
    TaskRow, UsageGroupingView, UsageRowView, UsageSpanView,
};
pub use search::{LineHit, SearchHit, SearchUpdate, Utf16Range};
pub use shell::{ShellRunKind, ShellRunRow, ShellUpdate};
pub use timeline::{
    BlobFacts, BlobStateKind, LineStyle, MarkdownAlign, MarkdownBlock, MarkdownCell, MarkdownKind,
    MarkdownRow, MarkdownSpan, OutgoingState, OutgoingView, RowImage, RowKind, SectionFormat,
    SectionLine, SectionView, TimelineRow, TimelineUpdate, ToolFacts, ToolKindView, ToolState,
};
pub use workdir::{
    WorkdirChangeKind, WorkdirChangeRow, WorkdirContent, WorkdirEntryRow, WorkdirLine,
    WorkdirLineKind, WorkdirPaneKind, WorkdirScreen,
};
