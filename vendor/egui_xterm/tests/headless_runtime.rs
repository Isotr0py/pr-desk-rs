//! Headless `egui::Context::run_ui` integration tests for `egui_xterm`.

use egui::{
    CentralPanel, Context, CursorIcon, Event, Frame, ImeEvent, Key, Margin, Modifiers,
    MouseWheelUnit, OutputCommand, PointerButton, Pos2, RawInput, Rect, Role, Shape, TouchPhase,
    UiBuilder, ViewportId, pos2, vec2,
};
use egui_xterm::{
    ChannelInput, CursorShape, RingBufferInput, ScaleMode, TerminalConfig, TerminalEvent,
    TerminalInput, TerminalIoError, TerminalRuntime, TerminalSize, TerminalWidget, Theme,
};

fn run_frame_with_focus(
    ctx: &Context,
    screen_rect: Rect,
    ppp: f32,
    time: f64,
    focused: bool,
    events: Vec<Event>,
    ui_fn: impl FnMut(&mut egui::Ui),
) -> (Vec<egui::epaint::ClippedShape>, egui::PlatformOutput) {
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

    let out = ctx.run_ui(raw, ui_fn);
    let shapes = out.shapes.clone();
    let platform = out.platform_output.clone();
    out.drop_without_applying_deltas();
    (shapes, platform)
}

fn run_frame_with(
    ctx: &Context,
    screen_rect: Rect,
    ppp: f32,
    time: f64,
    events: Vec<Event>,
    ui_fn: impl FnMut(&mut egui::Ui),
) -> (Vec<egui::epaint::ClippedShape>, egui::PlatformOutput) {
    run_frame_with_focus(ctx, screen_rect, ppp, time, true, events, ui_fn)
}

#[test]
fn idle_input_polling_wakes_the_viewport_until_eof() {
    use std::io::Read as _;
    use std::sync::mpsc;
    use std::time::Duration;

    let ctx = Context::default();
    let (repaint_tx, repaint_rx) = mpsc::channel();
    ctx.set_repaint_observer(move |info| repaint_tx.send(info.delay).unwrap());
    let (input_tx, input_rx) = mpsc::channel::<&[u8]>();
    let mut runtime =
        TerminalRuntime::new(egui_xterm::FnInput::new(
            move |buf: &mut [u8]| match input_rx.try_recv() {
                Ok(mut bytes) => bytes.read(buf).map_err(Into::into),
                Err(mpsc::TryRecvError::Empty) => Ok(0),
                Err(mpsc::TryRecvError::Disconnected) => Err(TerminalIoError::Closed),
            },
        ));
    runtime.config_mut().cursor_blink = false;
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));

    run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        runtime.show(ui);
    });
    assert!(
        repaint_rx
            .try_iter()
            .any(|d| d == Duration::from_millis(50))
    );

    input_tx.send(b"late output").unwrap();
    drop(input_tx);
    let (clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.05, Vec::new(), |ui| {
        runtime.show(ui);
    });
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    assert_eq!(
        runtime.terminal().visible_line_text(0).as_deref(),
        Some("late output")
    );
    let text_mesh = find_text_mesh(&shapes).expect("coalesced text mesh for late output");
    assert_eq!(text_mesh.vertices.len(), "lateoutput".len() * 4);
    assert!(
        !repaint_rx
            .try_iter()
            .any(|d| d == Duration::from_millis(50))
    );
}

#[test]
fn synchronized_output_watchdog_wakes_an_event_driven_viewport() {
    use std::sync::mpsc;
    use std::time::Duration;

    let ctx = Context::default();
    let (repaint_tx, repaint_rx) = mpsc::channel();
    ctx.set_repaint_observer(move |info| repaint_tx.send(info.delay).unwrap());
    let cfg = TerminalConfig {
        cursor_blink: false,
        idle_poll_interval_ms: 0,
        ..TerminalConfig::default()
    };
    let input = RingBufferInput::from_bytes(b"\x1b[?2026hupdated");
    let mut runtime = TerminalRuntime::with_config(input, cfg);
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));

    run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        runtime.show(ui);
    });
    assert!(runtime.terminal().is_synchronized_output());
    assert!(
        repaint_rx
            .try_iter()
            .any(|d| d == Duration::from_millis(250))
    );

    let (clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.25, Vec::new(), |ui| {
        runtime.show(ui);
    });
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    assert!(!runtime.terminal().is_synchronized_output());
    assert_eq!(
        runtime.terminal().visible_line_text(0).as_deref(),
        Some("updated")
    );
    let text_mesh = find_text_mesh(&shapes).expect("coalesced text mesh for updated");
    assert_eq!(text_mesh.vertices.len(), "updated".len() * 4);
}

fn collect_shapes(shapes: &[egui::epaint::ClippedShape], out: &mut Vec<Shape>) {
    fn visit(s: &Shape, out: &mut Vec<Shape>) {
        match s {
            Shape::Vec(v) => {
                for c in v {
                    visit(c, out);
                }
            }
            other => out.push(other.clone()),
        }
    }
    for cs in shapes {
        visit(&cs.shape, out);
    }
}

fn all_text(shapes: &[Shape]) -> String {
    let mut buf = String::new();
    for s in shapes {
        if let Shape::Text(ts) = s {
            buf.push_str(&ts.galley.job.text);
            buf.push('\n');
        }
    }
    buf
}

fn find_text_mesh(shapes: &[Shape]) -> Option<&std::sync::Arc<egui::epaint::Mesh>> {
    shapes.iter().find_map(|s| match s {
        Shape::Mesh(m)
            if m.texture_id == egui::TextureId::default()
                && m.vertices.iter().any(|v| v.uv != egui::epaint::WHITE_UV) =>
        {
            Some(m)
        }
        _ => None,
    })
}

fn check_all_rects_snapped(shapes: &[Shape], ppp: f32) -> usize {
    let mut bad = 0usize;
    let is_snapped = |v: f32| {
        let s = v * ppp;
        (s - s.round()).abs() <= 1e-3
    };
    for s in shapes {
        if let Shape::Rect(r) = s
            && (r.fill != egui::Color32::TRANSPARENT || !r.stroke.is_empty())
            && !(is_snapped(r.rect.min.x)
                && is_snapped(r.rect.min.y)
                && is_snapped(r.rect.max.x)
                && is_snapped(r.rect.max.y))
        {
            bad += 1;
        }
    }
    bad
}

#[test]
fn auto_layout_handles_zero_tiny_infinite_and_zoom_clamping() {
    let ctx = Context::default();
    ctx.enable_accesskit();
    let mut rt = TerminalRuntime::new(RingBufferInput::new());
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));

    // Zero rect
    let mut zero_resp = None;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        ui.scope_builder(egui::UiBuilder::new().max_rect(Rect::ZERO), |ui| {
            zero_resp = Some(rt.show(ui));
        });
    });
    let zr = zero_resp.expect("zero resp");
    assert!(zr.cols >= 1 && zr.rows >= 1);
    assert!(zr.cell_size.x > 0.0 && zr.cell_size.y > 0.0);
    assert!(zr.effective_font_size > 0.0);

    // 1x1 tiny rect with AutoScaleToFit(0, 0)
    rt.config_mut().scale_mode = ScaleMode::AutoScaleToFit { cols: 0, rows: 0 };
    rt.config_mut().zoom_factor = -2.0;
    let mut tiny_resp = None;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.1, Vec::new(), |ui| {
        let tiny = Rect::from_min_size(Pos2::ZERO, vec2(1.0, 1.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(tiny), |ui| {
            tiny_resp = Some(rt.show(ui));
        });
    });
    let tr = tiny_resp.expect("tiny resp");
    assert_eq!((tr.cols, tr.rows), (1, 1));
    assert!(tr.effective_font_size.is_finite() && tr.effective_font_size > 0.0);

    // ScrollArea infinite-axis fallback + Role::Terminal
    rt.config_mut().scale_mode = ScaleMode::ReflowGrid;
    rt.config_mut().zoom_factor = 1.0;
    let mut scroll_resp = None;
    let (_, platform) = run_frame_with(&ctx, screen, 1.5, 0.2, Vec::new(), |ui| {
        egui::ScrollArea::both().show(ui, |ui| {
            scroll_resp = Some(rt.show(ui));
        });
    });
    let sr = scroll_resp.expect("scroll resp");
    assert!(sr.cols >= 1 && sr.rows >= 1);
    let has_terminal_role = platform
        .accesskit_update
        .as_ref()
        .is_some_and(|u| u.nodes.iter().any(|(_, n)| n.role() == Role::Terminal));
    assert!(has_terminal_role);
}

#[test]
fn terminal_widget_adapter_and_pixel_snapping() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"Hello Widget\r\n"));
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 480.0));

    for ppp in [1.0_f32, 1.25, 2.0] {
        let (clipped, _) = run_frame_with(&ctx, screen, ppp, 0.0, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.add(TerminalWidget::new(&mut rt));
            });
        });
        let mut shapes = Vec::new();
        collect_shapes(&clipped, &mut shapes);
        assert_eq!(check_all_rects_snapped(&shapes, ppp), 0);
        assert_eq!(
            rt.terminal().visible_line_text(0).as_deref(),
            Some("Hello Widget")
        );
        let text_mesh = find_text_mesh(&shapes).expect("coalesced text mesh for Hello Widget");
        assert_eq!(text_mesh.vertices.len(), "HelloWidget".len() * 4);
    }
}

#[test]
fn resize_debounce_and_channel_input_roundtrip() {
    let ctx = Context::default();
    let (tx, rx, input) = ChannelInput::duplex_pair();
    let cfg = TerminalConfig {
        resize_debounce_secs: 0.1,
        ..TerminalConfig::default()
    };
    let mut rt = TerminalRuntime::with_config(input, cfg);

    tx.send(b"READY\x1b]0;MyTerm\x07".to_vec()).expect("send");
    let mut r0 = None;
    let _ = run_frame_with(
        &ctx,
        Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0)),
        1.0,
        0.0,
        Vec::new(),
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                r0 = Some(rt.show(ui));
            });
        },
    );
    let r0 = r0.expect("r0");
    assert!(
        r0.events
            .contains(&TerminalEvent::TitleChanged("MyTerm".to_owned()))
    );

    // Click to focus and type text -> ChannelInput receives bytes.
    let center = r0.response.rect.center();
    let _ = run_frame_with(
        &ctx,
        Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0)),
        1.0,
        0.02,
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
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let _ = run_frame_with(
        &ctx,
        Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0)),
        1.0,
        0.04,
        vec![Event::Text("ping".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rx.try_recv().expect("written bytes"), b"ping");
}

#[test]
fn dec_2026_watchdog_and_ime_and_copy_selection() {
    let ctx = Context::default();
    ctx.set_os(egui::os::OperatingSystem::Windows);
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"OLD\r\n"));
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    // Focus terminal
    let c = term_rect.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.01,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    // Preedit IME & DEC 2026 sync latch
    rt.input_mut()
        .push_str("\x1b[?2026h\x1b[2J\x1b[HNEW_AFTER_WATCHDOG");
    let (clipped_mid, plat_mid) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![Event::Ime(ImeEvent::Preedit {
            text: "nihao".to_owned(),
            active_range_chars: Some(0..5),
        })],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(plat_mid.ime.is_some());
    let mut mid_shapes = Vec::new();
    collect_shapes(&clipped_mid, &mut mid_shapes);
    assert_eq!(
        rt.terminal().render_snapshot_line_text(0).as_deref(),
        Some("OLD")
    );
    let mid_mesh = find_text_mesh(&mid_shapes).expect("latched OLD text mesh");
    assert_eq!(mid_mesh.vertices.len(), "OLD".len() * 4);
    let mid_txt = all_text(&mid_shapes);
    assert!(mid_txt.contains("nihao"));

    // After >250ms, watchdog unlatches + empty preedit clears preedit.
    let (clipped_after, _) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.35,
        vec![Event::Ime(ImeEvent::Preedit {
            text: String::new(),
            active_range_chars: None,
        })],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let mut after_shapes = Vec::new();
    collect_shapes(&clipped_after, &mut after_shapes);
    assert_eq!(
        rt.terminal().render_snapshot_line_text(0).as_deref(),
        Some("NEW_AFTER_WATCHDOG")
    );
    let after_mesh = find_text_mesh(&after_shapes).expect("unlatched NEW_AFTER_WATCHDOG text mesh");
    assert_eq!(after_mesh.vertices.len(), "NEW_AFTER_WATCHDOG".len() * 4);
    assert!(!all_text(&after_shapes).contains("nihao"));

    // Copy without selection -> \x03
    let _ = run_frame_with(&ctx, screen, 1.0, 0.40, vec![Event::Copy], |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    assert_eq!(rt.input_mut().drain_written(), b"\x03");
}

#[test]
fn cursor_shapes_theme_and_clipboard_commands() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"alpha beta"));
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));

    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });

    rt.config_mut().theme = Theme::light();
    rt.config_mut().cursor_shape = CursorShape::Bar;
    rt.terminal_mut().selection_word(0, 1);

    let (_, plat) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.1,
        vec![
            Event::PointerButton {
                pos: pos2(10.0, 10.0),
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: pos2(10.0, 10.0),
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let _ = plat;

    // Select programmatically and send Event::Copy
    rt.terminal_mut()
        .selection_start(0, 0, egui_xterm::SelectionKind::Stream);
    rt.terminal_mut().selection_update(0, 4);
    rt.terminal_mut().selection_finish();
    let (_, copy_plat) = run_frame_with(&ctx, screen, 1.0, 0.2, vec![Event::Copy], |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    assert!(
        copy_plat
            .commands
            .iter()
            .any(|c| matches!(c, OutputCommand::CopyText(s) if s == "alpha"))
    );
}

#[cfg(feature = "hyperlinks")]
#[test]
fn hyperlink_hover_and_click_inside_grid_only() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(
        b"\x1b]8;;https://egui.rs\x07egui\x1b]8;;\x07",
    ));
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));

    let mut resp = None;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            resp = Some(rt.show(ui));
        });
    });
    let r = resp.expect("resp");
    let link_pos = pos2(
        r.response.rect.min.x + 1.5 * r.cell_size.x,
        r.response.rect.min.y + 0.5 * r.cell_size.y,
    );
    let (_, hover_plat) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.1,
        vec![Event::PointerMoved(link_pos)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(hover_plat.cursor_icon, CursorIcon::PointingHand);
}

#[cfg(feature = "search")]
#[test]
fn search_bar_open_and_query_highlights() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"find needle here\r\n"));
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));

    // Drain input on frame 0 so terminal grid contains "find needle here".
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });

    rt.open_search();
    rt.search_bar_mut().query_text = "needle".to_owned();
    let matches = rt
        .terminal_mut()
        .search(&egui_xterm::SearchQuery::literal("needle"))
        .expect("search");
    assert_eq!(matches.len(), 1);
    assert!(rt.terminal_mut().next_search_match().is_some());

    let (clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.1, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    assert!(all_text(&shapes).contains("1/1"));
}

#[test]
fn sgr_mouse_wheel_and_shift_bypass() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"row0\r\n\x1b[?1000;1006h"));
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));

    let mut r = None;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            r = Some(rt.show(ui));
        });
    });
    let r = r.expect("r");
    let cell = pos2(
        r.response.rect.min.x + 1.5 * r.cell_size.x,
        r.response.rect.min.y + 0.5 * r.cell_size.y,
    );
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.1,
        vec![
            Event::PointerMoved(cell),
            Event::MouseWheel {
                unit: MouseWheelUnit::Line,
                delta: vec2(0.0, 1.0),
                phase: TouchPhase::Move,
                source: egui::MouseWheelSource::Wheel,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let written = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
    assert!(written.contains("\x1b[<64;2;1M"), "got {written:?}");
}

#[test]
fn enter_and_space_keystrokes_never_crash_or_duplicate() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::new());
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));

    let mut rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            rect = rt.show(ui).response.rect;
        });
    });
    let c = rect.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.input_mut().drain_written(), b"\r");
}

#[cfg(feature = "vector-glyphs")]
#[test]
fn vector_glyphs_render_and_respect_clip_rect() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(
        "┌──┐\r\n│██│\r\n└──┘\r\n⣿⡇\r\n\u{E0B0}\u{E0B2}".as_bytes(),
    ));
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0));
    let sub = Rect::from_min_size(Pos2::ZERO, vec2(120.0, 80.0));

    let (clipped, _) = run_frame_with(&ctx, screen, 1.25, 0.0, Vec::new(), |ui| {
        ui.scope_builder(egui::UiBuilder::new().max_rect(sub), |ui| {
            ui.set_clip_rect(sub);
            let _ = rt.show(ui);
        });
    });

    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    assert_eq!(check_all_rects_snapped(&shapes, 1.25), 0);
    let mesh_verts: usize = shapes
        .iter()
        .filter_map(|s| match s {
            Shape::Mesh(m) => Some(m.vertices.len()),
            _ => None,
        })
        .sum();
    assert!(
        mesh_verts > 8,
        "expected vector-glyph vertices, got {mesh_verts}"
    );
}

#[cfg(feature = "shell-integration")]
#[test]
fn shell_integration_gutter_distinguishes_exit_codes() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(600.0, 400.0));

    let mut rt_ok = TerminalRuntime::new(RingBufferInput::from_bytes(
        b"\x1b]133;A\x07$ \x1b]133;B\x07ok\r\n\x1b]133;C\x07\x1b]133;D;0\x07",
    ));
    let mut rt_err = TerminalRuntime::new(RingBufferInput::from_bytes(
        b"\x1b]133;A\x07$ \x1b]133;B\x07err\r\n\x1b]133;C\x07\x1b]133;D;2\x07",
    ));

    let (c_ok, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt_ok.show(ui);
        });
    });
    let (c_err, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt_err.show(ui);
        });
    });

    let mut s_ok = Vec::new();
    let mut s_err = Vec::new();
    collect_shapes(&c_ok, &mut s_ok);
    collect_shapes(&c_err, &mut s_err);

    let colors = |shapes: &[Shape]| -> Vec<egui::Color32> {
        shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Mesh(m) => Some(m.vertices.iter().map(|v| v.color).collect::<Vec<_>>()),
                _ => None,
            })
            .flatten()
            .collect()
    };
    assert_ne!(colors(&s_ok), colors(&s_err));
}

#[cfg(feature = "kitty-keyboard")]
#[test]
fn kitty_keyboard_associated_text_does_not_double_send() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[>25u"));
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));

    let mut rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            rect = rt.show(ui).response.rect;
        });
    });
    let c = rect.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::Key {
                key: Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            Event::Text("a".to_owned()),
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let written = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
    assert_eq!(written, "\x1b[97;;97u");
}

#[cfg(feature = "metrics")]
#[test]
fn metrics_dirty_rows_invariant_across_idle_frames() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hello\r\n"));
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));

    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let d1 = rt.terminal().metrics().dirty_rows_rendered;
    assert!(d1 > 0);

    for i in 1..=3 {
        let _ = run_frame_with(&ctx, screen, 1.0, f64::from(i) * 0.05, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        });
    }
    assert_eq!(rt.terminal().metrics().frames_rendered, 4);
    assert_eq!(rt.terminal().metrics().dirty_rows_rendered, d1);
}

#[test]
fn multipane_columns_reorder_preserves_focus() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(900.0, 300.0));
    let mut rt_a = TerminalRuntime::new(RingBufferInput::from_bytes(b"PANE_A"));
    let mut rt_b = TerminalRuntime::new(RingBufferInput::from_bytes(b"PANE_B"));
    let mut rt_c = TerminalRuntime::new(RingBufferInput::from_bytes(b"PANE_C"));
    let mut c_rect = Rect::NOTHING;

    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        ui.columns(3, |cols| {
            let _ = rt_a.show(&mut cols[0]);
            let _ = rt_b.show(&mut cols[1]);
            c_rect = rt_c.show(&mut cols[2]).response.rect;
        });
    });
    let c = c_rect.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.05,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            ui.columns(3, |cols| {
                let _ = rt_a.show(&mut cols[0]);
                let _ = rt_b.show(&mut cols[1]);
                let _ = rt_c.show(&mut cols[2]);
            });
        },
    );

    // Drop pane B and swap [C, A]
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.10,
        vec![Event::Text("focused_c".to_owned())],
        |ui| {
            ui.columns(2, |cols| {
                let _ = rt_c.show(&mut cols[0]);
                let _ = rt_a.show(&mut cols[1]);
            });
        },
    );
    assert_eq!(rt_c.input_mut().drain_written(), b"focused_c");
    assert!(rt_a.input_mut().drain_written().is_empty());
}

