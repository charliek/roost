use iced::widget::{button, container, scrollable, text_input};
use iced::{Background, Border, Color, Shadow, Theme, Vector};

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

pub const SURFACE: Color = Color::from_rgb8(0x28, 0x28, 0x28);
pub const SURFACE_DARK: Color = Color::from_rgb8(0x21, 0x21, 0x21);
pub const ACTIVE_BLUE: Color = Color::from_rgb8(0x13, 0x50, 0x9d);
pub const ACTIVE_TAB: Color = Color::from_rgb8(0x24, 0x37, 0x51);
pub const HOVER: Color = Color::from_rgb8(0x39, 0x39, 0x39);
pub const ACTIVE_AGENT: Color = Color::from_rgb8(0x3a, 0x3a, 0x3a);
pub const TEXT: Color = Color::from_rgb8(0xf2, 0xf2, 0xf2);
pub const MUTED_TEXT: Color = Color::from_rgb8(0xa0, 0xa4, 0xb0);
pub const NOTIFICATION: Color = Color::from_rgb8(0x4e, 0x9a, 0xf1);
pub const PALETTE_SURFACE: Color = Color::from_rgb8(0x2d, 0x2d, 0x33);
pub const PALETTE_SELECTION: Color = Color::from_rgb8(0x48, 0x48, 0x4e);
pub const PALETTE_HOVER: Color = Color::from_rgb8(0x3d, 0x3d, 0x43);
pub const PALETTE_PLACEHOLDER: Color = Color::from_rgb8(0x9e, 0x9e, 0x9e);
pub const PALETTE_MATCH: Color = Color::from_rgb8(0x5f, 0xa3, 0xf0);
pub const ERROR_TEXT: Color = Color::from_rgb8(0xee, 0x78, 0x78);

pub fn surface(_: &Theme) -> container::Style {
    container::Style::default().background(SURFACE)
}

pub fn dark_surface(_: &Theme) -> container::Style {
    container::Style::default().background(SURFACE_DARK)
}

pub fn tab_pill(active: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        let mut style = container::Style::default();
        if active {
            style = style.background(ACTIVE_TAB);
        }
        style.border = Border::default().rounded(6);
        style
    }
}

pub fn badge(_: &Theme) -> container::Style {
    container::Style::default()
        .background(NOTIFICATION)
        .border(Border::default().rounded(NOTIFICATION_DOT_SIZE / 2.0))
}

pub fn project_pill(active: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        let mut style = container::Style::default();
        if active {
            style = style.background(ACTIVE_BLUE);
        }
        style.border = Border::default().rounded(5);
        style
    }
}

pub fn agent_button(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| chrome_button(active.then_some(ACTIVE_AGENT), status, 4.0)
}

pub fn transparent_button(_: &Theme, status: button::Status) -> button::Style {
    chrome_button(None, status, 4.0)
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

pub fn palette_scrollable(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
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
        let active = project_pill(true)(&theme);
        assert_eq!(active.background, Some(Background::Color(ACTIVE_BLUE)));
        assert_eq!(
            tab_pill(true)(&theme).background,
            Some(Background::Color(ACTIVE_TAB))
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
    fn status_toast_is_a_neutral_surface_with_an_error_accent() {
        let style = status_toast(&Theme::Dark);
        assert_eq!(style.background, Some(Background::Color(PALETTE_SURFACE)));
        assert_eq!(style.border.color, ERROR_TEXT.scale_alpha(0.55));
        assert_eq!(style.border.width, 1.0);
    }
}
