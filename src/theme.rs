use egui::{
    Color32, Context, CornerRadius, Frame, Mesh, Shadow, Shape, Stroke, Theme, Ui, Visuals,
};

pub const THEMES: [&str; 4] = ["light", "dark", "glass-light", "glass-dark"];

pub fn normalize(name: &str) -> &'static str {
    THEMES
        .iter()
        .copied()
        .find(|theme| *theme == name)
        .unwrap_or("light")
}

pub fn next(name: &str) -> &'static str {
    let index = THEMES.iter().position(|theme| *theme == name).unwrap_or(0);
    THEMES[(index + 1) % THEMES.len()]
}

pub fn is_glass(name: &str) -> bool {
    matches!(name, "glass-light" | "glass-dark")
}

pub fn label_key(name: &str) -> &'static str {
    match normalize(name) {
        "dark" => "theme_name_dark",
        "glass-light" => "theme_name_glass_light",
        "glass-dark" => "theme_name_glass_dark",
        _ => "theme_name_light",
    }
}

pub fn apply(ctx: &Context, name: &str) {
    let name = normalize(name);
    let dark = matches!(name, "dark" | "glass-dark");
    ctx.set_theme(if dark { Theme::Dark } else { Theme::Light });
    ctx.set_visuals(visuals_for(name));
    ctx.request_repaint();
}

pub fn paint_background(ctx: &Context, name: &str) {
    if !is_glass(name) {
        return;
    }
    let rect = ctx.content_rect();
    let dark = name == "glass-dark";
    let (top_left, top_right, bottom_left, bottom_right, glow_a, glow_b) = if dark {
        (
            Color32::from_rgb(15, 23, 42),
            Color32::from_rgb(35, 24, 55),
            Color32::from_rgb(10, 35, 47),
            Color32::from_rgb(25, 25, 39),
            Color32::from_rgb(108, 99, 255),
            Color32::from_rgb(45, 190, 210),
        )
    } else {
        (
            Color32::from_rgb(226, 239, 255),
            Color32::from_rgb(244, 232, 255),
            Color32::from_rgb(224, 246, 240),
            Color32::from_rgb(255, 241, 229),
            Color32::from_rgb(129, 140, 248),
            Color32::from_rgb(45, 190, 180),
        )
    };
    let painter = ctx.layer_painter(egui::LayerId::background());
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top_left);
    mesh.colored_vertex(rect.right_top(), top_right);
    mesh.colored_vertex(rect.left_bottom(), bottom_left);
    mesh.colored_vertex(rect.right_bottom(), bottom_right);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(2, 1, 3);
    painter.add(Shape::mesh(mesh));

    paint_glow(
        &painter,
        egui::pos2(
            rect.right() - rect.width() * 0.12,
            rect.top() + rect.height() * 0.12,
        ),
        rect.width().min(rect.height()) * 0.52,
        glow_a,
    );
    paint_glow(
        &painter,
        egui::pos2(
            rect.left() + rect.width() * 0.16,
            rect.bottom() - rect.height() * 0.06,
        ),
        rect.width().min(rect.height()) * 0.44,
        glow_b,
    );
}

fn paint_glow(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: Color32) {
    for step in (1..=28).rev() {
        let alpha = if color == Color32::from_rgb(108, 99, 255) {
            2
        } else {
            3
        };
        painter.circle_filled(
            center,
            radius * step as f32 / 28.0,
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha),
        );
    }
}

fn visuals_for(name: &str) -> Visuals {
    match normalize(name) {
        "dark" => regular_visuals(true),
        "glass-light" => glass_visuals(false),
        "glass-dark" => glass_visuals(true),
        _ => regular_visuals(false),
    }
}

fn regular_visuals(dark: bool) -> Visuals {
    let mut visuals = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    visuals.widgets.noninteractive.fg_stroke.color = if dark {
        Color32::from_gray(230)
    } else {
        Color32::from_gray(32)
    };
    visuals.weak_text_color = Some(if dark {
        Color32::from_gray(175)
    } else {
        Color32::from_gray(88)
    });
    visuals
}

