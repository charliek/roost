//! `RoostApp` over the bridge.

use flutter_rust_bridge::frb;

use super::terminal::BridgeTerminal;
use super::types::{BridgeGeometry, BridgeHostView, BridgeLeakCounts, BridgeWorkspaceView};
use crate::app::RoostApp;
use crate::frb_generated::StreamSink;

/// Load the saved hosts from `data_dir` and start the app's runtime and
/// terminal thread. One per process.
pub fn roost_init(data_dir: String, device_label: String) -> anyhow::Result<BridgeApp> {
    Ok(BridgeApp {
        app: Some(RoostApp::new(data_dir, device_label)?),
    })
}

#[frb(opaque)]
pub struct BridgeApp {
    /// Taken in `Drop`.
    app: Option<RoostApp>,
}

impl BridgeApp {
    fn app(&self) -> &RoostApp {
        self.app.as_ref().expect("the app lives until drop")
    }

    #[frb(sync)]
    pub fn client_id(&self) -> String {
        self.app().client_id()
    }

    pub fn hosts(&self, sink: StreamSink<Vec<BridgeHostView>>) {
        self.app().hosts(Box::new(move |hosts| {
            let _ = sink.add(hosts.into_iter().map(Into::into).collect());
        }));
    }

    pub fn add_host(&self, label: String, target: String) -> anyhow::Result<String> {
        Ok(self.app().add_host(label, target)?)
    }

    pub fn remove_host(&self, host_id: String) -> anyhow::Result<()> {
        Ok(self.app().remove_host(host_id)?)
    }

    pub fn connect(&self, host_id: String) -> anyhow::Result<()> {
        Ok(self.app().connect(host_id)?)
    }

    pub fn disconnect(&self, host_id: String) -> anyhow::Result<()> {
        Ok(self.app().disconnect(host_id)?)
    }

    pub fn trust_host_key(&self, host_id: String, fingerprint: String) -> anyhow::Result<()> {
        Ok(self.app().trust_host_key(host_id, fingerprint)?)
    }

    pub fn workspace(
        &self,
        host_id: String,
        sink: StreamSink<BridgeWorkspaceView>,
    ) -> anyhow::Result<()> {
        Ok(self.app().workspace(
            host_id,
            Box::new(move |workspace| {
                let _ = sink.add(workspace.into());
            }),
        )?)
    }

    pub async fn op(
        &self,
        host_id: String,
        op: String,
        params_json: String,
    ) -> anyhow::Result<String> {
        Ok(self.app().op(host_id, op, params_json).await?)
    }

    pub async fn send_text(
        &self,
        host_id: String,
        tab_id: i64,
        text: String,
        paste: bool,
    ) -> anyhow::Result<()> {
        Ok(self.app().send_text(host_id, tab_id, text, paste).await?)
    }

    pub async fn clear_notification(&self, host_id: String, tab_id: i64) -> anyhow::Result<()> {
        Ok(self.app().clear_notification(host_id, tab_id).await?)
    }

    pub fn attach(
        &self,
        host_id: String,
        tab_id: i64,
        geometry: BridgeGeometry,
        take: bool,
    ) -> anyhow::Result<BridgeTerminal> {
        let handle = self.app().attach(host_id, tab_id, geometry.into(), take)?;
        Ok(BridgeTerminal::new(handle))
    }

    /// Waits up to the pause budget for the size releases: the pool.
    pub fn app_paused(&self) {
        self.app().app_paused();
    }

    pub fn app_resumed(&self) {
        self.app().app_resumed();
    }

    #[frb(sync)]
    pub fn leak_counts(&self) -> BridgeLeakCounts {
        self.app().leak_counts().into()
    }
}

/// Dart drops the last reference from its own thread; `RoostApp`'s
/// teardown waits for its tasks, so it runs elsewhere.
impl Drop for BridgeApp {
    fn drop(&mut self) {
        if let Some(app) = self.app.take() {
            std::thread::spawn(move || drop(app));
        }
    }
}
