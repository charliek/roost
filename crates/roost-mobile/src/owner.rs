//! The one thread every attached tab's libghostty state lives on.
//!
//! libghostty handles are `Send` but not `Sync`, so they stay on a single
//! std thread fed by a command channel; callers on any thread (tokio
//! tasks forwarding attach events, the FFI layer answering Dart) only
//! ever hold a [`TerminalOwner`] handle. Each tab reports back through
//! its own [`TabSink`], called on the owner thread in command order.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use roost_client::attach::AttachEvent;
use roost_ipc::dataframe::InputTake;
use roost_ui_model::theme::Theme;

use crate::keys::KeyInput;
use crate::terminal::{Applied, AttachState, Input, TabTerminal};

/// Names one tab's terminal on its owner. Minted per [`TerminalOwner::open`],
/// so a re-attached tab never collides with the one it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TerminalId(u64);

/// C5's `TerminalEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// A frame is ready to pull. Sent once until the next
    /// [`TerminalOwner::take_frame`], however much changes meanwhile.
    FrameReady,
    Status {
        state: AttachState,
        message: Option<String>,
    },
    Owner {
        label: Option<String>,
        is_me: bool,
        cols: u16,
        rows: u16,
    },
    Exited {
        code: i32,
    },
}

/// What a tab hands back to whoever holds its attach stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabOutput {
    Event(TerminalEvent),
    /// Send to the program: `AttachStream::input(bytes, take)`.
    Input {
        bytes: Vec<u8>,
        take: InputTake,
    },
    /// The terminal is off the stream's resume point for good: forget
    /// it and re-attach for a fresh snapshot.
    Reattach {
        reason: String,
    },
}

pub type TabSink = Box<dyn FnMut(TabOutput) + Send>;

pub struct OwnerConfig {
    /// This install's `client_id`, to tell a size owner that is us.
    pub client_id: String,
    pub theme: Theme,
}

enum Command {
    Open {
        id: TerminalId,
        sink: TabSink,
    },
    Apply(TerminalId, AttachEvent),
    Key(TerminalId, KeyInput),
    Text(TerminalId, String),
    Paste(TerminalId, String),
    Scroll {
        id: TerminalId,
        rows: i32,
        col: u16,
        row: u16,
    },
    TakeFrame {
        id: TerminalId,
        full: bool,
        reply: mpsc::SyncSender<Vec<u8>>,
    },
    ViewportText {
        id: TerminalId,
        reply: mpsc::SyncSender<Vec<String>>,
    },
    SetTheme(Box<Theme>),
    Close(TerminalId),
    Shutdown,
}

pub struct TerminalOwner {
    commands: mpsc::Sender<Command>,
    thread: Option<JoinHandle<()>>,
    next_id: AtomicU64,
    live: Arc<AtomicUsize>,
}

impl TerminalOwner {
    pub fn spawn(config: OwnerConfig) -> std::io::Result<Self> {
        let (commands, inbox) = mpsc::channel();
        let live = Arc::new(AtomicUsize::new(0));
        let thread = std::thread::Builder::new()
            .name("roost-terminal-owner".into())
            .spawn({
                let live = Arc::clone(&live);
                move || run(config, &inbox, &live)
            })?;
        Ok(Self {
            commands,
            thread: Some(thread),
            next_id: AtomicU64::new(1),
            live,
        })
    }

    /// Start a tab's terminal. Nothing is drawn until its stream's first
    /// payload lands.
    pub fn open(&self, sink: TabSink) -> TerminalId {
        let id = TerminalId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.send(Command::Open { id, sink });
        id
    }

    /// Feed one attach event, in the stream's order.
    pub fn apply(&self, id: TerminalId, event: AttachEvent) {
        self.send(Command::Apply(id, event));
    }

    pub fn key(&self, id: TerminalId, key: KeyInput) {
        self.send(Command::Key(id, key));
    }

    /// Committed soft-keyboard text.
    pub fn text(&self, id: TerminalId, text: String) {
        self.send(Command::Text(id, text));
    }

    pub fn paste(&self, id: TerminalId, text: String) {
        self.send(Command::Paste(id, text));
    }

