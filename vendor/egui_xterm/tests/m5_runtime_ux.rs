//! M5 runtime UX, modern OSC/keyboard protocols, I/O error surfacing, and rendering safety tests.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use egui::{
    CentralPanel, Color32, Context, CursorIcon, Event, FontDefinitions, FontFamily, Frame, Key,
    Modifiers, OutputCommand, PointerButton, Pos2, RawInput, Rect, Shape, TextureId, ViewportId,
    pos2, vec2,
};
use egui_xterm::{
    ClipboardSelection, Osc52ReadPolicy, ProgressIndicator, RingBufferInput, TerminalConfig,
    TerminalEvent, TerminalInput, TerminalIoError, TerminalResponse, TerminalRuntime, TerminalSize,
};

fn run_frame(
    ctx: &Context,
    screen_rect: Rect,
    ppp: f32,
    time: f64,
    focused: bool,
    events: Vec<Event>,
    mut ui_fn: impl FnMut(&mut egui::Ui),
) -> (egui::FullOutput, Vec<Shape>) {
    let mut raw = RawInput {
        screen_rect: Some(screen_rect),
        time: Some(time),
        focused,
        events,
        ..Default::default()
    };
    raw.viewports
        .entry(ViewportId::ROOT)
        .or_default()
        .native_pixels_per_point = Some(ppp);

    let out = ctx.run_ui(raw, |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui_fn(ui);
        });
    });
    let mut shapes = Vec::new();
    collect_shapes(&out.shapes, &mut shapes);
    (out, shapes)
}

fn collect_shapes(clipped: &[egui::epaint::ClippedShape], out: &mut Vec<Shape>) {
    fn visit(s: &Shape, out: &mut Vec<Shape>) {
        match s {
            Shape::Vec(children) => {
                for child in children {
                    visit(child, out);
                }
            }
            other => out.push(other.clone()),
        }
    }
    for cs in clipped {
        visit(&cs.shape, out);
    }
}

fn find_text_mesh(shapes: &[Shape]) -> Option<&Arc<egui::epaint::Mesh>> {
    shapes.iter().find_map(|s| match s {
        Shape::Mesh(m)
            if m.texture_id == TextureId::default()
                && m.vertices.iter().any(|v| v.uv != egui::epaint::WHITE_UV) =>
        {
            Some(m)
        }
        _ => None,
    })
}

