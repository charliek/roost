//! `TerminalHandle` over the bridge.

use flutter_rust_bridge::frb;

use super::types::{BridgeGeometry, BridgeKeyInput, BridgeTerminalEvent};
use crate::frb_generated::StreamSink;
use crate::TerminalHandle;

#[frb(opaque)]
pub struct BridgeTerminal {
    handle: TerminalHandle,
}

impl BridgeTerminal {
    pub(crate) fn new(handle: TerminalHandle) -> Self {
        Self { handle }
    }

    pub fn events(&self, sink: StreamSink<BridgeTerminalEvent>) {
        self.handle.events(Box::new(move |event| {
            let _ = sink.add(event.into());
        }));
    }

    /// A round trip to the owner thread, which may be busy applying output:
    /// the pool, not the UI thread.
    pub fn take_frame(&self) -> Vec<u8> {
        self.handle.take_frame()
    }

    pub fn take_full_frame(&self) -> Vec<u8> {
        self.handle.take_full_frame()
    }

    #[frb(sync)]
    pub fn key(&self, key: BridgeKeyInput) {
        self.handle.key(key.into());
    }

    #[frb(sync)]
    pub fn text(&self, text: String) {
        self.handle.text(text);
    }

    #[frb(sync)]
    pub fn paste(&self, text: String) {
        self.handle.paste(text);
    }

    /// `rows` positive toward older history, aimed at cell (`col`, `row`).
    #[frb(sync)]
    pub fn scroll(&self, rows: i32, col: u16, row: u16) {
        self.handle.scroll(rows, col, row);
    }

    #[frb(sync)]
    pub fn resize(&self, geometry: BridgeGeometry, focused: bool) {
        self.handle.resize(geometry.into(), focused);
    }

    #[frb(sync)]
    pub fn take_size(&self) {
        self.handle.take_size();
    }

    #[frb(sync)]
    pub fn close(&self) {
        self.handle.close();
    }

    pub fn viewport_text(&self) -> Vec<String> {
        self.handle.viewport_text()
    }
}
