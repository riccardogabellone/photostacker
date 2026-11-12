//! Colours, fonts and spacing. Every colour the UI draws comes from a [`Palette`]; views
//! ask for the active one with [`palette`] instead of naming colours themselves. The
//! photos are the only colourful thing on screen: the neutrals are warm greys and the one
//! accent is the logo's amber.
//!
//! Copied from `genetic_algo`'s `ui/src/theme.rs` and adapted. The colour roles, the way
//! they map onto egui's visuals and the text-rendering adjustments follow fastframe
//! (https://fastframe.dev, MIT): fastframe-theme for the roles and the derivation recipe,
//! ZapFast's `theme.rs` for the mapping, fastframe-text for the text rendering. Their
//! licence (MIT, Copyright (c) 2026 Carmine Paolino) is in `THIRD-PARTY-NOTICES.md`,
//! section "fastframe and ZapFast". See `crates/ui/DESIGN.md`.

use std::collections::BTreeMap;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, TextStyle, Theme, ThemePreference, Visuals,
    epaint::{FontColorTransferFunction, text::VariationCoords},
    style::{Selection, TextCursorStyle, WidgetVisuals, Widgets},
};

/// Inter with fixed-width digits; see `assets/fonts/README.md`.
const INTER: &[u8] = include_bytes!("../../../assets/fonts/InterVariable.ttf");
const REGULAR: f32 = 400.0;
const SEMIBOLD: f32 = 600.0;
/// The font family for headings and values.
pub const SEMIBOLD_FAMILY: &str = "inter-semibold";

const DARK_BACKGROUND: Color32 = Color32::from_rgb(0x11, 0x13, 0x18);
const DARK_TEXT: Color32 = Color32::from_rgb(0xE9, 0xE6, 0xDF);
const LIGHT_BACKGROUND: Color32 = Color32::from_rgb(0xFA, 0xF8, 0xF4);
const LIGHT_TEXT: Color32 = Color32::from_rgb(0x1E, 0x1C, 0x18);

/// Every colour role the UI uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Behind the stacks: the darkest (or lightest) surface.
    pub window: Color32,
    /// Side panels, the toolbar and the status bar.
    pub panel: Color32,
    /// Cards and resting widgets.
    pub surface: Color32,
    /// A widget under the pointer.
    pub surface_hover: Color32,
    /// A widget being pressed.
    pub surface_active: Color32,
    /// Borders of frames and cards.
    pub outline: Color32,
    pub text: Color32,
    /// Labels and descriptions next to values.
    pub secondary: Color32,
    /// Hints and disabled text.
    pub dim: Color32,
    /// The one main action, the selection, focus.
    pub accent: Color32,
    pub accent_hover: Color32,
    /// Text and icons drawn on the accent.
    pub on_accent: Color32,
    /// Errors and failed stacks.
    pub danger: Color32,
    /// Warnings and skipped stacks.
    pub warning: Color32,
    /// Confirmed and written stacks.
    pub positive: Color32,
}

/// `from` blended towards `to` by `amount` (0 to 1), per channel.
pub(crate) const fn mix(from: Color32, to: Color32, amount: f32) -> Color32 {
    const fn channel(from: u8, to: u8, amount: f32) -> u8 {
        (from as f32 + (to as f32 - from as f32) * amount + 0.5) as u8
    }
    Color32::from_rgb(
        channel(from.r(), to.r(), amount),
        channel(from.g(), to.g(), amount),
        channel(from.b(), to.b(), amount),
    )
}

impl Palette {
    /// The fastframe-theme recipe: every neutral is the background blended towards the
    /// text colour, so a palette needs only a background, a text colour and an accent.
    /// The status colours are each theme's own.
    const fn derive(background: Color32, text: Color32, accent: Color32) -> Self {
        Self {
            window: background,
            panel: mix(background, text, 0.03),
            surface: mix(background, text, 0.08),
            surface_hover: mix(background, text, 0.12),
            surface_active: mix(background, text, 0.18),
            outline: mix(background, text, 0.20),
            text,
            secondary: mix(background, text, 0.70),
            dim: mix(background, text, 0.50),
            accent,
            accent_hover: mix(accent, text, 0.15),
            on_accent: background,
            danger: Color32::PLACEHOLDER,
            warning: Color32::PLACEHOLDER,
            positive: Color32::PLACEHOLDER,
        }
    }

    /// A warm near-black, with the logo's bright amber as the accent.
    pub const DARK: Self = Self {
        danger: Color32::from_rgb(0xFF, 0x6B, 0x6B),
        warning: Color32::from_rgb(0xFF, 0xD8, 0x4D),
        positive: Color32::from_rgb(0x5B, 0xD0, 0x8A),
        ..Self::derive(
            DARK_BACKGROUND,
            DARK_TEXT,
            Color32::from_rgb(0xF5, 0xA5, 0x24),
        )
    };

