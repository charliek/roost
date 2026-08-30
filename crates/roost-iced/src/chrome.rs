use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::Instant;

use iced::advanced::text::{self as advanced_text, Paragraph as _};
use iced::widget::{button, container, image, text_input};
use iced::{Background, Border, Color, Font, Pixels, Shadow, Size, Theme, Vector};

/// One application-owned band height keeps the sidebar header and tab strip
/// on the same seam. Native window decorations remain outside this geometry.
pub const BAND_HEIGHT: f32 = 32.0;
pub const ROW_HEIGHT: f32 = 32.0;
pub const PILL_HEIGHT: f32 = 24.0;
/// Vertical padding that centers a `PILL_HEIGHT` pill inside a `BAND_HEIGHT`
/// band — shared by the sidebar footer and the tab strip so both bands stay
/// in sync if either height changes.
pub const BAND_PILL_PADDING_Y: f32 = (BAND_HEIGHT - PILL_HEIGHT) / 2.0;
/// The sidebar footer's own band, separate from `BAND_HEIGHT` (which stays
/// pinned to the sidebar header + tab strip, both covered by
/// `test_tab_strip_pixels.py`). The mac gives its `+ New Project` button
/// 8pt above / 12pt below rather than centering it (App.swift:841-853:
/// `scrollView.bottomAnchor = addProject.topAnchor - 8`,
/// `addProject.bottomAnchor = pane.bottomAnchor - 12`) — measured live for
/// plan 026 C9 (mac pixel-sampled footer band: 8px fill, 24px button,
/// 12px fill, matching the source exactly).
pub const FOOTER_PADDING_TOP: f32 = 8.0;
pub const FOOTER_PADDING_BOTTOM: f32 = 12.0;
pub const FOOTER_BAND_HEIGHT: f32 = PILL_HEIGHT + FOOTER_PADDING_TOP + FOOTER_PADDING_BOTTOM;
/// The agent-rollup rail: 3px at the project row's leading edge, gapped 5px
/// top and bottom (Mac `SidebarRowView.drawBackground`, App.swift:5402-5405)
/// so adjacent active projects read as discrete segments rather than one
/// merged bar. It lives entirely inside the selection pill's leading inset,
/// so rail and pill never overlap.
pub const PROJECT_STRIPE_WIDTH: f32 = 3.0;
pub const PROJECT_STRIPE_INSET_Y: f32 = 5.0;
/// The selection pill is inset from the row's bounds on all four sides
/// (Mac `bounds.insetBy(dx: 6, dy: 1)`, App.swift:5419).
pub const PROJECT_PILL_INSET_X: f32 = 6.0;
pub const PROJECT_PILL_INSET_Y: f32 = 1.0;
/// Label leading inset and notification-dot trailing inset, both measured
/// from the pill's own edges. The label inset is what puts the project name
/// on the same left edge as the agent rows nested under it.
pub const PROJECT_LABEL_INSET: f32 = 18.0;
pub const PROJECT_DOT_INSET: f32 = 8.0;
pub const AGENT_DOT_INSET: f32 = 25.0;
pub const TAB_STATUS_SIZE: f32 = 7.0;
pub const NOTIFICATION_DOT_SIZE: f32 = 9.0;
/// Tab-pill width band, from the Mac (`App.swift:4757-4763`, config
/// `tabMaxWidth`): a pill never shrinks below `TAB_PILL_MIN_WIDTH` however
/// short its title, and a long title is tail-elided to keep the pill inside
/// `TAB_PILL_MAX_WIDTH` rather than letting one tab own the strip.
pub const TAB_PILL_MIN_WIDTH: f32 = 80.0;
pub const TAB_PILL_MAX_WIDTH: f32 = 220.0;
/// The pill's own horizontal inset, and the label block's inside it.
pub const TAB_PILL_PADDING_X: f32 = 2.0;
pub const TAB_PILL_LABEL_PADDING_X: f32 = 7.0;
/// Gap between the status dot and the title.
pub const TAB_PILL_LABEL_SPACING: f32 = 6.0;
pub const TAB_TITLE_SIZE: f32 = 12.0;
/// Everything a pill spends on chrome before its title gets a pixel: both
/// containers' side padding, the status dot, and the gap after it. Derived
/// rather than measured so the elision budget cannot drift from the layout
/// it is eliding for.
pub const TAB_PILL_CHROME_WIDTH: f32 = 2.0 * TAB_PILL_PADDING_X
    + 2.0 * TAB_PILL_LABEL_PADDING_X
    + TAB_STATUS_SIZE
    + TAB_PILL_LABEL_SPACING;
