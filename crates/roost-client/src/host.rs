//! One host's connection task for a client that is not iced: prologue,
//! mirror, event pump, control-plane ops and reconnect, all over a
//! [`crate::dial::Dialer`].

/// Who this client is to every session it reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostClientConfig {
    /// Stable per install: 1–64 bytes of `[A-Za-z0-9._-]`.
    pub client_id: String,
    /// The device name sessions show as a tab's size owner.
    pub label: String,
    /// This client's `roost_vt::libghostty_build()`, offered on attach so
    /// a matching session serves `ghostty-snapshot`.
    pub libghostty_build: String,
}

/// The session a connection reached, as `session.identify` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFacts {
    pub session_id: String,
    pub libghostty_build: String,
    pub payload_kinds: Vec<String>,
    pub app_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostState {
    Connecting,
    Connected,
    /// Dropped; the next attempt is scheduled.
    Retrying { in_ms: u64, reason: String },
    /// The host key is not pinned yet. Waits for the user.
    NeedsTrust { fingerprint: String },
    /// Needs the user: a protocol mismatch, a changed host key, a host
    /// that refuses the login. Waits for `reconnect_now`.
    Failed { reason: String },
    /// Disconnected on request.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    State(HostState),
    /// The mirror changed; read it through the client.
    Workspace,
    /// `notification.fired` — live only, never replayed.
    Notification {
        tab_id: i64,
        title: String,
        body: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostCallError {
    #[error("not connected")]
    NotConnected,
    #[error("{code}: {message}")]
    Server { code: String, message: String },
    #[error("{0}")]
    Transport(String),
}