    /// Scroll by `rows`, positive toward older history; a wheel report or
    /// an arrow key aimed at (`col`, `row`) when the program reads it.
    pub fn scroll(&self, id: TerminalId, rows: i32, col: u16, row: u16) {
        self.send(Command::Scroll { id, rows, col, row });
    }

    /// The next C6 frame; empty when nothing changed, when the tab has
    /// nothing drawable yet, or when the tab is gone. Blocks until the
    /// owner thread has built it.
    pub fn take_frame(&self, id: TerminalId, full: bool) -> Vec<u8> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.send(Command::TakeFrame { id, full, reply });
        answer.recv().unwrap_or_default()
    }

    /// The drawn viewport as text, one trimmed line per row.
    pub fn viewport_text(&self, id: TerminalId) -> Vec<String> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.send(Command::ViewportText { id, reply });
        answer.recv().unwrap_or_default()
    }

    pub fn set_theme(&self, theme: Theme) {
        self.send(Command::SetTheme(Box::new(theme)));
    }

    /// Drop a tab's terminal. Idempotent.
    pub fn close(&self, id: TerminalId) {
        self.send(Command::Close(id));
    }

    /// Tab terminals alive on this owner.
    pub fn live_tabs(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            tracing::warn!("terminal owner thread is gone; command dropped");
        }
    }
}

impl Drop for TerminalOwner {
    fn drop(&mut self) {
        self.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::warn!("terminal owner thread panicked");
            }
        }
    }
}

/// Counts a tab for [`TerminalOwner::live_tabs`] while it lives.
struct Live(Arc<AtomicUsize>);

impl Live {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(count))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Tab {
    terminal: TabTerminal,
    sink: TabSink,
    /// A `FrameReady` is out and no frame has been pulled since.
    frame_announced: bool,
    _live: Live,
}

impl Tab {
    fn frame_moved(&mut self) {
        if !self.frame_announced {
            self.frame_announced = true;
            (self.sink)(TabOutput::Event(TerminalEvent::FrameReady));
        }
    }

    fn deliver(&mut self, applied: Applied, client_id: &str) {
        if let Some(status) = applied.status {
            (self.sink)(TabOutput::Event(TerminalEvent::Status {
                state: status.state,
                message: status.message,
            }));
        }
        if let Some(view) = applied.owner {
            let is_me = view
                .owner
                .as_ref()
                .is_some_and(|owner| owner.client_id == client_id);
            (self.sink)(TabOutput::Event(TerminalEvent::Owner {
                label: view.owner.map(|owner| owner.label),
                is_me,
                cols: view.cols,
                rows: view.rows,
            }));
        }
        if let Some(code) = applied.exited {
            (self.sink)(TabOutput::Event(TerminalEvent::Exited { code }));
        }
        if let Some(reason) = applied.reattach {
            (self.sink)(TabOutput::Reattach { reason });
        }
        if applied.frame_dirty {
            self.frame_moved();
        }
    }

    fn send_input(&mut self, input: roost_vt::Result<Input>, what: &str) {
        let input = match input {
            Ok(input) => input,
            Err(error) => {
                tracing::warn!(%error, "{what} could not be encoded; dropped");
                return;
            }
        };
        if !input.bytes.is_empty() {
            (self.sink)(TabOutput::Input {
                bytes: input.bytes,
                take: input.take,
            });
        }
        if input.frame_dirty {
            self.frame_moved();
        }
    }
}