#[test]
fn ascii_fast_path_preserves_galley_sections_colors_reuse_and_fallback() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(
        b"  \x1b[38;2;10;20;30mAB\x1b[3mCD\x1b[8mSEC\x1b[0mEF  \r\nX\xE2\x94\x80Y\r\n",
    ));
    rt.config_mut().cursor_blink = false;

    // Frame 1: initial render populates AsciiGlyphCache and coalesces rows into text_mesh.
    let mut cell_size = vec2(0.0, 0.0);
    let mut origin = Pos2::ZERO;
    let (c1, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let resp = rt.show(ui);
            cell_size = resp.cell_size;
            origin = resp.response.rect.min;
        });
    });
    let mut s1 = Vec::new();
    collect_shapes(&c1, &mut s1);
    assert_eq!(check_all_rects_snapped(&s1, 1.0), 0);
    assert!(!s1.iter().any(|s| matches!(s, Shape::Text(_))));
    assert!(
        rt.terminal()
            .visible_line_text(0)
            .as_deref()
            .is_some_and(|line| line.contains("ABCDSECEF"))
    );
    assert_eq!(rt.terminal().visible_line_text(1).as_deref(), Some("X─Y"));

    let m1 = find_text_mesh(&s1).expect("coalesced text mesh frame 1");
    let row0_quads: Vec<&[egui::epaint::Vertex; 4]> = m1
        .vertices
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|q| ((q[0].pos.y + q[2].pos.y) * 0.5) < origin.y + cell_size.y)
        .collect();
    // Row 0 visible ASCII glyphs: A, B, C, D, E, F (spaces & SGR 8 HIDDEN "SEC" emit 0 quads).
    assert_eq!(row0_quads.len(), 6);
    assert_eq!(row0_quads[0][0].color, egui::Color32::from_rgb(10, 20, 30));
    assert_eq!(row0_quads[1][0].color, egui::Color32::from_rgb(10, 20, 30));
    assert!(row0_quads[0][2].pos.x >= origin.x + 2.0 * cell_size.x - 1.0);
    assert!(row0_quads[0][2].pos.x < origin.x + 3.0 * cell_size.x);
    assert!(
        row0_quads[2][0].pos.x > row0_quads[2][2].pos.x,
        "italic C should slant top vertices right"
    );
    assert!(
        row0_quads[3][0].pos.x > row0_quads[3][2].pos.x,
        "italic D should slant top vertices right"
    );
    assert!(
        row0_quads[5][2].pos.x >= origin.x + 10.0 * cell_size.x - 1.0
            && row0_quads[5][2].pos.x < origin.x + 11.0 * cell_size.x,
        "F should land at col 10 after hidden SEC"
    );

    // Row 1 has vector/Unicode box-drawing between X and Y; verify Y stays at column 2.
    let row1_quads: Vec<&[egui::epaint::Vertex; 4]> = m1
        .vertices
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|q| {
            let ym = (q[0].pos.y + q[2].pos.y) * 0.5;
            ym >= origin.y + cell_size.y && ym < origin.y + 2.0 * cell_size.y
        })
        .collect();
    let last_row1 = row1_quads.last().expect("row1 Y quad");
    assert!(last_row1[2].pos.x >= origin.x + 1.5 * cell_size.x);

    // Frame 2: overwrite row 0 in-place and verify text_mesh updates cleanly.
    rt.input_mut()
        .push_str("\x1b[1;1H\x1b[2K\x1b[38;2;200;100;50mUPDATED_ROW0");
    let (c2, _) = run_frame_with(&ctx, screen, 1.0, 0.05, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut s2 = Vec::new();
    collect_shapes(&c2, &mut s2);
    assert_eq!(
        rt.terminal().visible_line_text(0).as_deref(),
        Some("UPDATED_ROW0")
    );
    let m2 = find_text_mesh(&s2).expect("text mesh frame 2");
    assert!(
        m2.vertices
            .iter()
            .any(|v| v.color == egui::Color32::from_rgb(200, 100, 50))
    );
    assert!(
        !m2.vertices
            .iter()
            .any(|v| v.color == egui::Color32::from_rgb(10, 20, 30))
    );

    // Frame 3: change pixels_per_point to 2.0 and verify AsciiGlyphCache invalidates cleanly
    let (c3, _) = run_frame_with(&ctx, screen, 2.0, 0.10, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut s3 = Vec::new();
    collect_shapes(&c3, &mut s3);
    assert_eq!(check_all_rects_snapped(&s3, 2.0), 0);
    let m3 = find_text_mesh(&s3).expect("text mesh frame 3");
    assert!(!std::sync::Arc::ptr_eq(m2, m3));
    for v in &m3.vertices {
        assert!(v.uv.x.is_finite() && v.uv.y.is_finite());
        assert!(v.uv.x >= 0.0 && v.uv.x <= 1.0 && v.uv.y >= 0.0 && v.uv.y <= 1.0);
    }

    // Frame 4: idle frame at ppp=2.0 reuses cached text_mesh Arc.
    let (c4, _) = run_frame_with(&ctx, screen, 2.0, 0.15, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut s4 = Vec::new();
    collect_shapes(&c4, &mut s4);
    let m4 = find_text_mesh(&s4).expect("text mesh frame 4");
    assert!(std::sync::Arc::ptr_eq(m3, m4));
}

#[test]
fn unicode_slow_path_eliminates_cumulative_drift() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 400.0));
    let mut line2 = "🚀".repeat(20);
    line2.push_str("END");
    let payload = format!("你好🚀OK\r\n{line2}\r\n👨\u{200D}👩\u{200D}👧\u{200D}👦AB\r\n");
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(payload.as_bytes()));
    rt.config_mut().cursor_blink = false;

    let mut origin = Pos2::ZERO;
    let mut cell_size = vec2(0.0, 0.0);
    let mut eff_font_size = 14.0;
    let (clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let resp = rt.show(ui);
            origin = resp.response.rect.min;
            cell_size = resp.cell_size;
            eff_font_size = resp.effective_font_size;
        });
    });
    let cell_w = cell_size.x;
    let cell_h = cell_size.y;

    assert_eq!(
        rt.terminal().visible_line_text(0).as_deref(),
        Some("你好🚀OK")
    );
    assert_eq!(
        rt.terminal().visible_line_text(1).as_deref(),
        Some(line2.as_str())
    );
    assert_eq!(
        rt.terminal().visible_line_text(2).as_deref(),
        Some("👨\u{200D}👩\u{200D}👧\u{200D}👦AB")
    );

    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    let text_mesh = find_text_mesh(&shapes).expect("coalesced unicode text mesh");

    let probe_min_x = |ch: &str| -> f32 {
        ctx.fonts_mut(|f| {
            let mut job = egui::text::LayoutJob {
                first_row_min_height: cell_h,
                break_on_newline: false,
                keep_trailing_whitespace: true,
                ..Default::default()
            };
            job.append(
                ch,
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::new(eff_font_size, egui::FontFamily::Monospace),
                    ..Default::default()
                },
            );
            let g = f.layout_job(job);
            g.rows[0].row.visuals.mesh.vertices[0].pos.x
        })
    };
    let ref_o_dx = probe_min_x("O");
    let ref_e_dx = probe_min_x("E");
    let ref_a_dx = probe_min_x("A");

    let row_quads = |row_idx: usize| -> Vec<&[egui::epaint::Vertex; 4]> {
        let y0 = origin.y + row_idx as f32 * cell_h;
        let y1 = y0 + cell_h;
        text_mesh
            .vertices
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|q| {
                let ym = (q[0].pos.y + q[2].pos.y) * 0.5;
                ym >= y0 && ym < y1
            })
            .collect()
    };

    // Row 0: "你好🚀OK" -> 'O' (second-to-last quad) lands at col 6.
    let r0 = row_quads(0);
    assert!(r0.len() >= 2);
    let o_x = r0[r0.len() - 2][0].pos.x - ref_o_dx;
    let expected_o_x = origin.x + 6.0 * cell_w;
    assert!(
        (o_x - expected_o_x).abs() <= 0.5,
        "row0 'O' drifted: got {o_x}, expected {expected_o_x}"
    );

    // Row 1: 20x 🚀 + "END" -> 'E' (third-to-last quad) lands at col 40 within <= 0.5px.
    let r1 = row_quads(1);
    assert!(r1.len() >= 3);
    let e_x = r1[r1.len() - 3][0].pos.x - ref_e_dx;
    let expected_e_x = origin.x + 40.0 * cell_w;
    assert!(
        (e_x - expected_e_x).abs() <= 0.5,
        "20x🚀+END 'E' drifted: got {e_x}, expected {expected_e_x}"
    );

    // Row 2: ZWJ family emoji + "AB" -> 'A' (second-to-last quad) lands at col 2 (>= 1.5 * cell_w).
    let r2 = row_quads(2);
    assert!(r2.len() >= 2);
    let a_x = r2[r2.len() - 2][0].pos.x - ref_a_dx;
    assert!(
        a_x >= origin.x + 1.5 * cell_w && (a_x - (origin.x + 2.0 * cell_w)).abs() <= 0.5,
        "ZWJ family 'A' collided or drifted: got {a_x}, origin_x={}, cell_w={cell_w}",
        origin.x
    );
}

#[test]
fn immediate_escape_and_tab_after_click_preserve_focus_single_and_multipane() {
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(900.0, 400.0));

    // Single pane: click on Frame 1 -> immediate Escape on Frame 2, then click -> immediate Tab.
    for key in [Key::Escape, Key::Tab] {
        let ctx = Context::default();
        let mut rt = TerminalRuntime::new(RingBufferInput::new());
        let mut rect = Rect::NOTHING;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                rect = rt.show(ui).response.rect;
            });
        });

        // Frame 0: Escape when unfocused must NOT steal focus. (Tab when nothing is focused
        // advances focus to the first focusable widget per egui's Tab convention.)
        if key == Key::Escape {
            let mut unfocused_has_focus = true;
            let _ = run_frame_with(
                &ctx,
                screen,
                1.0,
                0.01,
                vec![Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                |ui| {
                    CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                        unfocused_has_focus = rt.show(ui).response.has_focus();
                    });
                },
            );
            assert!(!unfocused_has_focus);
            assert!(rt.input_mut().drain_written().is_empty());
        }

        // Frame 1: click terminal.
        let c = rect.center();
        let mut foc1 = false;
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.02,
            vec![
                Event::PointerMoved(c),
                Event::PointerButton {
                    pos: c,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: c,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    foc1 = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(foc1);

        // Frame 2: immediate Escape or Tab must retain focus and emit VT bytes.
        let mut foc2 = false;
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.03,
            vec![Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    foc2 = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(foc2, "focus dropped on immediate {key:?} after click");
        let expected: &[u8] = if key == Key::Escape { b"\x1b" } else { b"\t" };
        assert_eq!(rt.input_mut().drain_written(), expected);
    }

    // Multi-pane ui.columns(3) click C -> Frame 2 ui.columns(2) [C, A] with immediate Tab+ArrowUp+Escape.
    let ctx = Context::default();
    let mut rt_a = TerminalRuntime::new(RingBufferInput::from_bytes(b"A"));
    let mut rt_b = TerminalRuntime::new(RingBufferInput::from_bytes(b"B"));
    let mut rt_c = TerminalRuntime::new(RingBufferInput::from_bytes(b"C"));
    let mut c_rect = Rect::NOTHING;

    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        ui.columns(3, |cols| {
            let _ = rt_a.show(&mut cols[0]);
            let _ = rt_b.show(&mut cols[1]);
            c_rect = rt_c.show(&mut cols[2]).response.rect;
        });
    });

    let c = c_rect.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            ui.columns(3, |cols| {
                let _ = rt_a.show(&mut cols[0]);
                let _ = rt_b.show(&mut cols[1]);
                let _ = rt_c.show(&mut cols[2]);
            });
        },
    );

    let mut c_foc = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::Key {
                key: Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            Event::Key {
                key: Key::ArrowUp,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            ui.columns(2, |cols| {
                c_foc = rt_c.show(&mut cols[0]).response.has_focus();
                let _ = rt_a.show(&mut cols[1]);
            });
        },
    );
    assert!(c_foc);
    assert_eq!(rt_c.input_mut().drain_written(), b"\t\x1b[A\x1b");
    assert!(rt_a.input_mut().drain_written().is_empty());
}

#[test]
fn fractional_frame_inner_rect_never_leaks_mesh_vertices() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"Hello\r\n\x1b[4mWorld\x1b[0m"));
    let outer = Rect::from_min_size(pos2(10.3, 11.7), vec2(400.0, 300.0));

    for ppp in [1.0_f32, 1.25, 1.5] {
        let mut inner_rect = Rect::NOTHING;
        let (clipped, _) = run_frame_with(&ctx, screen, ppp, 0.0, Vec::new(), |ui| {
            ui.scope_builder(UiBuilder::new().max_rect(outer), |ui| {
                Frame::NONE.inner_margin(Margin::same(7)).show(ui, |ui| {
                    inner_rect = ui.available_rect_before_wrap();
                    let _ = rt.show(ui);
                });
            });
        });

        let mut shapes = Vec::new();
        collect_shapes(&clipped, &mut shapes);
        for shape in &shapes {
            if let Shape::Mesh(mesh) = shape {
                for v in &mesh.vertices {
                    assert!(
                        v.pos.x >= inner_rect.min.x - 1e-4
                            && v.pos.x <= inner_rect.max.x + 1e-4
                            && v.pos.y >= inner_rect.min.y - 1e-4
                            && v.pos.y <= inner_rect.max.y + 1e-4,
                        "mesh vertex {:?} escaped inner_rect {:?} at ppp={ppp}",
                        v.pos,
                        inner_rect
                    );
                }
            }
        }
    }
}

#[test]
fn extreme_duration_configs_never_panic() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::new());

    // Initial layout at 800x600.
    let mut rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            rect = rt.show(ui).response.rect;
        });
    });

    // Click to focus so cursor blink path runs.
    let c = rect.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    let mut time = 1.0;
    for extreme in [f64::MAX, f64::INFINITY, f64::NAN] {
        rt.config_mut().resize_debounce_secs = extreme;
        rt.config_mut().cursor_config.blink_period_secs = extreme;
        let small = Rect::from_min_size(Pos2::ZERO, vec2(500.0, 350.0));
        let _ = run_frame_with(&ctx, small, 1.0, time, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        });
        time += 0.1;
        let _ = run_frame_with(&ctx, small, 1.0, time, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        });
        time += 0.1;
    }
}

#[test]
fn adversarial_poll_read_over_budget_length_never_panics() {
    struct OverReportingInput;
    impl TerminalInput for OverReportingInput {
        fn poll_read(&mut self, buf: &mut [u8]) -> Result<usize, TerminalIoError> {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = if i == 0 { b'X' } else { b' ' };
            }
            Ok(buf.len().saturating_add(1024))
        }
        fn write_bytes(&mut self, _bytes: &[u8]) -> Result<(), TerminalIoError> {
            Ok(())
        }
        fn resize(&mut self, _size: TerminalSize) -> Result<(), TerminalIoError> {
            Ok(())
        }
    }

    let ctx = Context::default();
    let cfg = TerminalConfig {
        read_budget_bytes_per_frame: 32,
        ..TerminalConfig::default()
    };
    let mut rt = TerminalRuntime::with_config(OverReportingInput, cfg);
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));

    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
}

#[test]
fn virtual_col_empty_selection_sends_sigint_and_no_copy_text() {
    let ctx = Context::default();
    ctx.set_os(egui::os::OperatingSystem::Windows);
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hi\r\n"));
    let wide = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));

    let mut rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, wide, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            rect = rt.show(ui).response.rect;
        });
    });

    // Focus terminal.
    let c = rect.center();
    let _ = run_frame_with(
        &ctx,
        wide,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    // Create stream selection past col 50 on line 0 ("hi"), then narrow viewport to ~15 cols.
    rt.terminal_mut()
        .selection_start(0, 50, egui_xterm::SelectionKind::Stream);
    rt.terminal_mut().selection_update(0, 60);
    rt.terminal_mut().selection_finish();

    let narrow = Rect::from_min_size(Pos2::ZERO, vec2(125.0, 400.0));
    let _ = run_frame_with(&ctx, narrow, 1.0, 0.04, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    assert_eq!(rt.terminal().selection_text().as_deref(), Some(""));

    let (_, plat_copy) = run_frame_with(&ctx, narrow, 1.0, 0.06, vec![Event::Copy], |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    assert!(
        !plat_copy
            .commands
            .iter()
            .any(|cmd| matches!(cmd, OutputCommand::CopyText(_))),
        "empty selection must not emit CopyText"
    );
    assert_eq!(rt.input_mut().drain_written(), b"\x03");

    let (_, plat_ctrl_c) = run_frame_with(
        &ctx,
        narrow,
        1.0,
        0.08,
        vec![Event::Key {
            key: Key::C,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(
        !plat_ctrl_c
            .commands
            .iter()
            .any(|cmd| matches!(cmd, OutputCommand::CopyText(_)))
    );
    assert_eq!(rt.input_mut().drain_written(), b"\x03");
}

#[cfg(feature = "search")]
#[test]
fn search_bar_focus_isolation_enter_navigation_and_escape_close() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hit one\r\nhit two\r\n"));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    // Focus terminal, then press Ctrl+Shift+F -> immediately Escape next frame.
    let c = term_rect.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::Key {
            key: Key::F,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.search_bar_mut().open);

    // Immediate Escape closes bar without leaking 0x1B.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(!rt.search_bar_mut().open);
    assert!(rt.input_mut().drain_written().is_empty());

    // Open search bar, type "hit", press Enter twice, Shift+Enter once, click inside bar ("Aa"),
    // verify no selection created and Enter/Escape work reliably.
    rt.open_search();
    let _ = run_frame_with(&ctx, screen, 1.0, 0.08, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });

    // Click inside top-right search bar TextEdit (left side of bar_rect) must NOT start grid
    // selection or focus the terminal.
    let bar_click = pos2(term_rect.max.x - 280.0, term_rect.min.y + 20.0);
    let mut term_focused = true;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.10,
        vec![
            Event::PointerMoved(bar_click),
            Event::PointerButton {
                pos: bar_click,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: bar_click,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
            Event::Text("hit".to_owned()),
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                term_focused = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(!term_focused);
    assert!(rt.terminal().selection().is_none());
    assert_eq!(rt.terminal().search_matches().len(), 2);

    // Clicking "Aa" / ".*" on right side of search bar must NOT focus terminal or start selection.
    let aa_click = pos2(term_rect.max.x - 135.0, term_rect.min.y + 20.0);
    let mut term_focused_aa = true;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.11,
        vec![
            Event::PointerMoved(aa_click),
            Event::PointerButton {
                pos: aa_click,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: aa_click,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                term_focused_aa = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(!term_focused_aa);
    assert!(rt.terminal().selection().is_none());

    // Press Enter -> selects match 0. Press Enter again -> advances to match 1!
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.12,
        vec![Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let m1 = rt.terminal().active_search_match().copied();
    assert!(m1.is_some());

    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.14,
        vec![Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let m2 = rt.terminal().active_search_match().copied();
    assert_ne!(m1, m2, "repeated Enter must advance to second search match");

    // Shift+Enter steps back to match 0.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.16,
        vec![Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::SHIFT,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.terminal().active_search_match().copied(), m1);

    // Escape closes bar and returns focus to terminal without leaking \x1b.
    let mut returned_focus = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.18,
        vec![Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                returned_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(!rt.search_bar_mut().open);
    assert!(returned_focus);
    assert!(rt.input_mut().drain_written().is_empty());
}

#[test]
fn shift_bypass_mid_drag_shift_release_settles_selection_cleanly() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(
        b"select me please\r\n\x1b[?1003;1006h",
    ));

    let mut r = None;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            r = Some(rt.show(ui));
        });
    });
    let r = r.expect("resp");
    let p0 = pos2(
        r.response.rect.min.x + 0.5 * r.cell_size.x,
        r.response.rect.min.y + 0.5 * r.cell_size.y,
    );
    let p1 = pos2(
        r.response.rect.min.x + 5.5 * r.cell_size.x,
        r.response.rect.min.y + 0.5 * r.cell_size.y,
    );
    let p2 = pos2(
        r.response.rect.min.x + 8.5 * r.cell_size.x,
        r.response.rect.min.y + 0.5 * r.cell_size.y,
    );

    // Press Primary with SHIFT, drag, then release Primary WITHOUT SHIFT.
    let (_, plat) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(p0),
            Event::PointerButton {
                pos: p0,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::SHIFT,
            },
            Event::PointerMoved(p1),
            Event::PointerButton {
                pos: p1,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    assert!(
        rt.input_mut().drain_written().is_empty(),
        "releasing Shift mid-drag must not leak mouse release bytes"
    );
    assert!(
        plat.commands
            .iter()
            .any(|c| matches!(c, OutputCommand::TextSelectionSettled(s) if s == "select")),
        "expected TextSelectionSettled(\"select\"), got {:?}",
        plat.commands
    );

    // Subsequent PointerMoved must NOT mutate selection (left_down_in_grid not wedged).
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::PointerMoved(p2)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.terminal().selection_text().as_deref(), Some("select"));

    // Symmetric case: press WITHOUT Shift under ?1003;1006h, then release WITH Shift held ->
    // settles active_mouse_button and stops emitting motion reports on subsequent moves.
    let _ = rt.input_mut().drain_written();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![
            Event::PointerMoved(p0),
            Event::PointerButton {
                pos: p0,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: p0,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::SHIFT,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let bytes = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
    assert!(
        bytes.contains("\x1b[<0;1;1M") && bytes.contains("\x1b[<4;1;1m"),
        "got {bytes:?}"
    );
    // Subsequent PointerMoved must emit zero button-drag reports (no active_mouse_button leak).
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.08,
        vec![Event::PointerMoved(p2)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let post = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
    assert!(!post.contains("\x1b[<32;"), "leaked drag motion: {post:?}");
}

#[test]
fn scrollbar_drag_scrolls_without_creating_selection() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
    let mut data = String::new();
    for i in 0..80 {
        data.push_str(&format!("line {i:02} content\r\n"));
    }
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(data.as_bytes()));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });
    assert!(rt.terminal().scrollback_len() > 0);

    let sb_x = term_rect.max.x - 2.0;
    let p_bottom = pos2(sb_x, term_rect.max.y - 10.0);
    let p_top = pos2(sb_x, term_rect.min.y + 10.0);

    // Frame 1: press on scrollbar track.
    let (_, plat1) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(p_bottom),
            Event::PointerButton {
                pos: p_bottom,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.terminal().selection().is_none());

    // Frame 2: drag upward on scrollbar track.
    let (_, plat2) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::PointerMoved(p_top)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.terminal().selection().is_none());

    // Frame 3: release on scrollbar track.
    let (_, plat3) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![Event::PointerButton {
            pos: p_top,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    assert!(rt.terminal().display_offset() > 0);
    assert!(rt.terminal().selection().is_none());
    for plat in [&plat1, &plat2, &plat3] {
        assert!(
            !plat
                .commands
                .iter()
                .any(|c| matches!(c, OutputCommand::TextSelectionSettled(_)))
        );
    }
}

#[test]
fn middle_click_paste_gated_by_mouse_protocol_and_context_menu_smoke() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hello\r\n\x1b[?1000h"));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    let c = term_rect.center();
    // Click to focus.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    // Clear mouse press/release bytes emitted by the focus click under ?1000h.
    let _ = rt.input_mut().drain_written();

    // Under ?1000h without Shift, MiddleClickPaste must be gated.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::MiddleClickPaste {
            text: "pasted".to_owned(),
            pos: c,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.input_mut().drain_written().is_empty());

    // Disable ?1000h -> MiddleClickPaste emits text, and right-click context menu smoke runs cleanly.
    rt.input_mut().push_str("\x1b[?1000l");
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![
            Event::MiddleClickPaste {
                text: "pasted".to_owned(),
                pos: c,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.input_mut().drain_written(), b"pasted");

    // Run one more frame with context menu open to verify menu renders cleanly.
    let (menu_clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.08, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut menu_shapes = Vec::new();
    collect_shapes(&menu_clipped, &mut menu_shapes);
    let menu_txt = all_text(&menu_shapes);
    assert!(menu_txt.contains("Paste") && menu_txt.contains("Clear Scrollback"));
}

#[cfg(feature = "kitty-keyboard")]
#[test]
fn kitty_keyboard_ctrl_c_without_selection_sends_csi_u() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[>1u"));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    let c = term_rect.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::Key {
            key: Key::C,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.input_mut().drain_written(), b"\x1b[99;5u");
}

#[test]
fn mid_frame_glyph_atlas_exhaustion_completes_without_deadlock_and_recovers() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 900.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::new());

    // Initialize fonts via first frame before querying `has_glyph`.
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });

    // Build rows packed with supported Unicode characters across Latin, Greek/Cyrillic,
    // Punctuation/Symbols, Dingbats, and Emoji so large font sizes exhaust the GlyphAtlas.
    let mut unicode_chars: Vec<char> = Vec::new();
    for range in [
        0x0021u32..=0x024F,
        0x0370u32..=0x04FF,
        0x2000u32..=0x23FF,
        0x2600u32..=0x27BF,
        0x1F300u32..=0x1F64F,
    ] {
        for cp in range {
            if let Some(ch) = char::from_u32(cp) {
                let has_glyph = ctx.fonts_mut(|f| {
                    let fid = egui::FontId::new(24.0, egui::FontFamily::Monospace);
                    f.has_glyph(&fid, ch)
                });
                if has_glyph {
                    unicode_chars.push(ch);
                }
            }
        }
    }
    assert!(!unicode_chars.is_empty());

    let mut payload = String::new();
    for (idx, ch) in unicode_chars.iter().enumerate() {
        payload.push(*ch);
        if (idx + 1) % 40 == 0 {
            payload.push_str("\r\n");
        }
    }

    let mut time = 0.05;
    let mut saw_exhausted_or_reset = false;
    let mut prev_fill = 0.0_f32;
    let mut prev_h = 0usize;

    for font_size in [32.0_f32, 48.0, 64.0, 84.0, 104.0, 124.0] {
        rt.config_mut().font_size = font_size;
        rt.input_mut().push_str("\x1b[2J\x1b[H");
        rt.input_mut().push_str(&payload);
        let _ = run_frame_with(&ctx, screen, 1.0, time, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        });
        time += 0.05;
        let (fill, h) = ctx.fonts_mut(|f| (f.font_atlas_fill_ratio(), f.font_image_size()[1]));
        if fill >= 0.80 || fill < prev_fill || h > prev_h {
            saw_exhausted_or_reset = true;
        }
        prev_fill = fill;
        prev_h = h;
    }
    assert!(
        saw_exhausted_or_reset,
        "expected font atlas fill growth/reset/exhaustion during large-font Unicode stress"
    );

    // Run a repaint frame at normal size and verify rendered glyphs have valid UVs matching fonts.
    rt.config_mut().font_size = 14.0;
    rt.input_mut().push_str("\x1b[2J\x1b[HPOST_EXHAUST_OK\r\n");
    let _ = run_frame_with(&ctx, screen, 1.0, time, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    time += 0.05;
    let (clipped, _) = run_frame_with(&ctx, screen, 1.0, time, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });

    assert_eq!(
        rt.terminal().visible_line_text(0).as_deref(),
        Some("POST_EXHAUST_OK")
    );
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    let post_mesh = find_text_mesh(&shapes).expect("POST_EXHAUST_OK text mesh after recovery");
    assert_text_mesh_uvs_match_fresh_fonts(
        &ctx,
        post_mesh,
        "POST_EXHAUST_OK",
        "POST_EXHAUST_OK recovery",
    );
}

