//! gRPC client over the daemon's Unix domain socket.
//!
//! Wraps `roost_proto::v1::RoostClient` with the subset of RPCs the
//! Linux UI needs. Mirrors `mac/Sources/Roost/RoostClient.swift` on
//! the Mac side — same method shape, same auto-create-default-project
//! flow on bootstrap, same StreamPty bidi pattern.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use tonic::transport::Channel;

use roost_common::connect_uds;
use roost_proto::v1::roost_client::RoostClient as ProtoRoostClient;
use roost_proto::v1::{
    CreateProjectRequest, IdentifyRequest, IdentifyResponse, ListTabsRequest, OpenTabRequest,
    Project, Tab,
};

#[derive(Clone)]
pub struct RoostClient {
    inner: ProtoRoostClient<Channel>,
}

impl RoostClient {
    /// Connect over UDS at `socket`. Errors surface as `anyhow::Error`
    /// so the GTK side can dispatch them through the same path the
    /// Identify spike already uses.
    pub async fn connect(socket: PathBuf) -> Result<Self> {
        let channel = connect_uds(socket)
            .await
            .context("connect_uds (is the daemon running?)")?;
        Ok(Self {
            inner: ProtoRoostClient::new(channel),
        })
    }

    pub async fn identify(&mut self) -> Result<IdentifyResponse> {
        let resp = self
            .inner
            .identify(IdentifyRequest {
                client_name: "roost-linux".into(),
                client_version: env!("CARGO_PKG_VERSION").into(),
            })
            .await
            .context("Identify RPC failed")?
            .into_inner();
        Ok(resp)
    }

    /// List projects with their tabs. Used at bootstrap to pick or
    /// create the first project the UI hosts.
    pub async fn list_projects(&mut self) -> Result<Vec<Project>> {
        let resp = self
            .inner
            .list_tabs(ListTabsRequest {})
            .await
            .context("ListTabs RPC failed")?
            .into_inner();
        Ok(resp.projects)
    }

    /// Create a project, returning the new project record.
    pub async fn create_project(&mut self, name: &str, cwd: &str) -> Result<Project> {
        let resp = self
            .inner
            .create_project(CreateProjectRequest {
                name: name.into(),
                cwd: cwd.into(),
            })
            .await
            .context("CreateProject RPC failed")?
            .into_inner();
        resp.project
            .ok_or_else(|| anyhow!("CreateProject returned no project"))
    }

    /// Open a tab in the given project. `cwd` empty = the daemon
    /// picks `$HOME`. `cols/rows` are advisory — the real cell grid
    /// gets reflowed via `PtyResize` over StreamPty once the renderer
    /// is sized in commit 7.
    pub async fn open_tab(
        &mut self,
        project_id: i64,
        cwd: &str,
        cols: u32,
        rows: u32,
    ) -> Result<Tab> {
        let resp = self
            .inner
            .open_tab(OpenTabRequest {
                project_id,
                cwd: cwd.into(),
                argv: vec![],
                cols,
                rows,
                title: "roost-linux".into(),
            })
            .await
            .context("OpenTab RPC failed")?
            .into_inner();
        resp.tab.ok_or_else(|| anyhow!("OpenTab returned no tab"))
    }

    /// The underlying tonic client — exposed for the StreamPty
    /// session, which needs to construct a bidi stream directly.
    /// Stays `pub(crate)` so callers route through the typed methods
    /// above whenever a wrapper exists.
    pub(crate) fn inner(&mut self) -> &mut ProtoRoostClient<Channel> {
        &mut self.inner
    }
}
