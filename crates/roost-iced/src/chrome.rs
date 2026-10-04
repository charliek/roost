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

pub const DIVIDER_WIDTH: f32 = 1.0;
pub const HOST_DOT_SIZE: f32 = 7.0;
/// Gap between the dot, the label, and the rollup in a host band
/// (mockup `.hosthdr { gap: 7px }`).
pub const HOST_BAND_SPACING: f32 = 7.0;
pub const HOST_ROLLUP_SIZE: f32 = 10.0;
/// A disconnected section's rows stay listed at this opacity (mockup
/// `.dim`). Applied to the row colors rather than to a layer: iced has no
/// container opacity, and scaling alpha composites identically over the
/// flat list fill.
pub const HOST_SECTION_DIM: f32 = 0.45;
pub const HOST_BANNER_TEXT_SIZE: f32 = 12.0;
pub const HOST_BANNER_ACTION_SIZE: f32 = 11.5;

/// Roost's blue: the Mac's `NSColor.controlAccentColor` under the default
/// accent (`App.swift:4772`, `:5207`), and what `chrome-accent = system`
/// means on Linux. The desktop accent is deliberately not followed there —
/// on COSMIC `@accent_bg_color` renders teal.
pub const DEFAULT_ACCENT: Color = Color::from_rgb8(0x00, 0x7a, 0xff);

/// `NSColor(white: 0.12)` as an 8-bit channel (0.12 × 255 = 30.6), the
/// white Swift blends the accent with for the selected row.
const ROW_BLEND_WHITE: u8 = 31;

/// The chrome's colors, one field per role (plan 073 D4).
///
/// View code hands it to the style fns below, whose closures capture only
/// the colors they draw with.
///
/// This is the seam for chrome themes and user color overrides, neither of
/// which ships yet: a `chrome-theme = <name>` would load a palette file
/// into this struct, and overrides would be a field map over it — a data
/// change, not a restyle of every view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromePalette {
    /// The one solid chrome band: sidebar header, sidebar footer, tab band.
    pub band: Color,
    /// The sidebar's scrollable project list, one step lighter than the bands.
    pub list: Color,
    /// Hairline between the sidebar and the terminal, drawn inside the
    /// sidebar's own width so the terminal grid keeps every pixel it is
    /// sized for.
    pub divider: Color,
    pub footer_chip: Color,
    pub footer_chip_hover: Color,
    pub footer_chip_pressed: Color,
    /// The selected sidebar row's pill, a primary button's hover and
    /// press, and a text field's selection.
    pub active_row: Color,
    pub active_tab: Color,
    pub hover: Color,
    pub active_agent: Color,
    pub text: Color,
    pub muted_text: Color,
    /// Sidebar project label: the mac reads "bolder" when active, but both
    /// platforms use the same regular 13pt weight — the difference is COLOR
    /// (`SidebarRowView.applyLabelColor`, App.swift:5333-5342: white when
    /// selected/emphasized, `NSColor(white: 0.82)` otherwise). Measured live
    /// for plan 026 C9 (mac pixel-sampled: active text 255,255,255; inactive
    /// peak 209,209,209 — 0.82 * 255 rounds to 209, i.e. `0xd1`).
    pub project_label_active: Color,
    pub project_label_inactive: Color,
    /// The chrome's one accent color: the notification dots (tab-pill badge +
    /// sidebar project-row dot), the dragged-pill border, the primary
    /// button, the focus ring, and the inline-rename focus ring + selection
    /// all share it.
    pub accent: Color,
    pub dragged_pill: Color,
    pub palette_surface: Color,
    pub palette_selection: Color,
    pub palette_hover: Color,
    pub palette_placeholder: Color,
    pub palette_match: Color,
    /// The host-section header's connection dot (plan 037 §3.1). Sampled
    /// from the approved mockup's `.hdot` rules: green connected, grey gone,
    /// amber in flight — the amber is the mockup's own busy-agent shade, so
    /// "something is happening" reads the same everywhere in the chrome.
    pub host_dot_connected: Color,
    pub host_dot_offline: Color,
    pub host_dot_pending: Color,
    /// The band's right-aligned rollup ("2 agents", "disconnected") — one
    /// step quieter than [`Self::muted_text`], as the mockup's
    /// `.hosthdr small`.
    pub host_rollup_text: Color,
    /// The band's `reduced fidelity` pill (plan 056 §3.4), in the chrome's
    /// one "this wants your attention" amber — literally
    /// [`Self::host_dot_pending`]'s hue, because it is the same warning and a
    /// second amber would only invite the two to drift apart.
    ///
    /// The dot beside it stays green on purpose: the connection is up and
    /// serving, which is exactly what the dot reports. The fidelity is a
    /// different fact and it gets its own slot rather than overloading a
    /// 7px circle.
    pub host_fidelity_text: Color,
    /// The inline "↻ Reconnect" row (mockup `.reconnect`).
    pub host_reconnect_text: Color,
    /// The session-ended banner over a host tab's last frame
    /// (plan 037 §3.1). Sampled from the approved mockup's `.banner` rules —
    /// a warm amber band that is deliberately nothing like the terminal
    /// palette underneath it, because the whole point is that it is not part
    /// of the frame it sits on.
    pub host_banner_bg: Color,
    pub host_banner_text: Color,
    pub host_banner_border: Color,
    pub host_banner_button_border: Color,
    /// The scrim over the frozen frame. A layer rather than
    /// [`HOST_SECTION_DIM`]'s alpha scaling: the terminal is drawn by a
    /// custom widget from an owned snapshot, so the only way to dim it
    /// without touching every cell color is to composite over it.
    pub host_frame_scrim: Color,
    pub error_text: Color,
    pub danger: Color,
    pub danger_accent: Color,
}

