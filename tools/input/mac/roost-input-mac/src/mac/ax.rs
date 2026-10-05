//! The Accessibility side: the target's window, menu bar and open popup, and
//! pressing what they hold. While a native popup is open the target's main
//! thread is inside AppKit's menu tracking loop, so AX is the only channel that
//! still answers (§2.12: in milliseconds); the timeouts below keep a target
//! that stops answering from hanging the helper.

use super::cf::{self, Cf};
use super::ffi::*;
use super::point_json;
use crate::args::{Point, Rect};
use crate::claims::{self, Claim, WindowEntry};
use crate::{Failure, Outcome};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::time::{Duration, Instant};

/// The AX messaging timeout for an ordinary read or press.
const TIMEOUT: f32 = 2.0;
/// The short one used while a popup is open.
const POPUP_TIMEOUT: f32 = 0.5;
/// On each app's application element, when asking every on-screen app whether
/// it holds a key window. It does not cover the read from that app's window
/// element (test-runner/README.md, "Known limits").
const FOREIGN_TIMEOUT: f32 = 0.25;
/// Where to hit-test for an open popup, relative to the click that opened it.
/// A menu opens below and to the right of the click unless the screen edge
/// flips it, and its first item starts a few points in.
const POPUP_PROBES: [(f64, f64); 7] = [
    (30.0, 12.0),
    (20.0, 8.0),
    (60.0, 24.0),
    (30.0, -12.0),
    (-30.0, 12.0),
    (-30.0, -12.0),
    (12.0, 40.0),
];

pub struct Element(Cf);

fn failure(what: &str, code: AXError) -> Failure {
    if code == kAXErrorAPIDisabled {
        Failure::Unavailable(format!(
            "{what}: Accessibility is not granted (AXError {code})"
        ))
    } else {
        Failure::Failed(format!("{what}: AXError {code}"))
    }
}

pub fn require_trusted() -> Result<(), Failure> {
    // SAFETY: a read-only check (never the prompting variant).
    if unsafe { AXIsProcessTrusted() } != 0 {
        Ok(())
    } else {
        Err(Failure::Unavailable(
            "Accessibility is not granted to this process (its responsible app)".into(),
        ))
    }
}

/// The messaging timeout for every element this process talks to.
fn set_global_timeout(seconds: f32) {
    // SAFETY: the system-wide element is created and released here.
    unsafe {
        if let Some(system) = Cf::owned(AXUIElementCreateSystemWide()) {
            AXUIElementSetMessagingTimeout(system.ptr(), seconds);
        }
    }
}

impl Element {
    pub fn application(pid: i32) -> Result<Element, Failure> {
        // SAFETY: a Create result adopted once.
        unsafe { Cf::owned(AXUIElementCreateApplication(pid)) }
            .map(Element)
            .ok_or_else(|| Failure::Failed(format!("AXUIElementCreateApplication({pid}) failed")))
    }

    fn copy(&self, attribute: &str) -> Result<Option<Cf>, AXError> {
        let name = cf::string(attribute);
        let mut value: CFTypeRef = std::ptr::null();
        // SAFETY: `value` receives a +1 reference on success, adopted below.
        let code = unsafe { AXUIElementCopyAttributeValue(self.0.ptr(), name.ptr(), &mut value) };
        if code == kAXErrorSuccess {
            // SAFETY: a Copy result.
            Ok(unsafe { Cf::owned(value) })
        } else {
            Err(code)
        }
    }

    fn attr(&self, attribute: &str) -> Option<Cf> {
        self.copy(attribute).ok().flatten()
    }

    pub fn element(&self, attribute: &str) -> Option<Element> {
        let value = self.attr(attribute)?;
        // SAFETY: a plain type query.
        (value.type_id() == unsafe { AXUIElementGetTypeID() }).then_some(Element(value))
    }