pub const PALETTE_WIDTH: f32 = 660.0;
pub const PALETTE_MAX_HEIGHT: f32 = 500.0;
/// The palette card's own outer padding (`app.rs`'s `palette_panel`
/// container) — hoisted so the row-inset constants below can be checked
/// against it in one place.
pub const PALETTE_PANEL_PADDING: f32 = 10.0;
/// Extra inset around each row, beyond `PALETTE_PANEL_PADDING`, so the
/// selection highlight doesn't run edge-to-edge with the card — matches the
/// mac `NSTableView`'s scroll-view gutter (`PalettePanel.swift:204-207`,
/// scroll inset 8 from the card) less the panel's own share of it. Measured
/// live for plan 026 C9: mac's highlight sits 14px from the card edge
/// (gutter 8 + `drawSelection`'s `insetBy(dx: 6)`, PalettePanel.swift:529).
pub const PALETTE_ROW_OUTER_INSET: f32 = 4.0;
/// Row label's own left/right padding, inside the highlight box. Mac insets
/// its row text 14px from the row edge (`PaletteCellView`,
/// PalettePanel.swift:568,:571) on top of the same 8px gutter, for 22px
/// from the card edge; `PALETTE_PANEL_PADDING + PALETTE_ROW_OUTER_INSET +
/// PALETTE_ROW_PADDING_X` reproduces that total (see the pinning test).
pub const PALETTE_ROW_PADDING_X: f32 = 8.0;

/// The chrome's bundled sans (`third_party/inter/`, loaded via
/// `include_bytes!` in `main.rs`). This is the exact name-table family
/// cosmic-text reports for all three static weights it registers — Regular,
/// Medium, and SemiBold group under one "Inter" family, so a `Weight` alone
/// selects the right instance.
pub const CHROME_FONT_FAMILY: &str = "Inter";

/// The one solid chrome band: sidebar header, sidebar footer, tab band.
pub const BAND: Color = Color::from_rgb8(0x24, 0x29, 0x2c);
/// The sidebar's scrollable project list, one step lighter than the bands.
pub const LIST: Color = Color::from_rgb8(0x2d, 0x32, 0x35);
/// Hairline between the sidebar and the terminal, drawn inside the sidebar's
/// own width so the terminal grid keeps every pixel it is sized for.
pub const DIVIDER: Color = Color::from_rgb8(0x1a, 0x1d, 0x1e);
pub const DIVIDER_WIDTH: f32 = 1.0;
pub const FOOTER_CHIP: Color = Color::from_rgb8(0x34, 0x39, 0x3c);
pub const FOOTER_CHIP_HOVER: Color = Color::from_rgb8(0x3e, 0x44, 0x47);
pub const FOOTER_CHIP_PRESSED: Color = Color::from_rgb8(0x2a, 0x2f, 0x32);
pub const ACTIVE_BLUE: Color = Color::from_rgb8(0x13, 0x50, 0x9d);
pub const ACTIVE_TAB: Color = Color::from_rgb8(0x24, 0x37, 0x51);
pub const HOVER: Color = Color::from_rgb8(0x39, 0x39, 0x39);
pub const ACTIVE_AGENT: Color = Color::from_rgb8(0x3a, 0x3a, 0x3a);
pub const TEXT: Color = Color::from_rgb8(0xf2, 0xf2, 0xf2);
pub const MUTED_TEXT: Color = Color::from_rgb8(0xa0, 0xa4, 0xb0);
/// Sidebar project label: the mac reads "bolder" when active, but both
/// platforms use the same regular 13pt weight — the difference is COLOR
/// (`SidebarRowView.applyLabelColor`, App.swift:5333-5342: white when
/// selected/emphasized, `NSColor(white: 0.82)` otherwise). Measured live
/// for plan 026 C9 (mac pixel-sampled: active text 255,255,255; inactive
/// peak 209,209,209 — 0.82 * 255 rounds to 209, i.e. `0xd1`).
pub const PROJECT_LABEL_ACTIVE: Color = Color::WHITE;
pub const PROJECT_LABEL_INACTIVE: Color = Color::from_rgb8(0xd1, 0xd1, 0xd1);
/// The chrome's one accent color: the notification dots (tab-pill badge +
/// sidebar project-row dot), the dragged-pill border, and the inline-rename
/// focus ring + selection all share it. Pinned to the Mac's
/// `NSColor.controlAccentColor` (#007aff), which both Mac surfaces use
/// (`App.swift:4772`, `:5207`). Hardcoded rather than tracking the desktop
/// accent — on COSMIC `@accent_bg_color` renders teal.
/// Was two constants (`NOTIFICATION` #4e9af1 generic blue, `NOTIFICATION_BADGE`
/// #007aff mac accent) until the drag/rename surfaces flipped to the mac
/// accent too (#321), at which point both names pinned the same value and
/// were merged.
pub const ACCENT: Color = Color::from_rgb8(0x00, 0x7a, 0xff);
pub const DRAGGED_PILL: Color = Color::from_rgba8(0x55, 0x68, 0x7b, 0.65);
pub const PALETTE_SURFACE: Color = Color::from_rgb8(0x2d, 0x2d, 0x33);
pub const PALETTE_SELECTION: Color = Color::from_rgb8(0x48, 0x48, 0x4e);
pub const PALETTE_HOVER: Color = Color::from_rgb8(0x3d, 0x3d, 0x43);
pub const PALETTE_PLACEHOLDER: Color = Color::from_rgb8(0x9e, 0x9e, 0x9e);
pub const PALETTE_MATCH: Color = Color::from_rgb8(0x5f, 0xa3, 0xf0);
/// The host-section header's connection dot (plan 037 §3.1). Sampled
/// from the approved mockup's `.hdot` rules: green connected, grey gone,
/// amber in flight — the amber is the mockup's own busy-agent shade, so
/// "something is happening" reads the same everywhere in the chrome.
pub const HOST_DOT_CONNECTED: Color = Color::from_rgb8(0x3f, 0xca, 0x6b);
pub const HOST_DOT_OFFLINE: Color = Color::from_rgb8(0x6a, 0x70, 0x76);
pub const HOST_DOT_PENDING: Color = Color::from_rgb8(0xe6, 0xb2, 0x3a);
pub const HOST_DOT_SIZE: f32 = 7.0;
/// Gap between the dot, the label, and the rollup in a host band
/// (mockup `.hosthdr { gap: 7px }`).
pub const HOST_BAND_SPACING: f32 = 7.0;
/// The band's right-aligned rollup ("2 agents", "disconnected") — one
/// step quieter than [`MUTED_TEXT`], as the mockup's `.hosthdr small`.
pub const HOST_ROLLUP_TEXT: Color = Color::from_rgb8(0x6e, 0x74, 0x7a);
pub const HOST_ROLLUP_SIZE: f32 = 10.0;
/// A disconnected section's rows stay listed at this opacity (mockup
/// `.dim`). Applied to the row colors rather than to a layer: iced has no
/// container opacity, and scaling alpha composites identically over the
/// flat list fill.
pub const HOST_SECTION_DIM: f32 = 0.45;
/// The inline "↻ Reconnect" row (mockup `.reconnect`).
pub const HOST_RECONNECT_TEXT: Color = Color::from_rgb8(0x7f, 0xa8, 0xe8);
/// The takeover / session-ended banner over a host tab's last frame
/// (plan 037 §3.1). Sampled from the approved mockup's `.banner` rules —
/// a warm amber band that is deliberately nothing like the terminal
/// palette underneath it, because the whole point is that it is not part
/// of the frame it sits on.
pub const HOST_BANNER_BG: Color = Color::from_rgb8(0x4a, 0x33, 0x23);
pub const HOST_BANNER_TEXT: Color = Color::from_rgb8(0xee, 0xcf, 0xa2);
pub const HOST_BANNER_BORDER: Color = Color::from_rgb8(0x6b, 0x4a, 0x2a);
pub const HOST_BANNER_BUTTON_BORDER: Color = Color::from_rgb8(0x8a, 0x6a, 0x40);
pub const HOST_BANNER_TEXT_SIZE: f32 = 12.0;
pub const HOST_BANNER_ACTION_SIZE: f32 = 11.5;
/// The scrim over the frozen frame. A layer rather than
/// [`HOST_SECTION_DIM`]'s alpha scaling: the terminal is drawn by a
/// custom widget from an owned snapshot, so the only way to dim it
/// without touching every cell color is to composite over it.
pub const HOST_FRAME_SCRIM: Color = Color::from_rgba8(0x14, 0x16, 0x18, 0.55);
pub const ERROR_TEXT: Color = Color::from_rgb8(0xee, 0x78, 0x78);
pub const DANGER: Color = Color::from_rgb8(0x8a, 0x2a, 0x2a);
pub const DANGER_ACCENT: Color = Color::from_rgb8(0xa8, 0x33, 0x33);