fn assert_text_mesh_uvs_match_fresh_fonts(
    ctx: &Context,
    mesh: &egui::epaint::Mesh,
    expected_text: &str,
    label: &str,
) {
    let atlas_size = ctx.fonts_mut(|f| f.font_image_size());
    let tex_w = atlas_size[0] as f32;
    let tex_h = atlas_size[1] as f32;
    let text_chars: Vec<char> = expected_text.chars().collect();
    let verts = &mesh.vertices;
    assert_eq!(
        verts.len(),
        text_chars.len() * 4,
        "{label}: expected 4 vertices per char"
    );

    for (idx, &ch) in text_chars.iter().enumerate() {
        let mut s = [0u8; 4];
        let ch_str = ch.encode_utf8(&mut s);
        let ref_galley = ctx.fonts_mut(|f| {
            let mut job = egui::text::LayoutJob::default();
            job.append(
                ch_str,
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::new(14.0, egui::FontFamily::Monospace),
                    ..Default::default()
                },
            );
            f.layout_job(job)
        });
        let got_uvs: Vec<[f32; 2]> = verts[idx * 4..(idx + 1) * 4]
            .iter()
            .map(|v| [v.uv.x, v.uv.y])
            .collect();
        let ref_uvs: Vec<[f32; 2]> = ref_galley.rows[0]
            .row
            .visuals
            .mesh
            .vertices
            .iter()
            .map(|v| [v.uv.x / tex_w, v.uv.y / tex_h])
            .collect();
        assert_eq!(got_uvs.len(), ref_uvs.len());
        for (got, expected) in got_uvs.iter().zip(ref_uvs.iter()) {
            assert!(
                (got[0] - expected[0]).abs() <= 1e-5 && (got[1] - expected[1]).abs() <= 1e-5,
                "{label}: UV mismatch for char {ch:?}: got {got:?}, expected {expected:?}"
            );
            assert!(
                got[0].is_finite()
                    && got[1].is_finite()
                    && got[0] >= 0.0
                    && got[1] >= 0.0
                    && got[0] <= 1.0
                    && got[1] <= 1.0,
                "{label}: normalized UV ({}, {}) out of 0..=1 bounds",
                got[0],
                got[1]
            );
        }
    }
}

#[test]
fn two_widget_sibling_atlas_clear_invalidates_terminal_uvs() {
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 900.0));

    // Probe supported Unicode characters in an isolated Context so `ctx` starts clean.
    let probe_ctx = Context::default();
    let _ = run_frame_with(&probe_ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        ui.label("init");
    });
    let mut non_hello_chars: Vec<char> = Vec::new();
    for range in [
        0x0021u32..=0x024F,
        0x0370u32..=0x04FF,
        0x2000u32..=0x23FF,
        0x2600u32..=0x27BF,
        0x1F300u32..=0x1F64F,
    ] {
        for cp in range {
            if let Some(ch) = char::from_u32(cp)
                && !"HELLO_WORLD".contains(ch)
            {
                let has_glyph = probe_ctx.fonts_mut(|f| {
                    let fid = egui::FontId::new(24.0, egui::FontFamily::Monospace);
                    f.has_glyph(&fid, ch)
                });
                if has_glyph {
                    non_hello_chars.push(ch);
                }
            }
        }
    }
    assert!(
        non_hello_chars.len() >= 120,
        "expected >= 120 supported glyphs, got {}",
        non_hello_chars.len()
    );

    let chunks: Vec<String> = non_hello_chars
        .chunks(32)
        .map(|c| c.iter().collect())
        .collect();
    let pre_term_chunks: Vec<String> = non_hello_chars
        .iter()
        .take(120)
        .collect::<Vec<_>>()
        .chunks(20)
        .map(|c| c.iter().copied().collect())
        .collect();

    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"HELLO_WORLD\r\n"));
    rt.config_mut().cursor_blink = false;

    // Frame 0: render terminal first, record (fill0, h0), then push sibling labels > 0.80 fill.
    let mut fill0 = 0.0_f32;
    let mut h0 = 0usize;
    let mut fill_after_frame0 = 0.0_f32;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.allocate_ui(vec2(600.0, 200.0), |ui| {
                let _ = rt.show(ui);
            });
            let (f0, height0) = ui
                .ctx()
                .fonts_mut(|f| (f.font_atlas_fill_ratio(), f.font_image_size()[1]));
            fill0 = f0;
            h0 = height0;

            'outer: for size in [48.0_f32, 72.0, 96.0] {
                for chunk in &chunks {
                    ui.label(egui::RichText::new(chunk).size(size));
                    if ui.ctx().fonts_mut(|f| f.font_atlas_fill_ratio() > 0.80) {
                        break 'outer;
                    }
                }
            }
            fill_after_frame0 = ui.ctx().fonts_mut(|f| f.font_atlas_fill_ratio());
        });
    });
    assert!(
        fill_after_frame0 > 0.80,
        "expected Frame 0 sibling labels to push atlas fill > 0.80, got {fill_after_frame0}"
    );

    // Frame 1: `begin_pass` clears GlyphAtlas & galley_caches. Render sibling labels *before* `rt`
    // until `curr_fill >= fill0 && curr_height >= h0`, then render `rt` with unchanged text.
    let mut pre_rt_fill = 0.0_f32;
    let mut pre_rt_h = 0usize;
    let (clipped1, _) = run_frame_with(&ctx, screen, 1.0, 0.05, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            for chunk in &pre_term_chunks {
                ui.label(egui::RichText::new(chunk).size(48.0));
                let (cf, ch) = ui
                    .ctx()
                    .fonts_mut(|f| (f.font_atlas_fill_ratio(), f.font_image_size()[1]));
                if cf >= fill0 && ch >= h0 {
                    break;
                }
            }
            let (pf, ph) = ui
                .ctx()
                .fonts_mut(|f| (f.font_atlas_fill_ratio(), f.font_image_size()[1]));
            pre_rt_fill = pf;
            pre_rt_h = ph;

            ui.allocate_ui(vec2(600.0, 200.0), |ui| {
                let _ = rt.show(ui);
            });
        });
    });
    assert!(
        pre_rt_fill >= fill0 && pre_rt_h >= h0,
        "test setup must reproduce fill >= fill0 ({pre_rt_fill} >= {fill0}) and h >= h0 ({pre_rt_h} >= {h0})"
    );
    assert_eq!(
        rt.terminal().visible_line_text(0).as_deref(),
        Some("HELLO_WORLD")
    );
    let mut shapes1 = Vec::new();
    collect_shapes(&clipped1, &mut shapes1);
    let frame1_mesh = find_text_mesh(&shapes1).expect("HELLO_WORLD text mesh on Frame 1");
    assert_text_mesh_uvs_match_fresh_fonts(&ctx, frame1_mesh, "HELLO_WORLD", "Frame 1");

    // Frame 2: steady-state normal frame (no atlas clear); verify UVs, then confirm Frame 2b
    // reuses the cached text Mesh Arc without spurious invalidation.
    let (clipped2, _) = run_frame_with(&ctx, screen, 1.0, 0.10, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.allocate_ui(vec2(600.0, 200.0), |ui| {
                let _ = rt.show(ui);
            });
        });
    });
    let mut shapes2 = Vec::new();
    collect_shapes(&clipped2, &mut shapes2);
    let frame2_mesh = find_text_mesh(&shapes2).expect("HELLO_WORLD text mesh on Frame 2");
    assert_text_mesh_uvs_match_fresh_fonts(&ctx, frame2_mesh, "HELLO_WORLD", "Frame 2");

    let (clipped2b, _) = run_frame_with(&ctx, screen, 1.0, 0.12, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.allocate_ui(vec2(600.0, 200.0), |ui| {
                let _ = rt.show(ui);
            });
        });
    });
    let mut shapes2b = Vec::new();
    collect_shapes(&clipped2b, &mut shapes2b);
    let frame2b_mesh = find_text_mesh(&shapes2b).expect("HELLO_WORLD text mesh on Frame 2b");
    assert!(
        std::sync::Arc::ptr_eq(frame2_mesh, frame2b_mesh),
        "steady-state Frame 2b should reuse cached text Mesh Arc without spurious invalidation"
    );

    // Frame 3: `ctx.set_fonts(...)` replaces Fonts/galley_caches in-place; UVs must stay valid.
    let mut new_defs = egui::FontDefinitions::default();
    if let Some(mono) = new_defs.families.get(&egui::FontFamily::Monospace).cloned() {
        new_defs
            .families
            .insert(egui::FontFamily::Name("alt_mono".into()), mono);
    }
    ctx.set_fonts(new_defs);

    let (clipped3, _) = run_frame_with(&ctx, screen, 1.0, 0.15, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.allocate_ui(vec2(600.0, 200.0), |ui| {
                let _ = rt.show(ui);
            });
        });
    });
    let mut shapes3 = Vec::new();
    collect_shapes(&clipped3, &mut shapes3);
    let frame3_mesh = find_text_mesh(&shapes3).expect("HELLO_WORLD text mesh on Frame 3");
    assert_text_mesh_uvs_match_fresh_fonts(&ctx, frame3_mesh, "HELLO_WORLD", "Frame 3 (set_fonts)");
}

#[test]
fn context_menu_copy_over_term_rect_preserves_selection_and_emits_copy_text() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"alpha beta gamma\r\n"));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    // Programmatically select "alpha" (matching user drag selection).
    rt.terminal_mut()
        .selection_start(0, 0, egui_xterm::SelectionKind::Stream);
    rt.terminal_mut().selection_update(0, 4);
    rt.terminal_mut().selection_finish();
    assert_eq!(rt.terminal().selection_text().as_deref(), Some("alpha"));

    // Right-click inside term_rect to open context menu.
    let menu_anchor = term_rect.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(menu_anchor),
            Event::PointerButton {
                pos: menu_anchor,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: menu_anchor,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    // Run a frame to locate the rendered "Copy" button inside the context menu popup.
    let (menu_clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.04, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut menu_shapes = Vec::new();
    collect_shapes(&menu_clipped, &mut menu_shapes);
    let copy_rect = menu_shapes
        .iter()
        .find_map(|s| match s {
            Shape::Text(ts) if ts.galley.job.text == "Copy" => {
                Some(ts.galley.rect.translate(ts.pos.to_vec2()))
            }
            _ => None,
        })
        .expect("Copy menu item must be rendered");
    assert!(
        term_rect.contains(copy_rect.center()),
        "test requires Copy menu item ({copy_rect:?}) to overlap term_rect ({term_rect:?})"
    );

    // Left-click "Copy" inside the popup over term_rect -> must emit CopyText("alpha").
    let copy_center = copy_rect.center();
    let (_, copy_plat) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![
            Event::PointerMoved(copy_center),
            Event::PointerButton {
                pos: copy_center,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: copy_center,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(
        copy_plat
            .commands
            .iter()
            .any(|c| matches!(c, OutputCommand::CopyText(s) if s == "alpha")),
        "expected CopyText(\"alpha\"), got {:?}",
        copy_plat.commands
    );
}

#[cfg(feature = "search")]
#[test]
fn search_bar_does_not_steal_focus_or_close_on_later_frames_after_clicking_terminal() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hit one\r\nhit two\r\n"));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    // Open search bar and type "hit".
    rt.open_search();
    let _ = run_frame_with(&ctx, screen, 1.0, 0.02, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::Text("hit".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.search_bar_mut().open);

    // Click back into the terminal grid (away from the top-right search bar).
    let grid_click = pos2(term_rect.min.x + 20.0, term_rect.center().y);
    let mut term_focused = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![
            Event::PointerMoved(grid_click),
            Event::PointerButton {
                pos: grid_click,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: grid_click,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                term_focused = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(term_focused);
    assert!(rt.search_bar_mut().open);

    // Frame N+1 and Frame N+2: Enter and Escape must go to the terminal (not stolen by search bar).
    for (frame_idx, t) in [(1usize, 0.08_f64), (2usize, 0.10_f64)] {
        let mut foc = false;
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            t,
            vec![
                Event::Key {
                    key: Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
                Event::Key {
                    key: Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    foc = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(foc, "terminal lost focus on Frame N+{frame_idx}");
        assert!(
            rt.search_bar_mut().open,
            "search bar unexpectedly closed on Frame N+{frame_idx}"
        );
        assert_eq!(
            rt.input_mut().drain_written(),
            b"\r\x1b",
            "expected Enter+Escape bytes in terminal on Frame N+{frame_idx}"
        );
    }
}

#[cfg(feature = "search")]
#[test]
fn search_bar_controls_not_clipped_and_match_count_updates_immediately() {
    for (w, h) in [(800.0_f32, 400.0_f32), (240.0_f32, 200.0_f32)] {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(w, h));
        let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(
            b"hit one\r\nHit two\r\nhit three\r\n",
        ));

        let mut term_rect = Rect::NOTHING;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                term_rect = rt.show(ui).response.rect;
            });
        });

        rt.open_search();
        let _ = run_frame_with(&ctx, screen, 1.0, 0.02, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        });

        // Type "hit" and verify "0/3" (not "0/0") and all controls including "x" appear inside term_rect.
        let (clipped, _) = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.04,
            vec![Event::Text("hit".to_owned())],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        let mut shapes = Vec::new();
        collect_shapes(&clipped, &mut shapes);

        let mut x_rect = None;
        for required in ["Aa", ".*", "0/3", "<", ">", "x"] {
            let found_rect = shapes.iter().find_map(|s| match s {
                Shape::Text(ts) if ts.galley.job.text == required => {
                    Some(ts.galley.rect.translate(ts.pos.to_vec2()))
                }
                _ => None,
            });
            let r = found_rect
                .unwrap_or_else(|| panic!("missing search bar control {required:?} at {w}x{h}"));
            assert!(
                r.min.x >= term_rect.min.x - 1.0
                    && r.max.x <= term_rect.max.x + 1.0
                    && r.min.y >= term_rect.min.y - 1.0
                    && r.max.y <= term_rect.max.y + 1.0,
                "control {required:?} rect {r:?} clipped outside term_rect {term_rect:?} at {w}x{h}"
            );
            if required == "x" {
                x_rect = Some(r);
            }
        }

        let x_pos = x_rect.expect("x_rect").center();
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.06,
            vec![
                Event::PointerMoved(x_pos),
                Event::PointerButton {
                    pos: x_pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: x_pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        assert!(
            !rt.search_bar_mut().open,
            "clicking 'x' must close search bar at {w}x{h}"
        );
    }
}

#[cfg(feature = "search")]
#[test]
fn search_bar_narrow_widths_unclipped_and_step_buttons_update_same_frame() {
    let mut payload = String::new();
    for i in 0..120 {
        payload.push_str(&format!("hit_{i:03}\r\n"));
    }

    for (w, h) in [
        (160.0_f32, 120.0_f32),
        (190.0_f32, 200.0_f32),
        (220.0_f32, 200.0_f32),
        (240.0_f32, 200.0_f32),
        (800.0_f32, 400.0_f32),
    ] {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(w, h));
        let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(payload.as_bytes()));

        let mut term_rect = Rect::NOTHING;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                term_rect = rt.show(ui).response.rect;
            });
        });

        rt.open_search();
        let _ = run_frame_with(&ctx, screen, 1.0, 0.02, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        });

        // Type "hit_" -> jumps 0 -> 120 matches on this frame.
        let (clipped, _) = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.04,
            vec![Event::Text("hit_".to_owned())],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        let mut shapes = Vec::new();
        collect_shapes(&clipped, &mut shapes);

        let find_ctrl = |shapes: &[Shape], label: &str| -> Rect {
            shapes
                .iter()
                .find_map(|s| match s {
                    Shape::Text(ts) if ts.galley.job.text == label => {
                        Some(ts.galley.rect.translate(ts.pos.to_vec2()))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing search control {label:?} at {w}x{h}"))
        };

        let assert_contained = |r: Rect, label: &str| {
            assert!(
                r.min.x >= term_rect.min.x
                    && r.max.x <= term_rect.max.x
                    && r.min.y >= term_rect.min.y
                    && r.max.y <= term_rect.max.y,
                "control {label:?} rect {r:?} overflowed term_rect {term_rect:?} at {w}x{h}"
            );
        };

        assert_contained(find_ctrl(&shapes, "0/120"), "0/120");
        let prev_rect = find_ctrl(&shapes, "<");
        let next_rect = find_ctrl(&shapes, ">");
        let close_rect = find_ctrl(&shapes, "x");
        assert_contained(prev_rect, "<");
        assert_contained(next_rect, ">");
        assert_contained(close_rect, "x");
        if w >= 240.0 {
            assert_contained(find_ctrl(&shapes, "Aa"), "Aa");
            assert_contained(find_ctrl(&shapes, ".*"), ".*");
        }

        let click_at =
            |t: f64, pos: Pos2, rt: &mut TerminalRuntime<RingBufferInput>| -> Vec<Shape> {
                let (c, _) = run_frame_with(
                    &ctx,
                    screen,
                    1.0,
                    t,
                    vec![
                        Event::PointerMoved(pos),
                        Event::PointerButton {
                            pos,
                            button: PointerButton::Primary,
                            pressed: true,
                            modifiers: Modifiers::NONE,
                        },
                        Event::PointerButton {
                            pos,
                            button: PointerButton::Primary,
                            pressed: false,
                            modifiers: Modifiers::NONE,
                        },
                    ],
                    |ui| {
                        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                            let _ = rt.show(ui);
                        });
                    },
                );
                let mut out = Vec::new();
                collect_shapes(&c, &mut out);
                out
            };

        // Click ">" -> same frame must immediately show "1/120".
        let s1 = click_at(0.06, next_rect.center(), &mut rt);
        assert_contained(find_ctrl(&s1, "1/120"), "1/120");
        assert_contained(find_ctrl(&s1, "x"), "x after >");

        // Click ">" again -> same frame must immediately show "2/120".
        let s2 = click_at(0.08, next_rect.center(), &mut rt);
        assert_contained(find_ctrl(&s2, "2/120"), "2/120");

        // Click "<" -> same frame must immediately show "1/120".
        let s3 = click_at(0.10, prev_rect.center(), &mut rt);
        assert_contained(find_ctrl(&s3, "1/120"), "1/120 after <");

        // Click "x" -> closes search bar.
        let _ = click_at(0.12, close_rect.center(), &mut rt);
        assert!(
            !rt.search_bar_mut().open,
            "clicking 'x' must close search bar at {w}x{h}"
        );
    }
}