    pub fn string(&self, attribute: &str) -> Option<String> {
        // SAFETY: `to_string` type-checks the live value.
        self.attr(attribute)
            .and_then(|value| unsafe { cf::to_string(value.ptr()) })
    }

    pub fn boolean(&self, attribute: &str) -> Option<bool> {
        // SAFETY: `to_bool` type-checks the live value.
        self.attr(attribute)
            .and_then(|value| unsafe { cf::to_bool(value.ptr()) })
    }

    pub fn integer(&self, attribute: &str) -> Option<i64> {
        // SAFETY: `to_i64` type-checks the live value.
        self.attr(attribute)
            .and_then(|value| unsafe { cf::to_i64(value.ptr()) })
    }

    fn ax_value<T: Default>(&self, attribute: &str, value_type: u32) -> Option<T> {
        let value = self.attr(attribute)?;
        // SAFETY: the AXValue type is checked before it is read into `T`,
        // which the caller pairs with `value_type`.
        unsafe {
            if value.type_id() != AXValueGetTypeID() {
                return None;
            }
            let mut out = T::default();
            (AXValueGetValue(
                value.ptr(),
                value_type,
                (&mut out as *mut T).cast::<c_void>(),
            ) != 0)
                .then_some(out)
        }
    }

    pub fn frame(&self) -> Option<CGRect> {
        Some(CGRect {
            origin: self.ax_value::<CGPoint>("AXPosition", kAXValueCGPointType)?,
            size: self.ax_value::<CGSize>("AXSize", kAXValueCGSizeType)?,
        })
    }

    pub fn children(&self) -> Vec<Element> {
        let Some(array) = self.attr("AXChildren") else {
            return Vec::new();
        };
        // SAFETY: `array_items` type-checks the live value.
        unsafe { cf::array_items(array.ptr()) }
            .into_iter()
            // SAFETY: a plain type query.
            .filter(|item| item.type_id() == unsafe { AXUIElementGetTypeID() })
            .map(Element)
            .collect()
    }

    pub fn role(&self) -> Option<String> {
        self.string("AXRole")
    }

    pub fn title(&self) -> Option<String> {
        self.string("AXTitle")
    }

    pub fn pid(&self) -> Option<i32> {
        let mut pid = 0;
        // SAFETY: `pid` is written on success.
        (unsafe { AXUIElementGetPid(self.0.ptr(), &mut pid) } == kAXErrorSuccess).then_some(pid)
    }

    pub fn press(&self, what: &str) -> Result<(), Failure> {
        let action = cf::string("AXPress");
        // SAFETY: a plain action call on a live element.
        let code = unsafe { AXUIElementPerformAction(self.0.ptr(), action.ptr()) };
        if code == kAXErrorSuccess {
            Ok(())
        } else {
            Err(failure(&format!("AXPress on {what}"), code))
        }
    }

    fn set_value<T>(&self, attribute: &str, value_type: u32, value: &T) -> Result<(), Failure> {
        // SAFETY: `value_type` matches `T` at every call site; the AXValue is
        // released after the set.
        unsafe {
            let boxed = Cf::owned(AXValueCreate(
                value_type,
                (value as *const T).cast::<c_void>(),
            ))
            .ok_or_else(|| Failure::Failed(format!("AXValueCreate for {attribute} failed")))?;
            let name = cf::string(attribute);
            let code = AXUIElementSetAttributeValue(self.0.ptr(), name.ptr(), boxed.ptr());
            if code == kAXErrorSuccess {
                Ok(())
            } else {
                Err(failure(&format!("setting {attribute}"), code))
            }
        }
    }

    fn describe(&self) -> Value {
        json!({"role": self.role(), "title": self.title()})
    }
}

impl From<CGRect> for Rect {
    fn from(rect: CGRect) -> Rect {
        Rect {
            x: rect.origin.x,
            y: rect.origin.y,
            width: rect.size.width,
            height: rect.size.height,
        }
    }
}