pub fn chrome_font(weight: iced::font::Weight) -> Font {
    Font {
        family: iced::font::Family::Name(CHROME_FONT_FAMILY),
        weight,
        ..Font::default()
    }
}

/// The app icon, embedded from the very PNG the Linux package installs into
/// hicolor. An `NSAlert` gets the app icon for free from the bundle; iced has
/// no bundle to read one from on either platform, so the bytes ride along
/// like the Inter faces do.
const APP_ICON_PNG: &[u8] =
    include_bytes!("../../../packaging/icons/hicolor/256x256/apps/roost.png");

/// Icon edge in the confirm dialog, matching the 64pt `NSAlert` draws.
pub const APP_ICON_SIZE: f32 = 64.0;

/// Decoded once and cloned: `Handle::from_rgba` mints a fresh id per call, so
/// building the handle inside `view` would re-upload the texture every frame.
static APP_ICON: LazyLock<Option<image::Handle>> = LazyLock::new(decode_app_icon);

/// `None` only if the embedded asset stops being 8-bit RGBA, in which case
/// the dialog simply renders without an icon (pinned by a unit test).
pub fn app_icon() -> Option<image::Handle> {
    APP_ICON.clone()
}

fn decode_app_icon() -> Option<image::Handle> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(APP_ICON_PNG))
        .read_info()
        .ok()?;
    if reader.output_color_type() != (png::ColorType::Rgba, png::BitDepth::Eight) {
        return None;
    }
    let mut pixels = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut pixels).ok()?;
    pixels.truncate(info.buffer_size());
    Some(image::Handle::from_rgba(info.width, info.height, pixels))
}

/// The tail marker an elided label ends with.
pub const ELLIPSIS: &str = "…";