fn glass_visuals(dark: bool) -> Visuals {
    let mut visuals = regular_visuals(dark);
    let (panel, window, faint, extreme, input, border, selection, inactive, hovered, active) =
        if dark {
            (
                Color32::from_rgba_unmultiplied(20, 27, 44, 190),
                Color32::from_rgba_unmultiplied(24, 32, 51, 238),
                Color32::from_rgba_unmultiplied(255, 255, 255, 12),
                Color32::from_rgba_unmultiplied(8, 14, 26, 196),
                Color32::from_rgba_unmultiplied(12, 20, 35, 210),
                Color32::from_rgba_unmultiplied(185, 205, 245, 45),
                Color32::from_rgba_unmultiplied(111, 143, 250, 90),
                Color32::from_rgba_unmultiplied(52, 64, 91, 165),
                Color32::from_rgba_unmultiplied(70, 84, 119, 185),
                Color32::from_rgba_unmultiplied(85, 103, 148, 205),
            )
        } else {
            (
                Color32::from_rgba_unmultiplied(250, 252, 255, 160),
                Color32::from_rgba_unmultiplied(250, 252, 255, 240),
                Color32::from_rgba_unmultiplied(255, 255, 255, 95),
                Color32::from_rgba_unmultiplied(220, 230, 244, 190),
                Color32::from_rgba_unmultiplied(255, 255, 255, 205),
                Color32::from_rgba_unmultiplied(92, 111, 148, 55),
                Color32::from_rgba_unmultiplied(91, 124, 250, 65),
                Color32::from_rgba_unmultiplied(237, 242, 251, 190),
                Color32::from_rgba_unmultiplied(218, 228, 248, 210),
                Color32::from_rgba_unmultiplied(196, 211, 245, 225),
            )
        };
    visuals.dark_mode = dark;
    visuals.panel_fill = panel;
    visuals.window_fill = window;
    visuals.window_stroke = Stroke::new(1.0, border);
    visuals.window_corner_radius = CornerRadius::same(14);
    visuals.menu_corner_radius = CornerRadius::same(10);
    let shadow = Shadow {
        offset: [0, 7],
        blur: 22,
        spread: 0,
        color: if dark {
            Color32::from_black_alpha(75)
        } else {
            Color32::from_rgba_unmultiplied(45, 60, 95, 35)
        },
    };
    visuals.window_shadow = shadow;
    visuals.popup_shadow = shadow;
    visuals.faint_bg_color = faint;
    visuals.extreme_bg_color = extreme;
    visuals.text_edit_bg_color = Some(input);
    visuals.code_bg_color = faint;
    visuals.selection.bg_fill = selection;
    visuals.selection.stroke = Stroke::new(
        1.0,
        if dark {
            Color32::from_rgb(157, 178, 255)
        } else {
            Color32::from_rgb(66, 94, 190)
        },
    );
    visuals.hyperlink_color = if dark {
        Color32::from_rgb(145, 175, 255)
    } else {
        Color32::from_rgb(55, 91, 180)
    };
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(7);
        widget.bg_stroke = Stroke::new(1.0, border);
    }
    visuals.widgets.noninteractive.bg_fill = window;
    visuals.widgets.noninteractive.weak_bg_fill = faint;
    for widget in [&mut visuals.widgets.inactive, &mut visuals.widgets.open] {
        widget.bg_fill = inactive;
        widget.weak_bg_fill = inactive;
    }
    visuals.widgets.hovered.bg_fill = hovered;
    visuals.widgets.hovered.weak_bg_fill = hovered;
    visuals.widgets.active.bg_fill = active;
    visuals.widgets.active.weak_bg_fill = active;
    visuals
}

pub fn card_frame(ui: &Ui, name: &str) -> Frame {
    let frame = Frame::group(ui.style());
    if !is_glass(name) {
        return frame;
    }
    let dark = name == "glass-dark";
    frame
        .fill(if dark {
            Color32::from_rgba_unmultiplied(34, 44, 67, 145)
        } else {
            Color32::from_rgba_unmultiplied(255, 255, 255, 105)
        })
        .stroke(Stroke::new(1.0, ui.visuals().window_stroke.color))
        .corner_radius(CornerRadius::same(10))
        .shadow(Shadow {
            offset: [0, 3],
            blur: 12,
            spread: 0,
            color: if dark {
                Color32::from_black_alpha(35)
            } else {
                Color32::from_rgba_unmultiplied(45, 60, 95, 18)
            },
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glass_variants_use_matching_color_semantics() {
        for (name, dark) in [("glass-light", false), ("glass-dark", true)] {
            let visuals = visuals_for(name);
            assert_eq!(visuals.dark_mode, dark);
            assert!(visuals.panel_fill.a() < 255);
            assert!(visuals.window_fill.a() < 255);
            assert_eq!(visuals.window_corner_radius, CornerRadius::same(14));
            assert!(visuals.window_shadow.blur > 0);
        }
    }

    #[test]
    fn switching_back_restores_regular_visuals() {
        let ctx = Context::default();
        for glass in ["glass-light", "glass-dark"] {
            for regular in ["light", "dark"] {
                apply(&ctx, glass);
                assert!(ctx.global_style().visuals.panel_fill.a() < 255);
                apply(&ctx, regular);
                let actual = ctx.global_style().visuals.clone();
                let expected = regular_visuals(regular == "dark");
                assert_eq!(actual, expected);
            }
        }
    }

    #[test]
    fn theme_names_validate_and_cycle() {
        assert_eq!(normalize("glass-light"), "glass-light");
        assert_eq!(normalize("glass-dark"), "glass-dark");
        assert_eq!(normalize("unknown"), "light");
        assert_eq!(next("light"), "dark");
        assert_eq!(next("dark"), "glass-light");
        assert_eq!(next("glass-light"), "glass-dark");
        assert_eq!(next("glass-dark"), "light");
    }
}