#[cfg(feature = "kitty-keyboard")]
#[test]
fn kitty_keyboard_event_copy_without_selection_sends_csi_u_non_mac() {
    let ctx = Context::default();
    ctx.set_os(egui::os::OperatingSystem::Windows);
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[>1u"));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    let c = term_rect.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    let (_, plat) = run_frame_with(&ctx, screen, 1.0, 0.04, vec![Event::Copy], |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    assert!(
        !plat
            .commands
            .iter()
            .any(|cmd| matches!(cmd, OutputCommand::CopyText(_)))
    );
    assert_eq!(rt.input_mut().drain_written(), b"\x1b[99;5u");
}

#[test]
fn scrollbar_drag_released_outside_track_over_term_rect_emits_no_stray_mouse_release() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
    let mut data = String::new();
    for i in 0..80 {
        data.push_str(&format!("line {i:02} content\r\n"));
    }
    data.push_str("\x1b[?1000;1006h");
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(data.as_bytes()));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });
    assert!(rt.terminal().scrollback_len() > 0);
    let _ = rt.input_mut().drain_written();

    let sb_x = term_rect.max.x - 2.0;
    let p_bottom = pos2(sb_x, term_rect.max.y - 10.0);
    let p_mid_term = term_rect.center();

    // Frame 1: press on scrollbar track.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(p_bottom),
            Event::PointerButton {
                pos: p_bottom,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    // Frame 2: drag out of track_rect over term_rect.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::PointerMoved(p_mid_term)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    // Frame 3: release primary button while pointer is over term_rect (outside track_rect).
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![Event::PointerButton {
            pos: p_mid_term,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    assert!(
        rt.input_mut().drain_written().is_empty(),
        "releasing scrollbar drag over term_rect must not emit stray mouse release bytes"
    );
    assert!(rt.terminal().selection().is_none());
}

#[cfg(feature = "search")]
#[test]
fn search_bar_1250_matches_unclipped_across_narrow_viewports() {
    fn find_text_with_clip(
        clipped: &[egui::epaint::ClippedShape],
        target: &str,
    ) -> Option<(Rect, Rect)> {
        fn visit(shape: &Shape, clip: Rect, target: &str) -> Option<(Rect, Rect)> {
            match shape {
                Shape::Vec(v) => v.iter().find_map(|s| visit(s, clip, target)),
                Shape::Text(ts) if ts.galley.job.text == target => {
                    Some((ts.galley.rect.translate(ts.pos.to_vec2()), clip))
                }
                _ => None,
            }
        }
        clipped
            .iter()
            .find_map(|cs| visit(&cs.shape, cs.clip_rect, target))
    }

    let mut payload_120 = String::new();
    for i in 0..120 {
        payload_120.push_str(&format!("hit_{i:03}\r\n"));
    }
    let mut payload_1250 = String::new();
    for i in 0..1250 {
        payload_1250.push_str(&format!("hit_{i:04}\r\n"));
    }

    for (w, h) in [
        (160.0_f32, 120.0_f32),
        (190.0_f32, 200.0_f32),
        (216.0_f32, 200.0_f32),
        (220.0_f32, 200.0_f32),
        (240.0_f32, 200.0_f32),
        (800.0_f32, 400.0_f32),
    ] {
        for (payload, query, step_clicks, expected_after_steps) in [
            (payload_120.as_bytes(), "nomatch", 0usize, "0/0"),
            (payload_120.as_bytes(), "hit_", 2usize, "2/120"),
            (payload_1250.as_bytes(), "hit_", 2usize, "1249/1250"),
        ] {
            let ctx = Context::default();
            let screen = Rect::from_min_size(Pos2::ZERO, vec2(w, h));
            let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(payload));

            let mut term_rect = Rect::NOTHING;
            let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    term_rect = rt.show(ui).response.rect;
                });
            });

            rt.open_search();
            let _ = run_frame_with(&ctx, screen, 1.0, 0.02, Vec::new(), |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            });

            let (mut last_clipped, _) = run_frame_with(
                &ctx,
                screen,
                1.0,
                0.04,
                vec![Event::Text(query.to_owned())],
                |ui| {
                    CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                        let _ = rt.show(ui);
                    });
                },
            );

            // Step backward "<" for 1250 matches to wrap to "1249/1250", or forward ">" for 120.
            let step_btn = if expected_after_steps == "1249/1250" {
                "<"
            } else {
                ">"
            };
            for step_idx in 0..step_clicks {
                let (btn_rect, _) = find_text_with_clip(&last_clipped, step_btn)
                    .unwrap_or_else(|| panic!("missing {step_btn:?} at {w}x{h}"));
                let p = btn_rect.center();
                let t = 0.06 + (step_idx as f64) * 0.02;
                let (c, _) = run_frame_with(
                    &ctx,
                    screen,
                    1.0,
                    t,
                    vec![
                        Event::PointerMoved(p),
                        Event::PointerButton {
                            pos: p,
                            button: PointerButton::Primary,
                            pressed: true,
                            modifiers: Modifiers::NONE,
                        },
                        Event::PointerButton {
                            pos: p,
                            button: PointerButton::Primary,
                            pressed: false,
                            modifiers: Modifiers::NONE,
                        },
                    ],
                    |ui| {
                        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                            let _ = rt.show(ui);
                        });
                    },
                );
                last_clipped = c;
            }

            let mut required = vec!["<", ">", "x", expected_after_steps];
            if w >= 240.0 {
                required.push("Aa");
                required.push(".*");
            }
            for label in required {
                let (rect, clip) = find_text_with_clip(&last_clipped, label).unwrap_or_else(|| {
                    panic!("missing {label:?} at {w}x{h} for {expected_after_steps}")
                });
                assert!(
                    rect.min.x >= clip.min.x
                        && rect.max.x <= clip.max.x
                        && rect.min.y >= clip.min.y
                        && rect.max.y <= clip.max.y,
                    "control {label:?} rect {rect:?} clipped by {clip:?} at {w}x{h} ({expected_after_steps})"
                );
                assert!(
                    rect.min.x >= term_rect.min.x
                        && rect.max.x <= term_rect.max.x
                        && rect.min.y >= term_rect.min.y
                        && rect.max.y <= term_rect.max.y,
                    "control {label:?} rect {rect:?} overflowed term_rect {term_rect:?} at {w}x{h} ({expected_after_steps})"
                );
            }
        }
    }
}

#[test]
fn shift_right_click_context_menu_stays_open_after_shift_release_under_mouse_tracking() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"alpha\r\n\x1b[?1000;1006h"));

    let mut term_rect = Rect::NOTHING;
    let mut cell_size = egui::Vec2::ZERO;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let r = rt.show(ui);
            term_rect = r.response.rect;
            cell_size = r.cell_size;
        });
    });
    let _ = rt.input_mut().drain_written();

    // Drag-select "alpha" with Shift held to bypass mouse tracking.
    let p_start = pos2(
        term_rect.min.x + 0.5 * cell_size.x,
        term_rect.min.y + 0.5 * cell_size.y,
    );
    let p_end = pos2(
        term_rect.min.x + 5.5 * cell_size.x,
        term_rect.min.y + 0.5 * cell_size.y,
    );
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::ModifiersChanged(Modifiers::SHIFT),
            Event::PointerMoved(p_start),
            Event::PointerButton {
                pos: p_start,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::SHIFT,
            },
            Event::PointerMoved(p_end),
            Event::PointerButton {
                pos: p_end,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::SHIFT,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.terminal().selection_text().as_deref(), Some("alpha"));
    assert!(rt.input_mut().drain_written().is_empty());

    // Shift+RightClick opens context menu.
    let menu_pos = term_rect.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::ModifiersChanged(Modifiers::SHIFT),
            Event::PointerMoved(menu_pos),
            Event::PointerButton {
                pos: menu_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::SHIFT,
            },
            Event::PointerButton {
                pos: menu_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::SHIFT,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.input_mut().drain_written().is_empty());

    // Frame while Shift is still held: context menu renders "Copy".
    let (c_open, _) = run_frame_with(&ctx, screen, 1.0, 0.06, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut s_open = Vec::new();
    collect_shapes(&c_open, &mut s_open);
    assert!(
        all_text(&s_open).contains("Copy"),
        "expected Copy menu item while Shift held"
    );
    assert!(rt.input_mut().drain_written().is_empty());

    // Release Shift on next frame; context menu must remain open!
    let (c_released, _) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.08,
        vec![Event::ModifiersChanged(Modifiers::NONE)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let mut s_released = Vec::new();
    collect_shapes(&c_released, &mut s_released);
    let copy_rect = s_released
        .iter()
        .find_map(|s| match s {
            Shape::Text(ts) if ts.galley.job.text == "Copy" => {
                Some(ts.galley.rect.translate(ts.pos.to_vec2()))
            }
            _ => None,
        })
        .expect("Copy menu item vanished when Shift was released");
    assert!(rt.input_mut().drain_written().is_empty());

    // Click "Copy" and verify CopyText("alpha") is emitted with zero leaked SGR bytes.
    let copy_center = copy_rect.center();
    let (_, plat) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.10,
        vec![
            Event::PointerMoved(copy_center),
            Event::PointerButton {
                pos: copy_center,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: copy_center,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(
        plat.commands
            .iter()
            .any(|c| matches!(c, OutputCommand::CopyText(s) if s == "alpha")),
        "expected CopyText(\"alpha\"), got {:?}",
        plat.commands
    );
    assert!(rt.input_mut().drain_written().is_empty());
}

#[cfg(feature = "search")]
#[test]
fn ime_platform_output_emitted_when_terminal_focused_with_search_bar_open() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hello world\r\n"));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    rt.open_search();
    let _ = run_frame_with(&ctx, screen, 1.0, 0.02, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });

    // Click back onto the terminal grid while search bar remains open.
    let grid_pos = pos2(term_rect.min.x + 20.0, term_rect.center().y);
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::PointerMoved(grid_pos),
            Event::PointerButton {
                pos: grid_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: grid_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.search_bar_mut().open);

    // Send IME preedit -> both inline preedit and PlatformOutput::ime must be active.
    let (clipped, plat) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![Event::Ime(ImeEvent::Preedit {
            text: "nihao".to_owned(),
            active_range_chars: Some(0..5),
        })],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    assert!(all_text(&shapes).contains("nihao"));
    assert!(
        plat.ime.is_some(),
        "expected PlatformOutput::ime when terminal owns focus with background search bar open"
    );
}

#[cfg(feature = "search")]
#[test]
fn search_bar_margin_and_label_click_preserves_focus_for_escape_close() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hit one\r\nhit two\r\n"));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    rt.open_search();
    let _ = run_frame_with(&ctx, screen, 1.0, 0.02, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let (clipped, _) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::Text("hit".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    let count_center = shapes
        .iter()
        .find_map(|s| match s {
            Shape::Text(ts) if ts.galley.job.text == "0/2" => {
                Some(ts.galley.rect.translate(ts.pos.to_vec2()).center())
            }
            _ => None,
        })
        .expect("missing 0/2 label");

    // Click the non-interactive count label ("0/2") first, then re-open and click margin inside bar_rect.
    let margin_pos = pos2(term_rect.max.x - 10.0, term_rect.min.y + 10.0);
    for (idx, click_pos) in [count_center, margin_pos].into_iter().enumerate() {
        let base_t = 0.06 + (idx as f64) * 0.10;
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            base_t,
            vec![
                Event::PointerMoved(click_pos),
                Event::PointerButton {
                    pos: click_pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: click_pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        assert!(rt.search_bar_mut().open);

        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            base_t + 0.02,
            vec![Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        assert!(
            !rt.search_bar_mut().open,
            "Escape must close search bar after clicking inside bar_rect (case {idx})"
        );
        assert!(
            rt.input_mut().drain_written().is_empty(),
            "Escape must not leak into terminal (case {idx})"
        );

        // Re-open for second case (count label click).
        if idx == 0 {
            rt.open_search();
            let _ = run_frame_with(&ctx, screen, 1.0, base_t + 0.04, Vec::new(), |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            });
        }
    }
}

#[cfg(feature = "search")]
#[test]
fn open_search_on_tiny_pane_does_not_trap_terminal_focus() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(100.0, 30.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004h"));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    // Frame 1: click inside 100x30 terminal -> focus gained and DEC ?1004 `\x1b[I` written.
    let c = term_rect.center();
    let mut f1_focus = false;
    let mut term_id = egui::Id::NULL;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let r = rt.show(ui).response;
                f1_focus = r.has_focus();
                term_id = r.id;
            });
        },
    );
    assert!(f1_focus, "Frame 1 click must focus tiny terminal");
    assert_eq!(rt.input_mut().drain_written(), b"\x1b[I");

    // Frame 2: open_search() on <120x36 pane + immediate Event::Text("z") must NOT drop "z",
    // must NOT emit spurious `\x1b[O`, and must keep TERMINAL_FOCUS_FILTER active.
    rt.open_search();
    let mut f2_focus = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::Text("z".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                f2_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(f2_focus, "Frame 2 must keep terminal focus on <120x36 pane");
    assert_eq!(rt.input_mut().drain_written(), b"z");
    assert!(
        ctx.memory(|m| m.focused() == Some(term_id)),
        "terminal id must remain focused in egui memory"
    );

    // Frame 3: Event::Text("abc") + Key::Escape must stay focused and emit `b"abc\x1b"`.
    let mut f3_focus = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![
            Event::Text("abc".to_owned()),
            Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                f3_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(
        f3_focus,
        "Frame 3 Escape must not blur terminal (TERMINAL_FOCUS_FILTER must stay active)"
    );
    assert_eq!(rt.input_mut().drain_written(), b"abc\x1b");

    // Frame 4: Key::Tab must also remain locked to the terminal.
    let mut f4_focus = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.08,
        vec![Event::Key {
            key: Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                f4_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(f4_focus, "Frame 4 Tab must not blur terminal");
    assert_eq!(rt.input_mut().drain_written(), b"\t");
}

#[cfg(feature = "search")]
#[test]
fn tiny_pane_open_search_immediate_escape_and_ctrl_shift_f_and_widen_roundtrip() {
    // Part A: Immediate Key::Escape on Frame 2 right after Frame 1 click + open_search().
    {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(100.0, 30.0));
        let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004h"));

        let mut term_rect = Rect::NOTHING;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                term_rect = rt.show(ui).response.rect;
            });
        });

        let c = term_rect.center();
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.02,
            vec![
                Event::PointerMoved(c),
                Event::PointerButton {
                    pos: c,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: c,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        assert_eq!(rt.input_mut().drain_written(), b"\x1b[I");

        rt.open_search();
        let mut f2_focus = false;
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.04,
            vec![Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    f2_focus = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(
            f2_focus,
            "Frame 2 immediate Escape after open_search() on <120x36 must not blur"
        );
        assert_eq!(rt.input_mut().drain_written(), b"\x1b");
    }

    // Part B: Ctrl+Shift+F on <120x36 pane keeps focus, then widening to 400x200 renders bar
    // and shrinking to 100x30 allows immediate click + text on the shrink frame itself.
    {
        let ctx = Context::default();
        let tiny = Rect::from_min_size(Pos2::ZERO, vec2(100.0, 30.0));
        let wide = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
        let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hello\r\n"));

        let mut term_rect = Rect::NOTHING;
        let _ = run_frame_with(&ctx, tiny, 1.0, 0.0, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                term_rect = rt.show(ui).response.rect;
            });
        });

        let c = term_rect.center();
        let _ = run_frame_with(
            &ctx,
            tiny,
            1.0,
            0.02,
            vec![
                Event::PointerMoved(c),
                Event::PointerButton {
                    pos: c,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: c,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );

        // Press Ctrl+Shift+F on tiny pane.
        let mut f_focus = false;
        let _ = run_frame_with(
            &ctx,
            tiny,
            1.0,
            0.04,
            vec![Event::Key {
                key: Key::F,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL | Modifiers::SHIFT,
            }],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    f_focus = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(f_focus);

        // Typing + Escape on next tiny frame still goes to terminal.
        let mut f_next_focus = false;
        let _ = run_frame_with(
            &ctx,
            tiny,
            1.0,
            0.06,
            vec![
                Event::Text("z".to_owned()),
                Event::Key {
                    key: Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    f_next_focus = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(f_next_focus);
        assert_eq!(rt.input_mut().drain_written(), b"z\x1b");

        // Widen to 400x200: search bar becomes visible while terminal keeps focus until bar is focused.
        let (wide_clipped, _) = run_frame_with(&ctx, wide, 1.0, 0.08, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        });
        let mut wide_shapes = Vec::new();
        collect_shapes(&wide_clipped, &mut wide_shapes);
        assert!(
            all_text(&wide_shapes).contains("Search..."),
            "widening back to >=120x36 should display open search bar"
        );

        // Focus search bar on wide pane, then shrink to 100x30 and click immediately on shrink frame.
        rt.open_search();
        let _ = run_frame_with(&ctx, wide, 1.0, 0.10, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        });
        let _ = run_frame_with(
            &ctx,
            wide,
            1.0,
            0.12,
            vec![Event::Text("he".to_owned())],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        assert_eq!(rt.search_bar_mut().query_text, "he");

        let tiny_center = pos2(50.0, 15.0);
        let mut shrink_focus = false;
        let _ = run_frame_with(
            &ctx,
            tiny,
            1.0,
            0.14,
            vec![
                Event::PointerMoved(tiny_center),
                Event::PointerButton {
                    pos: tiny_center,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: tiny_center,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    shrink_focus = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(
            shrink_focus,
            "clicking on the shrink-to-<120x36 frame itself must focus terminal"
        );
        let _ = run_frame_with(
            &ctx,
            tiny,
            1.0,
            0.16,
            vec![Event::Text("ok".to_owned())],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        assert_eq!(rt.input_mut().drain_written(), b"ok");
    }
}

#[cfg(feature = "search")]
#[test]
fn search_bar_horizontal_move_and_widen_do_not_steal_focus_from_stale_rect() {
    // Covers probe_i6_4a (move RIGHT & LEFT) and probe_i6_4b (resize WIDER + click old "x" pos).
    let click_events = |p: Pos2| {
        vec![
            Event::PointerMoved(p),
            Event::PointerButton {
                pos: p,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: p,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ]
    };

    // Part A (probe_i6_4a): horizontal move RIGHT (0..400 -> 60..460) and click at (64, 20).
    {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
        let pane_left = Rect::from_min_max(pos2(0.0, 0.0), pos2(400.0, 200.0));
        let pane_right = Rect::from_min_max(pos2(60.0, 0.0), pos2(460.0, 200.0));
        let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004hhello\r\n"));

        rt.open_search();
        for (i, evs) in [Vec::new(), vec![Event::Text("he".to_owned())]]
            .into_iter()
            .enumerate()
        {
            let _ = run_frame_with(&ctx, screen, 1.0, (i as f64) * 0.02, evs, |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    ui.scope_builder(UiBuilder::new().max_rect(pane_left), |ui| {
                        let _ = rt.show(ui);
                    });
                });
            });
        }
        assert_eq!(rt.search_bar_mut().query_text, "he");
        let _ = rt.input_mut().drain_written();

        // Move RIGHT so new bar_rect is ~72..452; click at (64, 20) inside new term_rect, outside new bar_rect,
        // overlapping old TextEdit rect.
        let mut focus1 = false;
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.04,
            click_events(pos2(64.0, 20.0)),
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    ui.scope_builder(UiBuilder::new().max_rect(pane_right), |ui| {
                        focus1 = rt.show(ui).response.has_focus();
                    });
                });
            },
        );
        assert!(
            focus1,
            "move RIGHT: click at (64, 20) outside new bar_rect must focus terminal"
        );
        assert_eq!(rt.input_mut().drain_written(), b"\x1b[I");

        let mut focus2 = false;
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.06,
            vec![Event::Text("a".to_owned())],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    ui.scope_builder(UiBuilder::new().max_rect(pane_right), |ui| {
                        focus2 = rt.show(ui).response.has_focus();
                    });
                });
            },
        );
        assert!(focus2, "move RIGHT: terminal must keep focus on next frame");
        assert_eq!(rt.input_mut().drain_written(), b"a");
        assert_eq!(rt.search_bar_mut().query_text, "he");

        // Move LEFT back to 0..400 while clicking a sibling area at (450, 20) where old ">" / "x" sat:
        // search bar must NOT spuriously close or steal focus.
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.08,
            click_events(pos2(450.0, 20.0)),
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    ui.scope_builder(UiBuilder::new().max_rect(pane_left), |ui| {
                        let _ = rt.show(ui);
                    });
                });
            },
        );
        assert!(
            rt.search_bar_mut().open,
            "move LEFT: click outside pane at old right-edge button spot must not close search bar"
        );
    }

    // Part B (probe_i6_4b): resize WIDER (400x200 -> 600x200) and click at (100, 20) outside new bar_rect,
    // plus clicking (384, 20) inside new bar_rect where old "x" button used to be.
    {
        let ctx = Context::default();
        let narrow = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
        let wide = Rect::from_min_size(Pos2::ZERO, vec2(600.0, 200.0));
        let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004hhello\r\n"));

        rt.open_search();
        for (i, evs) in [Vec::new(), vec![Event::Text("he".to_owned())]]
            .into_iter()
            .enumerate()
        {
            let _ = run_frame_with(&ctx, narrow, 1.0, (i as f64) * 0.02, evs, |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            });
        }
        let _ = rt.input_mut().drain_written();

        let mut focus1 = false;
        let _ = run_frame_with(
            &ctx,
            wide,
            1.0,
            0.04,
            click_events(pos2(100.0, 20.0)),
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    focus1 = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(
            focus1,
            "resize WIDER: click at (100, 20) outside new bar_rect must focus terminal"
        );
        assert_eq!(rt.input_mut().drain_written(), b"\x1b[I");

        let mut focus2 = false;
        let _ = run_frame_with(
            &ctx,
            wide,
            1.0,
            0.06,
            vec![Event::Text("b".to_owned())],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    focus2 = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(focus2);
        assert_eq!(rt.input_mut().drain_written(), b"b");
        assert_eq!(rt.search_bar_mut().query_text, "he");
    }

    // Part C: resize WIDER (400x200 -> 600x200) and click at (382, 20) (inside new TextEdit,
    // where old "x" button was on 400x200) must focus search bar without closing it.
    {
        let ctx = Context::default();
        let narrow = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
        let wide = Rect::from_min_size(Pos2::ZERO, vec2(600.0, 200.0));
        let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hello\r\n"));

        rt.open_search();
        for i in 0..2 {
            let _ = run_frame_with(&ctx, narrow, 1.0, (i as f64) * 0.02, Vec::new(), |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            });
        }
        let mut term_focus = true;
        let _ = run_frame_with(
            &ctx,
            wide,
            1.0,
            0.04,
            click_events(pos2(382.0, 20.0)),
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    term_focus = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(!term_focus);
        assert!(
            rt.search_bar_mut().open,
            "clicking inside new bar_rect at old 'x' location must not spuriously close search bar"
        );
    }
}

