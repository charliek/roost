//! What Dart renders: plain projections of a host's connection state and
//! its mirrored workspace (plan 089 C5's `HostView`, `WorkspaceView`).

use roost_client::dial::DialError;
use roost_client::host::HostState;
use roost_client::mirror::HostMirror;
use roost_ipc::agent::AgentLifecycle;
use roost_ipc::messages::Tab;
use roost_ipc::ssh::SshFailure;
use roost_ui_model::agent_palette::tab_status_text;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostStateView {
    Disconnected,
    Connecting,
    Connected,
    Retrying { in_ms: u64, reason: String },
    NeedsTrust { fingerprint: String },
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostView {
    pub id: String,
    pub label: String,
    pub target: String,
    pub state: HostStateView,
    pub waiting_tabs: u32,
    pub working_tabs: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceView {
    pub projects: Vec<ProjectView>,
    pub active_project_id: i64,
    pub active_tab_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectView {
    pub id: i64,
    pub name: String,
    pub cwd: String,
    pub tabs: Vec<TabView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabView {
    pub id: i64,
    pub title: String,
    pub cwd: String,
    pub agent: AgentBadge,
    pub has_notification: bool,
    /// The desktop's agent status line ("Waiting · <OSC 7501 msg>"),
    /// for a tab with an agent badge.
    pub status_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentBadge {
    None,
    Idle,
    Working,
    Waiting,
}

/// A host connection's state as the app shows it. For a dial the far
/// side refused, the dialer's classified remote error says more than the
/// transport's read failure does; a host with no session running is a
/// state the user has to fix there, so it shows as failed even while the
/// connection keeps retrying underneath.
pub fn host_state(state: HostState, remote: Option<DialError>, host: &str) -> HostStateView {
    let remote = match remote {
        Some(DialError::Remote { failure, message }) => Some((failure, message)),
        _ => None,
    };
    let (retry_in, reason) = match state {
        HostState::Idle => return HostStateView::Disconnected,
        HostState::Connecting => return HostStateView::Connecting,
        HostState::Connected => return HostStateView::Connected,
        HostState::NeedsTrust { fingerprint } => return HostStateView::NeedsTrust { fingerprint },
        HostState::Retrying { in_ms, reason } => (Some(in_ms), reason),
        HostState::Failed { reason } => (None, reason),
    };
    let reason = match remote {
        Some((Some(SshFailure::NoSession), _)) => {
            return HostStateView::Failed {
                reason: format!(
                    "No roost-session is running on {host}. Start one there with `roostctl \
                     session start`."
                ),
            }
        }
        Some((_, message)) => message,
        None => reason,
    };
    match retry_in {
        Some(in_ms) => HostStateView::Retrying { in_ms, reason },
        None => HostStateView::Failed { reason },
    }
}

pub fn workspace(mirror: &HostMirror) -> WorkspaceView {
    WorkspaceView {
        projects: mirror
            .projects
            .iter()
            .map(|project| ProjectView {
                id: project.id,
                name: project.name.clone(),
                cwd: project.cwd.clone(),
                tabs: project.tabs.iter().map(tab).collect(),
            })
            .collect(),
        active_project_id: mirror.active_project_id,
        active_tab_id: mirror.active_tab_id,
    }
}

/// The badge is what the tab presents — its effective lifecycle, program
/// status composed over the hooks — so the phone and the desktop's dot
/// agree. A failed agent needs the user as much as a waiting one.
pub fn tab(tab: &Tab) -> TabView {
    let effective = tab.effective_lifecycle();
    let has_agent = tab.ownership.is_some()
        || tab
            .program_status
            .as_ref()
            .and_then(|status| status.record.as_ref())
            .is_some_and(|record| !record.app.is_empty());
    let agent = match effective {
        AgentLifecycle::Working => AgentBadge::Working,
        AgentLifecycle::Waiting | AgentLifecycle::Failed => AgentBadge::Waiting,
        AgentLifecycle::Finished => AgentBadge::Idle,
        AgentLifecycle::Inactive if has_agent => AgentBadge::Idle,
        AgentLifecycle::Inactive => AgentBadge::None,
    };
    let detail = tab
        .ownership
        .as_ref()
        .map_or("", |ownership| ownership.detail.as_str());
    TabView {
        id: tab.id,
        title: tab.title.clone(),
        cwd: tab.cwd.clone(),
        agent,
        has_notification: tab.has_notification,
        status_text: (agent != AgentBadge::None).then(|| tab_status_text(tab, effective, detail)),
    }
}

/// (waiting, working) tabs across the workspace.
pub fn agent_counts(mirror: &HostMirror) -> (u32, u32) {
    mirror.tabs().fold((0, 0), |(waiting, working), tab| {
        match self::tab(tab).agent {
            AgentBadge::Waiting => (waiting + 1, working),
            AgentBadge::Working => (waiting, working + 1),
            AgentBadge::Idle | AgentBadge::None => (waiting, working),
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn wire_tab(extra: serde_json::Value) -> Tab {
        let mut value = json!({
            "id": "7", "project_id": "1", "title": "sh", "cwd": "/home/me",
            "state": "none", "has_notification": false, "is_active": true,
            "user_titled": false, "position": 0, "created_at": 0, "last_active": 0,
            "hook_active": false,
        });
        if let (Some(base), serde_json::Value::Object(extra)) = (value.as_object_mut(), extra) {
            base.extend(extra);
        }
        serde_json::from_value(value).expect("a wire tab")
    }

    #[test]
    fn a_plain_shell_has_no_badge_and_a_running_command_works() {
        assert_eq!(tab(&wire_tab(json!({}))).agent, AgentBadge::None);
        assert_eq!(tab(&wire_tab(json!({}))).status_text, None);
        let busy = tab(&wire_tab(json!({"shell_state": "foreground_process"})));
        assert_eq!(busy.agent, AgentBadge::Working);
    }

    #[test]
    fn an_agent_waiting_on_the_user_shows_why() {
        let waiting = tab(&wire_tab(json!({
            "agent_lifecycle": "waiting",
            "ownership": {"source": "claude", "session_id": "s1"},
        })));
        assert_eq!(waiting.agent, AgentBadge::Waiting);
        assert_eq!(waiting.status_text.as_deref(), Some("Waiting for input"));

        let failed = tab(&wire_tab(json!({
            "agent_lifecycle": "failed",
            "ownership": {"source": "claude", "session_id": "s1"},
        })));
        assert_eq!(
            failed.agent,
            AgentBadge::Waiting,
            "a failure needs the user too"
        );

        let idle = tab(&wire_tab(json!({
            "agent_lifecycle": "inactive",
            "ownership": {"source": "claude", "session_id": "s1"},
        })));
        assert_eq!(idle.agent, AgentBadge::Idle, "an agent between turns");
    }

    #[test]
    fn a_program_status_record_decides_the_badge_and_its_message() {
        let blocked = tab(&wire_tab(json!({
            "program_status": {
                "effective_lifecycle": "waiting",
                "decided_by": "records",
                "children": 0,
                "record": {"state": "blocked", "msg": "approve rm -rf?", "app": "claude-code",
                           "updated_at": 0},
            },
        })));
        assert_eq!(blocked.agent, AgentBadge::Waiting);
        assert_eq!(
            blocked.status_text.as_deref(),
            Some("Waiting · approve rm -rf?")
        );
    }

    #[test]
    fn a_host_with_no_session_is_failed_with_the_remedy() {
        let no_session = DialError::Remote {
            failure: Some(SshFailure::NoSession),
            message: "client-bridge: no session".into(),
        };
        let view = host_state(
            HostState::Retrying {
                in_ms: 250,
                reason: "the data connection closed".into(),
            },
            Some(no_session),
            "popos",
        );
        assert_eq!(
            view,
            HostStateView::Failed {
                reason: "No roost-session is running on popos. Start one there with `roostctl \
                         session start`."
                    .into()
            }
        );

        let auth = DialError::Remote {
            failure: Some(SshFailure::Auth),
            message: "the host refused the login".into(),
        };
        assert_eq!(
            host_state(
                HostState::Failed {
                    reason: "io".into()
                },
                Some(auth),
                "popos"
            ),
            HostStateView::Failed {
                reason: "the host refused the login".into()
            }
        );
        assert_eq!(
            host_state(HostState::Connected, None, "popos"),
            HostStateView::Connected
        );
        assert_eq!(
            host_state(HostState::Idle, None, "popos"),
            HostStateView::Disconnected
        );
    }
}