pub fn rect_json(rect: impl Into<Rect>) -> Value {
    let rect = rect.into();
    json!({"x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height})
}

fn system_wide() -> Result<Cf, Failure> {
    // SAFETY: a Create result adopted once.
    unsafe { Cf::owned(AXUIElementCreateSystemWide()) }
        .ok_or_else(|| Failure::Failed("AXUIElementCreateSystemWide failed".into()))
}

/// The element the window server's hit test puts under `at`.
fn element_at(system: &Cf, at: Point) -> Result<Option<Element>, AXError> {
    let mut hit: AXUIElementRef = std::ptr::null();
    // SAFETY: `hit` receives a +1 reference on success, adopted below.
    let code = unsafe {
        AXUIElementCopyElementAtPosition(system.ptr(), at.x as f32, at.y as f32, &mut hit)
    };
    if code != kAXErrorSuccess {
        return Err(code);
    }
    // SAFETY: a Copy result.
    Ok(unsafe { Cf::owned(hit) }.map(Element))
}

/// The pid of the app a click at `at` would reach, and nothing slower: this
/// is the check that runs immediately before a mouse event is posted. It is
/// the window server's own hit test: a window list also names windows that
/// clicks pass through (the Dock keeps a full-screen one above every app), so
/// the first window there is not always the click's target.
pub fn click_target(at: Point) -> Result<i32, Failure> {
    require_trusted()?;
    set_global_timeout(TIMEOUT);
    let unknown = |why: String| {
        Failure::Refused(format!(
            "cannot tell which app a click at ({}, {}) reaches: {why}",
            at.x, at.y
        ))
    };
    element_at(&system_wide()?, at)
        .map_err(|code| unknown(format!("AXError {code}")))?
        .ok_or_else(|| unknown("nothing is there".into()))?
        .pid()
        .ok_or_else(|| unknown("the element has no pid".into()))
}

/// What a click at `at` would hit, for a refusal's message: read only after a
/// check has already refused, never between a check and a post.
pub fn describe_at(at: Point) -> Value {
    let Ok(system) = system_wide() else {
        return Value::Null;
    };
    match element_at(&system, at) {
        Ok(Some(element)) => json!({
            "pid": element.pid(),
            "role": element.role(),
            "title": element.title(),
        }),
        Ok(None) => Value::Null,
        Err(code) => json!({"ax_error": code}),
    }
}

/// The apps other than `pid` (and this process) that claim the keyboard with
/// a window in front of `pid`'s own (`claims`). Every app keeps its own idea
/// of key: while a foreign non-activating panel holds the keyboard, Roost's
/// window still reports itself key, the front process is still Roost, and the
/// system-wide focus queries answer `kAXErrorCannotComplete` — so the one
/// signal left is the other app's own claim. An app that does not answer is
/// taken as not claiming (test-runner/README.md, "Known limits").
pub fn foreign_key_windows(pid: i32) -> Result<Vec<i32>, Failure> {
    require_trusted()?;
    let windows = window_list()?;
    let reference = claims::reference_index(&windows, pid)
        .ok_or_else(|| Failure::Refused(format!("pid {pid} has no window on screen")))?;
    Ok(
        claims::foreign_owners(&windows, pid, std::process::id() as i32)
            .into_iter()
            .filter(|owner| {
                key_claim(*owner).is_some_and(|claimed| {
                    claims::judge(&windows, reference, *owner, claimed).blocks()
                })
            })
            .collect(),
    )
}

/// What `foreign_key_windows` would decide now, with its working, read only.
/// An owner that does not claim the keyboard is placed by its frontmost
/// window, so a report shows where it would sit if it did. `ahead` is
/// `position == "ahead"` only; `blocks` is whether a key would be refused
/// over this owner (an unlocated claim blocks without being ahead). Both are
/// null when `pid` has no window to judge against.
pub fn claimants(pid: i32) -> Outcome {
    require_trusted()?;
    let windows = window_list()?;
    let reference = claims::reference_index(&windows, pid);
    let rows: Vec<Value> = claims::foreign_owners(&windows, pid, std::process::id() as i32)
        .into_iter()
        .map(|owner| {
            let claim = key_claim(owner);
            let position = reference.and_then(|reference| match claim {
                Some(claimed) => Some(claims::judge(&windows, reference, owner, claimed)),
                None => claims::first_window(&windows, reference, owner),
            });
            json!({
                "pid": owner,
                "claims_key": claim.is_some(),
                "claimed_bounds": claim.flatten().map(rect_json),
                "position": position.map(Claim::name),
                "ahead": position.map(|position| position == Claim::Ahead),
                "blocks": position.map(|position| claim.is_some() && position.blocks()),
                "layers": claims::layers(&windows, owner),
            })
        })
        .collect();
    Ok(json!({
        "pid": pid,
        "reference": reference.map(|index| json!({
            "index": index,
            "layer": windows[index].layer,
            "bounds": windows[index].bounds.map(rect_json),
        })),
        "layers": claims::layers(&windows, pid),
        "claimants": rows,
    }))
}

/// The on-screen windows, front to back: the order the window list documents
/// for `kCGWindowListOptionOnScreenOnly`.
fn window_list() -> Result<Vec<WindowEntry>, Failure> {
    // SAFETY: a Copy result adopted once; each value is borrowed from its
    // retained dictionary while it lives, and type-checked before it is read.
    unsafe {
        let Some(list) = Cf::owned(CGWindowListCopyWindowInfo(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            0,
        )) else {
            return Err(Failure::Refused("the window list is unavailable".into()));
        };
        let listed = cf::array_items(list.ptr())
            .into_iter()
            .map(|info| {
                if !cf::is_dict(info.ptr()) {
                    return (None, None, None);
                }
                let owner = cf::to_i64(CFDictionaryGetValue(info.ptr(), kCGWindowOwnerPID))
                    .and_then(|owner| i32::try_from(owner).ok());
                let layer = cf::to_i64(CFDictionaryGetValue(info.ptr(), kCGWindowLayer));
                let bounds = CFDictionaryGetValue(info.ptr(), kCGWindowBounds);
                let mut rect = CGRect::default();
                let bounds = (cf::is_dict(bounds)
                    && CGRectMakeWithDictionaryRepresentation(bounds, &mut rect)
                    && [
                        rect.origin.x,
                        rect.origin.y,
                        rect.size.width,
                        rect.size.height,
                    ]
                    .iter()
                    .all(|value| value.is_finite()))
                .then(|| rect.into());
                (owner, layer, bounds)
            })
            .collect();
        claims::snapshot(listed).map_err(Failure::Refused)
    }
}

/// `None` when `pid` does not say a window of its own is key; otherwise the
/// frame of the window it names (`None` inside when that has no frame).
fn key_claim(pid: i32) -> Option<Option<Rect>> {
    let app = Element::application(pid).ok()?;
    // SAFETY: a plain timeout on a live element.
    unsafe { AXUIElementSetMessagingTimeout(app.0.ptr(), FOREIGN_TIMEOUT) };
    let window = app.element("AXFocusedWindow")?;
    (window.boolean("AXFocused") == Some(true)).then(|| window.frame().map(Rect::from))
}

pub fn main_window(pid: i32) -> Result<Element, Failure> {
    set_global_timeout(TIMEOUT);
    let app = Element::application(pid)?;
    if let Err(code) = app.copy("AXRole") {
        return Err(failure(&format!("reading pid {pid}"), code));
    }
    app.element("AXMainWindow")
        .or_else(|| app.element("AXFocusedWindow"))
        .or_else(|| {
            let windows = app.attr("AXWindows")?;
            // SAFETY: `array_items` type-checks the live value.
            unsafe { cf::array_items(windows.ptr()) }
                .into_iter()
                .next()
                .map(Element)
        })
        .ok_or_else(|| Failure::Failed(format!("pid {pid} has no AX window")))
}

fn window_json(pid: i32, window: &Element) -> Result<Value, Failure> {
    let frame = window
        .frame()
        .ok_or_else(|| Failure::Failed(format!("pid {pid}'s window has no AX frame")))?;
    Ok(json!({
        "pid": pid,
        "frame": rect_json(frame),
        "title": window.title(),
        "full_screen": window.boolean("AXFullScreen"),
        "minimized": window.boolean("AXMinimized"),
        "full_screen_button": window
            .element("AXFullScreenButton")
            .and_then(|button| button.frame())
            .map(rect_json),
    }))
}

pub fn window(pid: i32) -> Outcome {
    window_json(pid, &main_window(pid)?)
}

pub fn window_set(pid: i32, frame: Rect) -> Outcome {
    let window = main_window(pid)?;
    let origin = CGPoint {
        x: frame.x,
        y: frame.y,
    };
    let size = CGSize {
        width: frame.width,
        height: frame.height,
    };
    // Position, size, position: a resize near a screen edge can move the
    // window, and a move can clamp the size.
    window.set_value("AXPosition", kAXValueCGPointType, &origin)?;
    window.set_value("AXSize", kAXValueCGSizeType, &size)?;
    window.set_value("AXPosition", kAXValueCGPointType, &origin)?;
    // Read back once the frame lands, or after a short wait for one the
    // screen constrained.
    let requested = CGRect { origin, size };
    let landed_by = Instant::now() + Duration::from_millis(150);
    while window.frame() != Some(requested) && Instant::now() < landed_by {
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut report = window_json(pid, &window)?;
    report["requested"] = rect_json(requested);
    Ok(report)
}

fn menu_items(menu: &Element, depth: usize) -> Vec<Value> {
    menu.children()
        .iter()
        .map(|item| {
            let mut row = json!({
                "title": item.title(),
                "role": item.role(),
                "enabled": item.boolean("AXEnabled"),
                "mark": item.string("AXMenuItemMarkChar"),
                "cmd_char": item.string("AXMenuItemCmdChar"),
                "cmd_modifiers": item.integer("AXMenuItemCmdModifiers"),
            });
            if depth > 0 {
                if let Some(submenu) = submenu(item) {
                    row["items"] = Value::Array(menu_items(&submenu, depth - 1));
                }
            }
            row
        })
        .collect()
}

fn submenu(item: &Element) -> Option<Element> {
    item.children()
        .into_iter()
        .find(|child| child.role().as_deref() == Some("AXMenu"))
}

fn menu_bar_of(pid: i32) -> Result<Element, Failure> {
    set_global_timeout(TIMEOUT);
    let app = Element::application(pid)?;
    match app.copy("AXMenuBar") {
        Ok(Some(bar)) => Ok(Element(bar)),
        Ok(None) => Err(Failure::Failed(format!("pid {pid} has no AX menu bar"))),
        Err(code) => Err(failure(&format!("reading pid {pid}'s menu bar"), code)),
    }
}

pub fn menu_bar(pid: i32) -> Outcome {
    let bar = menu_bar_of(pid)?;
    let menus: Vec<Value> = bar
        .children()
        .iter()
        .map(|item| {
            json!({
                "title": item.title(),
                "items": submenu(item).map(|menu| menu_items(&menu, 2)).unwrap_or_default(),
            })
        })
        .collect();
    Ok(json!({"pid": pid, "menus": menus}))
}

/// The open popup of `pid` near `at`, found by hit-testing: an open popup is
/// not among the application's `AXChildren`, but the element under a point
/// inside it is one of its items (§2.12).
fn find_popup(pid: i32, at: Point, wait: Duration) -> Result<(Element, Point), Failure> {
    set_global_timeout(POPUP_TIMEOUT);
    let system = system_wide()?;
    let deadline = Instant::now() + wait;
    loop {
        for (dx, dy) in POPUP_PROBES {
            let probe = Point {
                x: at.x + dx,
                y: at.y + dy,
            };
            let hit = match element_at(&system, probe) {
                Ok(Some(hit)) => hit,
                Err(code) if code == kAXErrorAPIDisabled => {
                    return Err(failure("hit-testing for the popup", code))
                }
                _ => continue,
            };
            if hit.pid() != Some(pid) {
                continue;
            }
            let menu = match hit.role().as_deref() {
                Some("AXMenu") => Some(hit),
                Some("AXMenuItem") => hit
                    .element("AXParent")
                    .filter(|parent| parent.role().as_deref() == Some("AXMenu")),
                _ => None,
            };
            if let Some(menu) = menu {
                return Ok((menu, probe));
            }
        }
        if Instant::now() >= deadline {
            return Err(Failure::Failed(format!(
                "no open popup menu of pid {pid} near ({}, {}) within {} ms",
                at.x,
                at.y,
                wait.as_millis()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn popup(pid: i32, at: Point, wait: Duration) -> Outcome {
    let started = Instant::now();
    let (menu, probe) = find_popup(pid, at, wait)?;
    Ok(json!({
        "pid": pid,
        "probe": point_json(probe),
        "frame": menu.frame().map(rect_json),
        "items": menu_items(&menu, 1),
        "elapsed_ms": started.elapsed().as_millis() as u64,
    }))
}

/// One path segment among `children`: a title, or `#N` for the Nth child.
fn pick(children: Vec<Element>, segment: &str) -> Result<Element, Failure> {
    if let Some(index) = segment
        .strip_prefix('#')
        .and_then(|n| n.parse::<usize>().ok())
    {
        let count = children.len();
        return children
            .into_iter()
            .nth(index)
            .ok_or_else(|| Failure::Failed(format!("no child {segment} (there are {count})")));
    }
    let titles: Vec<String> = children
        .iter()
        .map(|child| child.title().unwrap_or_default())
        .collect();
    match titles.iter().position(|title| title == segment) {
        Some(index) => Ok(children
            .into_iter()
            .nth(index)
            .expect("index from position")),
        None => Err(Failure::Failed(format!(
            "no item titled {segment:?} (have {titles:?})"
        ))),
    }
}

/// `segments` down from `root` (a menu bar or an open menu): the first among
/// its own items, each later one in the previous item's submenu.
fn descend_menu(root: &Element, segments: &[String]) -> Result<Element, Failure> {
    let (first, rest) = segments.split_first().expect("parse rejects a bare root");
    let mut item = pick(root.children(), first)?;
    for segment in rest {
        let menu = submenu(&item).ok_or_else(|| {
            Failure::Failed(format!(
                "{} has no submenu to find {segment:?} in",
                item.describe()
            ))
        })?;
        item = pick(menu.children(), segment)?;
    }
    Ok(item)
}

pub fn press(pid: i32, path: &[String], at: Option<Point>, wait: Duration) -> Outcome {
    let (root, segments) = path.split_first().expect("parse rejects an empty path");
    let target = match root.as_str() {
        "menu-bar" => descend_menu(&menu_bar_of(pid)?, segments)?,
        "popup" => {
            let at = at.expect("parse pairs --at with a popup path");
            descend_menu(&find_popup(pid, at, wait)?.0, segments)?
        }
        "window" => {
            let mut element = main_window(pid)?;
            for segment in segments {
                element = if segment.starts_with("AX") {
                    element
                        .element(segment)
                        .ok_or_else(|| Failure::Failed(format!("the window has no {segment}")))?
                } else {
                    pick(element.children(), segment)?
                };
            }
            element
        }
        other => unreachable!("parse rejects the root {other}"),
    };
    let pressed = target.describe();
    if target.boolean("AXEnabled") == Some(false) {
        return Err(Failure::Failed(format!("{pressed} is disabled")));
    }
    target.press(&pressed.to_string())?;
    Ok(json!({"pid": pid, "path": path, "pressed": pressed}))
}