#[cfg(feature = "search")]
#[test]
fn search_bar_vertical_move_up_does_not_swallow_clicks_below_bar() {
    // Covers probe_i6_4c: vertical move UP (y=24..224 -> y=0..200) and click at (200, 52).
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(600.0, 300.0));
    let pane_low = Rect::from_min_max(pos2(0.0, 24.0), pos2(400.0, 224.0));
    let pane_high = Rect::from_min_max(pos2(0.0, 0.0), pos2(400.0, 200.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004hhello\r\n"));

    rt.open_search();
    for (i, evs) in [Vec::new(), vec![Event::Text("he".to_owned())]]
        .into_iter()
        .enumerate()
    {
        let _ = run_frame_with(&ctx, screen, 1.0, (i as f64) * 0.02, evs, |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.scope_builder(UiBuilder::new().max_rect(pane_low), |ui| {
                    let _ = rt.show(ui);
                });
            });
        });
    }
    assert_eq!(rt.search_bar_mut().query_text, "he");
    let _ = rt.input_mut().drain_written();

    let click_p = pos2(200.0, 52.0);
    let mut focus1 = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::PointerMoved(click_p),
            Event::PointerButton {
                pos: click_p,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: click_p,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.scope_builder(UiBuilder::new().max_rect(pane_high), |ui| {
                    focus1 = rt.show(ui).response.has_focus();
                });
            });
        },
    );
    assert!(
        focus1,
        "move UP: click at (200, 52) below visible bar_rect (y=8..38) must focus terminal"
    );
    assert_eq!(rt.input_mut().drain_written(), b"\x1b[I");

    let mut focus2 = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![Event::Text("c".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.scope_builder(UiBuilder::new().max_rect(pane_high), |ui| {
                    focus2 = rt.show(ui).response.has_focus();
                });
            });
        },
    );
    assert!(focus2);
    assert_eq!(rt.input_mut().drain_written(), b"c");
    assert_eq!(rt.search_bar_mut().query_text, "he");
}

#[cfg(feature = "search")]
#[test]
fn background_search_bar_widen_and_x_close_preserve_terminal_focus_under_dec_1004() {
    // Covers probe_i6_4d (unfocused background search bar + resize wider + click old bar spot)
    // and probe_i6_2 (clicking "x" close while terminal holds focus emits no `\x1b[O`/`\x1b[I` bounce).
    let ctx = Context::default();
    let narrow = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
    let wide = Rect::from_min_size(Pos2::ZERO, vec2(600.0, 200.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004hhello\r\n"));

    rt.open_search();
    let _ = run_frame_with(&ctx, narrow, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });

    // Click bottom-left (10, 150) to focus terminal while search bar remains open unfocused in background.
    let p_term = pos2(10.0, 150.0);
    let mut term_focus = false;
    let _ = run_frame_with(
        &ctx,
        narrow,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(p_term),
            Event::PointerButton {
                pos: p_term,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: p_term,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                term_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(term_focus);
    assert_eq!(rt.input_mut().drain_written(), b"\x1b[I");

    // Settle 1 frame.
    let _ = run_frame_with(&ctx, narrow, 1.0, 0.04, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    assert!(rt.input_mut().drain_written().is_empty());

    // Resize wider to 600x200 and click (100, 20) (112px left of new bar_rect ~212..592, inside old TextEdit rect).
    let p_old_bar = pos2(100.0, 20.0);
    let mut widen_focus = false;
    let _ = run_frame_with(
        &ctx,
        wide,
        1.0,
        0.06,
        vec![
            Event::PointerMoved(p_old_bar),
            Event::PointerButton {
                pos: p_old_bar,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: p_old_bar,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                widen_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(
        widen_focus,
        "background search bar must not steal focus when clicking (100, 20) after widening"
    );
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "must not emit spurious DEC ?1004 focus-out"
    );

    let mut next_focus = false;
    let _ = run_frame_with(
        &ctx,
        wide,
        1.0,
        0.08,
        vec![Event::Text("d".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                next_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(next_focus);
    assert_eq!(rt.input_mut().drain_written(), b"d");
    assert!(rt.search_bar_mut().query_text.is_empty());

    // Now locate the "x" close button on the 600x200 bar and click it while terminal holds focus:
    // search bar closes cleanly with zero `\x1b[O` / `\x1b[I` bounce.
    let (wide_clipped, _) = run_frame_with(&ctx, wide, 1.0, 0.10, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut wide_shapes = Vec::new();
    collect_shapes(&wide_clipped, &mut wide_shapes);
    let x_center = wide_shapes
        .iter()
        .find_map(|s| {
            if let Shape::Text(ts) = s
                && ts.galley.job.text == "x"
            {
                Some(ts.pos + ts.galley.size() * 0.5)
            } else {
                None
            }
        })
        .expect("find 'x' button glyph in search bar");

    let mut after_x_focus = false;
    let _ = run_frame_with(
        &ctx,
        wide,
        1.0,
        0.12,
        vec![
            Event::PointerMoved(x_center),
            Event::PointerButton {
                pos: x_center,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: x_center,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                after_x_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(
        !rt.search_bar_mut().open,
        "'x' button must close search bar"
    );
    assert!(
        after_x_focus,
        "terminal must retain focus after closing via 'x'"
    );
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "closing background search bar via 'x' while terminal holds focus must not emit DEC ?1004 bounce"
    );

    let _ = run_frame_with(
        &ctx,
        wide,
        1.0,
        0.14,
        vec![Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.input_mut().drain_written(), b"\x1b");
}

#[cfg(feature = "search")]
#[test]
fn multipane_shared_panel_search_bars_isolate_focus_text_and_buttons() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(900.0, 240.0));
    let pane_a = Rect::from_min_max(pos2(0.0, 0.0), pos2(420.0, 200.0));
    let pane_b = Rect::from_min_max(pos2(440.0, 0.0), pos2(860.0, 200.0));
    let mut rt_a = TerminalRuntime::new(RingBufferInput::from_bytes(b"hello 1   hello 2\r\n"));
    let mut rt_b = TerminalRuntime::new(RingBufferInput::from_bytes(b"world 1   world 2\r\n"));

    let click_events = |p: Pos2| {
        vec![
            Event::PointerMoved(p),
            Event::PointerButton {
                pos: p,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: p,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ]
    };
    let find_glyph_center = |shapes: &[Shape], text: &str, in_rect: Rect| -> Pos2 {
        shapes
            .iter()
            .find_map(|s| {
                if let Shape::Text(ts) = s
                    && ts.galley.job.text == text
                    && in_rect.contains(ts.pos)
                {
                    Some(ts.pos + ts.galley.size() * 0.5)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("expected glyph {text:?} inside {in_rect:?}"))
    };

    rt_a.open_search();
    rt_b.open_search();

    let (clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                let _ = rt_a.show(ui);
            });
            ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                let _ = rt_b.show(ui);
            });
        });
    });
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);

    let edit_a_center = find_glyph_center(&shapes, "Search...", pane_a);
    let edit_b_center = find_glyph_center(&shapes, "Search...", pane_b);
    let next_a_center = find_glyph_center(&shapes, ">", pane_a);
    let prev_a_center = find_glyph_center(&shapes, "<", pane_a);
    let next_b_center = find_glyph_center(&shapes, ">", pane_b);
    let aa_a_center = find_glyph_center(&shapes, "Aa", pane_a);
    let re_b_center = find_glyph_center(&shapes, ".*", pane_b);
    let x_a_center = find_glyph_center(&shapes, "x", pane_a);

    // Click A's search bar -> focus A only, type "hello" -> populates only A.
    let _ = run_frame_with(&ctx, screen, 1.0, 0.02, click_events(edit_a_center), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                let _ = rt_a.show(ui);
            });
            ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                let _ = rt_b.show(ui);
            });
        });
    });
    let focus_a = ctx.memory(|m| m.focused());
    assert!(
        focus_a.is_some(),
        "clicking pane A search bar must focus it"
    );

    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::Text("hello".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                    let _ = rt_a.show(ui);
                });
                ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                    let _ = rt_b.show(ui);
                });
            });
        },
    );
    assert_eq!(rt_a.search_bar_mut().query_text, "hello");
    assert_eq!(rt_b.search_bar_mut().query_text, "");

    // Click B's search bar -> distinct focus ID, type "world" -> populates only B.
    let _ = run_frame_with(&ctx, screen, 1.0, 0.06, click_events(edit_b_center), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                let _ = rt_a.show(ui);
            });
            ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                let _ = rt_b.show(ui);
            });
        });
    });
    let focus_b = ctx.memory(|m| m.focused());
    assert!(
        focus_b.is_some(),
        "clicking pane B search bar must focus it"
    );
    assert_ne!(
        focus_a, focus_b,
        "pane A and pane B search TextEdit widgets must have distinct egui::Ids"
    );

    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.08,
        vec![Event::Text("world".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                    let _ = rt_a.show(ui);
                });
                ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                    let _ = rt_b.show(ui);
                });
            });
        },
    );
    assert_eq!(rt_a.search_bar_mut().query_text, "hello");
    assert_eq!(rt_b.search_bar_mut().query_text, "world");
    assert_eq!(rt_a.terminal().search_matches().len(), 2);
    assert_eq!(rt_b.terminal().search_matches().len(), 2);
    assert_eq!(rt_a.terminal().active_search_match(), None);
    assert_eq!(rt_b.terminal().active_search_match(), None);

    // Click A's ">" twice (0 -> 10), A's "<" (10 -> 0), and B's ">" (None -> 0) independently.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.085,
        click_events(next_a_center),
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                    let _ = rt_a.show(ui);
                });
                ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                    let _ = rt_b.show(ui);
                });
            });
        },
    );
    assert_eq!(
        rt_a.terminal().active_search_match().map(|m| m.start_col),
        Some(0)
    );
    assert_eq!(rt_b.terminal().active_search_match(), None);

    let _ = run_frame_with(&ctx, screen, 1.0, 0.09, click_events(next_a_center), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                let _ = rt_a.show(ui);
            });
            ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                let _ = rt_b.show(ui);
            });
        });
    });
    assert_eq!(
        rt_a.terminal().active_search_match().map(|m| m.start_col),
        Some(10)
    );
    assert_eq!(rt_b.terminal().active_search_match(), None);

    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.095,
        click_events(prev_a_center),
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                    let _ = rt_a.show(ui);
                });
                ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                    let _ = rt_b.show(ui);
                });
            });
        },
    );
    assert_eq!(
        rt_a.terminal().active_search_match().map(|m| m.start_col),
        Some(0)
    );
    assert_eq!(rt_b.terminal().active_search_match(), None);

    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.098,
        click_events(next_b_center),
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                    let _ = rt_a.show(ui);
                });
                ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                    let _ = rt_b.show(ui);
                });
            });
        },
    );
    assert_eq!(
        rt_a.terminal().active_search_match().map(|m| m.start_col),
        Some(0)
    );
    assert_eq!(
        rt_b.terminal().active_search_match().map(|m| m.start_col),
        Some(0)
    );

    // Click A's "Aa" toggle, B's ".*" toggle, and A's "x" close button independently.
    let _ = run_frame_with(&ctx, screen, 1.0, 0.10, click_events(aa_a_center), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                let _ = rt_a.show(ui);
            });
            ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                let _ = rt_b.show(ui);
            });
        });
    });
    assert!(rt_a.search_bar_mut().case_sensitive);
    assert!(!rt_b.search_bar_mut().case_sensitive);

    let _ = run_frame_with(&ctx, screen, 1.0, 0.12, click_events(re_b_center), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                let _ = rt_a.show(ui);
            });
            ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                let _ = rt_b.show(ui);
            });
        });
    });
    assert!(!rt_a.search_bar_mut().is_regex);
    assert!(rt_b.search_bar_mut().is_regex);

    let _ = run_frame_with(&ctx, screen, 1.0, 0.14, click_events(x_a_center), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.scope_builder(UiBuilder::new().max_rect(pane_a), |ui| {
                let _ = rt_a.show(ui);
            });
            ui.scope_builder(UiBuilder::new().max_rect(pane_b), |ui| {
                let _ = rt_b.show(ui);
            });
        });
    });
    assert!(!rt_a.search_bar_mut().open);
    assert!(rt_b.search_bar_mut().open);
}

#[cfg(feature = "search")]
#[test]
fn multipane_columns_and_allocate_ui_search_bar_isolation_and_reorder() {
    // Part A: ui.columns(2, ...) static isolation + [A, B] -> [B, A] reorder + Escape closes only B.
    {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(900.0, 300.0));
        let mut rt_a = TerminalRuntime::new(RingBufferInput::from_bytes(b"one alpha\r\n"));
        let mut rt_b = TerminalRuntime::new(RingBufferInput::from_bytes(b"two beta\r\n"));

        rt_a.open_search();
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            ui.columns(2, |cols| {
                let _ = rt_a.show(&mut cols[0]);
                let _ = rt_b.show(&mut cols[1]);
            });
        });
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.02,
            vec![Event::Text("one".to_owned())],
            |ui| {
                ui.columns(2, |cols| {
                    let _ = rt_a.show(&mut cols[0]);
                    let _ = rt_b.show(&mut cols[1]);
                });
            },
        );
        assert_eq!(rt_a.search_bar_mut().query_text, "one");
        assert_eq!(rt_b.search_bar_mut().query_text, "");

        rt_b.open_search();
        let _ = run_frame_with(&ctx, screen, 1.0, 0.04, Vec::new(), |ui| {
            ui.columns(2, |cols| {
                let _ = rt_a.show(&mut cols[0]);
                let _ = rt_b.show(&mut cols[1]);
            });
        });
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.06,
            vec![Event::Text("two".to_owned())],
            |ui| {
                ui.columns(2, |cols| {
                    let _ = rt_a.show(&mut cols[0]);
                    let _ = rt_b.show(&mut cols[1]);
                });
            },
        );
        assert_eq!(rt_a.search_bar_mut().query_text, "one");
        assert_eq!(rt_b.search_bar_mut().query_text, "two");
        let focused_b_before = ctx.memory(|m| m.focused());
        assert!(focused_b_before.is_some());

        // Dynamic reorder [A, B] -> [B, A] must preserve focus identity and query isolation.
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.08,
            vec![Event::Text("!".to_owned())],
            |ui| {
                ui.columns(2, |cols| {
                    let _ = rt_b.show(&mut cols[0]);
                    let _ = rt_a.show(&mut cols[1]);
                });
            },
        );
        assert_eq!(ctx.memory(|m| m.focused()), focused_b_before);
        assert_eq!(rt_a.search_bar_mut().query_text, "one");
        assert_eq!(rt_b.search_bar_mut().query_text, "two!");

        // Escape closes only B.
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.10,
            vec![Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            |ui| {
                ui.columns(2, |cols| {
                    let _ = rt_b.show(&mut cols[0]);
                    let _ = rt_a.show(&mut cols[1]);
                });
            },
        );
        assert!(rt_a.search_bar_mut().open);
        assert!(!rt_b.search_bar_mut().open);
    }

    // Part B: sibling ui.allocate_ui(...) blocks inside CentralPanel.
    {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(500.0, 340.0));
        let mut d_a = TerminalRuntime::new(RingBufferInput::from_bytes(b"one\r\n"));
        let mut d_b = TerminalRuntime::new(RingBufferInput::from_bytes(b"two\r\n"));

        d_a.open_search();
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.allocate_ui(vec2(400.0, 140.0), |ui| {
                    let _ = d_a.show(ui);
                });
                ui.allocate_ui(vec2(400.0, 140.0), |ui| {
                    let _ = d_b.show(ui);
                });
            });
        });
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.02,
            vec![Event::Text("one".to_owned())],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    ui.allocate_ui(vec2(400.0, 140.0), |ui| {
                        let _ = d_a.show(ui);
                    });
                    ui.allocate_ui(vec2(400.0, 140.0), |ui| {
                        let _ = d_b.show(ui);
                    });
                });
            },
        );
        assert_eq!(d_a.search_bar_mut().query_text, "one");
        assert_eq!(d_b.search_bar_mut().query_text, "");

        d_b.open_search();
        let _ = run_frame_with(&ctx, screen, 1.0, 0.04, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.allocate_ui(vec2(400.0, 140.0), |ui| {
                    let _ = d_a.show(ui);
                });
                ui.allocate_ui(vec2(400.0, 140.0), |ui| {
                    let _ = d_b.show(ui);
                });
            });
        });
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.06,
            vec![Event::Text("two".to_owned())],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    ui.allocate_ui(vec2(400.0, 140.0), |ui| {
                        let _ = d_a.show(ui);
                    });
                    ui.allocate_ui(vec2(400.0, 140.0), |ui| {
                        let _ = d_b.show(ui);
                    });
                });
            },
        );
        assert_eq!(d_a.search_bar_mut().query_text, "one");
        assert_eq!(d_b.search_bar_mut().query_text, "two");

        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.08,
            vec![Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    ui.allocate_ui(vec2(400.0, 140.0), |ui| {
                        let _ = d_a.show(ui);
                    });
                    ui.allocate_ui(vec2(400.0, 140.0), |ui| {
                        let _ = d_b.show(ui);
                    });
                });
            },
        );
        assert!(d_a.search_bar_mut().open);
        assert!(!d_b.search_bar_mut().open);
    }
}

