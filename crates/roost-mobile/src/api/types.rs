//! The values that cross the bridge. Flat on purpose: an enum with fields
//! becomes a freezed class on the Dart side, which would add a second
//! codegen step for no gain, since Dart maps these to its own models.

use crate::app::{LeakCounts, SettingsState, ThemeSummary};
use crate::keys::KeyInput;
use crate::owner::TerminalEvent;
use crate::registry::{Appearance, MobileSettings, ReadableText};
use crate::terminal::{AttachState, Geometry};
use crate::views::{AgentBadge, HostStateView, HostView, ProjectView, TabView, WorkspaceView};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeGeometry {
    pub cols: u16,
    pub rows: u16,
    pub cell_w_px: u16,
    pub cell_h_px: u16,
}

impl From<BridgeGeometry> for Geometry {
    fn from(g: BridgeGeometry) -> Self {
        Self {
            cols: g.cols,
            rows: g.rows,
            cell_w_px: g.cell_w_px,
            cell_h_px: g.cell_h_px,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeKeyInput {
    pub key: String,
    pub text: Option<String>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub repeat: bool,
}

impl From<BridgeKeyInput> for KeyInput {
    fn from(k: BridgeKeyInput) -> Self {
        Self {
            key: k.key,
            text: k.text,
            ctrl: k.ctrl,
            alt: k.alt,
            shift: k.shift,
            meta: k.meta,
            repeat: k.repeat,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeHostStateKind {
    Disconnected,
    Connecting,
    Connected,
    Retrying,
    NeedsTrust,
    Failed,
}

/// `HostStateView`, flattened: `retry_in_ms` for `Retrying`, `reason` for
/// `Retrying` and `Failed`, `fingerprint` for `NeedsTrust`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeHostState {
    pub kind: BridgeHostStateKind,
    pub retry_in_ms: u32,
    pub reason: Option<String>,
    pub fingerprint: Option<String>,
}

impl From<HostStateView> for BridgeHostState {
    fn from(state: HostStateView) -> Self {
        let plain = |kind| Self {
            kind,
            retry_in_ms: 0,
            reason: None,
            fingerprint: None,
        };
        match state {
            HostStateView::Disconnected => plain(BridgeHostStateKind::Disconnected),
            HostStateView::Connecting => plain(BridgeHostStateKind::Connecting),
            HostStateView::Connected => plain(BridgeHostStateKind::Connected),
            HostStateView::Retrying { in_ms, reason } => Self {
                retry_in_ms: u32::try_from(in_ms).unwrap_or(u32::MAX),
                reason: Some(reason),
                ..plain(BridgeHostStateKind::Retrying)
            },
            HostStateView::NeedsTrust { fingerprint } => Self {
                fingerprint: Some(fingerprint),
                ..plain(BridgeHostStateKind::NeedsTrust)
            },
            HostStateView::Failed { reason } => Self {
                reason: Some(reason),
                ..plain(BridgeHostStateKind::Failed)
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeHostView {
    pub id: String,
    pub label: String,
    pub target: String,
    pub state: BridgeHostState,
    pub waiting_tabs: u32,
    pub working_tabs: u32,
}

impl From<HostView> for BridgeHostView {
    fn from(h: HostView) -> Self {
        Self {
            id: h.id,
            label: h.label,
            target: h.target,
            state: h.state.into(),
            waiting_tabs: h.waiting_tabs,
            working_tabs: h.working_tabs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeAgentBadge {
    None,
    Idle,
    Working,
    Waiting,
}

impl From<AgentBadge> for BridgeAgentBadge {
    fn from(badge: AgentBadge) -> Self {
        match badge {
            AgentBadge::None => Self::None,
            AgentBadge::Idle => Self::Idle,
            AgentBadge::Working => Self::Working,
            AgentBadge::Waiting => Self::Waiting,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeTabView {
    pub id: i64,
    pub title: String,
    pub cwd: String,
    pub agent: BridgeAgentBadge,
    pub has_notification: bool,
    pub status_text: Option<String>,
}

impl From<TabView> for BridgeTabView {
    fn from(t: TabView) -> Self {
        Self {
            id: t.id,
            title: t.title,
            cwd: t.cwd,
            agent: t.agent.into(),
            has_notification: t.has_notification,
            status_text: t.status_text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeProjectView {
    pub id: i64,
    pub name: String,
    pub cwd: String,
    pub tabs: Vec<BridgeTabView>,
}

impl From<ProjectView> for BridgeProjectView {
    fn from(p: ProjectView) -> Self {
        Self {
            id: p.id,
            name: p.name,
            cwd: p.cwd,
            tabs: p.tabs.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeWorkspaceView {
    pub projects: Vec<BridgeProjectView>,
    pub active_project_id: i64,
    pub active_tab_id: i64,
}

impl From<WorkspaceView> for BridgeWorkspaceView {
    fn from(w: WorkspaceView) -> Self {
        Self {
            projects: w.projects.into_iter().map(Into::into).collect(),
            active_project_id: w.active_project_id,
            active_tab_id: w.active_tab_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeAttachState {
    Attaching,
    Live,
    Reconnecting,
    Ended,
}

impl From<AttachState> for BridgeAttachState {
    fn from(state: AttachState) -> Self {
        match state {
            AttachState::Attaching => Self::Attaching,
            AttachState::Live => Self::Live,
            AttachState::Reconnecting => Self::Reconnecting,
            AttachState::Ended => Self::Ended,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeTerminalEventKind {
    FrameReady,
    Status,
    Owner,
    Exited,
    Clipboard,
}

/// `TerminalEvent`, flattened: `state` and `message` for `Status`;
/// `owner_label`, `is_me`, `cols`, `rows` for `Owner`; `exit_code` for
/// `Exited`; `text` for `Clipboard`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeTerminalEvent {
    pub kind: BridgeTerminalEventKind,
    pub state: BridgeAttachState,
    pub message: Option<String>,
    pub owner_label: Option<String>,
    pub is_me: bool,
    pub cols: u16,
    pub rows: u16,
    pub exit_code: i32,
    pub text: Option<String>,
}

impl From<TerminalEvent> for BridgeTerminalEvent {
    fn from(event: TerminalEvent) -> Self {
        let plain = |kind| Self {
            kind,
            state: BridgeAttachState::Live,
            message: None,
            owner_label: None,
            is_me: false,
            cols: 0,
            rows: 0,
            exit_code: 0,
            text: None,
        };
        match event {
            TerminalEvent::FrameReady => plain(BridgeTerminalEventKind::FrameReady),
            TerminalEvent::Status { state, message } => Self {
                state: state.into(),
                message,
                ..plain(BridgeTerminalEventKind::Status)
            },
            TerminalEvent::Owner {
                label,
                is_me,
                cols,
                rows,
            } => Self {
                owner_label: label,
                is_me,
                cols,
                rows,
                ..plain(BridgeTerminalEventKind::Owner)
            },
            TerminalEvent::Exited { code } => Self {
                exit_code: code,
                ..plain(BridgeTerminalEventKind::Exited)
            },
            TerminalEvent::Clipboard { text } => Self {
                text: Some(text),
                ..plain(BridgeTerminalEventKind::Clipboard)
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeLeakCounts {
    pub hosts: u32,
    pub host_pumps: u32,
    pub attach_pumps: u32,
    pub attachments: u32,
    pub terminals: u32,
}

impl From<LeakCounts> for BridgeLeakCounts {
    fn from(c: LeakCounts) -> Self {
        let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        Self {
            hosts: n(c.hosts),
            host_pumps: n(c.host_pumps),
            attach_pumps: n(c.attach_pumps),
            attachments: n(c.attachments),
            terminals: n(c.terminals),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeAppearance {
    System,
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeReadableText {
    Auto,
    On,
    Off,
}

/// `MobileSettings` without its file version.
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeSettings {
    pub terminal_font_size_compact: f64,
    pub terminal_font_size_wide: f64,
    pub ui_text_scale: f64,
    pub appearance: BridgeAppearance,
    pub dark_theme: String,
    pub light_theme: String,
    pub readable_text: BridgeReadableText,
    pub bright_faint: bool,
}

impl From<BridgeSettings> for MobileSettings {
    fn from(s: BridgeSettings) -> Self {
        Self {
            terminal_font_size_compact: s.terminal_font_size_compact,
            terminal_font_size_wide: s.terminal_font_size_wide,
            ui_text_scale: s.ui_text_scale,
            appearance: match s.appearance {
                BridgeAppearance::System => Appearance::System,
                BridgeAppearance::Dark => Appearance::Dark,
                BridgeAppearance::Light => Appearance::Light,
            },
            dark_theme: s.dark_theme,
            light_theme: s.light_theme,
            readable_text: match s.readable_text {
                BridgeReadableText::Auto => ReadableText::Auto,
                BridgeReadableText::On => ReadableText::On,
                BridgeReadableText::Off => ReadableText::Off,
            },
            bright_faint: s.bright_faint,
            ..Self::default()
        }
    }
}

impl From<MobileSettings> for BridgeSettings {
    fn from(s: MobileSettings) -> Self {
        Self {
            terminal_font_size_compact: s.terminal_font_size_compact,
            terminal_font_size_wide: s.terminal_font_size_wide,
            ui_text_scale: s.ui_text_scale,
            appearance: match s.appearance {
                Appearance::System => BridgeAppearance::System,
                Appearance::Dark => BridgeAppearance::Dark,
                Appearance::Light => BridgeAppearance::Light,
            },
            dark_theme: s.dark_theme,
            light_theme: s.light_theme,
            readable_text: match s.readable_text {
                ReadableText::Auto => BridgeReadableText::Auto,
                ReadableText::On => BridgeReadableText::On,
                ReadableText::Off => BridgeReadableText::Off,
            },
            bright_faint: s.bright_faint,
        }
    }
}

/// The settings, and the brightness and terminal theme they make now.
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeSettingsState {
    pub settings: BridgeSettings,
    pub effective_dark: bool,
    pub effective_theme: String,
}

impl From<SettingsState> for BridgeSettingsState {
    fn from(state: SettingsState) -> Self {
        Self {
            effective_dark: state.effective_dark(),
            effective_theme: state.effective_theme_name().to_string(),
            settings: state.settings.into(),
        }
    }
}

/// Colors are `0x00RRGGBB`; `ansi` holds the 16 ANSI colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeThemeSummary {
    pub name: String,
    pub light: bool,
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    pub ansi: Vec<u32>,
}

impl From<ThemeSummary> for BridgeThemeSummary {
    fn from(t: ThemeSummary) -> Self {
        Self {
            name: t.name,
            light: t.light,
            background: t.background,
            foreground: t.foreground,
            cursor: t.cursor,
            ansi: t.ansi,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_states_flatten_with_their_detail() {
        let retrying: BridgeHostState = HostStateView::Retrying {
            in_ms: 8_000,
            reason: "reset".into(),
        }
        .into();
        assert_eq!(retrying.kind, BridgeHostStateKind::Retrying);
        assert_eq!(retrying.retry_in_ms, 8_000);
        assert_eq!(retrying.reason.as_deref(), Some("reset"));

        let trust: BridgeHostState = HostStateView::NeedsTrust {
            fingerprint: "SHA256:x".into(),
        }
        .into();
        assert_eq!(trust.kind, BridgeHostStateKind::NeedsTrust);
        assert_eq!(trust.fingerprint.as_deref(), Some("SHA256:x"));
    }

    #[test]
    fn terminal_events_flatten_with_their_detail() {
        let owner: BridgeTerminalEvent = TerminalEvent::Owner {
            label: Some("laptop".into()),
            is_me: false,
            cols: 132,
            rows: 40,
        }
        .into();
        assert_eq!(owner.kind, BridgeTerminalEventKind::Owner);
        assert_eq!(owner.owner_label.as_deref(), Some("laptop"));
        assert_eq!((owner.cols, owner.rows), (132, 40));

        let exited: BridgeTerminalEvent = TerminalEvent::Exited { code: 3 }.into();
        assert_eq!(
            (exited.kind, exited.exit_code),
            (BridgeTerminalEventKind::Exited, 3)
        );
    }
}