fn focus_terminal<I: TerminalInput>(
    ctx: &Context,
    rt: &mut TerminalRuntime<I>,
    screen: Rect,
    time: f64,
) -> Option<TerminalResponse> {
    let mut resp0 = None;
    let (out0, _) = run_frame(ctx, screen, 1.0, time, true, Vec::new(), |ui| {
        resp0 = Some(rt.show(ui));
    });
    out0.drop_without_applying_deltas();
    let r0 = resp0?;
    let center = r0.response.rect.center();

    let mut resp1 = None;
    let (out1, _) = run_frame(
        ctx,
        screen,
        1.0,
        time + 0.02,
        true,
        vec![
            Event::PointerMoved(center),
            Event::PointerButton {
                pos: center,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: center,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            resp1 = Some(rt.show(ui));
        },
    );
    out1.drop_without_applying_deltas();
    resp1
}

#[test]
fn bold_render_mode_faux_bold_and_custom_font_family() {
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));

    // Default fonts lack `monospace_bold` / variable `wght` -> FauxBold double-stamps (8 verts).
    let ctx_faux = Context::default();
    let mut rt_normal = TerminalRuntime::new(RingBufferInput::from_bytes(b"A"));
    rt_normal.config_mut().cursor_blink = false;
    let (out_n, shapes_n) = run_frame(&ctx_faux, screen, 1.0, 0.0, true, Vec::new(), |ui| {
        let _ = rt_normal.show(ui);
    });
    out_n.drop_without_applying_deltas();
    let mesh_n = find_text_mesh(&shapes_n).expect("normal text mesh");
    assert_eq!(mesh_n.vertices.len(), 4);

    let mut rt_bold = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[1mA\x1b[0m"));
    rt_bold.config_mut().cursor_blink = false;
    let (out_b, shapes_b) = run_frame(&ctx_faux, screen, 1.0, 0.0, true, Vec::new(), |ui| {
        let _ = rt_bold.show(ui);
    });
    out_b.drop_without_applying_deltas();
    let mesh_b = find_text_mesh(&shapes_b).expect("faux bold text mesh");
    assert_eq!(
        mesh_b.vertices.len(),
        8,
        "FauxBold must double-stamp bold 'A' into 2 quads (8 vertices)"
    );
    assert!(
        mesh_b.vertices[4].pos.x > mesh_b.vertices[0].pos.x,
        "second FauxBold stamp must shift horizontally right"
    );

    // Custom `monospace_bold` family switches BoldRenderMode::FontFamily (single quad = 4 verts).
    let mut defs = FontDefinitions::default();
    if let Some(mono) = defs.families.get(&FontFamily::Monospace).cloned() {
        defs.families
            .insert(FontFamily::Name("monospace_bold".into()), mono);
    }
    ctx_faux.set_fonts(defs);
    let (out_fam, shapes_fam) = run_frame(&ctx_faux, screen, 1.0, 0.1, true, Vec::new(), |ui| {
        let _ = rt_bold.show(ui);
    });
    out_fam.drop_without_applying_deltas();
    let mesh_fam = find_text_mesh(&shapes_fam).expect("font-family bold text mesh");
    assert_eq!(
        mesh_fam.vertices.len(),
        4,
        "FontFamily bold mode must emit 1 quad (4 vertices) without faux double-stamp"
    );
}

#[test]
fn visual_bell_custom_color_opacity_decay_and_external_config() {
    let ctx = Context::default();
    let (tx, rx) = mpsc::channel();
    ctx.set_repaint_observer(move |info| {
        if tx.send(info.delay).is_err() {
            // Observer may outlive receiver after test ends.
        }
    });

    let bell_rgb = Color32::from_rgb(220, 40, 110);
    let cfg = TerminalConfig {
        cursor_blink: false,
        idle_poll_interval_ms: 0,
        visual_bell_duration_ms: 200,
        visual_bell_color: Some(bell_rgb),
        visual_bell_opacity: 0.5,
        ..TerminalConfig::default()
    };
    let mut rt = TerminalRuntime::with_config(RingBufferInput::from_bytes(b"ready"), cfg);
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));

    let find_bell_flash = |shapes: &[Shape], target_rgb: Color32| -> Option<u8> {
        shapes.iter().find_map(|s| match s {
            Shape::Rect(r)
                if r.rect.width() > 300.0 && r.rect.height() > 200.0 && r.fill.a() > 0 =>
            {
                let expected = Color32::from_rgba_unmultiplied(
                    target_rgb.r(),
                    target_rgb.g(),
                    target_rgb.b(),
                    r.fill.a(),
                );
                (r.fill == expected).then_some(r.fill.a())
            }
            _ => None,
        })
    };

    // Baseline frame: no BEL -> no tint overlay.
    let (out0, shapes0) = run_frame(&ctx, screen, 1.0, 0.0, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out0.drop_without_applying_deltas();
    assert_eq!(find_bell_flash(&shapes0, bell_rgb), None);
    let _ = rx.try_iter().count();

    // Frame 1 at t = 1.0: BEL triggers custom tint at ~50% opacity (~128 alpha) and schedules repaint.
    rt.input_mut().push_str("\x07");
    let mut events1 = Vec::new();
    let (out1, shapes1) = run_frame(&ctx, screen, 1.0, 1.0, true, Vec::new(), |ui| {
        events1 = rt.show(ui).events;
    });
    out1.drop_without_applying_deltas();
    assert!(events1.contains(&TerminalEvent::Bell));
    let alpha1 = find_bell_flash(&shapes1, bell_rgb).expect("custom visual bell rect at t=1.0");
    assert!(
        (124..=132).contains(&alpha1),
        "expected ~50% peak alpha (~128), got {alpha1}"
    );
    assert!(
        rx.try_iter()
            .any(|d| d > Duration::ZERO && d <= Duration::from_millis(20))
    );

    // Frame 2 at t = 1.1 (50% of 200ms remaining): alpha decays roughly by half (~64).
    let (out2, shapes2) = run_frame(&ctx, screen, 1.0, 1.1, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out2.drop_without_applying_deltas();
    let alpha2 = find_bell_flash(&shapes2, bell_rgb).expect("decaying bell rect at t=1.1");
    assert!(alpha2 < alpha1 && (56..=72).contains(&alpha2));
    let _ = rx.try_iter().count();

    // Frame 3 at t = 1.25 (past 200ms): overlay disappears and schedules zero decay repaints.
    let (out3, shapes3) = run_frame(&ctx, screen, 1.0, 1.25, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out3.drop_without_applying_deltas();
    assert_eq!(find_bell_flash(&shapes3, bell_rgb), None);
    assert!(!rx.try_iter().any(|d| d > Duration::ZERO));

    // show_with_config overrides visual_bell_color/opacity; opacity=0.0 suppresses flash.
    let ext_rgb = Color32::from_rgb(40, 200, 90);
    let mut ext_cfg = TerminalConfig {
        cursor_blink: false,
        idle_poll_interval_ms: 0,
        visual_bell_duration_ms: 200,
        visual_bell_color: Some(ext_rgb),
        visual_bell_opacity: 0.25,
        ..TerminalConfig::default()
    };
    rt.input_mut().push_str("\x07");
    let (out4, shapes4) = run_frame(&ctx, screen, 1.0, 2.0, true, Vec::new(), |ui| {
        let _ = rt.show_with_config(ui, &mut ext_cfg);
    });
    out4.drop_without_applying_deltas();
    let alpha4 = find_bell_flash(&shapes4, ext_rgb).expect("external config bell color");
    assert!((60..=68).contains(&alpha4));

    ext_cfg.visual_bell_opacity = 0.0;
    rt.input_mut().push_str("\x07");
    let (out5, shapes5) = run_frame(&ctx, screen, 1.0, 3.0, true, Vec::new(), |ui| {
        let _ = rt.show_with_config(ui, &mut ext_cfg);
    });
    out5.drop_without_applying_deltas();
    assert_eq!(find_bell_flash(&shapes5, ext_rgb), None);
}

#[test]
fn osc_9_4_progress_bar_states_rendering_and_show_progress_bar_toggle() {
    let ctx = Context::default();
    let (tx, rx) = mpsc::channel();
    ctx.set_repaint_observer(move |info| {
        if tx.send(info.delay).is_err() {
            // Observer may outlive receiver after test ends.
        }
    });

    let mut rt = TerminalRuntime::new(RingBufferInput::new());
    rt.config_mut().cursor_blink = false;
    rt.config_mut().idle_poll_interval_ms = 0;
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(600.0, 400.0));

    let extract_bar_rects = |shapes: &[Shape]| -> Vec<(f32, Color32, f32)> {
        shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Rect(r)
                    if r.rect.min.y == 0.0 && r.rect.height() <= 4.0 && r.rect.width() > 30.0 =>
                {
                    Some((r.rect.min.x, r.fill, r.rect.width()))
                }
                _ => None,
            })
            .collect()
    };

    // State 1 (normal 60%): blue fill, 360px wide on 600px viewport, no idle repaints.
    rt.input_mut().push_str("\x1b]9;4;1;60\x07");
    let mut events1 = Vec::new();
    let (out1, shapes1) = run_frame(&ctx, screen, 1.0, 0.0, true, Vec::new(), |ui| {
        events1 = rt.show(ui).events;
    });
    out1.drop_without_applying_deltas();
    assert!(events1.contains(&TerminalEvent::Progress {
        state: 1,
        progress: 60,
    }));
    assert_eq!(
        rt.progress(),
        Some(ProgressIndicator {
            state: 1,
            progress: 60,
        })
    );
    let bars1 = extract_bar_rects(&shapes1);
    assert_eq!(bars1.len(), 2);
    assert_eq!(bars1[1].1, Color32::from_rgb(66, 165, 245));
    assert!((bars1[1].2 - 360.0).abs() <= 1.0);
    assert!(!rx.try_iter().any(|d| d > Duration::ZERO));

    // State 2 (error default -> 100% red bar).
    rt.input_mut().push_str("\x1b]9;4;2\x07");
    let (out2, shapes2) = run_frame(&ctx, screen, 1.0, 0.1, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out2.drop_without_applying_deltas();
    assert_eq!(
        rt.progress(),
        Some(ProgressIndicator {
            state: 2,
            progress: 0,
        })
    );
    let bars2 = extract_bar_rects(&shapes2);
    assert_eq!(bars2.len(), 2);
    assert_eq!(bars2[1].1, Color32::from_rgb(239, 83, 80));
    assert!((bars2[1].2 - 600.0).abs() <= 1.0);

    // State 4 (paused 25% amber bar).
    rt.input_mut().push_str("\x1b]9;4;4;25\x07");
    let (out4, shapes4) = run_frame(&ctx, screen, 1.0, 0.2, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out4.drop_without_applying_deltas();
    assert_eq!(
        rt.progress(),
        Some(ProgressIndicator {
            state: 4,
            progress: 25,
        })
    );
    let bars4 = extract_bar_rects(&shapes4);
    assert_eq!(bars4.len(), 2);
    assert_eq!(bars4[1].1, Color32::from_rgb(255, 183, 77));
    assert!((bars4[1].2 - 150.0).abs() <= 1.0);

    // State 3 (indeterminate): animated 25% segment moves over time and schedules ~16ms repaint.
    rt.input_mut().push_str("\x1b]9;4;3\x07");
    let (out3a, shapes3a) = run_frame(&ctx, screen, 1.0, 0.3, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out3a.drop_without_applying_deltas();
    let bars3a = extract_bar_rects(&shapes3a);
    assert_eq!(bars3a.len(), 2);
    assert!((bars3a[1].2 - 150.0).abs() <= 1.0);
    assert!(
        rx.try_iter()
            .any(|d| d > Duration::ZERO && d <= Duration::from_millis(20))
    );

    let (out3b, shapes3b) = run_frame(&ctx, screen, 1.0, 0.6, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out3b.drop_without_applying_deltas();
    let bars3b = extract_bar_rects(&shapes3b);
    assert_ne!(bars3b[1].0, bars3a[1].0);

    // show_progress_bar = false suppresses rendering & repaint without clearing runtime.progress().
    let _ = rx.try_iter().count();
    rt.config_mut().show_progress_bar = false;
    let (out_sup, shapes_sup) = run_frame(&ctx, screen, 1.0, 0.7, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out_sup.drop_without_applying_deltas();
    assert!(extract_bar_rects(&shapes_sup).is_empty());
    assert!(rt.progress().is_some());
    assert!(!rx.try_iter().any(|d| d > Duration::ZERO));

    // State 0 clears runtime.progress().
    rt.config_mut().show_progress_bar = true;
    rt.input_mut().push_str("\x1b]9;4;0\x07");
    let (out0, shapes0) = run_frame(&ctx, screen, 1.0, 0.8, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out0.drop_without_applying_deltas();
    assert_eq!(rt.progress(), None);
    assert!(extract_bar_rects(&shapes0).is_empty());
}

#[test]
fn osc_22_cursor_icon_mapping_and_hyperlink_hover_precedence() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::new());
    rt.config_mut().cursor_blink = false;

    let mut term_rect = Rect::NOTHING;
    let mut cell_size = vec2(0.0, 0.0);
    let (out0, _) = run_frame(&ctx, screen, 1.0, 0.0, true, Vec::new(), |ui| {
        let r = rt.show(ui);
        term_rect = r.response.rect;
        cell_size = r.cell_size;
    });
    out0.drop_without_applying_deltas();

    let grid_hover = pos2(
        term_rect.min.x + 12.0 * cell_size.x,
        term_rect.min.y + 0.5 * cell_size.y,
    );

    let cases = [
        ("crosshair", CursorIcon::Crosshair),
        ("pointer", CursorIcon::PointingHand),
        ("ew-resize", CursorIcon::ResizeHorizontal),
        ("=Copy", CursorIcon::Copy),
        ("nonexistent, wait", CursorIcon::Wait),
        ("text", CursorIcon::Text),
        ("", CursorIcon::Default),
    ];

    let mut t = 0.1;
    for (shape_name, expected_icon) in cases {
        rt.input_mut()
            .push_str(&format!("\x1b]22;{shape_name}\x07"));
        let (out, _) = run_frame(
            &ctx,
            screen,
            1.0,
            t,
            true,
            vec![Event::PointerMoved(grid_hover)],
            |ui| {
                let _ = rt.show(ui);
            },
        );
        assert_eq!(
            out.platform_output.cursor_icon, expected_icon,
            "OSC 22 {shape_name:?} mismatch"
        );
        out.drop_without_applying_deltas();
        t += 0.05;
    }

    #[cfg(feature = "hyperlinks")]
    {
        rt.config_mut().hyperlink_require_ctrl = true;
        rt.input_mut()
            .push_str("\x1b[H\x1b]22;crosshair\x07\x1b]8;;https://example.org\x07link\x1b]8;;\x07");
        let (out_ing, _) = run_frame(&ctx, screen, 1.0, t, true, Vec::new(), |ui| {
            let _ = rt.show(ui);
        });
        out_ing.drop_without_applying_deltas();
        t += 0.05;

        let link_pos = pos2(
            term_rect.min.x + 1.5 * cell_size.x,
            term_rect.min.y + 0.5 * cell_size.y,
        );

        // Hovering link without Ctrl when hyperlink_require_ctrl=true: OSC 22 Crosshair wins.
        let (out_no_ctrl, _) = run_frame(
            &ctx,
            screen,
            1.0,
            t,
            true,
            vec![Event::PointerMoved(link_pos)],
            |ui| {
                let _ = rt.show(ui);
            },
        );
        assert_eq!(
            out_no_ctrl.platform_output.cursor_icon,
            CursorIcon::Crosshair
        );
        out_no_ctrl.drop_without_applying_deltas();
        t += 0.05;

        // Hovering link WITH Ctrl: hyperlink PointingHand wins over OSC 22 Crosshair.
        let (out_ctrl, _) = run_frame(
            &ctx,
            screen,
            1.0,
            t,
            true,
            vec![
                Event::ModifiersChanged(Modifiers::CTRL),
                Event::PointerMoved(link_pos),
            ],
            |ui| {
                let _ = rt.show(ui);
            },
        );
        assert_eq!(
            out_ctrl.platform_output.cursor_icon,
            CursorIcon::PointingHand
        );
        out_ctrl.drop_without_applying_deltas();
        t += 0.05;

        // Hovering non-link cell even with Ctrl active keeps OSC 22 Crosshair.
        let (out_off_link, _) = run_frame(
            &ctx,
            screen,
            1.0,
            t,
            true,
            vec![
                Event::ModifiersChanged(Modifiers::CTRL),
                Event::PointerMoved(grid_hover),
            ],
            |ui| {
                let _ = rt.show(ui);
            },
        );
        assert_eq!(
            out_off_link.platform_output.cursor_icon,
            CursorIcon::Crosshair
        );
        out_off_link.drop_without_applying_deltas();
        t += 0.05;

        // With hyperlink_require_ctrl=false, plain hover over link sets PointingHand immediately.
        rt.config_mut().hyperlink_require_ctrl = false;
        let (out_plain_link, _) = run_frame(
            &ctx,
            screen,
            1.0,
            t,
            true,
            vec![
                Event::ModifiersChanged(Modifiers::NONE),
                Event::PointerMoved(link_pos),
            ],
            |ui| {
                let _ = rt.show(ui);
            },
        );
        assert_eq!(
            out_plain_link.platform_output.cursor_icon,
            CursorIcon::PointingHand
        );
        out_plain_link.drop_without_applying_deltas();
    }
}

#[test]
fn osc_52_read_policies_and_write_toggle() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::new());
    assert_eq!(rt.config().osc52_read_policy, Osc52ReadPolicy::Deny);
    assert!(rt.config().osc52_write_enabled);

    // 1. Default Deny: read query produces no events, no pending prompt, and no outgoing reply.
    rt.input_mut().push_str("\x1b]52;c;?\x07");
    let mut events_deny = Vec::new();
    let (out_deny, _) = run_frame(&ctx, screen, 1.0, 0.0, true, Vec::new(), |ui| {
        events_deny = rt.show(ui).events;
    });
    out_deny.drop_without_applying_deltas();
    assert_eq!(rt.pending_osc52_read(), None);
    assert!(rt.input_mut().drain_written().is_empty());
    assert!(!events_deny.iter().any(|e| matches!(
        e,
        TerminalEvent::ClipboardReadRequested { .. } | TerminalEvent::ClipboardReadPrompt { .. }
    )));

    // 2. Allow: emits ClipboardReadRequested and host can provide clipboard reply.
    rt.config_mut().osc52_read_policy = Osc52ReadPolicy::Allow;
    rt.input_mut().push_str("\x1b]52;p;?\x07");
    let mut events_allow = Vec::new();
    let (out_allow, _) = run_frame(&ctx, screen, 1.0, 0.1, true, Vec::new(), |ui| {
        events_allow = rt.show(ui).events;
    });
    out_allow.drop_without_applying_deltas();
    assert!(
        events_allow.contains(&TerminalEvent::ClipboardReadRequested {
            selection: ClipboardSelection::Primary,
        })
    );
    assert_eq!(rt.pending_osc52_read(), None);
    rt.terminal_mut()
        .provide_clipboard_read(ClipboardSelection::Primary, "allow-data");
    let (out_allow_tx, _) = run_frame(&ctx, screen, 1.0, 0.15, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out_allow_tx.drop_without_applying_deltas();
    assert_eq!(
        rt.input_mut().drain_written(),
        b"\x1b]52;p;YWxsb3ctZGF0YQ==\x1b\\"
    );

    // 3. Prompt: emits ClipboardReadPrompt, sets pending_osc52_read, approve/deny roundtrip.
    rt.config_mut().osc52_read_policy = Osc52ReadPolicy::Prompt;
    rt.input_mut().push_str("\x1b]52;c;?\x07");
    let mut events_prompt = Vec::new();
    let (out_prompt, _) = run_frame(&ctx, screen, 1.0, 0.2, true, Vec::new(), |ui| {
        events_prompt = rt.show(ui).events;
    });
    out_prompt.drop_without_applying_deltas();
    assert!(events_prompt.contains(&TerminalEvent::ClipboardReadPrompt {
        selection: ClipboardSelection::Clipboard,
    }));
    assert_eq!(rt.pending_osc52_read(), Some(ClipboardSelection::Clipboard));
    assert!(rt.approve_osc52_read("secret"));
    assert_eq!(rt.pending_osc52_read(), None);
    assert!(!rt.approve_osc52_read("ignored"));
    assert_eq!(rt.input_mut().drain_written(), b"\x1b]52;c;c2VjcmV0\x1b\\");

    // Deny path drops prompt without sending bytes.
    rt.input_mut().push_str("\x1b]52;p;?\x07");
    let (out_prompt2, _) = run_frame(&ctx, screen, 1.0, 0.3, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out_prompt2.drop_without_applying_deltas();
    assert_eq!(rt.pending_osc52_read(), Some(ClipboardSelection::Primary));
    assert!(rt.deny_osc52_read());
    assert_eq!(rt.pending_osc52_read(), None);
    assert!(!rt.deny_osc52_read());
    assert!(rt.input_mut().drain_written().is_empty());

    // Switching back to Deny clears any stale pending prompt.
    rt.input_mut().push_str("\x1b]52;c;?\x07");
    let (out_prompt3, _) = run_frame(&ctx, screen, 1.0, 0.4, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out_prompt3.drop_without_applying_deltas();
    assert_eq!(rt.pending_osc52_read(), Some(ClipboardSelection::Clipboard));
    rt.config_mut().osc52_read_policy = Osc52ReadPolicy::Deny;
    let (out_clear, _) = run_frame(&ctx, screen, 1.0, 0.45, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    out_clear.drop_without_applying_deltas();
    assert_eq!(rt.pending_osc52_read(), None);

    // 4. osc52_write_enabled gating.
    rt.input_mut().push_str("\x1b]52;c;aGk=\x07");
    let (out_w_on, _) = run_frame(&ctx, screen, 1.0, 0.5, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    assert!(
        out_w_on
            .platform_output
            .commands
            .iter()
            .any(|c| matches!(c, OutputCommand::CopyText(s) if s == "hi"))
    );
    out_w_on.drop_without_applying_deltas();

    rt.config_mut().osc52_write_enabled = false;
    rt.input_mut().push_str("\x1b]52;c;bm8=\x07");
    let (out_w_off, _) = run_frame(&ctx, screen, 1.0, 0.6, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    assert!(
        !out_w_off
            .platform_output
            .commands
            .iter()
            .any(|c| matches!(c, OutputCommand::CopyText(_)))
    );
    out_w_off.drop_without_applying_deltas();
}

#[test]
fn osc_6_document_uri_and_osc_99_notifications() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(
        b"\x1b]6;file:///workspace/src/main.rs\x07\
          \x1b]99;i=job1:p=title:d=0;Build\x1b\\\
          \x1b]99;i=job1:p=body:e=1:d=1;aGVsbG8=\x1b\\",
    ));

    let mut events1 = Vec::new();
    let (out1, _) = run_frame(&ctx, screen, 1.0, 0.0, true, Vec::new(), |ui| {
        events1 = rt.show(ui).events;
    });
    out1.drop_without_applying_deltas();

    assert_eq!(rt.document_uri(), Some("file:///workspace/src/main.rs"));
    assert!(events1.contains(&TerminalEvent::DocumentChanged(Some(
        "file:///workspace/src/main.rs".to_owned(),
    ))));
    assert!(events1.contains(&TerminalEvent::Notification {
        title: "Build".to_owned(),
        body: "hello".to_owned(),
    }));

    // Empty OSC 6 clears document_uri(); OSC 99 p=close emits NotificationClosed.
    rt.input_mut()
        .push_str("\x1b]6;\x07\x1b]99;i=job1:p=close;\x1b\\");
    let mut events2 = Vec::new();
    let (out2, _) = run_frame(&ctx, screen, 1.0, 0.1, true, Vec::new(), |ui| {
        events2 = rt.show(ui).events;
    });
    out2.drop_without_applying_deltas();

    assert_eq!(rt.document_uri(), None);
    assert!(events2.contains(&TerminalEvent::DocumentChanged(None)));
    assert!(events2.contains(&TerminalEvent::NotificationClosed {
        id: "job1".to_owned(),
    }));
}

#[test]
fn modify_other_keys_and_lock_modifiers_through_runtime() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::new());
    let _ = focus_terminal(&ctx, &mut rt, screen, 0.0).expect("focus terminal");

    // Default modifyOtherKeys=0: Shift+Enter sends plain CR.
    let (out0, _) = run_frame(
        &ctx,
        screen,
        1.0,
        0.1,
        true,
        vec![Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::SHIFT,
        }],
        |ui| {
            let _ = rt.show(ui);
        },
    );
    out0.drop_without_applying_deltas();
    assert_eq!(rt.input_mut().drain_written(), b"\r");

    // modifyOtherKeys=1: Shift+Enter encodes CSI 27;2;13~, Ctrl+a still sends 0x01.
    rt.terminal_mut().advance(b"\x1b[>4;1m");
    let (out1, _) = run_frame(
        &ctx,
        screen,
        1.0,
        0.2,
        true,
        vec![Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::SHIFT,
        }],
        |ui| {
            let _ = rt.show(ui);
        },
    );
    out1.drop_without_applying_deltas();
    assert_eq!(rt.input_mut().drain_written(), b"\x1b[27;2;13~");

    // modifyOtherKeys=2: Ctrl+a encodes CSI 27;5;97~.
    rt.terminal_mut().advance(b"\x1b[>4;2m");
    let (out2, _) = run_frame(
        &ctx,
        screen,
        1.0,
        0.3,
        true,
        vec![Event::Key {
            key: Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        }],
        |ui| {
            let _ = rt.show(ui);
        },
    );
    out2.drop_without_applying_deltas();
    assert_eq!(rt.input_mut().drain_written(), b"\x1b[27;5;97~");

    // Explicit set_lock_modifiers + Key::CapsLock / NumLock toggling + Kitty precedence.
    rt.set_lock_modifiers(true, true);
    assert!(rt.caps_lock());
    assert!(rt.num_lock());

    #[cfg(feature = "kitty-keyboard")]
    {
        rt.terminal_mut().advance(b"\x1b[>9u");
        // Enter with CapsLock (64) + NumLock (128) -> 1 + 192 = 193.
        let (out_k1, _) = run_frame(
            &ctx,
            screen,
            1.0,
            0.4,
            true,
            vec![Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            |ui| {
                let _ = rt.show(ui);
            },
        );
        out_k1.drop_without_applying_deltas();
        assert_eq!(rt.input_mut().drain_written(), b"\x1b[13;193u");

        // Toggle CapsLock off via Key::CapsLock press (leaves NumLock=true -> 1 + 128 = 129).
        let (out_k2, _) = run_frame(
            &ctx,
            screen,
            1.0,
            0.5,
            true,
            vec![
                Event::Key {
                    key: Key::CapsLock,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
                Event::Key {
                    key: Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::CTRL,
                },
            ],
            |ui| {
                let _ = rt.show(ui);
            },
        );
        out_k2.drop_without_applying_deltas();
        assert!(!rt.caps_lock());
        assert!(rt.num_lock());
        let written = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
        assert!(
            written.contains("\x1b[97;133u"),
            "expected Kitty Ctrl(4)+NumLock(128)+1=133 precedence over modifyOtherKeys, got {written:?}"
        );
    }

    rt.set_lock_modifiers(false, false);
    assert!(!rt.caps_lock());
    assert!(!rt.num_lock());
}

#[derive(Default)]
struct MockFailingIoBackend {
    read_queue: VecDeque<Result<Vec<u8>, TerminalIoError>>,
    write_error: Option<TerminalIoError>,
    resize_error: Option<TerminalIoError>,
    closed: bool,
}

impl TerminalInput for MockFailingIoBackend {
    fn poll_read(&mut self, buf: &mut [u8]) -> Result<usize, TerminalIoError> {
        let Some(front) = self.read_queue.front_mut() else {
            return if self.closed {
                Err(TerminalIoError::Closed)
            } else {
                Ok(0)
            };
        };
        match front {
            Ok(data) => {
                if data.is_empty() {
                    self.read_queue.pop_front();
                    return Ok(0);
                }
                let n = buf.len().min(data.len());
                buf[..n].copy_from_slice(&data[..n]);
                data.drain(..n);
                if data.is_empty() {
                    self.read_queue.pop_front();
                }
                Ok(n)
            }
            Err(_) => match self.read_queue.pop_front() {
                Some(Err(e)) => {
                    if e.is_closed() {
                        self.closed = true;
                    }
                    Err(e)
                }
                _ => Ok(0),
            },
        }
    }

    fn write_bytes(&mut self, _bytes: &[u8]) -> Result<(), TerminalIoError> {
        if let Some(err) = self.write_error.as_ref() {
            Err(err.clone())
        } else {
            Ok(())
        }
    }

    fn resize(&mut self, _size: TerminalSize) -> Result<(), TerminalIoError> {
        if let Some(err) = self.resize_error.as_ref() {
            Err(err.clone())
        } else {
            Ok(())
        }
    }

    fn is_closed(&self) -> bool {
        self.closed && self.read_queue.is_empty()
    }
}

#[test]
fn io_errors_surfaced_on_poll_read_write_bytes_and_resize() {
    let ctx = Context::default();
    let screen_a = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 480.0));
    let screen_b = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 240.0));

    let backend = MockFailingIoBackend {
        resize_error: Some(TerminalIoError::message("resize io err")),
        ..Default::default()
    };
    let mut rt = TerminalRuntime::new(backend);
    rt.config_mut().resize_debounce_secs = 0.0;
    assert!(rt.last_io_error().is_none());

    // 1. Resize failure on initial size negotiation.
    let mut ev_resize = Vec::new();
    let (out_r, _) = run_frame(&ctx, screen_a, 1.0, 0.0, true, Vec::new(), |ui| {
        ev_resize = rt.show(ui).events;
    });
    out_r.drop_without_applying_deltas();
    assert!(ev_resize.contains(&TerminalEvent::IoError("resize io err".to_owned())));
    assert_eq!(
        rt.last_io_error().map(ToString::to_string).as_deref(),
        Some("resize io err")
    );
    assert_eq!(
        rt.take_io_error().map(|e| e.to_string()).as_deref(),
        Some("resize io err")
    );
    assert!(rt.last_io_error().is_none());

    // 2. Subsequent resize failure with screen_b after clearing error.
    rt.input_mut().resize_error = Some(TerminalIoError::message("resize shrink err"));
    let mut ev_resize2 = Vec::new();
    let (out_r2, _) = run_frame(&ctx, screen_b, 1.0, 0.1, true, Vec::new(), |ui| {
        ev_resize2 = rt.show(ui).events;
    });
    out_r2.drop_without_applying_deltas();
    assert!(ev_resize2.contains(&TerminalEvent::IoError("resize shrink err".to_owned())));
    assert!(rt.take_io_error().is_some());

    // 3. write_bytes failure (via VT DSR reply).
    rt.input_mut().resize_error = None;
    rt.input_mut().write_error = Some(TerminalIoError::message("write io err"));
    rt.input_mut().read_queue.push_back(Ok(b"\x1b[5n".to_vec()));
    let mut ev_write = Vec::new();
    let (out_w, _) = run_frame(&ctx, screen_b, 1.0, 0.2, true, Vec::new(), |ui| {
        ev_write = rt.show(ui).events;
    });
    out_w.drop_without_applying_deltas();
    assert!(ev_write.contains(&TerminalEvent::IoError("write io err".to_owned())));
    assert_eq!(
        rt.take_io_error().map(|e| e.to_string()).as_deref(),
        Some("write io err")
    );

    // 4. poll_read failure.
    rt.input_mut().write_error = None;
    rt.input_mut()
        .read_queue
        .push_back(Err(TerminalIoError::message("poll_read io err")));
    let mut ev_read = Vec::new();
    let (out_p, _) = run_frame(&ctx, screen_b, 1.0, 0.3, true, Vec::new(), |ui| {
        ev_read = rt.show(ui).events;
    });
    out_p.drop_without_applying_deltas();
    assert!(ev_read.contains(&TerminalEvent::IoError("poll_read io err".to_owned())));
    assert_eq!(
        rt.take_io_error().map(|e| e.to_string()).as_deref(),
        Some("poll_read io err")
    );

    // 5. TerminalIoError::Closed is clean EOF and must NOT surface as IoError.
    rt.input_mut()
        .read_queue
        .push_back(Err(TerminalIoError::Closed));
    let mut ev_closed = Vec::new();
    let (out_c, _) = run_frame(&ctx, screen_b, 1.0, 0.4, true, Vec::new(), |ui| {
        ev_closed = rt.show(ui).events;
    });
    out_c.drop_without_applying_deltas();
    assert!(rt.last_io_error().is_none());
    assert!(
        !ev_closed
            .iter()
            .any(|e| matches!(e, TerminalEvent::IoError(_)))
    );
}