#[cfg(feature = "search")]
#[test]
fn search_bar_non_primary_and_mixed_press_latching_preserve_terminal_focus() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hello\r\n"));

    rt.open_search();
    let (clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    let bar_count_pos = shapes
        .iter()
        .find_map(|s| {
            if let Shape::Text(ts) = s
                && ts.galley.job.text == "0/0"
            {
                Some(ts.pos + ts.galley.size() * 0.5)
            } else {
                None
            }
        })
        .expect("search bar count label");

    // Focus terminal at bottom-left, then settle 1 frame.
    let term_pos = pos2(12.0, 150.0);
    let mut term_focus = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                term_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(term_focus);

    // Secondary press inside bar_rect (on count label) must NOT activate TextEdit focus.
    let mut f_sec = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::PointerMoved(bar_count_pos),
            Event::PointerButton {
                pos: bar_count_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: bar_count_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                f_sec = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(
        f_sec,
        "Secondary click on count label must preserve terminal focus"
    );
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![Event::Text("x".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.search_bar_mut().query_text.is_empty());
    assert_eq!(rt.input_mut().drain_written(), b"x");

    // Same-frame outside Primary press followed by inside Primary press (on count label):
    // outside must latch (`pointer_pressed_outside_bar == true`, `clicked_in_bar == false`)
    // so terminal keeps focus and receives `"y"`.
    let outside_pos = pos2(12.0, 150.0);
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.08,
        vec![
            Event::PointerButton {
                pos: outside_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerMoved(bar_count_pos),
            Event::PointerButton {
                pos: bar_count_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: bar_count_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.10,
        vec![Event::Text("y".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.search_bar_mut().query_text.is_empty());
    assert_eq!(rt.input_mut().drain_written(), b"y");
}

#[cfg(feature = "search")]
#[test]
fn search_bar_non_primary_clicks_on_count_label_and_buttons_preserve_focus_dec_1004() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(500.0, 220.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004hhello\r\n"));

    rt.open_search();
    let (clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    let find_text_center = |label: &str| {
        shapes
            .iter()
            .find_map(|s| match s {
                Shape::Text(ts) if ts.galley.job.text == label => {
                    Some(ts.galley.rect.translate(ts.pos.to_vec2()).center())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing search bar text {label:?}"))
    };
    let targets = [
        ("0/0", find_text_center("0/0")),
        ("x", find_text_center("x")),
        (">", find_text_center(">")),
        ("<", find_text_center("<")),
        ("Aa", find_text_center("Aa")),
        (".*", find_text_center(".*")),
    ];
    let edit_pos = shapes
        .iter()
        .find_map(|s| match s {
            Shape::Text(ts) if ts.galley.job.text == "Search..." => {
                Some(ts.galley.rect.translate(ts.pos.to_vec2()).center())
            }
            _ => None,
        })
        .expect("missing Search... hint");

    // Focus terminal and consume initial DEC ?1004 focus-in `\x1b[I`.
    let term_pos = pos2(16.0, 170.0);
    let mut term_id = egui::Id::NULL;
    let mut term_focus = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let resp = rt.show(ui);
                term_id = resp.response.id;
                term_focus = resp.response.has_focus();
            });
        },
    );
    assert!(term_focus);
    assert_eq!(ctx.memory(|m| m.focused()), Some(term_id));
    assert_eq!(rt.input_mut().drain_written(), b"\x1b[I");

    let mut t = 0.04;
    for button in [PointerButton::Secondary, PointerButton::Middle] {
        for (name, pos) in targets {
            let mut has_focus_after_click = false;
            let _ = run_frame_with(
                &ctx,
                screen,
                1.0,
                t,
                vec![
                    Event::PointerMoved(pos),
                    Event::PointerButton {
                        pos,
                        button,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    },
                    Event::PointerButton {
                        pos,
                        button,
                        pressed: false,
                        modifiers: Modifiers::NONE,
                    },
                ],
                |ui| {
                    CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                        has_focus_after_click = rt.show(ui).response.has_focus();
                    });
                },
            );
            t += 0.02;
            assert!(
                has_focus_after_click,
                "{button:?} click on {name} must preserve response.has_focus()"
            );
            assert_eq!(
                ctx.memory(|m| m.focused()),
                Some(term_id),
                "{button:?} click on {name} must keep term_id focused"
            );
            assert_eq!(
                rt.input_mut().drain_written(),
                Vec::<u8>::new(),
                "{button:?} click on {name} must not emit DEC ?1004 focus-out"
            );
            assert!(
                rt.search_bar_mut().open,
                "{button:?} click on {name} must not close search bar"
            );

            let _ = run_frame_with(
                &ctx,
                screen,
                1.0,
                t,
                vec![Event::Text("q".to_owned())],
                |ui| {
                    CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                        let _ = rt.show(ui);
                    });
                },
            );
            t += 0.02;
            assert_eq!(
                rt.input_mut().drain_written(),
                b"q",
                "subsequent Text after {button:?} click on {name} must reach terminal"
            );
            assert!(rt.search_bar_mut().query_text.is_empty());
        }
    }

    // Split-frame Secondary press (Frame N) then release (Frame N+1) on "x" must also keep focus.
    let x_pos = targets[1].1;
    let mut split_focus_down = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![
            Event::PointerMoved(x_pos),
            Event::PointerButton {
                pos: x_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                split_focus_down = rt.show(ui).response.has_focus();
            });
        },
    );
    t += 0.02;
    assert!(split_focus_down);
    assert_eq!(rt.input_mut().drain_written(), Vec::<u8>::new());

    let mut split_focus_up = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![Event::PointerButton {
            pos: x_pos,
            button: PointerButton::Secondary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                split_focus_up = rt.show(ui).response.has_focus();
            });
        },
    );
    t += 0.02;
    assert!(split_focus_up);
    assert_eq!(ctx.memory(|m| m.focused()), Some(term_id));
    assert_eq!(rt.input_mut().drain_written(), Vec::<u8>::new());

    // Shift+RightClick on term_rect outside bar_rect must still open context menu.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![
            Event::ModifiersChanged(Modifiers::SHIFT),
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::SHIFT,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::SHIFT,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    t += 0.02;
    let (clipped_menu, _) = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![Event::ModifiersChanged(Modifiers::NONE)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    t += 0.02;
    let mut menu_shapes = Vec::new();
    collect_shapes(&clipped_menu, &mut menu_shapes);
    assert!(
        all_text(&menu_shapes).contains("Clear Scrollback"),
        "Shift+RightClick outside search bar must still open context menu"
    );

    // Close context menu via Escape on term_rect, then verify Secondary click directly inside
    // TextEdit (`edit_pos`) still moves focus into `search_query_edit`.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    t += 0.02;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    t += 0.02;
    let _ = rt.input_mut().drain_written();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![
            Event::PointerMoved(edit_pos),
            Event::PointerButton {
                pos: edit_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: edit_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    t += 0.02;
    assert_eq!(rt.input_mut().drain_written(), b"\x1b[O");
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![Event::Text("q".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.search_bar_mut().query_text, "q");
}

#[cfg(feature = "search")]
#[test]
fn multipane_sequential_focus_escape_does_not_steal_from_earlier_pane_or_sibling() {
    let escape_event = || {
        vec![Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }]
    };

    // Variant A: rt_a.open_search() focused on Frame 0 -> rt_b.open_search() takes focus on
    // Frame 1 -> immediate Key::Escape on Frame 2 closes ONLY rt_b and moves focus to rt_b's
    // terminal (rt_a stays open and never steals focus).
    {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(900.0, 300.0));
        let mut rt_a = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004haaa\r\n"));
        let mut rt_b = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004hbbb\r\n"));

        rt_a.open_search();
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            ui.columns(2, |cols| {
                let _ = rt_a.show(&mut cols[0]);
                let _ = rt_b.show(&mut cols[1]);
            });
        });

        rt_b.open_search();
        let mut term_b_id = egui::Id::NULL;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.02, Vec::new(), |ui| {
            ui.columns(2, |cols| {
                let _ = rt_a.show(&mut cols[0]);
                term_b_id = rt_b.show(&mut cols[1]).response.id;
            });
        });

        let mut term_a_focus = true;
        let mut term_b_focus = false;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.04, escape_event(), |ui| {
            ui.columns(2, |cols| {
                term_a_focus = rt_a.show(&mut cols[0]).response.has_focus();
                term_b_focus = rt_b.show(&mut cols[1]).response.has_focus();
            });
        });
        assert!(
            rt_a.search_bar_mut().open,
            "Variant A: rt_a must remain open"
        );
        assert!(
            !rt_b.search_bar_mut().open,
            "Variant A: rt_b must close on Escape"
        );
        assert!(!term_a_focus, "Variant A: rt_a must not steal focus");
        assert!(term_b_focus, "Variant A: rt_b terminal must receive focus");
        assert_eq!(ctx.memory(|m| m.focused()), Some(term_b_id));
        assert!(rt_a.input_mut().drain_written().is_empty());
    }

    // Case A: both rt_a.open_search() and rt_b.open_search() called before Frame 1 ->
    // immediate Key::Escape on Frame 2 closes ONLY rt_b.
    {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(900.0, 300.0));
        let mut rt_a = TerminalRuntime::new(RingBufferInput::from_bytes(b"aaa\r\n"));
        let mut rt_b = TerminalRuntime::new(RingBufferInput::from_bytes(b"bbb\r\n"));

        rt_a.open_search();
        rt_b.open_search();
        let mut term_b_id = egui::Id::NULL;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            ui.columns(2, |cols| {
                let _ = rt_a.show(&mut cols[0]);
                term_b_id = rt_b.show(&mut cols[1]).response.id;
            });
        });

        let mut term_a_focus = true;
        let mut term_b_focus = false;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.02, escape_event(), |ui| {
            ui.columns(2, |cols| {
                term_a_focus = rt_a.show(&mut cols[0]).response.has_focus();
                term_b_focus = rt_b.show(&mut cols[1]).response.has_focus();
            });
        });
        assert!(rt_a.search_bar_mut().open, "Case A: rt_a must remain open");
        assert!(
            !rt_b.search_bar_mut().open,
            "Case A: rt_b must close on Escape"
        );
        assert!(!term_a_focus, "Case A: rt_a must not steal focus");
        assert!(term_b_focus, "Case A: rt_b terminal must receive focus");
        assert_eq!(ctx.memory(|m| m.focused()), Some(term_b_id));
    }

    // Variant B: rt_a.open_search() on Frame 0 -> sibling TextEdit rendered after rt_a calls
    // request_focus() on Frame 1 -> immediate Key::Escape on Frame 2 leaves rt_a open and does
    // NOT steal focus back to rt_a's terminal.
    {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(600.0, 300.0));
        let mut rt_a = TerminalRuntime::new(RingBufferInput::from_bytes(b"aaa\r\n"));
        let mut sibling_text = String::new();

        rt_a.open_search();
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.allocate_ui(vec2(580.0, 200.0), |ui| {
                    let _ = rt_a.show(ui);
                });
                let _ = ui.text_edit_singleline(&mut sibling_text);
            });
        });

        let mut sibling_id = egui::Id::NULL;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.02, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.allocate_ui(vec2(580.0, 200.0), |ui| {
                    let _ = rt_a.show(ui);
                });
                let r = ui.text_edit_singleline(&mut sibling_text);
                r.request_focus();
                sibling_id = r.id;
            });
        });
        assert_eq!(ctx.memory(|m| m.focused()), Some(sibling_id));

        let mut term_a_focus = true;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.04, escape_event(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.allocate_ui(vec2(580.0, 200.0), |ui| {
                    term_a_focus = rt_a.show(ui).response.has_focus();
                });
                let _ = ui.text_edit_singleline(&mut sibling_text);
            });
        });
        assert!(
            rt_a.search_bar_mut().open,
            "Variant B: rt_a search bar must remain open"
        );
        assert!(
            !term_a_focus,
            "Variant B: rt_a terminal must not steal focus"
        );
        assert_eq!(ctx.memory(|m| m.focused()), None);
    }

    // Single search bar: Tab onto a toggle button inside the bar followed by Escape still closes
    // the search bar cleanly and returns focus to the terminal.
    {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(600.0, 300.0));
        let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"aaa\r\n"));
        rt.open_search();
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        });
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.02,
            vec![Event::Key {
                key: Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        let mut returned_focus = false;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.04, escape_event(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                returned_focus = rt.show(ui).response.has_focus();
            });
        });
        assert!(
            !rt.search_bar_mut().open,
            "Tab + Escape inside bar must close search bar"
        );
        assert!(
            returned_focus,
            "Tab + Escape inside bar must return focus to terminal"
        );
    }
}

#[test]
fn multipane_window_focused_only_reports_for_focused_pane() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 300.0));
    let mut rt_a = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004ha\r\n"));
    let mut rt_b = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004hb\r\n"));

    let mut rect_a = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        ui.columns(2, |cols| {
            rect_a = rt_a.show(&mut cols[0]).response.rect;
            let _ = rt_b.show(&mut cols[1]);
        });
    });

    // Focus pane A only.
    let c = rect_a.center();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            ui.columns(2, |cols| {
                let _ = rt_a.show(&mut cols[0]);
                let _ = rt_b.show(&mut cols[1]);
            });
        },
    );
    assert_eq!(rt_a.input_mut().drain_written(), b"\x1b[I");
    assert!(rt_b.input_mut().drain_written().is_empty());

    // Host window loses focus -> only focused pane A emits \x1b[O.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::WindowFocused(false)],
        |ui| {
            ui.columns(2, |cols| {
                let _ = rt_a.show(&mut cols[0]);
                let _ = rt_b.show(&mut cols[1]);
            });
        },
    );
    assert_eq!(rt_a.input_mut().drain_written(), b"\x1b[O");
    assert!(
        rt_b.input_mut().drain_written().is_empty(),
        "unfocused pane B must not emit DEC ?1004 on WindowFocused(false)"
    );

    // Host window regains focus -> only focused pane A emits \x1b[I.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![Event::WindowFocused(true)],
        |ui| {
            ui.columns(2, |cols| {
                let _ = rt_a.show(&mut cols[0]);
                let _ = rt_b.show(&mut cols[1]);
            });
        },
    );
    assert_eq!(rt_a.input_mut().drain_written(), b"\x1b[I");
    assert!(
        rt_b.input_mut().drain_written().is_empty(),
        "unfocused pane B must not emit DEC ?1004 on WindowFocused(true)"
    );
}

#[cfg(feature = "search")]
#[test]
fn search_bar_drag_out_to_term_rect_under_dec_1000_emits_no_orphan_release() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(500.0, 240.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(
        b"\x1b[?1000h\x1b[?1006h\x1b[?1004hhello\r\n",
    ));

    rt.open_search();
    let mut term_rect = Rect::NOTHING;
    let (clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.add_space(20.0);
            ui.allocate_ui(vec2(500.0, 220.0), |ui| {
                term_rect = rt.show(ui).response.rect;
            });
        });
    });
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    let x_pos = shapes
        .iter()
        .find_map(|s| match s {
            Shape::Text(ts) if ts.galley.job.text == "x" => {
                Some(ts.galley.rect.translate(ts.pos.to_vec2()).center())
            }
            _ => None,
        })
        .expect("missing search bar close button");

    let term_pos = pos2(term_rect.min.x + 20.0, term_rect.min.y + 150.0);
    let mut term_id = egui::Id::NULL;
    let mut term_focus = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.add_space(20.0);
                ui.allocate_ui(vec2(500.0, 220.0), |ui| {
                    let resp = rt.show(ui);
                    term_id = resp.response.id;
                    term_focus = resp.response.has_focus();
                });
            });
        },
    );
    assert!(term_focus);
    let initial_bytes = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
    assert!(initial_bytes.contains("\x1b[I"));

    // Press inside search_bar_rect ("x"), then drag to term_pos and release on Frame 2.
    let mut t = 0.04;
    for btn in [
        PointerButton::Secondary,
        PointerButton::Middle,
        PointerButton::Primary,
    ] {
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            t,
            vec![
                Event::PointerMoved(x_pos),
                Event::PointerButton {
                    pos: x_pos,
                    button: btn,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    ui.add_space(20.0);
                    ui.allocate_ui(vec2(500.0, 220.0), |ui| {
                        let _ = rt.show(ui);
                    });
                });
            },
        );
        t += 0.02;
        let press_bytes = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
        assert!(
            !press_bytes.contains("\x1b[<"),
            "{btn:?} press on search bar must emit no mouse bytes, got {press_bytes:?}"
        );
        if btn != PointerButton::Primary {
            assert!(
                press_bytes.is_empty(),
                "{btn:?} press on search bar must preserve terminal focus (got {press_bytes:?})"
            );
        }

        let mut focus_after = false;
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            t,
            vec![
                Event::PointerMoved(term_pos),
                Event::PointerButton {
                    pos: term_pos,
                    button: btn,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    ui.add_space(20.0);
                    ui.allocate_ui(vec2(500.0, 220.0), |ui| {
                        focus_after = rt.show(ui).response.has_focus();
                    });
                });
            },
        );
        t += 0.02;
        assert!(
            rt.input_mut().drain_written().is_empty(),
            "{btn:?} drag-out release from search_bar_rect into term_rect must emit zero mouse bytes"
        );
        if btn != PointerButton::Primary {
            assert!(focus_after);
            assert_eq!(ctx.memory(|m| m.focused()), Some(term_id));
        }
    }

    // Re-focus terminal after Primary test above.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.add_space(20.0);
                ui.allocate_ui(vec2(500.0, 220.0), |ui| {
                    let _ = rt.show(ui);
                });
            });
        },
    );
    t += 0.02;
    let _ = rt.input_mut().drain_written();

    // Also verify press outside term_rect (above term_rect in spacer) dragged into term_pos
    // emits zero orphan release bytes.
    let outside_pos = pos2(20.0, 5.0);
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![
            Event::PointerMoved(outside_pos),
            Event::PointerButton {
                pos: outside_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.add_space(20.0);
                ui.allocate_ui(vec2(500.0, 220.0), |ui| {
                    let _ = rt.show(ui);
                });
            });
        },
    );
    t += 0.02;
    let _ = rt.input_mut().drain_written();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.add_space(20.0);
                ui.allocate_ui(vec2(500.0, 220.0), |ui| {
                    let _ = rt.show(ui);
                });
            });
        },
    );
    t += 0.02;
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "drag-out release from outside term_rect must emit zero orphan mouse bytes"
    );

    // Normal press -> release inside term_rect must still emit paired SGR press and release.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.add_space(20.0);
                ui.allocate_ui(vec2(500.0, 220.0), |ui| {
                    let _ = rt.show(ui);
                });
            });
        },
    );
    t += 0.02;
    let press_bytes = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
    assert!(
        press_bytes.starts_with("\x1b[<2;") && press_bytes.ends_with('M'),
        "expected SGR secondary press, got {press_bytes:?}"
    );

    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        t,
        vec![Event::PointerButton {
            pos: term_pos,
            button: PointerButton::Secondary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.add_space(20.0);
                ui.allocate_ui(vec2(500.0, 220.0), |ui| {
                    let _ = rt.show(ui);
                });
            });
        },
    );
    let release_bytes = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
    assert!(
        release_bytes.starts_with("\x1b[<2;") && release_bytes.ends_with('m'),
        "expected SGR secondary release, got {release_bytes:?}"
    );
}

