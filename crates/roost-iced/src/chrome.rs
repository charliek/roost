use iced::widget::{button, container, scrollable, text_input};
use iced::{Background, Border, Color, Font, Shadow, Theme, Vector};

/// One application-owned band height keeps the sidebar header and tab strip
/// on the same seam. Native window decorations remain outside this geometry.
pub const BAND_HEIGHT: f32 = 34.0;
pub const ROW_HEIGHT: f32 = 28.0;
pub const PILL_HEIGHT: f32 = 24.0;
pub const PROJECT_STRIPE_WIDTH: f32 = 3.0;
pub const PROJECT_STRIPE_GAP: f32 = 11.0;
pub const PROJECT_LABEL_INSET: f32 = 10.0;
pub const PROJECT_RIGHT_INSET: f32 = 8.0;
pub const AGENT_DOT_INSET: f32 = 25.0;
pub const TAB_STATUS_SIZE: f32 = 7.0;
pub const NOTIFICATION_DOT_SIZE: f32 = 8.0;
pub const PALETTE_WIDTH: f32 = 660.0;
pub const PALETTE_MAX_HEIGHT: f32 = 500.0;

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
pub const NOTIFICATION: Color = Color::from_rgb8(0x4e, 0x9a, 0xf1);
pub const DRAGGED_PILL: Color = Color::from_rgba8(0x55, 0x68, 0x7b, 0.65);
pub const PALETTE_SURFACE: Color = Color::from_rgb8(0x2d, 0x2d, 0x33);
pub const PALETTE_SELECTION: Color = Color::from_rgb8(0x48, 0x48, 0x4e);
pub const PALETTE_HOVER: Color = Color::from_rgb8(0x3d, 0x3d, 0x43);
pub const PALETTE_PLACEHOLDER: Color = Color::from_rgb8(0x9e, 0x9e, 0x9e);
pub const PALETTE_MATCH: Color = Color::from_rgb8(0x5f, 0xa3, 0xf0);
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
        color: if dragging {
            NOTIFICATION
        } else {
            Color::TRANSPARENT
        },
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
        .background(NOTIFICATION)
        .border(Border::default().rounded(NOTIFICATION_DOT_SIZE / 2.0))
}

pub fn project_pill(active: bool, dragging: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| pill(ACTIVE_BLUE, 5.0, active, dragging)
}

pub fn agent_button(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| chrome_button(active.then_some(ACTIVE_AGENT), status, 4.0)
}

pub fn transparent_button(_: &Theme, status: button::Status) -> button::Style {
    chrome_button(None, status, 4.0)
}

/// The sidebar-footer "+ New Project" chip: a centered rounded button with
/// a resting fill, matching the shipped Mac bezel and GTK chip affordances
/// rather than the flat text buttons used elsewhere in the chrome.
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

pub fn palette_divider(_: &Theme) -> container::Style {
    container::Style::default().background(Color::from_rgba8(0xff, 0xff, 0xff, 0.10))
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

pub fn inline_rename_input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border {
            color: if focused { NOTIFICATION } else { MUTED_TEXT },
            width: 1.0,
            radius: 3.0.into(),
        },
        icon: MUTED_TEXT,
        placeholder: MUTED_TEXT,
        value: TEXT,
        selection: NOTIFICATION.scale_alpha(0.65),
    }
}

/// Overlay-style scrollbar: no rail fill, just a translucent scroller. The
/// stock iced style always paints a full-length rail, which reads as a solid
/// band wherever it overlays short content — used by the palette list below.
/// The tab strip needs zero chrome instead (its own `Scrollbar::hidden()` in
/// `app.rs`, #281): even this style's translucent scroller was too visible
/// overlaying the 24px tab pills.
pub fn overlay_scrollable(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: Background::Color(PALETTE_PLACEHOLDER.scale_alpha(0.35)),
            border: Border::default().rounded(2),
        },
    };
    style.vertical_rail = rail;
    style.horizontal_rail = rail;
    style
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

    #[test]
    fn active_rows_and_pills_use_roost_selection_colors() {
        let theme = Theme::Dark;
        let active = project_pill(true, false)(&theme);
        assert_eq!(active.background, Some(Background::Color(ACTIVE_BLUE)));
        assert_eq!(
            project_pill(true, true)(&theme).border.color,
            NOTIFICATION,
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
        let project_text = PROJECT_STRIPE_WIDTH + PROJECT_STRIPE_GAP + PROJECT_LABEL_INSET;
        assert!((project_text - AGENT_DOT_INSET).abs() <= 1.0);
        assert_eq!(TAB_STATUS_SIZE, 7.0);
        assert_eq!(NOTIFICATION_DOT_SIZE, 8.0);
    }

    #[test]
    fn overlay_scrollable_never_fills_a_rail() {
        let theme = Theme::Dark;
        let style = overlay_scrollable(
            &theme,
            scrollable::Status::Active {
                is_horizontal_scrollbar_disabled: false,
                is_vertical_scrollbar_disabled: false,
            },
        );
        assert_eq!(style.horizontal_rail.background, None);
        assert_eq!(style.vertical_rail.background, None);
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
}