impl ChromePalette {
    /// Roost's dark chrome around `accent`.
    ///
    /// [`DEFAULT_ACCENT`] returns the hand-picked fills the chrome has always
    /// had, byte for byte, so Linux and the e2e seed draw exactly what they
    /// drew before the palette existed. Any other accent derives the four
    /// accent roles the way the Swift app does and leaves every other role
    /// alone. The accent is compared and blended as 8-bit sRGB, so an OS
    /// accent a float hair off the default still lands on it.
    pub fn roost_dark(accent: Color) -> Self {
        let pending = Color::from_rgb8(0xe6, 0xb2, 0x3a);
        let roost = Self {
            band: Color::from_rgb8(0x24, 0x29, 0x2c),
            list: Color::from_rgb8(0x2d, 0x32, 0x35),
            divider: Color::from_rgb8(0x1a, 0x1d, 0x1e),
            footer_chip: Color::from_rgb8(0x34, 0x39, 0x3c),
            footer_chip_hover: Color::from_rgb8(0x3e, 0x44, 0x47),
            footer_chip_pressed: Color::from_rgb8(0x2a, 0x2f, 0x32),
            active_row: Color::from_rgb8(0x13, 0x50, 0x9d),
            active_tab: Color::from_rgb8(0x24, 0x37, 0x51),
            hover: Color::from_rgb8(0x39, 0x39, 0x39),
            active_agent: Color::from_rgb8(0x3a, 0x3a, 0x3a),
            text: Color::from_rgb8(0xf2, 0xf2, 0xf2),
            muted_text: Color::from_rgb8(0xa0, 0xa4, 0xb0),
            project_label_active: Color::WHITE,
            project_label_inactive: Color::from_rgb8(0xd1, 0xd1, 0xd1),
            accent: DEFAULT_ACCENT,
            dragged_pill: Color::from_rgba8(0x55, 0x68, 0x7b, 0.65),
            palette_surface: Color::from_rgb8(0x2d, 0x2d, 0x33),
            palette_selection: Color::from_rgb8(0x48, 0x48, 0x4e),
            palette_hover: Color::from_rgb8(0x3d, 0x3d, 0x43),
            palette_placeholder: Color::from_rgb8(0x9e, 0x9e, 0x9e),
            palette_match: Color::from_rgb8(0x5f, 0xa3, 0xf0),
            host_dot_connected: Color::from_rgb8(0x3f, 0xca, 0x6b),
            host_dot_offline: Color::from_rgb8(0x6a, 0x70, 0x76),
            host_dot_pending: pending,
            host_rollup_text: Color::from_rgb8(0x6e, 0x74, 0x7a),
            host_fidelity_text: pending,
            host_reconnect_text: Color::from_rgb8(0x7f, 0xa8, 0xe8),
            host_banner_bg: Color::from_rgb8(0x4a, 0x33, 0x23),
            host_banner_text: Color::from_rgb8(0xee, 0xcf, 0xa2),
            host_banner_border: Color::from_rgb8(0x6b, 0x4a, 0x2a),
            host_banner_button_border: Color::from_rgb8(0x8a, 0x6a, 0x40),
            host_frame_scrim: Color::from_rgba8(0x14, 0x16, 0x18, 0.55),
            error_text: Color::from_rgb8(0xee, 0x78, 0x78),
            danger: Color::from_rgb8(0x8a, 0x2a, 0x2a),
            danger_accent: Color::from_rgb8(0xa8, 0x33, 0x33),
        };
        let channels = rgb8(accent);
        if channels == rgb8(DEFAULT_ACCENT) {
            return roost;
        }
        let accent = Color::from_rgb8(channels[0], channels[1], channels[2]);
        Self {
            accent,
            // Swift's active tab and row fill is the accent at α0.18
            // (`App.swift:5114`, `:5224`); composited over the band here
            // so the pill stays opaque.
            active_tab: mix(channels, rgb8(roost.band), 18, 100),
            // Swift's selected row (`App.swift:5741`).
            active_row: mix(channels, [ROW_BLEND_WHITE; 3], 1, 2),
            // `PalettePanel.swift:606`.
            palette_match: accent,
            ..roost
        }
    }
}