#[cfg(feature = "search")]
#[test]
fn search_bar_count_label_secondary_click_same_frame_move_does_not_open_menu() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(500.0, 220.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hello\r\n"));

    rt.open_search();
    let mut term_rect = Rect::NOTHING;
    let (clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    let count_pos = shapes
        .iter()
        .find_map(|s| match s {
            Shape::Text(ts) if ts.galley.job.text == "0/0" => {
                Some(ts.galley.rect.translate(ts.pos.to_vec2()).center())
            }
            _ => None,
        })
        .expect("missing 0/0 count label");
    let bar_w = (term_rect.width() - 16.0)
        .clamp(104.0, 380.0)
        .min(term_rect.width());
    let bar_rect = Rect::from_min_size(
        pos2(
            (term_rect.max.x - bar_w - 8.0).max(term_rect.min.x),
            term_rect.min.y + 8.0,
        ),
        vec2(bar_w, 30.0),
    );

    // Focus terminal first.
    let term_pos = pos2(16.0, 170.0);
    let mut term_id = egui::Id::NULL;
    let mut term_focus = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let resp = rt.show(ui);
                term_id = resp.response.id;
                term_focus = resp.response.has_focus();
            });
        },
    );
    assert!(term_focus);

    // Single-frame Secondary click on "0/0" followed by PointerMoved(term_pos) must NOT open
    // context menu and must preserve terminal focus.
    let mut focus_after = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::PointerMoved(count_pos),
            Event::PointerButton {
                pos: count_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: count_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
            Event::PointerMoved(term_pos),
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                focus_after = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(focus_after);
    assert_eq!(ctx.memory(|m| m.focused()), Some(term_id));

    let (next_clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.06, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            focus_after = rt.show(ui).response.has_focus();
        });
    });
    let mut next_shapes = Vec::new();
    collect_shapes(&next_clipped, &mut next_shapes);
    assert!(
        !all_text(&next_shapes).contains("Clear Scrollback"),
        "Secondary click on count label + same-frame move must NOT open context menu"
    );
    assert!(focus_after);
    assert_eq!(ctx.memory(|m| m.focused()), Some(term_id));

    // Split-frame Secondary press on "0/0" followed by PointerMoved + release outside bar_rect
    // must also NOT open context menu.
    let near_outside = pos2(count_pos.x, bar_rect.max.y + 1.5);
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.08,
        vec![
            Event::PointerMoved(count_pos),
            Event::PointerButton {
                pos: count_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.10,
        vec![
            Event::PointerMoved(near_outside),
            Event::PointerButton {
                pos: near_outside,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let (split_clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.12, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut split_shapes = Vec::new();
    collect_shapes(&split_clipped, &mut split_shapes);
    assert!(
        !all_text(&split_shapes).contains("Clear Scrollback"),
        "Split-frame Secondary press on count label released outside must NOT open context menu"
    );

    // Subsequent Shift+RightClick adjacent to bar_rect (2px below) DOES open context menu.
    let adj_pos = pos2(count_pos.x, bar_rect.max.y + 2.0);
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.14,
        vec![
            Event::ModifiersChanged(Modifiers::SHIFT),
            Event::PointerMoved(adj_pos),
            Event::PointerButton {
                pos: adj_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::SHIFT,
            },
            Event::PointerButton {
                pos: adj_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::SHIFT,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let (menu_clipped, _) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.16,
        vec![Event::ModifiersChanged(Modifiers::NONE)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let mut menu_shapes = Vec::new();
    collect_shapes(&menu_clipped, &mut menu_shapes);
    assert!(
        all_text(&menu_shapes).contains("Clear Scrollback"),
        "Shift+RightClick 2px below search bar must still open context menu"
    );
}

#[test]
fn scrollbar_secondary_click_same_frame_move_and_dec_1000_does_not_open_menu() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
    let mut payload = String::new();
    for i in 0..80 {
        use std::fmt::Write as _;
        writeln!(payload, "line {i}\r").expect("String fmt::Write is infallible");
    }
    payload.push_str("\x1b[?1000h\x1b[?1006h");
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(payload.as_bytes()));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });
    assert!(rt.terminal().scrollback_len() > 0);

    let term_pos = pos2(term_rect.min.x + 24.0, term_rect.center().y);
    let sb_pos = pos2(term_rect.max.x - 2.0, term_rect.center().y);
    let adj_pos = pos2(term_rect.max.x - 10.0, term_rect.center().y);

    // Focus terminal first.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let _ = rt.input_mut().drain_written();

    // 1. Single-frame Secondary click inside scrollbar track_rect + same-frame PointerMoved(term_pos)
    //    must emit zero mouse bytes and must NOT open context menu.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::PointerMoved(sb_pos),
            Event::PointerButton {
                pos: sb_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: sb_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
            Event::PointerMoved(term_pos),
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "Secondary click on scrollbar + same-frame move must emit zero DEC ?1000 bytes"
    );
    let (clipped1, _) = run_frame_with(&ctx, screen, 1.0, 0.06, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes1 = Vec::new();
    collect_shapes(&clipped1, &mut shapes1);
    assert!(
        !all_text(&shapes1).contains("Clear Scrollback"),
        "Secondary click on scrollbar + same-frame move must NOT open context menu"
    );

    // 2. Split-frame Secondary press inside track_rect -> release at term_pos must emit zero bytes
    //    and must NOT open context menu.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.08,
        vec![
            Event::PointerMoved(sb_pos),
            Event::PointerButton {
                pos: sb_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.10,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "Split-frame Secondary press on scrollbar released in term_rect must emit zero mouse bytes"
    );
    let (clipped2, _) = run_frame_with(&ctx, screen, 1.0, 0.12, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes2 = Vec::new();
    collect_shapes(&clipped2, &mut shapes2);
    assert!(
        !all_text(&shapes2).contains("Clear Scrollback"),
        "Split-frame Secondary press on scrollbar released in term_rect must NOT open context menu"
    );

    // 3. Middle click on scrollbar + same-frame move emits zero DEC ?1000 bytes.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.14,
        vec![
            Event::PointerMoved(sb_pos),
            Event::PointerButton {
                pos: sb_pos,
                button: PointerButton::Middle,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: sb_pos,
                button: PointerButton::Middle,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
            Event::PointerMoved(term_pos),
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "Middle click on scrollbar + same-frame move must emit zero mouse bytes"
    );

    // 4. Subsequent Shift+RightClick in term_rect adjacent to scrollbar DOES open context menu.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.16,
        vec![
            Event::ModifiersChanged(Modifiers::SHIFT),
            Event::PointerMoved(adj_pos),
            Event::PointerButton {
                pos: adj_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::SHIFT,
            },
            Event::PointerButton {
                pos: adj_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::SHIFT,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let (menu_clipped, _) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.18,
        vec![Event::ModifiersChanged(Modifiers::NONE)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let mut menu_shapes = Vec::new();
    collect_shapes(&menu_clipped, &mut menu_shapes);
    assert!(
        all_text(&menu_shapes).contains("Clear Scrollback"),
        "Shift+RightClick adjacent to scrollbar must still open context menu"
    );
}

#[test]
fn chorded_primary_secondary_release_preserves_active_drag_under_dec_1002_and_1003() {
    for (seq, label) in [
        (b"\x1b[?1002h\x1b[?1006h".as_slice(), "DEC ?1002"),
        (b"\x1b[?1003h\x1b[?1006h".as_slice(), "DEC ?1003"),
    ] {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
        let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(seq));

        let mut cell_size = egui::Vec2::ZERO;
        let mut term_rect = Rect::NOTHING;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let resp = rt.show(ui);
                cell_size = resp.cell_size;
                term_rect = resp.response.rect;
            });
        });

        let p1 = pos2(
            term_rect.min.x + cell_size.x * 2.5,
            term_rect.min.y + cell_size.y * 1.5,
        );
        let p2 = pos2(
            term_rect.min.x + cell_size.x * 6.5,
            term_rect.min.y + cell_size.y * 2.5,
        );
        let p_outside = pos2(term_rect.max.x + 25.0, term_rect.min.y + cell_size.y * 3.5);

        // Press Primary at p1.
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.02,
            vec![
                Event::PointerMoved(p1),
                Event::PointerButton {
                    pos: p1,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        let b1 = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
        let expected_b1 = if label == "DEC ?1003" {
            "\x1b[<35;3;2M\x1b[<0;3;2M"
        } else {
            "\x1b[<0;3;2M"
        };
        assert_eq!(b1, expected_b1, "{label}: primary press");

        // Press Secondary while Primary is held, then release Secondary while Primary stays held.
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.04,
            vec![
                Event::PointerButton {
                    pos: p1,
                    button: PointerButton::Secondary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: p1,
                    button: PointerButton::Secondary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        let b2 = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
        assert_eq!(
            b2, "\x1b[<2;3;2M\x1b[<2;3;2m",
            "{label}: chorded secondary press/release"
        );

        // Drag Primary to p2 -> must report left-drag (\x1b[<32;7;3M), NOT "" or \x1b[<35;...M.
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.06,
            vec![Event::PointerMoved(p2)],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        let b3 = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
        assert_eq!(
            b3, "\x1b[<32;7;3M",
            "{label}: drag after releasing chorded Secondary must preserve Primary"
        );

        // Drag Primary outside term_rect -> must clamp and continue reporting left-drag (\x1b[<32;...).
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.08,
            vec![Event::PointerMoved(p_outside)],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        let b4 = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
        assert!(
            b4.starts_with("\x1b[<32;") && b4.ends_with('M'),
            "{label}: outside drag must clamp and preserve Primary, got {b4:?}"
        );

        // Release Primary -> emits \x1b[<0;...m.
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.10,
            vec![Event::PointerButton {
                pos: p2,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        let b5 = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
        assert_eq!(b5, "\x1b[<0;7;3m", "{label}: final primary release");
    }
}

#[cfg(feature = "search")]
#[test]
fn open_search_then_same_frame_terminal_click_has_no_focus_bounce() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(500.0, 220.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004hhello\r\n"));

    // Warm up frame so widget rect is registered.
    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    // Call open_search(), then immediately click inside term_rect on the very next frame.
    rt.open_search();
    let term_pos = pos2(term_rect.min.x + 20.0, term_rect.max.y - 20.0);
    let mut has_focus_f1 = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                has_focus_f1 = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(has_focus_f1, "terminal click must focus terminal");
    assert_eq!(
        rt.input_mut().drain_written(),
        b"\x1b[I",
        "open_search() + same-frame terminal click must emit only \\x1b[I (no \\x1b[I\\x1b[O bounce)"
    );

    // Frame 2: typing must go to the terminal, NOT get stolen by search bar.
    let mut has_focus_f2 = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::Text("abc".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                has_focus_f2 = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(has_focus_f2, "terminal must retain focus on Frame 2");
    assert_eq!(
        rt.input_mut().drain_written(),
        b"abc",
        "Frame 2 typing must reach terminal instead of being stolen by search bar"
    );
    assert!(
        rt.search_bar_mut().query_text.is_empty(),
        "search bar query must remain empty"
    );
}

#[cfg(feature = "search")]
#[test]
fn search_bar_secondary_press_then_close_or_shrink_before_release_does_not_open_menu() {
    let ctx = Context::default();
    let full_screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
    let compact_screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 100.0));
    let tiny_screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 34.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"hello\r\n"));

    rt.open_search();
    let (clipped, _) = run_frame_with(&ctx, full_screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    let count_pos = shapes
        .iter()
        .find_map(|s| match s {
            Shape::Text(ts) if ts.galley.job.text == "0/0" => {
                Some(ts.galley.rect.translate(ts.pos.to_vec2()).center())
            }
            _ => None,
        })
        .expect("missing 0/0 count label");

    // 1. Secondary press on "0/0" -> rt.close_search() -> release 1px away must NOT open menu.
    let _ = run_frame_with(
        &ctx,
        full_screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(count_pos),
            Event::PointerButton {
                pos: count_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    rt.close_search();
    let _ = run_frame_with(
        &ctx,
        full_screen,
        1.0,
        0.04,
        vec![Event::PointerButton {
            pos: count_pos + vec2(1.0, 1.0),
            button: PointerButton::Secondary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let (clipped_after_close, _) = run_frame_with(&ctx, full_screen, 1.0, 0.06, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes_after_close = Vec::new();
    collect_shapes(&clipped_after_close, &mut shapes_after_close);
    let text_after_close = all_text(&shapes_after_close);
    assert!(
        !text_after_close.contains("Clear Scrollback") && !text_after_close.contains("Paste"),
        "Secondary press on 0/0 followed by close_search() before release must NOT open context menu, got {text_after_close:?}"
    );

    // 2. Secondary press on "0/0" -> shrink pane (<36px) across hold -> release at count_pos must NOT open menu.
    rt.open_search();
    let _ = run_frame_with(&ctx, compact_screen, 1.0, 0.08, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let _ = run_frame_with(
        &ctx,
        compact_screen,
        1.0,
        0.10,
        vec![
            Event::PointerMoved(count_pos),
            Event::PointerButton {
                pos: count_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    // Intermediate shrunk frame while Secondary is still held down.
    let _ = run_frame_with(&ctx, tiny_screen, 1.0, 0.12, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    // Release Secondary while shrunk < 36px.
    let _ = run_frame_with(
        &ctx,
        tiny_screen,
        1.0,
        0.14,
        vec![Event::PointerButton {
            pos: count_pos,
            button: PointerButton::Secondary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let (clipped_after_shrink, _) =
        run_frame_with(&ctx, full_screen, 1.0, 0.16, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        });
    let mut shapes_after_shrink = Vec::new();
    collect_shapes(&clipped_after_shrink, &mut shapes_after_shrink);
    let text_after_shrink = all_text(&shapes_after_shrink);
    assert!(
        !text_after_shrink.contains("Clear Scrollback") && !text_after_shrink.contains("Paste"),
        "Secondary press on 0/0 followed by shrink (<36px) before release must NOT open context menu, got {text_after_shrink:?}"
    );

    // 3. Subsequent normal RightClick in term_rect below search bar DOES open context menu.
    let term_pos = pos2(24.0, 90.0);
    let _ = run_frame_with(
        &ctx,
        full_screen,
        1.0,
        0.18,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let (menu_clipped, _) = run_frame_with(&ctx, full_screen, 1.0, 0.20, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut menu_shapes = Vec::new();
    collect_shapes(&menu_clipped, &mut menu_shapes);
    let menu_text = all_text(&menu_shapes);
    assert!(
        menu_text.contains("Clear Scrollback"),
        "normal RightClick in term_rect after search bar press-and-shrink must open context menu, got {menu_text:?}"
    );
}

#[cfg(feature = "search")]
#[test]
fn context_menu_find_single_frame_immediate_and_settled_escape_closes_search_bar() {
    for settle_frames in [0usize, 1usize] {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(500.0, 240.0));
        let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004hhello\r\n"));

        let mut term_rect = Rect::NOTHING;
        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                term_rect = rt.show(ui).response.rect;
            });
        });

        // Focus terminal and open context menu via RightClick.
        let click_pos = pos2(term_rect.min.x + 24.0, term_rect.center().y);
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.02,
            vec![
                Event::PointerMoved(click_pos),
                Event::PointerButton {
                    pos: click_pos,
                    button: PointerButton::Secondary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: click_pos,
                    button: PointerButton::Secondary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        let (menu_clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.04, Vec::new(), |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        });
        let mut menu_shapes = Vec::new();
        collect_shapes(&menu_clipped, &mut menu_shapes);
        let find_pos = menu_shapes
            .iter()
            .find_map(|s| match s {
                Shape::Text(ts) if ts.galley.job.text.contains("Find") => {
                    Some(ts.galley.rect.translate(ts.pos.to_vec2()).center())
                }
                _ => None,
            })
            .expect("missing Find... context menu item");

        // Single-frame click on "Find..." (PointerMoved + pressed: true + pressed: false).
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            0.06,
            vec![
                Event::PointerMoved(find_pos),
                Event::PointerButton {
                    pos: find_pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: find_pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        assert!(
            rt.search_bar_mut().open,
            "single-frame Find... click must open search bar (settle={settle_frames})"
        );

        let mut t = 0.08;
        for _ in 0..settle_frames {
            let _ = run_frame_with(&ctx, screen, 1.0, t, Vec::new(), |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            });
            t += 0.02;
        }

        // Verify typing goes to search bar and not terminal.
        let _ = rt.input_mut().drain_written();
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            t,
            vec![Event::Text("hello".to_owned())],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        assert_eq!(
            rt.search_bar_mut().query_text,
            "hello",
            "newly opened search bar from Find... must accept typed query (settle={settle_frames})"
        );
        assert!(
            rt.input_mut().drain_written().is_empty(),
            "terminal must not receive typed query while search bar is focused (settle={settle_frames})"
        );

        // Press Escape -> search bar must close and return focus to terminal.
        let mut has_focus_after_esc = false;
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            t + 0.02,
            vec![Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            |ui| {
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    has_focus_after_esc = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(
            !rt.search_bar_mut().open,
            "Escape after single-frame Find... click must close search bar (settle={settle_frames})"
        );
        assert!(
            has_focus_after_esc,
            "terminal must reclaim focus after Escape closes search bar (settle={settle_frames})"
        );
    }
}

#[cfg(feature = "search")]
#[test]
fn ctrl_shift_f_with_same_frame_outside_click_focuses_search_bar_and_escape_closes() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(500.0, 240.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004hhello\r\n"));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    let term_pos = pos2(term_rect.min.x + 20.0, term_rect.max.y - 20.0);
    // Focus terminal first.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.input_mut().drain_written(), b"\x1b[I");

    // Same frame: click inside term_rect (outside bar_rect) + Ctrl+Shift+F.
    let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
            Event::Key {
                key: Key::F,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: ctrl_shift,
            },
            Event::Text("f".to_owned()),
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(
        rt.search_bar_mut().open,
        "Ctrl+Shift+F with same-frame outside click must open search bar"
    );

    // Next frame: typing must go into search bar, and Escape must close it.
    let _ = rt.input_mut().drain_written();
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![Event::Text("abc".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(
        rt.search_bar_mut().query_text,
        "abc",
        "search bar opened via Ctrl+Shift+F + same-frame click must receive typing"
    );
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "terminal must not receive typing while search bar is focused"
    );

    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.08,
        vec![Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(
        !rt.search_bar_mut().open,
        "Escape after Ctrl+Shift+F + same-frame outside click must close search bar"
    );
}

#[test]
fn scrollbar_primary_and_shift_secondary_preserve_focus_under_dec_1004_and_1000() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
    let mut payload = String::new();
    for i in 0..80 {
        use std::fmt::Write as _;
        writeln!(payload, "line {i}\r").expect("String fmt::Write is infallible");
    }
    payload.push_str("\x1b[?1004h\x1b[?1000h\x1b[?1006h");
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(payload.as_bytes()));

    let mut term_rect = Rect::NOTHING;
    let mut term_id = egui::Id::NULL;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let resp = rt.show(ui).response;
            term_rect = resp.rect;
            term_id = resp.id;
        });
    });
    assert!(rt.terminal().scrollback_len() > 0);

    let term_pos = pos2(term_rect.min.x + 24.0, term_rect.center().y);
    let sb_pos = pos2(term_rect.max.x - 2.0, term_rect.center().y);
    let adj_pos = pos2(term_rect.max.x - 10.0, term_rect.center().y);

    // Focus terminal first and drain focus-in + mouse bytes.
    let mut has_focus = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                has_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(has_focus);
    let _ = rt.input_mut().drain_written();

    // 1. Primary click on scrollbar track_rect must preserve focus (no \x1b[O).
    let mut sb_click_focus = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::PointerMoved(sb_pos),
            Event::PointerButton {
                pos: sb_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: sb_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                sb_click_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(
        sb_click_focus,
        "Primary click on scrollbar must keep terminal focus"
    );
    assert_eq!(ctx.memory(|m| m.focused()), Some(term_id));
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "Primary click on own scrollbar must emit zero bytes (no \\x1b[O)"
    );

    // 2. Split-frame Primary drag from scrollbar top into term_rect and release outside track.
    let sb_top = pos2(term_rect.max.x - 2.0, term_rect.min.y + 8.0);
    let mut drag_focus_1 = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![
            Event::PointerMoved(sb_top),
            Event::PointerButton {
                pos: sb_top,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                drag_focus_1 = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(drag_focus_1);
    assert!(rt.input_mut().drain_written().is_empty());

    let mut drag_focus_2 = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.08,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                drag_focus_2 = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(
        drag_focus_2,
        "split-frame scrollbar drag release must preserve focus"
    );
    assert!(rt.terminal().display_offset() > 0);
    assert!(rt.input_mut().drain_written().is_empty());

    // Subsequent typing still reaches terminal.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.10,
        vec![Event::Text("a".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.input_mut().drain_written(), b"a");

    // 3. Shift+Secondary click on scrollbar preserves focus, emits zero bytes, and suppresses menu.
    let mut shift_sec_focus = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.12,
        vec![
            Event::ModifiersChanged(Modifiers::SHIFT),
            Event::PointerMoved(sb_pos),
            Event::PointerButton {
                pos: sb_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::SHIFT,
            },
            Event::PointerButton {
                pos: sb_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::SHIFT,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                shift_sec_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(
        shift_sec_focus,
        "Shift+Secondary on scrollbar must preserve focus"
    );
    assert_eq!(ctx.memory(|m| m.focused()), Some(term_id));
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "Shift+Secondary on scrollbar must emit zero bytes (no \\x1b[O and no mouse tracking)"
    );
    let (sb_menu_clipped, _) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.14,
        vec![Event::ModifiersChanged(Modifiers::NONE)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let mut sb_menu_shapes = Vec::new();
    collect_shapes(&sb_menu_clipped, &mut sb_menu_shapes);
    assert!(
        !all_text(&sb_menu_shapes).contains("Clear Scrollback"),
        "Shift+Secondary on scrollbar must NOT open context menu"
    );

    // 4. Subsequent Shift+RightClick adjacent to scrollbar DOES open context menu and keeps focus.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.16,
        vec![
            Event::ModifiersChanged(Modifiers::SHIFT),
            Event::PointerMoved(adj_pos),
            Event::PointerButton {
                pos: adj_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::SHIFT,
            },
            Event::PointerButton {
                pos: adj_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::SHIFT,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.input_mut().drain_written().is_empty());
    let (adj_menu_clipped, _) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.18,
        vec![Event::ModifiersChanged(Modifiers::NONE)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let mut adj_menu_shapes = Vec::new();
    collect_shapes(&adj_menu_clipped, &mut adj_menu_shapes);
    assert!(
        all_text(&adj_menu_shapes).contains("Clear Scrollback"),
        "Shift+RightClick adjacent to scrollbar must still open context menu"
    );
}

#[test]
fn scrollbar_disappearing_mid_hold_preserves_focus_under_dec_1004() {
    let ctx = Context::default();
    let full_screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
    let tiny_screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 20.0));
    let mut payload = String::new();
    for i in 0..80 {
        use std::fmt::Write as _;
        writeln!(payload, "line {i}\r").expect("String fmt::Write is infallible");
    }
    payload.push_str("\x1b[?1004h");
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(payload.as_bytes()));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, full_screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    let term_pos = pos2(term_rect.min.x + 24.0, term_rect.center().y);
    let sb_pos = pos2(term_rect.max.x - 2.0, 10.0);

    // Focus terminal first and drain \x1b[I.
    let _ = run_frame_with(
        &ctx,
        full_screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.input_mut().drain_written(), b"\x1b[I");

    // Press Secondary on scrollbar, shrink pane < 24px so scrollbar disappears mid-hold, then release.
    let mut focus_press = false;
    let _ = run_frame_with(
        &ctx,
        full_screen,
        1.0,
        0.04,
        vec![
            Event::PointerMoved(sb_pos),
            Event::PointerButton {
                pos: sb_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                focus_press = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(focus_press);
    assert!(rt.input_mut().drain_written().is_empty());

    let mut focus_release = false;
    let _ = run_frame_with(
        &ctx,
        tiny_screen,
        1.0,
        0.06,
        vec![Event::PointerButton {
            pos: sb_pos,
            button: PointerButton::Secondary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                focus_release = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(
        focus_release,
        "releasing scrollbar hold after scrollbar disappears must preserve terminal focus"
    );
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "releasing disappearing scrollbar hold must emit zero \\x1b[O"
    );
}

#[cfg(feature = "search")]
#[test]
fn multipane_scrollbar_drag_does_not_steal_focus_from_other_pane_search_bar() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(700.0, 240.0));
    let mut payload_a = String::new();
    for i in 0..80 {
        use std::fmt::Write as _;
        writeln!(payload_a, "pane a line {i}\r").expect("String fmt::Write is infallible");
    }
    let mut rt_a = TerminalRuntime::new(RingBufferInput::from_bytes(payload_a.as_bytes()));
    let mut rt_b = TerminalRuntime::new(RingBufferInput::from_bytes(b"pane b\r\n"));

    let mut rect_a = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.columns(2, |cols| {
                rect_a = rt_a.show(&mut cols[0]).response.rect;
                let _ = rt_b.show(&mut cols[1]);
            });
        });
    });

    // Open and focus Pane B's search bar and verify it receives typing.
    rt_b.open_search();
    let _ = run_frame_with(&ctx, screen, 1.0, 0.02, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ui.columns(2, |cols| {
                let _ = rt_a.show(&mut cols[0]);
                let _ = rt_b.show(&mut cols[1]);
            });
        });
    });
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.03,
        vec![Event::Text("b".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.columns(2, |cols| {
                    let _ = rt_a.show(&mut cols[0]);
                    let _ = rt_b.show(&mut cols[1]);
                });
            });
        },
    );
    assert_eq!(rt_b.search_bar_mut().query_text, "b");

    // Drag Pane A's scrollbar while Pane B's search bar held focus:
    // Pane A must NOT steal terminal focus (`rt_a.has_focus() == false`).
    let sb_a_pos = pos2(rect_a.max.x - 2.0, rect_a.min.y + 10.0);
    let mut focus_a = false;
    let mut term_a_id = egui::Id::NULL;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::PointerMoved(sb_a_pos),
            Event::PointerButton {
                pos: sb_a_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerMoved(pos2(sb_a_pos.x, rect_a.center().y)),
            Event::PointerButton {
                pos: pos2(sb_a_pos.x, rect_a.center().y),
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.columns(2, |cols| {
                    let resp_a = rt_a.show(&mut cols[0]).response;
                    focus_a = resp_a.has_focus();
                    term_a_id = resp_a.id;
                    let _ = rt_b.show(&mut cols[1]);
                });
            });
        },
    );
    assert!(
        !focus_a,
        "Pane A scrollbar interaction must not steal focus when Pane B search bar held focus"
    );
    assert_ne!(ctx.memory(|m| m.focused()), Some(term_a_id));

    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![Event::Text("z".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                ui.columns(2, |cols| {
                    let _ = rt_a.show(&mut cols[0]);
                    let _ = rt_b.show(&mut cols[1]);
                });
            });
        },
    );
    assert!(
        rt_a.input_mut().drain_written().is_empty(),
        "Pane A must not receive keystrokes after dragging its unfocused scrollbar"
    );
    assert!(rt_b.search_bar_mut().open);
}

#[cfg(feature = "search")]
#[test]
fn open_search_then_same_frame_secondary_click_under_dec_1000_forwards_keystroke() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(500.0, 240.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(
        b"\x1b[?1000h\x1b[?1006h\x1b[?1004hhello\r\n",
    ));

    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_rect = rt.show(ui).response.rect;
        });
    });

    let term_pos = pos2(term_rect.min.x + 20.0, term_rect.max.y - 20.0);
    // Focus terminal first and drain \x1b[I + initial mouse click bytes.
    let mut focused = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.02,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                focused = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(focused);
    let _ = rt.input_mut().drain_written();

    // Call open_search(), then in the same frame send Secondary click inside term_rect + Event::Text("k").
    rt.open_search();
    let mut has_focus_after = false;
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![
            Event::PointerMoved(term_pos),
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: term_pos,
                button: PointerButton::Secondary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
            Event::Text("k".to_owned()),
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                has_focus_after = rt.show(ui).response.has_focus();
            });
        },
    );
    let written = String::from_utf8(rt.input_mut().drain_written()).expect("utf8");
    assert!(has_focus_after, "terminal must remain focused");
    assert!(
        !written.contains("\x1b[O"),
        "must not emit focus-out \\x1b[O, got {written:?}"
    );
    assert!(
        written.contains("\x1b[<2;") && written.ends_with('k'),
        "same-frame Secondary click + 'k' must both reach terminal, got {written:?}"
    );
    assert!(
        rt.search_bar_mut().query_text.is_empty(),
        "search bar must not steal 'k'"
    );
}

#[test]
fn unfocused_window_with_memory_focus_forwards_text_and_enter_without_1004_bounce() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"\x1b[?1004hready\r\n"));

    let mut term_id = egui::Id::NULL;
    let mut term_rect = Rect::NOTHING;
    let _ = run_frame_with_focus(&ctx, screen, 1.0, 0.0, false, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let resp = rt.show(ui);
            term_id = resp.response.id;
            term_rect = resp.response.rect;
        });
    });

    // Click terminal while RawInput.focused == false (simulating automated / non-foreground input).
    let c = term_rect.center();
    let mut resp_has_focus = true;
    let _ = run_frame_with_focus(
        &ctx,
        screen,
        1.0,
        0.02,
        false,
        vec![
            Event::PointerMoved(c),
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: c,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                resp_has_focus = rt.show(ui).response.has_focus();
            });
        },
    );
    assert!(
        !resp_has_focus,
        "Response::has_focus() stays false when RawInput.focused is false so cursor renders hollow"
    );
    assert!(
        ctx.memory(|m| m.has_focus(term_id)),
        "terminal must hold egui::Memory focus after click"
    );
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "must not emit spurious DEC ?1004 \\x1b[I/\\x1b[O while RawInput.focused is false"
    );

    // Send Event::Text and Event::Key(Key::Enter) while RawInput.focused == false.
    let _ = run_frame_with_focus(
        &ctx,
        screen,
        1.0,
        0.04,
        false,
        vec![
            Event::Text("size".to_owned()),
            Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(
        rt.input_mut().drain_written(),
        b"size\r",
        "Event::Text and Event::Key(Enter) must reach write_bytes when memory-focused"
    );
}

#[cfg(feature = "search")]
#[test]
fn open_search_from_external_button_click_focuses_search_input() {
    for two_frame_click in [false, true] {
        let ctx = Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
        let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"Reflow target\r\n"));
        let mut find_rect = Rect::NOTHING;
        let mut term_rect = Rect::NOTHING;

        let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
            ui.horizontal(|ui| {
                let find_btn = ui.button("Find");
                find_rect = find_btn.rect;
                if find_btn.clicked() {
                    rt.open_search();
                }
            });
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                term_rect = rt.show(ui).response.rect;
            });
        });

        let find_center = find_rect.center();
        let mut t = 0.02;
        if two_frame_click {
            let _ = run_frame_with(
                &ctx,
                screen,
                1.0,
                t,
                vec![
                    Event::PointerMoved(find_center),
                    Event::PointerButton {
                        pos: find_center,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    },
                ],
                |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Find").clicked() {
                            rt.open_search();
                        }
                    });
                    CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                        let _ = rt.show(ui);
                    });
                },
            );
            t += 0.02;
            let _ = run_frame_with(
                &ctx,
                screen,
                1.0,
                t,
                vec![Event::PointerButton {
                    pos: find_center,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                }],
                |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Find").clicked() {
                            rt.open_search();
                        }
                    });
                    CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                        let _ = rt.show(ui);
                    });
                },
            );
        } else {
            let _ = run_frame_with(
                &ctx,
                screen,
                1.0,
                t,
                vec![
                    Event::PointerMoved(find_center),
                    Event::PointerButton {
                        pos: find_center,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    },
                    Event::PointerButton {
                        pos: find_center,
                        button: PointerButton::Primary,
                        pressed: false,
                        modifiers: Modifiers::NONE,
                    },
                ],
                |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Find").clicked() {
                            rt.open_search();
                        }
                    });
                    CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                        let _ = rt.show(ui);
                    });
                },
            );
        }
        assert!(rt.search_bar_mut().open);

        // Next frame: typing goes directly into the search bar TextEdit without clicking it.
        t += 0.02;
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            t,
            vec![Event::Text("Reflow".to_owned())],
            |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Find").clicked() {
                        rt.open_search();
                    }
                });
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    let _ = rt.show(ui);
                });
            },
        );
        assert_eq!(
            rt.search_bar_mut().query_text,
            "Reflow",
            "two_frame_click={two_frame_click}: Find button click must focus search TextEdit"
        );
        assert!(rt.input_mut().drain_written().is_empty());

        // Later frame: clicking outside bar_rect inside terminal still surrenders search focus.
        t += 0.02;
        let term_pos = pos2(term_rect.min.x + 20.0, term_rect.max.y - 20.0);
        let mut term_focused = false;
        let _ = run_frame_with(
            &ctx,
            screen,
            1.0,
            t,
            vec![
                Event::PointerMoved(term_pos),
                Event::PointerButton {
                    pos: term_pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                Event::PointerButton {
                    pos: term_pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ],
            |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Find").clicked() {
                        rt.open_search();
                    }
                });
                CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                    term_focused = rt.show(ui).response.has_focus();
                });
            },
        );
        assert!(
            term_focused,
            "clicking terminal on a later frame must surrender search focus"
        );
    }
}