    /// A warm near-white, with the amber darkened where it would wash out.
    pub const LIGHT: Self = Self {
        // The recipe's 70% falls short of 4.5:1 on pressed widgets in the light theme.
        secondary: mix(LIGHT_BACKGROUND, LIGHT_TEXT, 0.76),
        danger: Color32::from_rgb(0xC8, 0x34, 0x34),
        warning: Color32::from_rgb(0x8A, 0x6A, 0x00),
        positive: Color32::from_rgb(0x1E, 0x7F, 0x4A),
        ..Self::derive(
            LIGHT_BACKGROUND,
            LIGHT_TEXT,
            Color32::from_rgb(0xA8, 0x5B, 0x00),
        )
    };
}

/// The palette of the theme the window is showing now.
pub fn palette(context: &egui::Context) -> &'static Palette {
    match context.theme() {
        Theme::Dark => &Palette::DARK,
        Theme::Light => &Palette::LIGHT,
    }
}

/// Fonts, spacing and both themes, following the system's. Call once, before the first
/// frame.
pub fn install(context: &egui::Context) {
    context.set_fonts(fonts());
    // After `set_fonts`, which would otherwise replace it.
    egui_material_icons::initialize(context);
    for (theme, palette) in [
        (Theme::Dark, &Palette::DARK),
        (Theme::Light, &Palette::LIGHT),
    ] {
        context.style_mut_of(theme, |style| {
            style.visuals = visuals(palette, theme);
            style.text_styles = text_styles();
            let spacing = &mut style.spacing;
            spacing.item_spacing = egui::vec2(8.0, 6.0);
            spacing.button_padding = egui::vec2(10.0, 4.0);
            spacing.interact_size = egui::vec2(40.0, 24.0);
            spacing.window_margin = Margin::same(12);
            spacing.menu_margin = Margin::same(8);
        });
    }
    context.set_theme(ThemePreference::System);
}

fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let mut weighted = |name: &str, weight: f32| {
        let mut data = FontData::from_static(INTER);
        data.tweak.coords = VariationCoords::new([(b"wght", weight)]);
        fonts.font_data.insert(name.to_owned(), data.into());
    };
    weighted("inter", REGULAR);
    weighted(SEMIBOLD_FAMILY, SEMIBOLD);

    // egui's own fonts stay behind Inter as fallbacks (symbols, emoji). The system's fonts
    // come after every font named here: eframe's `system_fonts` appends one to a family
    // only for a character none of these has.
    let fallbacks = fonts.families[&FontFamily::Proportional].clone();
    fonts
        .families
        .get_mut(&FontFamily::Proportional)
        .expect("egui defines a proportional family")
        .insert(0, "inter".to_owned());
    fonts.families.insert(
        FontFamily::Name(SEMIBOLD_FAMILY.into()),
        std::iter::once(SEMIBOLD_FAMILY.to_owned())
            .chain(fallbacks)
            .collect(),
    );
    fonts
}

fn text_styles() -> BTreeMap<TextStyle, FontId> {
    let semibold = FontFamily::Name(SEMIBOLD_FAMILY.into());
    BTreeMap::from([
        (TextStyle::Small, FontId::proportional(11.5)),
        (TextStyle::Body, FontId::proportional(13.5)),
        (TextStyle::Button, FontId::proportional(13.5)),
        (TextStyle::Monospace, FontId::monospace(12.5)),
        (TextStyle::Heading, FontId::new(15.0, semibold)),
    ])
}

fn visuals(palette: &Palette, theme: Theme) -> Visuals {
    let mut visuals = match theme {
        Theme::Dark => Visuals::dark(),
        Theme::Light => Visuals::light(),
    };
    let widget = |fill: Color32, stroke: Stroke| WidgetVisuals {
        bg_fill: fill,
        weak_bg_fill: fill,
        bg_stroke: stroke,
        corner_radius: CornerRadius::same(6),
        fg_stroke: Stroke::new(1.0, palette.text),
        expansion: 0.0,
    };
    // Resting and hovered widgets differ by tint, not by a border.
    visuals.widgets = Widgets {
        noninteractive: widget(palette.panel, Stroke::new(1.0, palette.outline)),
        inactive: widget(palette.surface, Stroke::NONE),
        hovered: widget(palette.surface_hover, Stroke::NONE),
        active: widget(palette.surface_active, Stroke::new(1.0, palette.accent)),
        open: widget(palette.surface_hover, Stroke::NONE),
    };
    visuals.selection = Selection {
        bg_fill: palette.accent.gamma_multiply(0.35),
        stroke: Stroke::new(1.0, palette.accent),
    };
    visuals.weak_text_color = Some(palette.secondary);
    visuals.hyperlink_color = palette.accent;
    visuals.panel_fill = palette.panel;
    visuals.window_fill = palette.surface;
    visuals.window_stroke = Stroke::new(1.0, palette.outline);
    visuals.window_corner_radius = CornerRadius::same(8);
    visuals.menu_corner_radius = CornerRadius::same(8);
    let shadow = Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(if theme == Theme::Dark { 110 } else { 40 }),
    };
    visuals.window_shadow = shadow;
    visuals.popup_shadow = shadow;
    visuals.extreme_bg_color = palette.window;
    visuals.faint_bg_color = palette.surface;
    visuals.code_bg_color = palette.surface;
    visuals.warn_fg_color = palette.warning;
    visuals.error_fg_color = palette.danger;
    visuals.text_cursor = TextCursorStyle {
        stroke: Stroke::new(2.0, palette.accent),
        ..visuals.text_cursor
    };
    visuals.slider_trailing_fill = true;
    adjust_text_rendering(&mut visuals);
    visuals
}