fn rgb8(color: Color) -> [u8; 3] {
    let [r, g, b, _] = color.into_rgba8();
    [r, g, b]
}

/// `top_parts / parts` of `top` over `under`, per 8-bit channel, rounding
/// half up.
fn mix(top: [u8; 3], under: [u8; 3], top_parts: u32, parts: u32) -> Color {
    let channel = |index: usize| {
        let sum = u32::from(top[index]) * top_parts
            + u32::from(under[index]) * (parts - top_parts)
            + parts / 2;
        (sum / parts) as u8
    };
    Color::from_rgb8(channel(0), channel(1), channel(2))
}

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

fn fixed(style: container::Style) -> impl Fn(&Theme) -> container::Style {
    move |_| style
}

/// A container that paints one fill and nothing else.
fn fill(color: Color) -> impl Fn(&Theme) -> container::Style {
    fixed(container::background(color))
}

pub fn band(chrome: &ChromePalette) -> impl Fn(&Theme) -> container::Style {
    fill(chrome.band)
}

pub fn list(chrome: &ChromePalette) -> impl Fn(&Theme) -> container::Style {
    fill(chrome.list)
}

pub fn divider(chrome: &ChromePalette) -> impl Fn(&Theme) -> container::Style {
    fill(chrome.divider)
}

/// Both strips share one drag affordance, so the dragged row and the dragged
/// tab stay visually identical; only the resting fill and radius differ.
fn pill(
    chrome: &ChromePalette,
    active_background: Color,
    radius: f32,
    active: bool,
    dragging: bool,
) -> container::Style {
    let mut style = container::Style::default();
    if dragging {
        style = style.background(chrome.dragged_pill);
    } else if active {
        style = style.background(active_background);
    }
    style.border = Border {
        color: if dragging {
            chrome.accent
        } else {
            Color::TRANSPARENT
        },
        width: if dragging { 1.0 } else { 0.0 },
        radius: radius.into(),
    };
    style
}

pub fn tab_pill(
    chrome: &ChromePalette,
    active: bool,
    dragging: bool,
) -> impl Fn(&Theme) -> container::Style {
    fixed(pill(chrome, chrome.active_tab, 6.0, active, dragging))
}

pub fn badge(chrome: &ChromePalette) -> impl Fn(&Theme) -> container::Style {
    fixed(
        container::background(chrome.accent)
            .border(Border::default().rounded(NOTIFICATION_DOT_SIZE / 2.0)),
    )
}

/// The Secure Keyboard Entry lock (plan 074 §D3) is two containers, an
/// outlined shackle over a solid body, rather than a glyph: Inter has no
/// lock, and a fallback emoji would draw in its own colors.
pub const LOCK_SHACKLE_SIZE: Size = Size::new(6.0, 5.0);
pub const LOCK_BODY_SIZE: Size = Size::new(10.0, 7.0);

pub fn lock_shackle(chrome: &ChromePalette) -> impl Fn(&Theme) -> container::Style {
    fixed(container::Style {
        border: Border {
            color: chrome.muted_text,
            width: 1.5,
            radius: iced::border::top(3.0),
        },
        ..container::Style::default()
    })
}

pub fn lock_body(chrome: &ChromePalette) -> impl Fn(&Theme) -> container::Style {
    fixed(container::background(chrome.muted_text).border(Border::default().rounded(1.5)))
}

pub fn project_pill(
    chrome: &ChromePalette,
    active: bool,
    dragging: bool,
) -> impl Fn(&Theme) -> container::Style {
    fixed(pill(chrome, chrome.active_row, 6.0, active, dragging))
}

pub fn agent_button(
    chrome: &ChromePalette,
    active: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    chrome_button(chrome, active.then_some(chrome.active_agent), 4.0)
}

pub fn transparent_button(
    chrome: &ChromePalette,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    chrome_button(chrome, None, 4.0)
}

