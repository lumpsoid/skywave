//! The Dart contract: one file per feature, DTOs shaped for the GUI.
//! Run `rinf gen` in `gui/` after changing anything here.
//!
//! `rinf gen` reads every signal type in the crate into one flat namespace,
//! so names are unique across files and avoid Flutter's own.
//!
//! - [`boot`]: `Boot` in, `Booted`/`BootFailed` out.
//! - [`intents`]: every GUI intent after boot, as one ordered `GuiIntent`.
//! - [`link`]: the banner (`LinkUpdate`).
//! - [`screen`]: the screen and its data but the timeline (`ScreenUpdate`).
//! - [`timeline`]: the open session's timeline, snapshot then keyed diffs.
//! - [`search`]: the timeline search (`SearchUpdate`).
//! - [`effects`]: one-shot platform effects (`PlatformEffect`).

mod boot;
mod effects;
mod intents;
mod link;
mod screen;
mod search;
mod timeline;

pub use boot::{Boot, BootFailed, Booted};
pub use effects::PlatformEffect;
pub use intents::{ActionIntent, BlobPolicyChoice, CopyWhat, DecisionView, GuiIntent, RouteView};
pub use link::{LinkState, LinkUpdate};
pub use screen::{
    AgentSettingsView, AgentState, ApprovalPanel, BlobPolicyView, CandidateView, CompletionPopup,
    ComposerView, ContextRow, CreatePrompt, CreatingView, EffortOption, EffortView, ListKindView,
    ModeOption, ModelOption, PairForm, PairingState, PermissionModeView, RenamePrompt, RuleOption,
    ScreenUpdate, ScreenView, SessionRowView, SessionScreen, SnippetView,
};
pub use search::{LineHit, SearchHit, SearchUpdate, Utf16Range};
pub use timeline::{
    BlobFacts, BlobStateKind, LineStyle, OutgoingState, OutgoingView, RowKind, SectionFormat,
    SectionLine, SectionView, TimelineRow, TimelineUpdate, ToolFacts, ToolKindView, ToolState,
};