fn run(config: OwnerConfig, inbox: &mpsc::Receiver<Command>, live: &Arc<AtomicUsize>) {
    let OwnerConfig {
        client_id,
        mut theme,
    } = config;
    let mut tabs: HashMap<TerminalId, Tab> = HashMap::new();
    while let Ok(command) = inbox.recv() {
        match command {
            Command::Open { id, mut sink } => match TabTerminal::new(theme.clone()) {
                Ok(terminal) => {
                    tabs.insert(
                        id,
                        Tab {
                            terminal,
                            sink,
                            frame_announced: false,
                            _live: Live::new(live),
                        },
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, "tab terminal could not be built");
                    sink(TabOutput::Event(TerminalEvent::Status {
                        state: AttachState::Ended,
                        message: Some(format!("terminal could not be built: {error}")),
                    }));
                }
            },
            Command::Apply(id, event) => {
                if let Some(tab) = tabs.get_mut(&id) {
                    let applied = tab.terminal.apply(event);
                    tab.deliver(applied, &client_id);
                }
            }
            Command::Key(id, key) => {
                if let Some(tab) = tabs.get_mut(&id) {
                    let input = tab.terminal.key(&key);
                    tab.send_input(input, "key");
                }
            }
            Command::Text(id, text) => {
                if let Some(tab) = tabs.get_mut(&id) {
                    let input = tab.terminal.text(&text);
                    tab.send_input(input, "text");
                }
            }
            Command::Paste(id, text) => {
                if let Some(tab) = tabs.get_mut(&id) {
                    let input = tab.terminal.paste(&text);
                    tab.send_input(Ok(input), "paste");
                }
            }
            Command::Scroll { id, rows, col, row } => {
                if let Some(tab) = tabs.get_mut(&id) {
                    let input = tab.terminal.scroll(rows, col, row);
                    tab.send_input(input, "scroll");
                }
            }
            Command::TakeFrame { id, full, reply } => {
                let frame = match tabs.get_mut(&id) {
                    Some(tab) => {
                        tab.frame_announced = false;
                        tab.terminal.take_frame(full).unwrap_or_else(|error| {
                            tracing::warn!(%error, "frame could not be built");
                            None
                        })
                    }
                    None => None,
                };
                let _ = reply.send(frame.unwrap_or_default());
            }
            Command::ViewportText { id, reply } => {
                let text = match tabs.get_mut(&id) {
                    Some(tab) => tab.terminal.viewport_text().unwrap_or_else(|error| {
                        tracing::warn!(%error, "viewport could not be read");
                        Vec::new()
                    }),
                    None => Vec::new(),
                };
                let _ = reply.send(text);
            }
            Command::SetTheme(next) => {
                theme = *next;
                for tab in tabs.values_mut() {
                    if let Err(error) = tab.terminal.set_theme(theme.clone()) {
                        tracing::warn!(%error, "theme could not be applied to a tab");
                    }
                    tab.frame_moved();
                }
            }
            Command::Close(id) => {
                tabs.remove(&id);
            }
            Command::Shutdown => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use roost_client::attach::PayloadKind;
    use roost_ipc::messages::{AttachMode, SizeOwner};
    use roost_vt::{vt_snapshot, RenderState, Terminal, TerminalOptions};

    use super::*;
    use crate::frame::{decode, row_text};

    fn owner() -> TerminalOwner {
        TerminalOwner::spawn(OwnerConfig {
            client_id: "phone".into(),
            theme: Theme::roost_dark_fallback(),
        })
        .expect("owner thread")
    }

    type Outputs = Arc<Mutex<Vec<TabOutput>>>;

    fn recording(owner: &TerminalOwner) -> (TerminalId, Outputs) {
        let outputs: Outputs = Arc::default();
        let sink = Arc::clone(&outputs);
        let id = owner.open(Box::new(move |output| {
            sink.lock().expect("outputs").push(output);
        }));
        (id, outputs)
    }

    fn drain(outputs: &Outputs) -> Vec<TabOutput> {
        std::mem::take(&mut *outputs.lock().expect("outputs"))
    }

    fn frame_ready_count(outputs: &[TabOutput]) -> usize {
        outputs
            .iter()
            .filter(|output| **output == TabOutput::Event(TerminalEvent::FrameReady))
            .count()
    }

    /// A `vt` attach of "hello" at 20x5, owned by `owner_id`.
    fn attach_hello(owner: &TerminalOwner, id: TerminalId, owner_id: &str) {
        let mut src = Terminal::new(TerminalOptions {
            cols: 20,
            rows: 5,
            continuation_max_bytes: 4096,
            ..Default::default()
        })
        .expect("source");
        src.vt_write(b"hello");
        let payload = vt_snapshot(&src, &mut RenderState::new().expect("render"))
            .expect("compose")
            .expect("ground");
        owner.apply(
            id,
            AttachEvent::Accepted {
                kind: PayloadKind::Vt,
                mode: AttachMode::Snapshot,
                cols: 20,
                rows: 5,
                cell_w_px: 8,
                cell_h_px: 16,
                owner: Some(SizeOwner {
                    client_id: owner_id.into(),
                    label: "Pixel".into(),
                }),
            },
        );
        owner.apply(id, AttachEvent::Snap(payload));
        owner.apply(id, AttachEvent::Snap(Vec::new()));
    }

    #[test]
    fn events_flow_and_frame_ready_is_coalesced_until_a_pull() {
        let owner = owner();
        let (id, outputs) = recording(&owner);
        attach_hello(&owner, id, "phone");
        for _ in 0..3 {
            owner.apply(id, AttachEvent::Pty(b"!".to_vec()));
        }
        assert_eq!(owner.viewport_text(id)[0], "hello!!!");

        let seen = drain(&outputs);
        assert_eq!(frame_ready_count(&seen), 1, "{seen:?}");
        assert!(seen.contains(&TabOutput::Event(TerminalEvent::Owner {
            label: Some("Pixel".into()),
            is_me: true,
            cols: 20,
            rows: 5,
        })));
        assert!(seen.contains(&TabOutput::Event(TerminalEvent::Status {
            state: AttachState::Live,
            message: None,
        })));

        let frame = decode(&owner.take_frame(id, false)).expect("a frame");
        assert_eq!(row_text(&frame.rows[0], 20), "hello!!!");
        assert!(owner.take_frame(id, false).is_empty(), "nothing new");

        owner.apply(id, AttachEvent::Pty(b"?".to_vec()));
        owner.apply(id, AttachEvent::Pty(b"?".to_vec()));
        owner.viewport_text(id);
        assert_eq!(
            frame_ready_count(&drain(&outputs)),
            1,
            "announced again after the pull"
        );
    }

    #[test]
    fn another_clients_size_is_not_mine() {
        let owner = owner();
        let (id, outputs) = recording(&owner);
        attach_hello(&owner, id, "laptop");
        owner.viewport_text(id);
        assert!(
            drain(&outputs).contains(&TabOutput::Event(TerminalEvent::Owner {
                label: Some("Pixel".into()),
                is_me: false,
                cols: 20,
                rows: 5,
            }))
        );
    }

    #[test]
    fn keys_and_pastes_come_back_as_input_to_send() {
        let owner = owner();
        let (id, outputs) = recording(&owner);
        attach_hello(&owner, id, "phone");
        owner.key(
            id,
            KeyInput {
                key: "Enter".into(),
                ..Default::default()
            },
        );
        owner.text(id, "ls".into());
        owner.paste(id, "a\nb".into());
        owner.scroll(id, 1, 0, 0);
        owner.viewport_text(id);
        let inputs: Vec<TabOutput> = drain(&outputs)
            .into_iter()
            .filter(|output| matches!(output, TabOutput::Input { .. }))
            .collect();
        assert_eq!(
            inputs,
            vec![
                TabOutput::Input {
                    bytes: b"\r".to_vec(),
                    take: InputTake::Before
                },
                TabOutput::Input {
                    bytes: b"ls".to_vec(),
                    take: InputTake::Before
                },
                TabOutput::Input {
                    bytes: b"a\rb".to_vec(),
                    take: InputTake::Before
                },
            ],
            "a local scroll sends nothing"
        );
    }

    #[test]
    fn a_reattach_reaches_the_stream_holder() {
        let owner = owner();
        let (id, outputs) = recording(&owner);
        owner.apply(id, AttachEvent::Pty(b"stray".to_vec()));
        owner.viewport_text(id);
        assert!(drain(&outputs)
            .iter()
            .any(|output| matches!(output, TabOutput::Reattach { .. })));
    }

    #[test]
    fn tabs_are_counted_until_closed_and_the_owner_drops_the_rest() {
        let owner = owner();
        let (first, _) = recording(&owner);
        let (second, _) = recording(&owner);
        attach_hello(&owner, first, "phone");
        owner.viewport_text(first);
        assert_eq!(owner.live_tabs(), 2);

        owner.close(first);
        owner.close(first);
        assert!(owner.take_frame(first, true).is_empty(), "a closed tab");
        assert_eq!(owner.live_tabs(), 1);

        let live = Arc::clone(&owner.live);
        drop(owner);
        assert_eq!(
            live.load(Ordering::SeqCst),
            0,
            "{second:?} dropped with its owner"
        );
    }
}