#[cfg(feature = "kitty-graphics")]
#[test]
fn kitty_animations_tick_and_u10eeee_virtual_placeholder_rendering() {
    let ctx = Context::default();
    let (tx, rx) = mpsc::channel();
    ctx.set_repaint_observer(move |info| {
        if tx.send(info.delay).is_err() {
            // Observer may outlive receiver after test ends.
        }
    });

    // Animated 2x2 RGBA image i=9 with virtual placement p=12, c=2, r=2, and 100ms second frame.
    let mut payload = Vec::new();
    payload.extend_from_slice(
        b"\x1b_Ga=T,f=32,s=2,v=2,i=9,p=12,U=1,c=2,r=2,q=2;/////////////////////w==\x1b\\",
    );
    payload
        .extend_from_slice(b"\x1b_Ga=f,f=32,s=2,v=2,i=9,z=100,q=2;AP8AAP8AAP8AAP8AAP8AAA==\x1b\\");
    payload.extend_from_slice(b"\x1b_Ga=a,i=9,r=1,z=100,s=3,v=2,q=2\x1b\\");
    payload.extend_from_slice(
        "\x1b[38;2;0;0;9m\x1b[58;2;0;0;12m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\r\n\
         \u{10EEEE}\u{030D}\u{0305}\u{10EEEE}\x1b[0m"
            .as_bytes(),
    );

    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(&payload));
    rt.config_mut().cursor_blink = false;
    rt.config_mut().idle_poll_interval_ms = 0;
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));

    // Frame 1 at t=1.0: renders 2 contiguous U+10EEEE virtual tile meshes, suppresses text glyphs,
    // and schedules next_animation_deadline repaint (~100ms).
    let (out1, shapes1) = run_frame(&ctx, screen, 1.0, 1.0, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    assert!(
        find_text_mesh(&shapes1).is_none(),
        "U+10EEEE placeholders must suppress normal text mesh emission"
    );
    let img_meshes_1: Vec<Arc<egui::epaint::Mesh>> = shapes1
        .iter()
        .filter_map(|s| match s {
            Shape::Mesh(m) if m.texture_id != TextureId::default() => Some(Arc::clone(m)),
            _ => None,
        })
        .collect();
    assert_eq!(img_meshes_1.len(), 2);
    assert!((img_meshes_1[0].vertices[0].uv.y - 0.0).abs() < 1e-4);
    assert!((img_meshes_1[0].vertices[3].uv.y - 0.5).abs() < 1e-4);
    assert!((img_meshes_1[1].vertices[0].uv.y - 0.5).abs() < 1e-4);
    assert!((img_meshes_1[1].vertices[3].uv.y - 1.0).abs() < 1e-4);
    let tex_id_1 = img_meshes_1[0].texture_id;
    let rev1 = rt.terminal().image_data(9).expect("image 9").revision;
    assert!(rt.terminal().next_animation_deadline().is_some());
    out1.drop_without_applying_deltas();

    let delays1: Vec<Duration> = rx.try_iter().collect();
    assert!(
        delays1
            .iter()
            .any(|d| *d >= Duration::from_millis(90) && *d <= Duration::from_millis(110)),
        "expected ~100ms animation deadline repaint, got {delays1:?}"
    );

    // Frame 2 at t=1.15 (past 100ms delay): tick_animations advances frame & bumps revision,
    // re-uploading texture via TextureHandle::set onto the SAME TextureId.
    let (out2, shapes2) = run_frame(&ctx, screen, 1.0, 1.15, true, Vec::new(), |ui| {
        let _ = rt.show(ui);
    });
    let rev2 = rt
        .terminal()
        .image_data(9)
        .expect("image 9 after tick")
        .revision;
    assert!(rev2 > rev1);
    let has_tex_delta = !out2.textures_delta.set.is_empty();
    let tex_id_2 = shapes2
        .iter()
        .find_map(|s| match s {
            Shape::Mesh(m) if m.texture_id != TextureId::default() => Some(m.texture_id),
            _ => None,
        })
        .expect("virtual placement mesh after animation tick");
    out2.drop_without_applying_deltas();
    assert!(has_tex_delta);
    assert_eq!(tex_id_2, tex_id_1);
}

