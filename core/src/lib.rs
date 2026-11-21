#![cfg_attr(feature = "ffi", allow(unsafe_code))]

mod daemon;
mod harness;
mod highlight;
mod images;
mod model;
mod oneshot;
mod platform;
pub mod plugins;
mod scratch;
mod session;
mod skills;
mod store;
mod studio;
mod threads;
mod transcript;
pub mod tunnel;
#[cfg(feature = "wire")]
mod wire;
mod workspace;

pub use local_studio_version::{CHANNEL, VERSION};

pub use highlight::{highlight, highlight_lines, CodeSegment, SyntaxKind};
pub use model::{
    AccessMode, Approval, ApprovalAnswer, Block, CommandKind, Connection, ContextUsage, DaemonMismatch, Delivery,
    FileChange, Goal, Harness, ImageAttachment, ImageThumbnail, Item, ItemKind, ModelKey, ModelOption, ModelRef,
    NoticeLevel, NotificationKind, Project, PromptDraft, QueuedMessage, Shelf, SkillEntry, SkillSource, SlashCommand,
    Subagent, ThreadInfo, ThreadRow, ThreadSearchHit, ThreadSettings, ThreadStatus, ToolKind, ToolState, TranscriptOp,
    Update, WorktreeChoice,
};
pub use scratch::{check_scratch_agent, stored_keys_empty};
pub use store::data_dir;
pub use studio::{process_name, DaemonLaunch, Studio, StudioConfig, StudioError, Subscription, Waker};
pub use workspace::model::*;
pub use workspace::{endpoint_presets, terminal_preview};

#[cfg_attr(feature = "ffi", uniffi::export)]
#[must_use]
pub fn build_version() -> String {
    VERSION.to_string()
}

#[cfg_attr(feature = "ffi", uniffi::export)]
#[must_use]
pub fn build_channel() -> String {
    CHANNEL.to_string()
}

#[cfg(feature = "ffi")]
uniffi::setup_scaffolding!("local_studio_core");
