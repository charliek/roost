//! The macOS native seam — the one place in this crate that talks to
//! AppKit.
//!
//! Roadmap M6 § 6b picked `objc2` over a Swift static-lib shim on spike
//! evidence (plan 027 C6): zero new build machinery, one ObjC ecosystem
//! covering the known consumers, and the generation it rides
//! (`objc2 0.6` / `objc2-app-kit 0.3` / `objc2-foundation 0.3`) is
//! already compiled into every macOS build via `arboard` + `softbuffer`.
//! `Cargo.toml` carries the version-coupling policy that follows from
//! that.
//!
//! Two rules hold for everything under here:
//!
//! * **Main thread only.** AppKit is main-thread-only (CLAUDE.md's
//!   threading table), so every entry point either takes a
//!   [`objc2::MainThreadMarker`] or acquires one and refuses to proceed
//!   without it. The iced update loop *is* the main thread, which is why
//!   every caller lives there.
//! * **Nothing retained escapes.** Callers hand in plain data and get
//!   plain data back; no `Retained<_>` crosses out of this module.
//!
//! First consumer: [`dock_badge`], the parity port of `App.swift`'s
//! `refreshDockBadge()`. Second: [`menu`], the native menu bar. Third:
//! [`sparkle`], the runtime-loaded updater.

pub(crate) mod dock_badge;
pub(crate) mod menu;
pub(crate) mod sparkle;