#[cfg(feature = "sixel")]
#[test]
fn sixel_8192x6_clamps_to_max_texture_side_without_panic() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(
        b"\x1bPq\"1;1;8192;6!8192~\x1b\\",
    ));
    rt.config_mut().cursor_blink = false;
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));

    let raw = RawInput {
        screen_rect: Some(screen),
        time: Some(0.0),
        focused: true,
        max_texture_side: Some(2048),
        ..Default::default()
    };
    let out = ctx.run_ui(raw, |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });

    let stored_dims = rt
        .terminal()
        .image_placements()
        .first()
        .and_then(|p| rt.terminal().image_data(p.image_id))
        .map(|img| (img.width, img.height))
        .expect("8192x6 sixel stored");
    assert_eq!(stored_dims, (8192, 6));

    let max_side = ctx.input(|i| i.max_texture_side);
    assert!(max_side <= 2048);
    let uploaded_ok = out.textures_delta.set.iter().any(|(id, deltas)| {
        *id != TextureId::default()
            && deltas.iter().any(|d| {
                let [w, h] = d.image.size();
                w > 0 && h > 0 && w <= max_side && h <= max_side
            })
    });
    let mut shapes = Vec::new();
    collect_shapes(&out.shapes, &mut shapes);
    let has_mesh = shapes
        .iter()
        .any(|s| matches!(s, Shape::Mesh(m) if m.texture_id != TextureId::default()));
    out.drop_without_applying_deltas();

    assert!(uploaded_ok, "8192x6 Sixel texture must clamp <= {max_side}");
    assert!(
        has_mesh,
        "clamped Sixel image must still emit textured Mesh"
    );
}
