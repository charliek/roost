#![cfg(feature = "server-vt")]
//! The process layer (#621) over real PTYs: a session's tab task reads
//! the shell's prompt marks through libghostty, the probe reads who has
//! the terminal, and `LocalClient::watch_processes`'s sampler reads the
//! stamped job — the whole loop a `roost-session` runs, minus the socket.
//!
//! The shells are `bash --norc --noprofile -i` with a `PROMPT_COMMAND`
//! printing the `D` mark, as Roost's own integration does, so nothing
//! from a user's startup files runs. After the `D` it sets the title to
//! the prompt's number: a title is applied after the marks of the same
//! read, so a test waiting on it knows that prompt's `D` has landed. The
//! agent is a plain job and the hook claim comes from the test.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use roost_engine::tab_task::ServerVtConfig;
use roost_engine::{LocalClient, PtySupervisor, TabOpener, Workspace};
use roost_ipc::agent::{AgentLifecycle, OwnershipAction, ShellState, TabAgentReportParams};
use roost_ipc::messages::Tab;
use roost_ipc::process::{ProcessState, TabProcess};

const BUDGET: Duration = Duration::from_secs(20);

struct Rig {
    workspace: Arc<Workspace>,
    supervisor: Arc<PtySupervisor>,
    client: LocalClient,
    project: i64,
    _tabs: HangUp,
}

/// Hangs up every tab a test opened, whatever the test did.
struct HangUp(Arc<PtySupervisor>, std::sync::Mutex<Vec<i64>>);

impl Drop for HangUp {
    fn drop(&mut self) {
        for tab in self.1.lock().unwrap().iter() {
            self.0.close(*tab);
        }
    }
}

impl Rig {
    fn new() -> Option<Self> {
        if which("bash").is_none() {
            eprintln!("skipping the process-layer test: bash not found on PATH");
            return None;
        }
        let workspace = Arc::new(Workspace::new());
        workspace.set_window_focused(false);
        let project = workspace.create_project("p", "/tmp").unwrap().id;
        let supervisor = Arc::new(PtySupervisor::new());
        supervisor
            .enable_server_vt(ServerVtConfig::new(workspace.clone()))
            .expect("server-vt enables once");
        let client = LocalClient::new(
            Arc::clone(&workspace),
            Arc::clone(&supervisor),
            PathBuf::from("/tmp/roost-process-layer-test.sock"),
        );
        client.watch_processes();
        Some(Self {
            _tabs: HangUp(supervisor.clone(), std::sync::Mutex::new(Vec::new())),
            workspace,
            supervisor,
            client,
            project,
        })
    }

    /// A bash tab at a prompt whose every return prints `OSC 133 ; D`,
    /// and its shell's pid.
    async fn bash_tab(&self) -> (i64, u32) {
        let argv = ["bash", "--norc", "--noprofile", "-i"].map(String::from);
        let tab = self
            .client
            .open_tab(self.project, "/tmp", "t", &argv, 80, 24, &TabOpener::None)
            .await
            .expect("open a bash tab")
            .id;
        self._tabs.1.lock().unwrap().push(tab);
        let mut output = self
            .supervisor
            .take_initial_receiver(tab)
            .expect("the tab's first receiver");
        wait_for_text(&mut output, "$ ").await;
        // The pid is printed through `%s`, so only the output has digits
        // after the marker; the echoed command line has `%s`.
        self.type_line(
            tab,
            r#"PROMPT_COMMAND='n=$((n+1)); printf "\033]133;D\007\033]0;prompt %s\007" $n'; printf 'SHELL_PID_%s_\n' "$$""#,
        )
        .await;
        let seen = wait_for_text(&mut output, "_\r\n").await;
        let shell = seen
            .split("SHELL_PID_")
            .filter_map(|rest| rest.split('_').next()?.parse().ok())
            .next()
            .expect("the shell's pid");
        self.prompted(tab, 1).await;
        (tab, shell)
    }

    /// Wait until the tab's shell has drawn its `n`th prompt, its `D`
    /// mark included.
    async fn prompted(&self, tab: i64, n: u32) {
        let title = format!("prompt {n}");
        let listed = self.wait(tab, &title, |tab| tab.title == title).await;
        assert_eq!(listed.shell_state, ShellState::AtPrompt);
    }

    async fn type_line(&self, tab: i64, line: &str) {
        self.type_bytes(tab, format!("{line}\n").as_bytes()).await;
    }

    async fn type_bytes(&self, tab: i64, bytes: &[u8]) {
        self.supervisor
            .write(tab, bytes.to_vec())
            .await
            .expect("type into the tab");
    }

    fn claim(&self, tab: i64, lifecycle: AgentLifecycle) {
        let report = TabAgentReportParams {
            session_id: format!("s-{tab}"),
            ..TabAgentReportParams::sessionless(
                tab,
                "claude",
                OwnershipAction::Claim,
                Some(lifecycle),
            )
        };
        let (accepted, _) = self.workspace.agent_report(&report).expect("report");
        assert!(accepted, "the claim is accepted");
    }