/// Makes the boxes of checkboxes and the circles of radio buttons in `ui` show on a panel
/// or a card, where a resting widget's `surface` hardly differs from the ground: an
/// unchecked one rests on `surface_active` inside a 1 px `secondary` outline (at least
/// 3:1 on the panel, the cards and its own fill, tested; `dim` does not reach it), with a
/// `text` outline under the pointer. Only for a scope that holds such choices: a button in
/// it would take the outline too.
pub fn show_choices(ui: &mut egui::Ui) {
    let palette = palette(ui.ctx());
    let widgets = &mut ui.visuals_mut().widgets;
    widgets.inactive.bg_fill = palette.surface_active;
    widgets.inactive.bg_stroke = Stroke::new(1.0, palette.secondary);
    widgets.hovered.bg_stroke = Stroke::new(1.0, palette.text);
}

/// Matches the desktop's text rendering, after fastframe-text, which measured egui against
/// each platform's native text. On Linux, egui's dark-theme coverage curve draws text
/// 11-18% heavier than GTK, while linear coverage comes within 2%. macOS renders unhinted.
/// Windows matches egui's defaults.
fn adjust_text_rendering(visuals: &mut Visuals) {
    if cfg!(target_os = "linux") {
        visuals.text_options.color_transfer_function = FontColorTransferFunction::Off;
    }
    if cfg!(target_os = "macos") {
        visuals.text_options.font_hinting = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2 contrast ratio between two opaque colours.
    fn contrast(a: Color32, b: Color32) -> f32 {
        let luminance = |color: Color32| {
            let linear = |channel: u8| {
                let value = f32::from(channel) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
        };
        let (light, dark) = {
            let (x, y) = (luminance(a), luminance(b));
            (x.max(y), x.min(y))
        };
        (light + 0.05) / (dark + 0.05)
    }

    #[test]
    fn text_is_readable_on_every_surface_in_both_themes() {
        for palette in [&Palette::DARK, &Palette::LIGHT] {
            for surface in [
                palette.window,
                palette.panel,
                palette.surface,
                palette.surface_active,
            ] {
                assert!(contrast(palette.text, surface) >= 7.0, "{palette:?}");
                assert!(contrast(palette.secondary, surface) >= 4.5, "{palette:?}");
            }
            // Status colours are marks (chip dots, icons, outlines), not text: 3:1 on
            // every surface they sit on.
            for color in [
                palette.accent,
                palette.warning,
                palette.danger,
                palette.positive,
            ] {
                for surface in [palette.panel, palette.surface, palette.surface_active] {
                    assert!(
                        contrast(color, surface) >= 3.0,
                        "{color:?} on {surface:?} in {palette:?}"
                    );
                }
            }
            // The new-job form's errors, on its cards.
            assert!(
                contrast(palette.danger, palette.panel) >= 4.5,
                "{palette:?}"
            );
            // The outline of a checkbox or radio circle at rest (`show_choices`), a
            // component's boundary: 3:1 on the panel and the cards it sits on, and on its
            // own fill.
            for ground in [palette.panel, palette.surface_active] {
                assert!(contrast(palette.secondary, ground) >= 3.0, "{palette:?}");
            }
            // The text or icon of a primary button, of a destructive one, and of a
            // confirmed stack's Confirm.
            for fill in [palette.accent, palette.danger, palette.positive] {
                assert!(contrast(palette.on_accent, fill) >= 4.5, "{palette:?}");
            }
        }
    }

    #[test]
    fn all_digits_lay_out_at_the_same_width() {
        // egui's default font has tabular digits too, so check first that Inter is the one
        // laying them out: upstream Inter's digits have nine different widths.
        assert_eq!(fonts().families[&FontFamily::Proportional][0], "inter");
        let context = egui::Context::default();
        install(&context);
        // Fonts load on the first frame; there is no renderer here to take the textures.
        let mut output = context.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        let widths: Vec<f32> = context.fonts_mut(|fonts| {
            ('0'..='9')
                .map(|digit| {
                    fonts
                        .layout_no_wrap(
                            digit.to_string(),
                            FontId::proportional(14.0),
                            Color32::WHITE,
                        )
                        .size()
                        .x
                })
                .collect()
        });
        assert!(widths.iter().all(|width| *width == widths[0]), "{widths:?}");
    }

    #[test]
    fn both_themes_follow_the_system_by_default() {
        let context = egui::Context::default();
        install(&context);
        assert_eq!(
            context.options(|options| options.theme_preference),
            ThemePreference::System
        );
        context.set_theme(Theme::Light);
        assert_eq!(palette(&context), &Palette::LIGHT);
        context.set_theme(Theme::Dark);
        assert_eq!(palette(&context), &Palette::DARK);
    }
}