/// Shaped width of one chrome text run, measured through the very
/// `Paragraph` type the renderer lays it out with (the same seam
/// `TerminalMetrics::measure_with_font` uses for the cell grid), so a
/// budget computed here matches what will be drawn.
pub fn text_width(content: &str, font: Font, size: f32) -> f32 {
    if content.is_empty() {
        return 0.0;
    }
    type Paragraph = <iced::Renderer as advanced_text::Renderer>::Paragraph;
    Paragraph::with_text(advanced_text::Text {
        content,
        bounds: Size::INFINITE,
        size: Pixels(size),
        line_height: advanced_text::LineHeight::default(),
        font,
        align_x: advanced_text::Alignment::Default,
        align_y: iced::alignment::Vertical::Top,
        shaping: advanced_text::Shaping::Advanced,
        wrapping: advanced_text::Wrapping::None,
    })
    .min_bounds()
    .width
}

/// Tail-elide `content` to `max_width`, returning what to draw and how wide
/// it measures. Iced 0.14's text widget has no ellipsis mode — an
/// overlong label just stops mid-glyph at its clip edge, which reads as a
/// rendering fault — so the string itself is shortened and marked, the way
/// the Mac's tab pills show `/Users/charliek/project…`.
pub fn elide_to_width(content: &str, font: Font, size: f32, max_width: f32) -> (Cow<'_, str>, f32) {
    let started = Instant::now();
    let full = text_width(content, font, size);
    if full <= max_width {
        crate::perf::record_elide(started.elapsed());
        return (Cow::Borrowed(content), full);
    }
    // Cut points are char boundaries: no grapheme segmenter is in the
    // dependency set, so a cut can drop a trailing combining mark — never
    // split a code point, and never produce invalid UTF-8.
    let cuts: Vec<usize> = content.char_indices().map(|(index, _)| index).collect();
    // Largest prefix whose marked form still fits. Shaped width is
    // non-decreasing in prefix length, which is what makes the search
    // sound; the full string is already known not to fit, so it is not a
    // candidate.
    let (mut low, mut high) = (0usize, cuts.len());
    let mut best: Option<(String, f32)> = None;
    while low < high {
        let mid = low + (high - low) / 2;
        let candidate = format!("{}{ELLIPSIS}", &content[..cuts[mid]]);
        let width = text_width(&candidate, font, size);
        if width <= max_width {
            best = Some((candidate, width));
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    let result = match best {
        Some((elided, width)) => (Cow::Owned(elided), width),
        // Not even the marker fits. Drawing it anyway keeps the pill
        // honest about having dropped something.
        None => (Cow::Borrowed(ELLIPSIS), text_width(ELLIPSIS, font, size)),
    };
    crate::perf::record_elide(started.elapsed());
    result
}

pub fn band(_: &Theme) -> container::Style {
    container::Style::default().background(BAND)
}

pub fn list(_: &Theme) -> container::Style {
    container::Style::default().background(LIST)
}

pub fn divider(_: &Theme) -> container::Style {
    container::Style::default().background(DIVIDER)
}

/// Both strips share one drag affordance, so the dragged row and the dragged
/// tab stay visually identical; only the resting fill and radius differ.
fn pill(active_background: Color, radius: f32, active: bool, dragging: bool) -> container::Style {
    let mut style = container::Style::default();
    if dragging {
        style = style.background(DRAGGED_PILL);
    } else if active {
        style = style.background(active_background);
    }
    style.border = Border {
        color: if dragging { ACCENT } else { Color::TRANSPARENT },
        width: if dragging { 1.0 } else { 0.0 },
        radius: radius.into(),
    };
    style
}

pub fn tab_pill(active: bool, dragging: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| pill(ACTIVE_TAB, 6.0, active, dragging)
}

pub fn badge(_: &Theme) -> container::Style {
    container::Style::default()
        .background(ACCENT)
        .border(Border::default().rounded(NOTIFICATION_DOT_SIZE / 2.0))
}

pub fn project_pill(active: bool, dragging: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| pill(ACTIVE_BLUE, 6.0, active, dragging)
}

pub fn agent_button(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| chrome_button(active.then_some(ACTIVE_AGENT), status, 4.0)
}

pub fn transparent_button(_: &Theme, status: button::Status) -> button::Style {
    chrome_button(None, status, 4.0)
}

/// The active tab pill's `×`. Deliberately NOT a `chrome_button`: a filled
/// `HOVER` rect on a 24px control inside a 24px pill recolors the pill's
/// whole trailing end, which is the hover Charlie called out (Q3). The
/// glyph reddens instead and the pill's own fill is left alone. `ERROR_TEXT`
/// is the chrome's text-weight red — the `DANGER` fills read near-black at
/// glyph weight.
pub fn close_button(_: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: None,
        text_color: match status {
            button::Status::Hovered | button::Status::Pressed => ERROR_TEXT,
            button::Status::Active => TEXT,
            button::Status::Disabled => MUTED_TEXT.scale_alpha(0.5),
        },
        border: Border::default().rounded(4),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// The sidebar-footer "+ New Project" chip: a centered rounded button with
/// a resting fill, matching the shipped Mac bezel (and the now-removed
/// GTK UI's chip affordance) rather than the flat text buttons used
/// elsewhere in the chrome.
pub fn footer_chip_button(_: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => FOOTER_CHIP_HOVER,
        button::Status::Pressed => FOOTER_CHIP_PRESSED,
        button::Status::Active => FOOTER_CHIP,
        button::Status::Disabled => FOOTER_CHIP.scale_alpha(0.5),
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: match status {
            button::Status::Disabled => MUTED_TEXT.scale_alpha(0.5),
            _ => TEXT,
        },
        border: Border::default().rounded(6.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A dialog's confirming action — Add Host's "Add & Connect" (plan 037
/// §3.1, the mock's `.btn.primary`).
///
/// The accent twin of [`danger_button`]: same geometry and the same
/// disabled treatment, so a dialog's button row reads as one control set
/// whichever of the two it ends with. `ACTIVE_BLUE` is the chrome's
/// existing pressed-blue, reused rather than inventing a shade.
pub fn primary_button(_: &Theme, status: button::Status) -> button::Style {
    let (background, text_color) = match status {
        button::Status::Hovered | button::Status::Pressed => (ACTIVE_BLUE, TEXT),
        button::Status::Active => (ACCENT, TEXT),
        button::Status::Disabled => (ACCENT.scale_alpha(0.5), MUTED_TEXT.scale_alpha(0.5)),
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color,
        border: Border::default().rounded(4),
        shadow: Shadow::default(),
        snap: true,
    }
}

pub fn danger_button(_: &Theme, status: button::Status) -> button::Style {
    let (background, text_color) = match status {
        button::Status::Hovered | button::Status::Pressed => (DANGER_ACCENT, TEXT),
        button::Status::Active => (DANGER, TEXT),
        button::Status::Disabled => (DANGER.scale_alpha(0.5), MUTED_TEXT.scale_alpha(0.5)),
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color,
        border: Border::default().rounded(4),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// The banner strip itself: a filled band with a hairline along its
/// bottom edge, so it reads as chrome laid over the frame rather than as
/// something the terminal drew.
pub fn host_banner(_: &Theme) -> container::Style {
    container::Style::default().background(HOST_BANNER_BG)
}

/// The hairline under the banner. A separate 1px container rather than a
/// border: iced borders draw on all four edges, and a box around the
/// full-width strip reads as a framed callout instead of a band.
pub fn host_banner_edge(_: &Theme) -> container::Style {
    container::Style::default().background(HOST_BANNER_BORDER)
}

/// The banner's one action ("Reconnect here"). Outlined in the band's
/// own border shade — a filled accent button here would compete with the
/// message for the eye, and the mockup's `.banner .btn` does not.
pub fn host_banner_button(_: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => Some(Background::Color(
            HOST_BANNER_BUTTON_BORDER.scale_alpha(0.4),
        )),
        button::Status::Active | button::Status::Disabled => None,
    };
    button::Style {
        background,
        text_color: HOST_BANNER_TEXT,
        border: Border {
            color: HOST_BANNER_BUTTON_BORDER,
            width: 1.0,
            radius: 4.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// The dimming layer over a frame nothing will update again.
pub fn host_frame_scrim(_: &Theme) -> container::Style {
    container::Style::default().background(HOST_FRAME_SCRIM)
}

pub fn palette_panel(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(PALETTE_SURFACE)),
        border: Border {
            color: Color::from_rgba8(0xff, 0xff, 0xff, 0.12),
            width: 1.0,
            radius: 10.0.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba8(0, 0, 0, 0.55),
            offset: Vector::new(0.0, 12.0),
            blur_radius: 34.0,
        },
        ..container::Style::default()
    }
}

pub fn status_toast(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(PALETTE_SURFACE)),
        border: Border {
            color: ERROR_TEXT.scale_alpha(0.55),
            width: 1.0,
            radius: 6.0.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba8(0, 0, 0, 0.45),
            offset: Vector::new(0.0, 5.0),
            blur_radius: 18.0,
        },
        ..container::Style::default()
    }
}

pub fn palette_input(_: &Theme, _: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        icon: PALETTE_PLACEHOLDER,
        placeholder: PALETTE_PLACEHOLDER,
        value: TEXT,
        selection: ACTIVE_BLUE,
    }
}

/// The mac reference field goes near-black while editing (a dark fill, not
/// the transparent one that let the selection pill's blue show through and
/// read as a blue field — W6). `DIVIDER` is the darkest existing chrome
/// neutral, so the field reads as its own surface rather than a new hex.
pub fn inline_rename_input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    text_input::Style {
        background: Background::Color(DIVIDER),
        border: Border {
            color: if focused { ACCENT } else { MUTED_TEXT },
            width: 1.0,
            radius: 3.0.into(),
        },
        icon: MUTED_TEXT,
        placeholder: MUTED_TEXT,
        value: TEXT,
        // Opaque, matching the mac field editor's rendered selection —
        // sampled live at RGB(0,122,255) with white glyphs (plan 029 F5);
        // a translucent tint over the dark field reads darker than mac.
        selection: ACCENT,
    }
}

pub fn palette_row(
    selected: bool,
    actionable: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let background = if selected {
            Some(PALETTE_SELECTION)
        } else {
            match status {
                button::Status::Hovered | button::Status::Pressed if actionable => {
                    Some(PALETTE_HOVER)
                }
                _ => None,
            }
        };
        button::Style {
            background: background.map(Background::Color),
            text_color: if actionable {
                TEXT
            } else {
                PALETTE_PLACEHOLDER.scale_alpha(0.6)
            },
            border: Border::default().rounded(6),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

fn chrome_button(selected: Option<Color>, status: button::Status, radius: f32) -> button::Style {
    let background = match status {
        button::Status::Hovered => Some(selected.unwrap_or(HOVER)),
        button::Status::Pressed => Some(selected.unwrap_or(ACTIVE_TAB)),
        button::Status::Active => selected,
        button::Status::Disabled => selected.map(|color| color.scale_alpha(0.5)),
    };
    button::Style {
        background: background.map(Background::Color),
        text_color: match status {
            button::Status::Disabled => MUTED_TEXT.scale_alpha(0.5),
            _ => TEXT,
        },
        border: Border::default().rounded(radius),
        shadow: Shadow::default(),
        snap: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The confirm dialog silently drops the icon if the embedded asset ever
    /// stops being 8-bit RGBA, so the decode gets pinned here instead.
    #[test]
    fn the_embedded_app_icon_decodes_to_rgba_pixels() {
        let handle = app_icon().expect("the embedded app icon decodes");
        match handle {
            image::Handle::Rgba {
                width,
                height,
                pixels,
                ..
            } => {
                assert_eq!((width, height), (256, 256));
                assert_eq!(pixels.len(), 256 * 256 * 4);
            }
            other => panic!("expected decoded pixels, got {other:?}"),
        }
    }

    #[test]
    fn active_rows_and_pills_use_roost_selection_colors() {
        let theme = Theme::Dark;
        let active = project_pill(true, false)(&theme);
        assert_eq!(active.background, Some(Background::Color(ACTIVE_BLUE)));
        assert_eq!(
            project_pill(true, true)(&theme).border.color,
            ACCENT,
            "the dragged project row is outlined like the dragged tab pill"
        );
        assert_eq!(
            tab_pill(true, false)(&theme).background,
            Some(Background::Color(ACTIVE_TAB))
        );
    }

    #[test]
    fn chrome_regions_split_into_one_band_color_and_one_list_color() {
        let theme = Theme::Dark;
        assert_eq!(band(&theme).background, Some(Background::Color(BAND)));
        assert_eq!(list(&theme).background, Some(Background::Color(LIST)));
        assert_eq!(divider(&theme).background, Some(Background::Color(DIVIDER)));
        assert_ne!(BAND, LIST, "the list region reads lighter than the bands");
        assert_eq!(DIVIDER_WIDTH, 1.0);
    }

    #[test]
    fn footer_chip_rests_on_a_filled_bezel_and_dims_when_disabled() {
        let theme = Theme::Dark;
        assert_eq!(
            footer_chip_button(&theme, button::Status::Active).background,
            Some(Background::Color(FOOTER_CHIP))
        );
        assert_eq!(
            footer_chip_button(&theme, button::Status::Hovered).background,
            Some(Background::Color(FOOTER_CHIP_HOVER))
        );
        assert_eq!(
            footer_chip_button(&theme, button::Status::Pressed).background,
            Some(Background::Color(FOOTER_CHIP_PRESSED))
        );
        assert_eq!(
            footer_chip_button(&theme, button::Status::Disabled).background,
            Some(Background::Color(FOOTER_CHIP.scale_alpha(0.5)))
        );
        assert_eq!(
            footer_chip_button(&theme, button::Status::Active).text_color,
            TEXT
        );
    }

    #[test]
    fn inactive_controls_are_transparent_until_hovered() {
        let theme = Theme::Dark;
        assert_eq!(
            transparent_button(&theme, button::Status::Active).background,
            None
        );
        assert_eq!(
            transparent_button(&theme, button::Status::Hovered).background,
            Some(Background::Color(HOVER))
        );
    }

    #[test]
    fn project_and_agent_content_share_the_reference_left_edge() {
        let project_text = PROJECT_PILL_INSET_X + PROJECT_LABEL_INSET;
        assert!((project_text - AGENT_DOT_INSET).abs() <= 1.0);
        assert_eq!(TAB_STATUS_SIZE, 7.0);
        assert_eq!(NOTIFICATION_DOT_SIZE, 9.0);
    }

    #[test]
    fn tab_pill_chrome_leaves_the_title_the_rest_of_its_width_band() {
        assert_eq!(TAB_PILL_CHROME_WIDTH, 31.0);
        assert_eq!(
            TAB_PILL_MIN_WIDTH.min(TAB_PILL_MAX_WIDTH),
            TAB_PILL_MIN_WIDTH
        );
        // Even the narrowest pill has room for a title beside its chrome.
        assert!((TAB_PILL_MIN_WIDTH - TAB_PILL_CHROME_WIDTH).max(0.0) > 0.0);
    }

    #[test]
    fn the_close_affordance_reddens_its_glyph_instead_of_filling_the_pill() {
        let theme = Theme::Dark;
        for status in [button::Status::Hovered, button::Status::Pressed] {
            let style = close_button(&theme, status);
            assert_eq!(
                style.background, None,
                "no pill-recoloring fill in any state"
            );
            assert_eq!(style.text_color, ERROR_TEXT);
        }
        let resting = close_button(&theme, button::Status::Active);
        assert_eq!(resting.background, None);
        assert_eq!(resting.text_color, TEXT);
    }

    #[test]
    fn elision_keeps_short_labels_verbatim_and_marks_what_it_drops() {
        let font = chrome_font(iced::font::Weight::Normal);
        let size = TAB_TITLE_SIZE;

        let (empty, width) = elide_to_width("", font, size, 100.0);
        assert_eq!(empty, "");
        assert_eq!(width, 0.0);

        let short = "shell";
        let natural = text_width(short, font, size);
        assert!(natural > 0.0);
        let (kept, kept_width) = elide_to_width(short, font, size, natural + 40.0);
        assert_eq!(kept, short, "a label with room to spare is untouched");
        assert_eq!(kept_width, natural);

        // Exact fit: the budget is the measured width itself, so nothing
        // may be dropped and no marker may appear.
        let (exact, exact_width) = elide_to_width(short, font, size, natural);
        assert_eq!(exact, short);
        assert_eq!(exact_width, natural);
    }

    #[test]
    fn elision_cuts_long_labels_to_a_marked_prefix_inside_the_budget() {
        let font = chrome_font(iced::font::Weight::Normal);
        let size = TAB_TITLE_SIZE;
        let long = "/Users/charliek/projects/roost/crates/roost-iced/src/chrome.rs";
        let budget = 120.0;
        let (elided, width) = elide_to_width(long, font, size, budget);

        assert!(
            width <= budget,
            "elided to {width}px, over the {budget}px budget"
        );
        assert!(
            elided.ends_with(ELLIPSIS),
            "no visible tail marker in {elided:?}"
        );
        assert!(elided.len() < long.len());
        let head = elided.strip_suffix(ELLIPSIS).expect("marked tail");
        assert!(
            long.starts_with(head),
            "{elided:?} is not a tail-elided {long:?}"
        );
        // Maximal: one more char would have overflowed.
        let next = long[head.len()..]
            .chars()
            .next()
            .expect("more label to drop");
        let overflowing = format!("{head}{next}{ELLIPSIS}");
        assert!(text_width(&overflowing, font, size) > budget);
    }

    #[test]
    fn elision_of_wide_glyphs_stays_on_code_point_boundaries() {
        let font = chrome_font(iced::font::Weight::Normal);
        let size = TAB_TITLE_SIZE;
        let cjk = "日本語のタブタイトルはとても長いことがあります";
        let budget = 60.0;
        let (elided, width) = elide_to_width(cjk, font, size, budget);

        assert!(width <= budget);
        assert!(elided.ends_with(ELLIPSIS));
        let head = elided.strip_suffix(ELLIPSIS).expect("marked tail");
        assert!(cjk.starts_with(head));
        // Wide glyphs cost more per character, so the same budget keeps
        // fewer of them than it does of ASCII — but only when the runner
        // actually has a CJK face; a font-less CI box shapes them as
        // narrow fallback boxes, so the comparison is gated on the
        // measured widths, not assumed.
        if text_width("日", font, size) > text_width("a", font, size) {
            let ascii = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let (ascii_elided, _) = elide_to_width(ascii, font, size, budget);
            assert!(
                head.chars().count()
                    < ascii_elided
                        .strip_suffix(ELLIPSIS)
                        .expect("marked tail")
                        .chars()
                        .count()
            );
        }

        // A budget too small even for the marker still says something was
        // dropped rather than rendering a bare truncation.
        assert_eq!(elide_to_width(cjk, font, size, 0.0).0, ELLIPSIS);
    }

    #[test]
    fn row_and_band_metrics_center_their_content() {
        assert_eq!(BAND_PILL_PADDING_Y, 4.0, "4px above and below a pill");
        // The pill is inset inside the row, the rail is gapped inside it, and
        // the rail stops short of where the pill begins.
        assert_eq!(ROW_HEIGHT - 2.0 * PROJECT_PILL_INSET_Y, 30.0);
        assert_eq!(ROW_HEIGHT - 2.0 * PROJECT_STRIPE_INSET_Y, 22.0);
        assert_eq!(
            PROJECT_PILL_INSET_X - PROJECT_STRIPE_WIDTH,
            3.0,
            "the rail clears the pill's leading edge"
        );
    }

    #[test]
    fn palette_styles_use_reference_neutrals_without_stock_primary_fill() {
        let theme = Theme::Dark;
        assert_eq!(
            palette_panel(&theme).background,
            Some(Background::Color(PALETTE_SURFACE))
        );
        assert_eq!(
            palette_row(true, true)(&theme, button::Status::Active).background,
            Some(Background::Color(PALETTE_SELECTION))
        );
        assert_eq!(
            palette_row(false, true)(&theme, button::Status::Active).background,
            None
        );
        assert_eq!(
            palette_input(&theme, text_input::Status::Focused { is_hovered: false })
                .border
                .width,
            0.0
        );
        let disabled = palette_row(false, false)(&theme, button::Status::Hovered);
        assert_eq!(disabled.background, None);
        assert_eq!(disabled.text_color, PALETTE_PLACEHOLDER.scale_alpha(0.6));
    }

    /// Plan 026 C9: mac's highlight sits 14px from the card edge, its row
    /// text 22px — see `PALETTE_ROW_OUTER_INSET`'s doc comment for the mac
    /// source split. `app.rs` composes these three constants (panel padding,
    /// the row's own outer container padding, the row button's padding) to
    /// reproduce both totals; pinned here so the three can't silently drift
    /// apart.
    #[test]
    fn palette_row_insets_match_the_measured_mac_card() {
        let highlight_inset = PALETTE_PANEL_PADDING + PALETTE_ROW_OUTER_INSET;
        let text_inset = highlight_inset + PALETTE_ROW_PADDING_X;
        assert_eq!(highlight_inset, 14.0);
        assert_eq!(text_inset, 22.0);
    }

    /// Plan 026 C9: the footer band is its own height/padding split from
    /// `BAND_HEIGHT` (sidebar header + tab strip stay pinned at 32 for
    /// `test_tab_strip_pixels.py`) — see `FOOTER_PADDING_TOP`'s doc comment
    /// for the mac source + measured values.
    #[test]
    fn footer_band_matches_the_measured_mac_padding() {
        assert_eq!(FOOTER_PADDING_TOP, 8.0);
        assert_eq!(FOOTER_PADDING_BOTTOM, 12.0);
        assert_eq!(FOOTER_BAND_HEIGHT, PILL_HEIGHT + 20.0);
        assert_ne!(
            FOOTER_BAND_HEIGHT, BAND_HEIGHT,
            "the footer band is deliberately taller than the header/tab-strip band"
        );
    }

    /// Plan 026 C9: both platforms use the same regular 13pt weight for the
    /// project label — the mac's "bolder" active read is color, not weight
    /// (`PROJECT_LABEL_ACTIVE`'s doc comment has the App.swift source +
    /// measured pixel values).
    #[test]
    fn project_label_colors_differ_only_the_mac_way() {
        assert_eq!(PROJECT_LABEL_ACTIVE, Color::WHITE);
        assert_eq!(PROJECT_LABEL_INACTIVE, Color::from_rgb8(0xd1, 0xd1, 0xd1));
        assert_ne!(PROJECT_LABEL_ACTIVE, PROJECT_LABEL_INACTIVE);
    }

    #[test]
    fn danger_button_always_paints_a_destructive_fill() {
        let theme = Theme::Dark;
        assert_eq!(
            danger_button(&theme, button::Status::Active).background,
            Some(Background::Color(DANGER))
        );
        assert_eq!(
            danger_button(&theme, button::Status::Hovered).background,
            Some(Background::Color(DANGER_ACCENT))
        );
        assert_eq!(
            danger_button(&theme, button::Status::Active).text_color,
            TEXT
        );
    }

    #[test]
    fn status_toast_is_a_neutral_surface_with_an_error_accent() {
        let style = status_toast(&Theme::Dark);
        assert_eq!(style.background, Some(Background::Color(PALETTE_SURFACE)));
        assert_eq!(style.border.color, ERROR_TEXT.scale_alpha(0.55));
        assert_eq!(style.border.width, 1.0);
    }

    #[test]
    fn accent_pins_the_mac_value_and_wires_every_surface_that_shares_it() {
        // Literal, not the constant itself: comparing a constant to itself
        // passes for any value and would not catch a re-flip (#321 flipped
        // this from #4e9af1 to the Mac's #007aff).
        assert_eq!(ACCENT, Color::from_rgb8(0x00, 0x7a, 0xff));

        // Wiring, not color: these are tautological on their own (they'd
        // hold for any value of the constant). They exist to catch a call
        // site being repointed at a *different* color; the literal above is
        // what pins the value.
        let theme = Theme::Dark;
        assert_eq!(
            badge(&theme).background,
            Some(Background::Color(ACCENT)),
            "both notification dots render the accent"
        );
        assert_eq!(
            pill(ACTIVE_TAB, 6.0, false, true).border.color,
            ACCENT,
            "the dragged-pill border renders the accent"
        );

        let focused =
            inline_rename_input(&theme, text_input::Status::Focused { is_hovered: false });
        assert_eq!(focused.border.color, ACCENT);
        assert_eq!(focused.selection, ACCENT);
    }

    #[test]
    fn rename_editor_uses_a_dark_field_not_the_pill_background() {
        let theme = Theme::Dark;
        let focused =
            inline_rename_input(&theme, text_input::Status::Focused { is_hovered: false });
        assert_eq!(
            focused.background,
            Background::Color(DIVIDER),
            "field reads as its own dark surface, not the transparent pill blue"
        );
        let unfocused = inline_rename_input(&theme, text_input::Status::Active);
        assert_eq!(
            unfocused.background,
            Background::Color(DIVIDER),
            "background stays dark whether or not the field is focused"
        );
        assert_eq!(
            unfocused.border.color, MUTED_TEXT,
            "only the border, not the background, reacts to focus"
        );
    }
}