/// The active tab pill's `×`. Deliberately NOT a `chrome_button`: a filled
/// `hover` rect on a 24px control inside a 24px pill recolors the pill's
/// whole trailing end, which is the hover Charlie called out (Q3). The
/// glyph reddens instead and the pill's own fill is left alone. `error_text`
/// is the chrome's text-weight red — the `danger` fills read near-black at
/// glyph weight.
pub fn close_button(chrome: &ChromePalette) -> impl Fn(&Theme, button::Status) -> button::Style {
    let (error_text, text, muted_text) = (chrome.error_text, chrome.text, chrome.muted_text);
    move |_, status| button::Style {
        background: None,
        text_color: match status {
            button::Status::Hovered | button::Status::Pressed => error_text,
            button::Status::Active => text,
            button::Status::Disabled => muted_text.scale_alpha(0.5),
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
pub fn footer_chip_button(
    chrome: &ChromePalette,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    let (footer_chip, footer_chip_hover, footer_chip_pressed) = (
        chrome.footer_chip,
        chrome.footer_chip_hover,
        chrome.footer_chip_pressed,
    );
    let (text, muted_text) = (chrome.text, chrome.muted_text);
    move |_, status| {
        let background = match status {
            button::Status::Hovered => footer_chip_hover,
            button::Status::Pressed => footer_chip_pressed,
            button::Status::Active => footer_chip,
            button::Status::Disabled => footer_chip.scale_alpha(0.5),
        };
        button::Style {
            background: Some(Background::Color(background)),
            text_color: match status {
                button::Status::Disabled => muted_text.scale_alpha(0.5),
                _ => text,
            },
            border: Border::default().rounded(6.0),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// A dialog's confirming action — Add Host's "Add & Connect" (plan 037
/// §3.1, the mock's `.btn.primary`).
///
/// The accent twin of [`danger_button`]: same geometry and the same
/// disabled treatment, so a dialog's button row reads as one control set
/// whichever of the two it ends with. `active_row` is the chrome's
/// existing pressed-blue, reused rather than inventing a shade.
pub fn primary_button(chrome: &ChromePalette) -> impl Fn(&Theme, button::Status) -> button::Style {
    let (accent, active_row) = (chrome.accent, chrome.active_row);
    let (text, muted_text) = (chrome.text, chrome.muted_text);
    move |_, status| {
        let (background, text_color) = match status {
            button::Status::Hovered | button::Status::Pressed => (active_row, text),
            button::Status::Active => (accent, text),
            button::Status::Disabled => (accent.scale_alpha(0.5), muted_text.scale_alpha(0.5)),
        };
        button::Style {
            background: Some(Background::Color(background)),
            text_color,
            border: Border::default().rounded(4),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// The gap between a modal button's edge and its focus ring, and the
/// ring's own width.
///
/// The ring is drawn by a wrapper *around* the button rather than on the
/// button's own border, because [`primary_button`]'s resting fill is the
/// accent itself — an accent border on that quad would be invisible
/// in exactly the state that matters most, a Tab landing on an
/// un-hovered "Add & Connect". Outside the fill it reads against the card
/// in all four button states, disabled included.
pub const FOCUS_RING_PADDING: u16 = 2;
pub const FOCUS_RING_WIDTH: f32 = 1.0;

/// The Tab-traversal ring around a modal button (plan 044 §3.4).
///
/// Transparent rather than absent when unfocused: the wrapper's padding
/// and border reserve their space either way, so moving the ring never
/// moves the buttons under the pointer.
pub fn focus_ring(chrome: &ChromePalette, focused: bool) -> impl Fn(&Theme) -> container::Style {
    fixed(container::Style {
        border: Border {
            color: if focused {
                chrome.accent
            } else {
                Color::TRANSPARENT
            },
            width: FOCUS_RING_WIDTH,
            radius: 6.0.into(),
        },
        ..container::Style::default()
    })
}

pub fn danger_button(chrome: &ChromePalette) -> impl Fn(&Theme, button::Status) -> button::Style {
    let (danger, danger_accent) = (chrome.danger, chrome.danger_accent);
    let (text, muted_text) = (chrome.text, chrome.muted_text);
    move |_, status| {
        let (background, text_color) = match status {
            button::Status::Hovered | button::Status::Pressed => (danger_accent, text),
            button::Status::Active => (danger, text),
            button::Status::Disabled => (danger.scale_alpha(0.5), muted_text.scale_alpha(0.5)),
        };
        button::Style {
            background: Some(Background::Color(background)),
            text_color,
            border: Border::default().rounded(4),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// The banner strip itself: a filled band with a hairline along its
/// bottom edge, so it reads as chrome laid over the frame rather than as
/// something the terminal drew.
pub fn host_banner(chrome: &ChromePalette) -> impl Fn(&Theme) -> container::Style {
    fill(chrome.host_banner_bg)
}

/// The hairline under the banner. A separate 1px container rather than a
/// border: iced borders draw on all four edges, and a box around the
/// full-width strip reads as a framed callout instead of a band.
pub fn host_banner_edge(chrome: &ChromePalette) -> impl Fn(&Theme) -> container::Style {
    fill(chrome.host_banner_border)
}

/// The one action a host tab's banner or status strip offers. Outlined
/// in the band's own border shade — a filled accent button here would
/// compete with the message for the eye, and the mockup's `.banner .btn`
/// does not.
pub fn host_banner_button(
    chrome: &ChromePalette,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    let (host_banner_button_border, host_banner_text) =
        (chrome.host_banner_button_border, chrome.host_banner_text);
    move |_, status| {
        let background = match status {
            button::Status::Hovered | button::Status::Pressed => Some(Background::Color(
                host_banner_button_border.scale_alpha(0.4),
            )),
            button::Status::Active | button::Status::Disabled => None,
        };
        button::Style {
            background,
            text_color: host_banner_text,
            border: Border {
                color: host_banner_button_border,
                width: 1.0,
                radius: 4.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// The dimming layer over a frame nothing will update again.
pub fn host_frame_scrim(chrome: &ChromePalette) -> impl Fn(&Theme) -> container::Style {
    fill(chrome.host_frame_scrim)
}

/// The rule between a context menu's groups.
pub fn menu_separator(chrome: &ChromePalette) -> impl Fn(&Theme) -> container::Style {
    fill(chrome.palette_selection)
}

pub fn palette_panel(chrome: &ChromePalette) -> impl Fn(&Theme) -> container::Style {
    fixed(container::Style {
        background: Some(Background::Color(chrome.palette_surface)),
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
    })
}

/// The bottom-right line's surface, framed in its text's own colour so a
/// receipt does not wear an error's red.
pub fn status_toast(
    chrome: &ChromePalette,
    line_color: Color,
) -> impl Fn(&Theme) -> container::Style {
    fixed(container::Style {
        background: Some(Background::Color(chrome.palette_surface)),
        border: Border {
            color: line_color.scale_alpha(0.55),
            width: 1.0,
            radius: 6.0.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba8(0, 0, 0, 0.45),
            offset: Vector::new(0.0, 5.0),
            blur_radius: 18.0,
        },
        ..container::Style::default()
    })
}

pub fn palette_input(
    chrome: &ChromePalette,
) -> impl Fn(&Theme, text_input::Status) -> text_input::Style {
    let style = text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        icon: chrome.palette_placeholder,
        placeholder: chrome.palette_placeholder,
        value: chrome.text,
        selection: chrome.active_row,
    };
    move |_, _| style
}

/// The mac reference field goes near-black while editing (a dark fill, not
/// the transparent one that let the selection pill's blue show through and
/// read as a blue field — W6). `divider` is the darkest existing chrome
/// neutral, so the field reads as its own surface rather than a new hex.
pub fn inline_rename_input(
    chrome: &ChromePalette,
) -> impl Fn(&Theme, text_input::Status) -> text_input::Style {
    let (divider, accent) = (chrome.divider, chrome.accent);
    let (text, muted_text) = (chrome.text, chrome.muted_text);
    move |_, status| {
        let focused = matches!(status, text_input::Status::Focused { .. });
        text_input::Style {
            background: Background::Color(divider),
            border: Border {
                color: if focused { accent } else { muted_text },
                width: 1.0,
                radius: 3.0.into(),
            },
            icon: muted_text,
            placeholder: muted_text,
            value: text,
            // Opaque, matching the mac field editor's rendered selection —
            // sampled live at RGB(0,122,255) with white glyphs (plan 029 F5);
            // a translucent tint over the dark field reads darker than mac.
            selection: accent,
        }
    }
}

pub fn palette_row(
    chrome: &ChromePalette,
    selected: bool,
    actionable: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    let (palette_selection, palette_hover) = (chrome.palette_selection, chrome.palette_hover);
    let (text, palette_placeholder) = (chrome.text, chrome.palette_placeholder);
    move |_, status| {
        let background = if selected {
            Some(palette_selection)
        } else {
            match status {
                button::Status::Hovered | button::Status::Pressed if actionable => {
                    Some(palette_hover)
                }
                _ => None,
            }
        };
        button::Style {
            background: background.map(Background::Color),
            text_color: if actionable {
                text
            } else {
                palette_placeholder.scale_alpha(0.6)
            },
            border: Border::default().rounded(6),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

fn chrome_button(
    chrome: &ChromePalette,
    selected: Option<Color>,
    radius: f32,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    let (hover, active_tab) = (chrome.hover, chrome.active_tab);
    let (text, muted_text) = (chrome.text, chrome.muted_text);
    move |_, status| {
        let background = match status {
            button::Status::Hovered => Some(selected.unwrap_or(hover)),
            button::Status::Pressed => Some(selected.unwrap_or(active_tab)),
            button::Status::Active => selected,
            button::Status::Disabled => selected.map(|color| color.scale_alpha(0.5)),
        };
        button::Style {
            background: background.map(Background::Color),
            text_color: match status {
                button::Status::Disabled => muted_text.scale_alpha(0.5),
                _ => text,
            },
            border: Border::default().rounded(radius),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// One agent's row on the consent card (plan 064 §3.5).
///
/// Highlighted when the agent is installed here: the found rows are what
/// the card's default answer is about, and the rest are there to be
/// switched on deliberately.
pub fn agent_hooks_row(chrome: &ChromePalette, found: bool) -> impl Fn(&Theme) -> container::Style {
    fixed(container::Style {
        background: found.then_some(Background::Color(chrome.active_tab)),
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roost() -> ChromePalette {
        ChromePalette::roost_dark(DEFAULT_ACCENT)
    }

    /// Roughly macOS's purple accent under darkAqua; its green channel
    /// lands the row blend on an exact half (55.5), so rounding is pinned.
    const PURPLE: Color = Color::from_rgb8(0xa5, 0x50, 0xa7);

    /// The colors the chrome drew before it had a palette, written out
    /// rather than read back from `roost_dark`, so a changed value cannot
    /// pass by comparing a palette with itself.
    #[test]
    fn the_default_accent_is_every_pre_palette_color_byte_for_byte() {
        let expected = ChromePalette {
            band: Color::from_rgb8(0x24, 0x29, 0x2c),
            list: Color::from_rgb8(0x2d, 0x32, 0x35),
            divider: Color::from_rgb8(0x1a, 0x1d, 0x1e),
            footer_chip: Color::from_rgb8(0x34, 0x39, 0x3c),
            footer_chip_hover: Color::from_rgb8(0x3e, 0x44, 0x47),
            footer_chip_pressed: Color::from_rgb8(0x2a, 0x2f, 0x32),
            active_row: Color::from_rgb8(0x13, 0x50, 0x9d),
            active_tab: Color::from_rgb8(0x24, 0x37, 0x51),
            hover: Color::from_rgb8(0x39, 0x39, 0x39),
            active_agent: Color::from_rgb8(0x3a, 0x3a, 0x3a),
            text: Color::from_rgb8(0xf2, 0xf2, 0xf2),
            muted_text: Color::from_rgb8(0xa0, 0xa4, 0xb0),
            project_label_active: Color::from_rgb8(0xff, 0xff, 0xff),
            project_label_inactive: Color::from_rgb8(0xd1, 0xd1, 0xd1),
            accent: Color::from_rgb8(0x00, 0x7a, 0xff),
            dragged_pill: Color::from_rgba8(0x55, 0x68, 0x7b, 0.65),
            palette_surface: Color::from_rgb8(0x2d, 0x2d, 0x33),
            palette_selection: Color::from_rgb8(0x48, 0x48, 0x4e),
            palette_hover: Color::from_rgb8(0x3d, 0x3d, 0x43),
            palette_placeholder: Color::from_rgb8(0x9e, 0x9e, 0x9e),
            palette_match: Color::from_rgb8(0x5f, 0xa3, 0xf0),
            host_dot_connected: Color::from_rgb8(0x3f, 0xca, 0x6b),
            host_dot_offline: Color::from_rgb8(0x6a, 0x70, 0x76),
            host_dot_pending: Color::from_rgb8(0xe6, 0xb2, 0x3a),
            host_rollup_text: Color::from_rgb8(0x6e, 0x74, 0x7a),
            host_fidelity_text: Color::from_rgb8(0xe6, 0xb2, 0x3a),
            host_reconnect_text: Color::from_rgb8(0x7f, 0xa8, 0xe8),
            host_banner_bg: Color::from_rgb8(0x4a, 0x33, 0x23),
            host_banner_text: Color::from_rgb8(0xee, 0xcf, 0xa2),
            host_banner_border: Color::from_rgb8(0x6b, 0x4a, 0x2a),
            host_banner_button_border: Color::from_rgb8(0x8a, 0x6a, 0x40),
            host_frame_scrim: Color::from_rgba8(0x14, 0x16, 0x18, 0.55),
            error_text: Color::from_rgb8(0xee, 0x78, 0x78),
            danger: Color::from_rgb8(0x8a, 0x2a, 0x2a),
            danger_accent: Color::from_rgb8(0xa8, 0x33, 0x33),
        };
        assert_eq!(ChromePalette::roost_dark(DEFAULT_ACCENT), expected);
        assert_eq!(
            ChromePalette::roost_dark(Color::from_rgb(0.0, 0.478, 1.0)),
            expected,
            "an OS float accent that rounds to #007aff takes the default branch"
        );
    }

    #[test]
    fn another_accent_derives_the_four_accent_roles_and_nothing_else() {
        let derived = ChromePalette::roost_dark(PURPLE);
        assert_eq!(derived.accent, PURPLE);
        assert_eq!(derived.palette_match, PURPLE);
        assert_eq!(
            derived.active_tab,
            Color::from_rgb8(59, 48, 66),
            "the accent at α0.18 over the band, composited opaque"
        );
        assert_eq!(
            derived.active_row,
            Color::from_rgb8(98, 56, 99),
            "the accent blended 0.5 with white 0.12 (31), half up"
        );
        let default = roost();
        assert_eq!(
            ChromePalette {
                accent: default.accent,
                palette_match: default.palette_match,
                active_tab: default.active_tab,
                active_row: default.active_row,
                ..derived
            },
            default,
            "every other role is untouched"
        );
    }

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
        let chrome = roost();
        let theme = Theme::Dark;
        let active = project_pill(&chrome, true, false)(&theme);
        assert_eq!(
            active.background,
            Some(Background::Color(chrome.active_row))
        );
        assert_eq!(
            project_pill(&chrome, true, true)(&theme).border.color,
            chrome.accent,
            "the dragged project row is outlined like the dragged tab pill"
        );
        assert_eq!(
            tab_pill(&chrome, true, false)(&theme).background,
            Some(Background::Color(chrome.active_tab))
        );
    }

    #[test]
    fn chrome_regions_split_into_one_band_color_and_one_list_color() {
        let chrome = roost();
        let theme = Theme::Dark;
        assert_eq!(
            band(&chrome)(&theme).background,
            Some(Background::Color(chrome.band))
        );
        assert_eq!(
            list(&chrome)(&theme).background,
            Some(Background::Color(chrome.list))
        );
        assert_eq!(
            divider(&chrome)(&theme).background,
            Some(Background::Color(chrome.divider))
        );
        assert_ne!(
            chrome.band, chrome.list,
            "the list region reads lighter than the bands"
        );
        assert_eq!(DIVIDER_WIDTH, 1.0);
    }

    #[test]
    fn footer_chip_rests_on_a_filled_bezel_and_dims_when_disabled() {
        let chrome = roost();
        let theme = Theme::Dark;
        let style = footer_chip_button(&chrome);
        assert_eq!(
            style(&theme, button::Status::Active).background,
            Some(Background::Color(chrome.footer_chip))
        );
        assert_eq!(
            style(&theme, button::Status::Hovered).background,
            Some(Background::Color(chrome.footer_chip_hover))
        );
        assert_eq!(
            style(&theme, button::Status::Pressed).background,
            Some(Background::Color(chrome.footer_chip_pressed))
        );
        assert_eq!(
            style(&theme, button::Status::Disabled).background,
            Some(Background::Color(chrome.footer_chip.scale_alpha(0.5)))
        );
        assert_eq!(
            style(&theme, button::Status::Active).text_color,
            chrome.text
        );
    }

    #[test]
    fn inactive_controls_are_transparent_until_hovered() {
        let chrome = roost();
        let theme = Theme::Dark;
        let style = transparent_button(&chrome);
        assert_eq!(style(&theme, button::Status::Active).background, None);
        assert_eq!(
            style(&theme, button::Status::Hovered).background,
            Some(Background::Color(chrome.hover))
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
        let chrome = roost();
        let theme = Theme::Dark;
        let close = close_button(&chrome);
        for status in [button::Status::Hovered, button::Status::Pressed] {
            let style = close(&theme, status);
            assert_eq!(
                style.background, None,
                "no pill-recoloring fill in any state"
            );
            assert_eq!(style.text_color, chrome.error_text);
        }
        let resting = close(&theme, button::Status::Active);
        assert_eq!(resting.background, None);
        assert_eq!(resting.text_color, chrome.text);
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
        let chrome = roost();
        let theme = Theme::Dark;
        assert_eq!(
            palette_panel(&chrome)(&theme).background,
            Some(Background::Color(chrome.palette_surface))
        );
        assert_eq!(
            palette_row(&chrome, true, true)(&theme, button::Status::Active).background,
            Some(Background::Color(chrome.palette_selection))
        );
        assert_eq!(
            palette_row(&chrome, false, true)(&theme, button::Status::Active).background,
            None
        );
        assert_eq!(
            palette_input(&chrome)(&theme, text_input::Status::Focused { is_hovered: false })
                .border
                .width,
            0.0
        );
        let disabled = palette_row(&chrome, false, false)(&theme, button::Status::Hovered);
        assert_eq!(disabled.background, None);
        assert_eq!(
            disabled.text_color,
            chrome.palette_placeholder.scale_alpha(0.6)
        );
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

    #[test]
    fn danger_button_always_paints_a_destructive_fill() {
        let chrome = roost();
        let theme = Theme::Dark;
        let style = danger_button(&chrome);
        assert_eq!(
            style(&theme, button::Status::Active).background,
            Some(Background::Color(chrome.danger))
        );
        assert_eq!(
            style(&theme, button::Status::Hovered).background,
            Some(Background::Color(chrome.danger_accent))
        );
        assert_eq!(
            style(&theme, button::Status::Active).text_color,
            chrome.text
        );
    }

    #[test]
    fn status_toast_is_a_neutral_surface_framed_in_its_lines_colour() {
        let chrome = roost();
        let style = status_toast(&chrome, chrome.error_text)(&Theme::Dark);
        assert_eq!(
            style.background,
            Some(Background::Color(chrome.palette_surface))
        );
        assert_eq!(style.border.color, chrome.error_text.scale_alpha(0.55));
        assert_eq!(style.border.width, 1.0);
        assert_eq!(
            status_toast(&chrome, chrome.text)(&Theme::Dark)
                .border
                .color,
            chrome.text.scale_alpha(0.55)
        );
    }

    /// Under an accent that is not the default, so a style fn reading any
    /// other role (or a stray literal blue) fails here.
    #[test]
    fn the_accent_wires_every_surface_that_shares_it() {
        let chrome = ChromePalette::roost_dark(PURPLE);
        let theme = Theme::Dark;
        assert_eq!(
            badge(&chrome)(&theme).background,
            Some(Background::Color(PURPLE)),
            "both notification dots render the accent"
        );
        assert_eq!(
            pill(&chrome, chrome.active_tab, 6.0, false, true)
                .border
                .color,
            PURPLE,
            "the dragged-pill border renders the accent"
        );

        let focused =
            inline_rename_input(&chrome)(&theme, text_input::Status::Focused { is_hovered: false });
        assert_eq!(focused.border.color, PURPLE);
        assert_eq!(focused.selection, PURPLE);
    }

    /// The ring has to be *seen* in every button state, which is why it
    /// is drawn outside the fill: the primary's resting background is
    /// the accent itself, so an accent border on the button would vanish
    /// there. What it must contrast with instead is the card behind it.
    #[test]
    fn the_focus_ring_reads_against_the_card_in_every_button_state() {
        let chrome = roost();
        let theme = Theme::Dark;
        assert_eq!(
            focus_ring(&chrome, true)(&theme).border.color,
            chrome.accent
        );
        assert_eq!(
            focus_ring(&chrome, true)(&theme).border.width,
            FOCUS_RING_WIDTH
        );
        assert_eq!(
            focus_ring(&chrome, false)(&theme).border.color,
            Color::TRANSPARENT,
            "an unfocused button reserves the ring's space and draws nothing"
        );
        assert_eq!(
            focus_ring(&chrome, false)(&theme).border.width,
            FOCUS_RING_WIDTH,
            "the same width either way, so the ring never moves the row"
        );
        assert_eq!(
            focus_ring(&chrome, true)(&theme).background,
            None,
            "the ring is an outline; the button keeps its own fill"
        );
        assert_ne!(
            Some(Background::Color(chrome.accent)),
            palette_panel(&chrome)(&theme).background,
            "the ring is drawn on the modal card, so it must not be the card's color"
        );

        // The states the ring is drawn *beside*. The primary's Active fill
        // is the accent, which is the whole reason the ring is not a
        // border on the button.
        assert_eq!(
            primary_button(&chrome)(&theme, button::Status::Active).background,
            Some(Background::Color(chrome.accent))
        );
    }

    #[test]
    fn rename_editor_uses_a_dark_field_not_the_pill_background() {
        let chrome = roost();
        let theme = Theme::Dark;
        let style = inline_rename_input(&chrome);
        let focused = style(&theme, text_input::Status::Focused { is_hovered: false });
        assert_eq!(
            focused.background,
            Background::Color(chrome.divider),
            "field reads as its own dark surface, not the transparent pill blue"
        );
        let unfocused = style(&theme, text_input::Status::Active);
        assert_eq!(
            unfocused.background,
            Background::Color(chrome.divider),
            "background stays dark whether or not the field is focused"
        );
        assert_eq!(
            unfocused.border.color, chrome.muted_text,
            "only the border, not the background, reacts to focus"
        );
    }
}