    async fn wait(&self, tab: i64, what: &str, done: impl Fn(&Tab) -> bool) -> Tab {
        let deadline = Instant::now() + BUDGET;
        loop {
            let listed = self.workspace.tab(tab).expect("the tab is listed");
            if done(&listed) {
                return listed;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {listed:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A `sleep` job in front of `shell`, in `tab`, claimed by an agent
    /// and stamped: its process, as `Tab.process` names it.
    async fn stamped_job(&self, (tab, shell): (i64, u32)) -> TabProcess {
        self.type_line(tab, "sleep 300").await;
        wait_until("the job leads the terminal", || pid_of_job(shell).is_some()).await;
        self.claim(tab, AgentLifecycle::Working);
        let listed = self
            .wait(tab, "the job's stamp", |tab| tab.process.is_some())
            .await;
        let process = listed.process.unwrap();
        assert_eq!(
            Some(process.pid),
            pid_of_job(shell),
            "the stamp is the job in front"
        );
        assert_eq!(process.state, ProcessState::Running);
        assert_eq!(process.agent, "claude");
        process
    }
}

/// I1: the stamped job is killed; the shell's `D` parks, and the sample it
/// asks for finds the process gone with the shell in front.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_agent_releases_its_owner_at_the_prompt() {
    let Some(rig) = Rig::new() else { return };
    let (tab, shell) = rig.bash_tab().await;
    let process = rig.stamped_job((tab, shell)).await;

    signal(process.pid, libc::SIGKILL);
    let listed = rig
        .wait(tab, "the owner's release", |tab| tab.ownership.is_none())
        .await;
    assert!(!listed.hook_active);
    assert_eq!(listed.process, None);
    assert_eq!(listed.agent_lifecycle, AgentLifecycle::Inactive);
}

/// I2: Ctrl-Z keeps the owner and parks what it was doing; the agent shows
/// suspended, and once it runs again in front, its lifecycle is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_suspended_agent_keeps_its_owner_and_resumes_on_fg() {
    let Some(rig) = Rig::new() else { return };
    let (tab, shell) = rig.bash_tab().await;
    let process = rig.stamped_job((tab, shell)).await;

    rig.type_bytes(tab, b"\x1a").await;
    let listed = rig
        .wait(tab, "the suspended agent", |tab| {
            tab.process.as_ref().map(|process| process.state) == Some(ProcessState::Suspended)
        })
        .await;
    assert_eq!(
        listed.ownership.as_ref().map(|owner| owner.source.as_str()),
        Some("claude"),
        "a stopped agent keeps its owner"
    );
    assert_eq!(listed.agent_lifecycle, AgentLifecycle::Inactive, "parked");

    rig.type_line(tab, "fg").await;
    wait_until("the job runs again", || !is_stopped(process.pid)).await;
    // A focus asks for a sample; bash < 4.4 prints no `C` for the `fg`.
    rig.workspace.focus_tab(tab).unwrap();
    let listed = rig
        .wait(tab, "the restored lifecycle", |tab| {
            tab.agent_lifecycle == AgentLifecycle::Working
        })
        .await;
    assert_eq!(
        listed.process.map(|process| process.state),
        Some(ProcessState::Running)
    );

    signal(process.pid, libc::SIGKILL);
    rig.wait(tab, "the owner's release", |tab| tab.ownership.is_none())
        .await;
}

/// I3: a claim made while the shell itself is in front — an agent in the
/// background — stamps nothing, so the shell's next prompt releases it.
/// A second tab's stamp is the fence: the sampler applies every request
/// it took in tab order, so once the later tab is stamped, the earlier
/// one's claim has been sampled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_with_the_shell_in_front_stamps_nothing() {
    let Some(rig) = Rig::new() else { return };
    let (background, _) = rig.bash_tab().await;
    let (fence, fence_shell) = rig.bash_tab().await;
    assert!(
        background < fence,
        "the precondition: tab order is id order"
    );

    rig.type_line(background, "sleep 300 &").await;
    rig.prompted(background, 2).await;
    rig.type_line(fence, "sleep 300").await;
    wait_until("the fence's job leads its terminal", || {
        pid_of_job(fence_shell).is_some()
    })
    .await;
    rig.claim(background, AgentLifecycle::Working);
    rig.claim(fence, AgentLifecycle::Working);
    rig.wait(fence, "the fence's stamp", |tab| tab.process.is_some())
        .await;

    let listed = rig.workspace.tab(background).unwrap();
    assert_eq!(
        listed.process, None,
        "the shell's own group is never stamped"
    );
    assert!(listed.ownership.is_some(), "the precondition: still owned");

    rig.type_line(background, "true").await;
    rig.wait(background, "the release at the shell's prompt", |tab| {
        tab.ownership.is_none()
    })
    .await;
}

/// Read the tab's output until `needle` shows, and answer what was read.
async fn wait_for_text(
    output: &mut tokio::sync::broadcast::Receiver<roost_engine::PtyOutputEvent>,
    needle: &str,
) -> String {
    let mut seen = Vec::new();
    let found = tokio::time::timeout(BUDGET, async {
        loop {
            match output.recv().await {
                Ok(roost_engine::PtyOutputEvent::Bytes { data, .. }) => {
                    seen.extend_from_slice(&data);
                    if String::from_utf8_lossy(&seen).contains(needle) {
                        return;
                    }
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(error) => panic!("the tab's output ended: {error}"),
            }
        }
    })
    .await;
    let seen = String::from_utf8_lossy(&seen).into_owned();
    assert!(found.is_ok(), "no {needle:?} in: {seen:?}");
    seen
}

async fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + BUDGET;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The leader of the job in front of `shell`'s terminal, when it is not
/// the shell: the shell's own `tpgid`, by `ps` (macOS's `sess` is a
/// pointer, not an id, so the session is not compared).
fn pid_of_job(shell: u32) -> Option<i64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "tpgid=", "-p", &shell.to_string()])
        .output()
        .ok()?;
    let tpgid: i64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (tpgid > 0 && tpgid != i64::from(shell)).then_some(tpgid)
}

fn is_stopped(pid: i64) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().starts_with('T'))
        .unwrap_or(false)
}

fn signal(pid: i64, signal: i32) {
    // SAFETY: a signal to a job this test started in its own tab.
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, signal) }, 0);
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}