#[cfg(feature = "search")]
#[test]
fn search_bar_escape_closes_when_window_unfocused() {
    let ctx = Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0));
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"Reflow sample\r\n"));
    let mut find_rect = Rect::NOTHING;
    let mut term_id = egui::Id::NULL;

    let _ = run_frame_with_focus(&ctx, screen, 1.0, 0.0, false, Vec::new(), |ui| {
        ui.horizontal(|ui| {
            find_rect = ui.button("Find").rect;
        });
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            term_id = rt.show(ui).response.id;
        });
    });

    // Click Find while RawInput.focused == false.
    let find_center = find_rect.center();
    let _ = run_frame_with_focus(
        &ctx,
        screen,
        1.0,
        0.02,
        false,
        vec![
            Event::PointerMoved(find_center),
            Event::PointerButton {
                pos: find_center,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: find_center,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            ui.horizontal(|ui| {
                if ui.button("Find").clicked() {
                    rt.open_search();
                }
            });
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.search_bar_mut().open);

    // Type query while RawInput.focused == false.
    let _ = run_frame_with_focus(
        &ctx,
        screen,
        1.0,
        0.04,
        false,
        vec![Event::Text("Reflow".to_owned())],
        |ui| {
            ui.horizontal(|ui| {
                if ui.button("Find").clicked() {
                    rt.open_search();
                }
            });
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_eq!(rt.search_bar_mut().query_text, "Reflow");

    // Press Escape while RawInput.focused == false -> closes bar and returns Memory focus to terminal.
    let _ = run_frame_with_focus(
        &ctx,
        screen,
        1.0,
        0.06,
        false,
        vec![Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        |ui| {
            ui.horizontal(|ui| {
                if ui.button("Find").clicked() {
                    rt.open_search();
                }
            });
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(
        !rt.search_bar_mut().open,
        "Escape must close search bar even when RawInput.focused is false"
    );
    assert!(
        rt.input_mut().drain_written().is_empty(),
        "Escape closing search bar must not leak \\x1b to terminal"
    );
    assert!(
        ctx.memory(|m| m.has_focus(term_id)),
        "terminal must reclaim egui::Memory focus after Escape closes search bar"
    );
}

#[test]
fn sgr_blink_schedules_repaint_only_when_visible_and_toggles_visibility() {
    use std::sync::mpsc;
    use std::time::Duration;

    let ctx = Context::default();
    let (repaint_tx, repaint_rx) = mpsc::channel();
    ctx.set_repaint_observer(move |info| repaint_tx.send(info.delay).unwrap());

    let cfg = TerminalConfig {
        cursor_blink: false,
        idle_poll_interval_ms: 0,
        enable_blink: true,
        ..TerminalConfig::default()
    };
    let mut rt = TerminalRuntime::with_config(RingBufferInput::from_bytes(b"plain text"), cfg);
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));

    // No blinking cells in viewport -> no blink repaint scheduled.
    run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    assert!(
        !repaint_rx
            .try_iter()
            .any(|d| d > Duration::ZERO && d <= Duration::from_millis(500)),
        "must not schedule blink repaint when no blinking cells are visible"
    );

    // Feed SGR 5 blinking text.
    rt.terminal_mut().advance(b"\r\n\x1b[5mBLINK\x1b[0m");

    assert_eq!(
        rt.terminal().visible_line_text(0).as_deref(),
        Some("plain text")
    );
    assert_eq!(rt.terminal().visible_line_text(1).as_deref(), Some("BLINK"));
    let plain_verts = "plaintext".len() * 4;
    let total_on_verts = ("plaintext".len() + "BLINK".len()) * 4;

    // At t = 0.0 (on phase), BLINK is visible and schedules <= 500ms repaint.
    let (clipped_on, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes_on = Vec::new();
    collect_shapes(&clipped_on, &mut shapes_on);
    let blink_visible_on = find_text_mesh(&shapes_on).is_some_and(|m| {
        m.vertices.len() == total_on_verts && m.vertices.iter().all(|v| v.color.a() > 0)
    });
    assert!(blink_visible_on, "blinking text should be visible at t=0.0");
    assert!(
        repaint_rx
            .try_iter()
            .any(|d| d > Duration::ZERO && d <= Duration::from_millis(500)),
        "must schedule blink repaint when blinking cells are visible"
    );

    // At t = 0.6 (off phase), BLINK is hidden from coalesced text_mesh while "plain text" remains.
    let (clipped_off, _) = run_frame_with(&ctx, screen, 1.0, 0.6, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes_off = Vec::new();
    collect_shapes(&clipped_off, &mut shapes_off);
    let blink_hidden_off = find_text_mesh(&shapes_off).is_some_and(|m| {
        m.vertices.len() == plain_verts && m.vertices.iter().all(|v| v.color.a() > 0)
    });
    assert!(
        blink_hidden_off,
        "blinking text should be hidden during off phase (t=0.6)"
    );

    // Disabling enable_blink keeps text visible and stops scheduling blink repaints.
    rt.config_mut().enable_blink = false;
    let _ = repaint_rx.try_iter().count();
    let (clipped_disabled, _) = run_frame_with(&ctx, screen, 1.0, 0.6, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes_disabled = Vec::new();
    collect_shapes(&clipped_disabled, &mut shapes_disabled);
    let blink_visible_disabled = find_text_mesh(&shapes_disabled).is_some_and(|m| {
        m.vertices.len() == total_on_verts && m.vertices.iter().all(|v| v.color.a() > 0)
    });
    assert!(
        blink_visible_disabled,
        "blinking text should stay visible when enable_blink=false"
    );
    assert!(
        !repaint_rx
            .try_iter()
            .any(|d| d > Duration::ZERO && d <= Duration::from_millis(500)),
        "must not schedule blink repaint when enable_blink=false"
    );
}

#[test]
fn visual_bell_flashes_and_decays_without_idle_repaints() {
    use std::sync::mpsc;
    use std::time::Duration;

    let ctx = Context::default();
    let (repaint_tx, repaint_rx) = mpsc::channel();
    ctx.set_repaint_observer(move |info| repaint_tx.send(info.delay).unwrap());

    let cfg = TerminalConfig {
        cursor_blink: false,
        idle_poll_interval_ms: 0,
        visual_bell_duration_ms: 150,
        ..TerminalConfig::default()
    };
    let mut rt = TerminalRuntime::with_config(RingBufferInput::from_bytes(b"hello"), cfg);
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));

    let count_translucent_term_rects = |clipped: &[egui::epaint::ClippedShape]| -> Vec<u8> {
        let mut shapes = Vec::new();
        collect_shapes(clipped, &mut shapes);
        shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Rect(r) if r.fill.a() > 0 && r.fill.a() < 100 && r.rect.width() > 300.0 => {
                    Some(r.fill.a())
                }
                _ => None,
            })
            .collect()
    };

    // Frame 0: no bell.
    let (clipped0, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    assert!(count_translucent_term_rects(&clipped0).is_empty());
    let _ = repaint_rx.try_iter().count();

    // Trigger BEL via input stream.
    rt.input_mut().push_str("\x07");
    let (clipped1, _) = run_frame_with(&ctx, screen, 1.0, 1.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let alphas1 = count_translucent_term_rects(&clipped1);
    assert_eq!(alphas1.len(), 1, "visual bell flash should render on BEL");
    assert!(
        repaint_rx
            .try_iter()
            .any(|d| d > Duration::ZERO && d <= Duration::from_millis(20)),
        "active visual bell must schedule decay repaint"
    );

    // Mid-decay at t = 1.075: lower alpha.
    let (clipped2, _) = run_frame_with(&ctx, screen, 1.0, 1.075, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let alphas2 = count_translucent_term_rects(&clipped2);
    assert_eq!(alphas2.len(), 1);
    assert!(alphas2[0] < alphas1[0], "visual bell alpha should decay");
    let _ = repaint_rx.try_iter().count();

    // Past duration at t = 1.20: flash gone and no further decay repaint.
    let (clipped3, _) = run_frame_with(&ctx, screen, 1.0, 1.20, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    assert!(count_translucent_term_rects(&clipped3).is_empty());
    assert!(
        !repaint_rx.try_iter().any(|d| d > Duration::ZERO),
        "expired visual bell must not schedule repaints"
    );
}

#[cfg(feature = "search")]
#[test]
fn search_bar_displays_regex_error_inline_and_supports_whole_word() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b"cat scatter category\r\n"));
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));

    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });

    rt.open_search();
    rt.search_bar_mut().is_regex = true;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.02, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });

    // Type invalid regex `[unclosed` -> error stored and rendered inline.
    let (clipped_err, _) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.04,
        vec![Event::Text("[unclosed".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.search_bar_mut().last_error.is_some());
    let mut err_shapes = Vec::new();
    collect_shapes(&clipped_err, &mut err_shapes);
    assert!(
        all_text(&err_shapes).contains("invalid search regex"),
        "search bar must display regex compilation error inline"
    );

    // Switch to literal + whole_word and search for "cat" -> matches only whole word "cat" (1/1).
    rt.search_bar_mut().is_regex = false;
    rt.search_bar_mut().whole_word = true;
    rt.search_bar_mut().query_text = "ca".to_owned();
    let (clipped_ww, _) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.06,
        vec![Event::Text("t".to_owned())],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(rt.search_bar_mut().last_error.is_none());
    assert_eq!(rt.terminal().search_matches().len(), 1);
    let mut ww_shapes = Vec::new();
    collect_shapes(&clipped_ww, &mut ww_shapes);
    assert!(all_text(&ww_shapes).contains("0/1"));
}

#[cfg(feature = "hyperlinks")]
#[test]
fn hyperlink_tooltip_and_require_ctrl_click() {
    let ctx = Context::default();
    let cfg = TerminalConfig {
        hyperlink_require_ctrl: true,
        ..TerminalConfig::default()
    };
    let mut rt = TerminalRuntime::with_config(
        RingBufferInput::from_bytes(b"\x1b]8;;https://example.org/docs\x07docs\x1b]8;;\x07"),
        cfg,
    );
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));

    let mut resp = None;
    let _ = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            resp = Some(rt.show(ui));
        });
    });
    let r = resp.expect("resp");
    let link_pos = pos2(
        r.response.rect.min.x + 1.5 * r.cell_size.x,
        r.response.rect.min.y + 0.5 * r.cell_size.y,
    );

    // Hover without Ctrl: shows tooltip with URI (allow 2nd frame for egui Popup sizing),
    // and does NOT set PointingHand.
    let _ = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.1,
        vec![Event::PointerMoved(link_pos)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    let (clipped_hover, plat_hover) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.12,
        vec![Event::PointerMoved(link_pos)],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert_ne!(plat_hover.cursor_icon, CursorIcon::PointingHand);
    let mut hover_shapes = Vec::new();
    collect_shapes(&clipped_hover, &mut hover_shapes);
    assert!(
        all_text(&hover_shapes).contains("https://example.org/docs"),
        "hovering hyperlink must show tooltip with target URI"
    );

    // Plain click without Ctrl: must NOT open URL when hyperlink_require_ctrl=true.
    let (_, plat_plain_click) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.2,
        vec![
            Event::PointerButton {
                pos: link_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerButton {
                pos: link_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(
        !plat_plain_click
            .commands
            .iter()
            .any(|c| matches!(c, OutputCommand::OpenUrl(_))),
        "plain click must not open URL when hyperlink_require_ctrl=true"
    );

    // Ctrl+Click (well after previous click so it's a single click): opens URL.
    let (_, plat_ctrl_click) = run_frame_with(
        &ctx,
        screen,
        1.0,
        1.0,
        vec![
            Event::PointerButton {
                pos: link_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::CTRL,
            },
            Event::PointerButton {
                pos: link_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::CTRL,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );
    assert!(
        plat_ctrl_click
            .commands
            .iter()
            .any(|c| matches!(c, OutputCommand::OpenUrl(u) if u.url == "https://example.org/docs")),
        "Ctrl+Click must open URL when hyperlink_require_ctrl=true"
    );
}

#[cfg(feature = "kitty-graphics")]
#[test]
fn kitty_graphics_renders_cached_texture_and_evicts_on_delete() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b""));
    rt.config_mut().cursor_blink = false;
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));

    // 2x2 RGBA (16 bytes of 0xFF = white) base64-encoded = "/////////////////////w=="? Wait:
    // 16 bytes of 0xFF: 15 bytes = 20 '////////////', 1 byte 0xFF = '/w==', total 24 chars: "/////////////////////w=="
    rt.terminal_mut()
        .advance(b"\x1b_Ga=T,f=32,s=2,v=2,c=2,r=1;/////////////////////w==\x1b\\");
    assert_eq!(rt.terminal().image_placements().len(), 1);

    let (clipped1, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes1 = Vec::new();
    collect_shapes(&clipped1, &mut shapes1);
    let tex_ids_1: Vec<egui::TextureId> = shapes1
        .iter()
        .filter_map(|s| match s {
            Shape::Mesh(m) if m.texture_id != egui::TextureId::Managed(0) => Some(m.texture_id),
            _ => None,
        })
        .collect();
    assert_eq!(tex_ids_1.len(), 1, "expected 1 inline image mesh");

    // Second frame: same image must reuse the exact same TextureId (no per-frame reload).
    let (clipped2, _) = run_frame_with(&ctx, screen, 1.0, 0.1, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes2 = Vec::new();
    collect_shapes(&clipped2, &mut shapes2);
    let tex_ids_2: Vec<egui::TextureId> = shapes2
        .iter()
        .filter_map(|s| match s {
            Shape::Mesh(m) if m.texture_id != egui::TextureId::Managed(0) => Some(m.texture_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        tex_ids_2, tex_ids_1,
        "texture handle must be cached across frames"
    );

    // Delete all images: mesh must disappear.
    rt.terminal_mut().advance(b"\x1b_Ga=d,d=a\x1b\\");
    assert!(rt.terminal().image_placements().is_empty());
    let (clipped3, _) = run_frame_with(&ctx, screen, 1.0, 0.2, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes3 = Vec::new();
    collect_shapes(&clipped3, &mut shapes3);
    let tex_ids_3: Vec<egui::TextureId> = shapes3
        .iter()
        .filter_map(|s| match s {
            Shape::Mesh(m) if m.texture_id != egui::TextureId::Managed(0) => Some(m.texture_id),
            _ => None,
        })
        .collect();
    assert!(tex_ids_3.is_empty(), "deleted image must no longer render");
}

#[cfg(feature = "sixel")]
#[test]
fn sixel_graphics_renders_textured_mesh() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b""));
    rt.config_mut().cursor_blink = false;
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));

    rt.terminal_mut().advance(b"\x1bPq#0;2;100;0;0#0~~-\x1b\\");
    assert_eq!(rt.terminal().image_placements().len(), 1);

    let (clipped, _) = run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    let mut shapes = Vec::new();
    collect_shapes(&clipped, &mut shapes);
    let has_image_mesh = shapes.iter().any(|s| match s {
        Shape::Mesh(m) => m.texture_id != egui::TextureId::Managed(0),
        _ => false,
    });
    assert!(
        has_image_mesh,
        "expected Sixel image to render as a textured mesh"
    );
}

#[test]
fn sgr_pixels_1016_encodes_pixel_coordinates_and_subcell_motion() {
    use std::sync::{Arc, Mutex};

    struct RecordingInput(Arc<Mutex<Vec<u8>>>);
    impl TerminalInput for RecordingInput {
        fn poll_read(&mut self, _buf: &mut [u8]) -> Result<usize, TerminalIoError> {
            Ok(0)
        }
        fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), TerminalIoError> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        }
    }

    let written = Arc::new(Mutex::new(Vec::<u8>::new()));
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RecordingInput(Arc::clone(&written)));
    rt.config_mut().cursor_blink = false;
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));

    // Enable AnyEvent (1003) + SGR-Pixels (1016).
    rt.terminal_mut().advance(b"\x1b[?1003;1016h");

    // Frame 1: establish layout.
    run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            let _ = rt.show(ui);
        });
    });
    written.lock().unwrap().clear();

    // Frame 2: press at (12, 20), move 2px within same cell to (14, 21), release at (14, 21).
    run_frame_with(
        &ctx,
        screen,
        1.0,
        0.1,
        vec![
            Event::PointerButton {
                pos: pos2(12.0, 20.0),
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            Event::PointerMoved(pos2(14.0, 21.0)),
            Event::PointerButton {
                pos: pos2(14.0, 21.0),
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    let out = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
    assert!(
        out.contains("\x1b[<0;12;20M"),
        "expected SGR-Pixels press at (12, 20), got {out:?}"
    );
    assert!(
        out.contains("\x1b[<32;14;21M"),
        "expected SGR-Pixels sub-cell drag motion at (14, 21), got {out:?}"
    );
    assert!(
        out.contains("\x1b[<0;14;21m"),
        "expected SGR-Pixels release at (14, 21), got {out:?}"
    );
}

#[test]
fn osc_22_mouse_shape_and_osc_9_events_flow_through_runtime() {
    let ctx = Context::default();
    let mut rt = TerminalRuntime::new(RingBufferInput::from_bytes(b""));
    rt.config_mut().cursor_blink = false;
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(640.0, 400.0));

    // Emit OSC 22 cursor shape, OSC 9 notification, and OSC 9;4 progress.
    rt.terminal_mut()
        .advance(b"\x1b]22;crosshair\x07\x1b]9;Build done\x07\x1b]9;4;1;75\x07");

    // Frame 1: establish layout and drain OSC events.
    let mut events_out = Vec::new();
    run_frame_with(&ctx, screen, 1.0, 0.0, Vec::new(), |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            events_out = rt.show(ui).events;
        });
    });

    // Frame 2: hover over the terminal grid to observe the mapped cursor icon.
    let (_, plat) = run_frame_with(
        &ctx,
        screen,
        1.0,
        0.1,
        vec![Event::PointerMoved(pos2(100.0, 100.0))],
        |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let _ = rt.show(ui);
            });
        },
    );

    assert!(events_out.contains(&TerminalEvent::MouseShape("crosshair".to_owned())));
    assert!(events_out.contains(&TerminalEvent::Notification {
        title: String::new(),
        body: "Build done".to_owned(),
    }));
    assert!(events_out.contains(&TerminalEvent::Progress {
        state: 1,
        progress: 75,
    }));
    assert_eq!(plat.cursor_icon, CursorIcon::Crosshair);
}
