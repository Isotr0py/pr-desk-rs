//! Criterion benchmarks measuring single-frame update + auto-layout + 4-stage cached render +
//! `epaint::Tessellator` latency for a `200x60` terminal across idle, partial dirty, scrollback,
//! selection, cursor blink, UTF-8 wide/combining/emoji, and vector-glyph TUI workloads.

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use egui::{CentralPanel, Context, Frame, Pos2, RawInput, Rect, Vec2, ViewportId};
use egui_xterm::{RingBufferInput, ScaleMode, SelectionKind, TerminalConfig, TerminalRuntime};

const TARGET_COLS: u16 = 200;
const TARGET_ROWS: u16 = 60;

fn run_single_frame_show_only(
    ctx: &Context,
    runtime: &mut TerminalRuntime<RingBufferInput>,
    screen_rect: Rect,
    sim_time: f64,
) {
    let mut raw_input = RawInput {
        screen_rect: Some(screen_rect),
        time: Some(sim_time),
        focused: true,
        ..Default::default()
    };
    raw_input
        .viewports
        .entry(ViewportId::ROOT)
        .or_default()
        .native_pixels_per_point = Some(1.0);

    let full_output = ctx.run_ui(raw_input, |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            runtime.show(ui);
        });
    });

    black_box(full_output.shapes.len());
    full_output.drop_without_applying_deltas();
}

fn run_single_frame_and_tessellate(
    ctx: &Context,
    runtime: &mut TerminalRuntime<RingBufferInput>,
    screen_rect: Rect,
    sim_time: f64,
) {
    let mut raw_input = RawInput {
        screen_rect: Some(screen_rect),
        time: Some(sim_time),
        focused: true,
        ..Default::default()
    };
    raw_input
        .viewports
        .entry(ViewportId::ROOT)
        .or_default()
        .native_pixels_per_point = Some(1.0);

    let full_output = ctx.run_ui(raw_input, |ui| {
        CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            runtime.show(ui);
        });
    });

    let shapes = full_output.shapes.clone();
    let ppp = full_output.pixels_per_point;
    full_output.drop_without_applying_deltas();
    let primitives = ctx.tessellate(shapes, ppp);
    black_box(primitives);
}

fn build_truecolor_screen(cols: u16, rows: u16, seed: u8) -> Vec<u8> {
    let mut out = String::with_capacity(usize::from(cols) * usize::from(rows) * 24);
    out.push_str("\x1b[H");
    for r in 0..rows {
        for c in 0..cols {
            let fg_r = ((c + u16::from(seed)) * 3) as u8;
            let fg_g = ((r + u16::from(seed)) * 5) as u8;
            let bg_b = ((c + r) % 40) as u8;
            let ch = (b'A' + ((c as u8).wrapping_add(seed)) % 26) as char;
            if write!(out, "\x1b[38;2;{fg_r};{fg_g};220;48;2;12;16;{bg_b}m{ch}").is_err() {
                // `String`'s `fmt::Write` impl is infallible.
            }
        }
        if r + 1 < rows {
            out.push_str("\r\n");
        }
    }
    out.into_bytes()
}

fn build_single_truecolor_row(row_1based: u16, cols: u16, seed: u8) -> Vec<u8> {
    let mut out = String::with_capacity(usize::from(cols) * 24 + 16);
    if write!(out, "\x1b[{row_1based};1H").is_err() {
        // `String`'s `fmt::Write` impl is infallible.
    }
    for c in 0..cols {
        let fg_r = ((c + u16::from(seed)) * 7) as u8;
        let bg_g = ((c + u16::from(seed)) % 48) as u8;
        let ch = (b'a' + ((c as u8).wrapping_add(seed)) % 26) as char;
        if write!(out, "\x1b[38;2;{fg_r};200;160;48;2;18;{bg_g};24m{ch}").is_err() {
            // `String`'s `fmt::Write` impl is infallible.
        }
    }
    out.into_bytes()
}

fn build_truecolor_scrollback_lines(lines: usize, cols: u16) -> Vec<u8> {
    let mut out = String::with_capacity(lines * (usize::from(cols) + 24));
    let pad_len = usize::from(cols).saturating_sub(24);
    let pad = "X".repeat(pad_len);
    for idx in 0..lines {
        let r = ((idx * 17) % 256) as u8;
        let g = ((idx * 31) % 256) as u8;
        let b = ((idx * 47) % 256) as u8;
        if write!(
            out,
            "\x1b[38;2;{r};{g};{b}mLINE_{idx:05}_{pad}__END\x1b[0m\r\n"
        )
        .is_err()
        {
            // `String`'s `fmt::Write` impl is infallible.
        }
    }
    out.into_bytes()
}

#[cfg(feature = "shell-integration")]
fn build_osc133_zones_payload(commands: usize) -> Vec<u8> {
    let mut out = String::with_capacity(commands * 96);
    for idx in 0..commands {
        let exit_code = usize::from(idx % 5 == 4);
        if write!(
            out,
            "\x1b]133;A\x07$ \x1b]133;B\x07cmd_{idx}\r\n\x1b]133;C\x07ok\r\n\x1b]133;D;{exit_code}\x07"
        )
        .is_err()
        {
            // `String`'s `fmt::Write` impl is infallible.
        }
    }
    out.into_bytes()
}

fn build_utf8_wide_screen(rows: u16, seed: usize) -> Vec<u8> {
    // 9 repetitions * up to 20 display columns <= 180 columns (< 200 TARGET_COLS, no line wrapping).
    const PATTERN_A: &str = "中文排版測試e\u{0301}🚀🎉👩‍🔬 ";
    const PATTERN_B: &str = "寬字元對齊a\u{0301}🔥✨👨\u{200D}💻 ";
    let mut out = String::with_capacity(usize::from(rows) * 256);
    out.push_str("\x1b[H");
    for r in 0..rows {
        let pat = if (usize::from(r) + seed).is_multiple_of(2) {
            PATTERN_A
        } else {
            PATTERN_B
        };
        let fg_r = ((usize::from(r) + seed) * 19 % 256) as u8;
        if write!(out, "\x1b[38;2;{fg_r};210;180m").is_err() {
            // `String`'s `fmt::Write` impl is infallible.
        }
        for _ in 0..9 {
            out.push_str(pat);
        }
        out.push_str("\x1b[K");
        if r + 1 < rows {
            out.push_str("\r\n");
        }
    }
    out.into_bytes()
}

fn build_utf8_wide_single_row(row_1based: u16, seed: usize) -> Vec<u8> {
    const PATTERN_A: &str = "中文排版測試e\u{0301}🚀🎉👩‍🔬 ";
    const PATTERN_B: &str = "寬字元對齊a\u{0301}🔥✨👨\u{200D}💻 ";
    let pat = if seed.is_multiple_of(2) {
        PATTERN_A
    } else {
        PATTERN_B
    };
    let mut out = String::with_capacity(320);
    let fg_g = ((seed * 29) % 256) as u8;
    if write!(out, "\x1b[{row_1based};1H\x1b[38;2;200;{fg_g};220m").is_err() {
        // `String`'s `fmt::Write` impl is infallible.
    }
    for _ in 0..9 {
        out.push_str(pat);
    }
    out.push_str("\x1b[K");
    out.into_bytes()
}

#[cfg(feature = "vector-glyphs")]
fn build_vector_glyphs_dashboard(rows: u16, seed: usize) -> Vec<u8> {
    const BLOCKS: &str = "▁▂▃▄▅▆▇█▇▆▅▄▃▂▁▂▃▄▅▆";
    const BRAILLE: &str = "⠁⠃⠇⠏⠟⠿⡿⣿⡿⠿⠟⠏⠇⠃⠁⠃⠇⠏⠟⠿";
    let mut out = String::with_capacity(usize::from(rows) * 320);
    out.push_str("\x1b[H");
    let top_border = format!("┌{}┬{}┐", "─".repeat(98), "─".repeat(99));
    let mid_border = format!("├{}┼{}┤", "─".repeat(98), "─".repeat(99));
    let bot_border = format!("└{}┴{}┘", "─".repeat(98), "─".repeat(99));
    for r in 0..rows {
        if r == 0 {
            out.push_str(&top_border);
        } else if r == rows - 1 {
            out.push_str(&bot_border);
        } else if r % 10 == 0 {
            out.push_str(&mid_border);
        } else {
            let phase = (usize::from(r) + seed) % 8;
            let status = if phase.is_multiple_of(2) {
                "\x1b[32;4:3;58:2::80:220:120mHEALTHY\x1b[0m"
            } else {
                "\x1b[33;4:3;58:2::240:180:60mWARNING\x1b[0m"
            };
            if write!(
                out,
                "│ \x1b[44;37m node-{r:02} \x1b[34;49m\u{E0B0} {BLOCKS} {BRAILLE} {status} \
                 \x1b[36m\u{E0B2}\x1b[46;30m p={phase:02} \x1b[0m\x1b[200G│",
            )
            .is_err()
            {
                // `String`'s `fmt::Write` impl is infallible.
            }
        }
        if r + 1 < rows {
            out.push_str("\r\n");
        }
    }
    out.into_bytes()
}

fn setup_200x60_harness_with_blink(
    cursor_blink: bool,
) -> (Context, TerminalRuntime<RingBufferInput>, Rect) {
    let ctx = Context::default();
    let cfg = TerminalConfig {
        scale_mode: ScaleMode::ReflowGrid,
        resize_debounce_secs: 0.0,
        read_budget_bytes_per_frame: usize::MAX,
        cursor_blink,
        ..TerminalConfig::default()
    };
    let mut runtime = TerminalRuntime::with_config(RingBufferInput::new(), cfg);

    // Measure cell_size on a warm-up frame, then calibrate exact 200x60 viewport.
    let probe_rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 960.0));
    let mut cell_size = Vec2::new(8.0, 16.0);
    for step in 0..2 {
        let mut raw = RawInput {
            screen_rect: Some(probe_rect),
            time: Some(step as f64 * 0.016),
            focused: true,
            ..Default::default()
        };
        raw.viewports
            .entry(ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(1.0);
        let full = ctx.run_ui(raw, |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let resp = runtime.show(ui);
                cell_size = resp.cell_size;
                if cursor_blink {
                    resp.response.request_focus();
                }
            });
        });
        full.drop_without_applying_deltas();
    }

    let screen_rect = Rect::from_min_size(
        Pos2::ZERO,
        Vec2::new(
            f32::from(TARGET_COLS) * cell_size.x + 1.0,
            f32::from(TARGET_ROWS) * cell_size.y + 1.0,
        ),
    );
    let initial = build_truecolor_screen(TARGET_COLS, TARGET_ROWS, 0);
    runtime.input_mut().push(&initial);
    let mut has_focus = false;
    for step in 2..5 {
        let mut raw = RawInput {
            screen_rect: Some(screen_rect),
            time: Some(step as f64 * 0.016),
            focused: true,
            ..Default::default()
        };
        raw.viewports
            .entry(ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(1.0);
        let full = ctx.run_ui(raw, |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let resp = runtime.show(ui);
                has_focus = resp.response.has_focus();
                if cursor_blink && !has_focus {
                    resp.response.request_focus();
                }
            });
        });
        let shapes = full.shapes.clone();
        let ppp = full.pixels_per_point;
        full.drop_without_applying_deltas();
        black_box(ctx.tessellate(shapes, ppp));
    }
    assert_eq!(runtime.terminal().size(), (TARGET_COLS, TARGET_ROWS));
    if cursor_blink {
        assert!(has_focus, "cursor_blink_toggle setup must hold focus");
    }
    (ctx, runtime, screen_rect)
}

fn setup_200x60_harness() -> (Context, TerminalRuntime<RingBufferInput>, Rect) {
    setup_200x60_harness_with_blink(false)
}

fn bench_frame_render_stages(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_render_stages");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(4));
    group.throughput(Throughput::Elements(
        u64::from(TARGET_COLS) * u64::from(TARGET_ROWS),
    ));

    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let mut t = 1.0;
        group.bench_function("show_only_idle", |b| {
            b.iter(|| {
                t += 1.0 / 60.0;
                run_single_frame_show_only(&ctx, &mut runtime, screen_rect, t);
            });
        });
    }

    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let mut t = 1.0;
        group.bench_function("show_and_tessellate_idle", |b| {
            b.iter(|| {
                t += 1.0 / 60.0;
                run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
            });
        });
    }

    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let screen_a = build_truecolor_screen(TARGET_COLS, TARGET_ROWS, 11);
        let screen_b = build_truecolor_screen(TARGET_COLS, TARGET_ROWS, 29);
        let mut t = 1.0;
        let mut toggle = false;
        group.bench_function("show_only_full_redraw", |b| {
            b.iter_batched(
                || {
                    toggle = !toggle;
                    if toggle { &screen_a } else { &screen_b }
                },
                |payload| {
                    runtime.input_mut().push(payload);
                    t += 1.0 / 60.0;
                    run_single_frame_show_only(&ctx, &mut runtime, screen_rect, t);
                },
                BatchSize::SmallInput,
            );
        });
    }

    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let screen_a = build_truecolor_screen(TARGET_COLS, TARGET_ROWS, 11);
        let screen_b = build_truecolor_screen(TARGET_COLS, TARGET_ROWS, 29);
        let mut t = 1.0;
        let mut toggle = false;
        group.bench_function("show_and_tessellate_full_redraw", |b| {
            b.iter_batched(
                || {
                    toggle = !toggle;
                    if toggle { &screen_a } else { &screen_b }
                },
                |payload| {
                    runtime.input_mut().push(payload);
                    t += 1.0 / 60.0;
                    run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn bench_frame_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_render_200x60_truecolor");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(4));
    group.throughput(Throughput::Elements(
        u64::from(TARGET_COLS) * u64::from(TARGET_ROWS),
    ));

    // 1. Idle frame (100% RowRenderCache hit)
    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let mut t = 1.0;
        group.bench_function("idle", |b| {
            b.iter(|| {
                t += 1.0 / 60.0;
                run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
            });
        });
    }

    // 2. 1 dirty row per frame
    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let row_even = build_single_truecolor_row(30, TARGET_COLS, 1);
        let row_odd = build_single_truecolor_row(30, TARGET_COLS, 2);
        let mut t = 1.0;
        let mut toggle = false;
        group.bench_function("1_dirty_row", |b| {
            b.iter_batched(
                || {
                    toggle = !toggle;
                    if toggle { &row_even } else { &row_odd }
                },
                |payload| {
                    runtime.input_mut().push(payload);
                    t += 1.0 / 60.0;
                    run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
                },
                BatchSize::SmallInput,
            );
        });
    }

    // 3. Scroll 1 line per frame
    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let mut line_payload = Vec::with_capacity(usize::from(TARGET_COLS) * 24 + 8);
        line_payload.extend_from_slice(b"\r\n");
        let row_bytes = build_single_truecolor_row(TARGET_ROWS, TARGET_COLS, 5);
        line_payload.extend_from_slice(&row_bytes);
        let mut t = 1.0;
        group.bench_function("scroll_1_line", |b| {
            b.iter_batched(
                || &line_payload,
                |payload| {
                    runtime.input_mut().push(payload);
                    t += 1.0 / 60.0;
                    run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
                },
                BatchSize::SmallInput,
            );
        });
    }

    // 4. Worst-case full 200x60 TrueColor redraw
    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let screen_a = build_truecolor_screen(TARGET_COLS, TARGET_ROWS, 11);
        let screen_b = build_truecolor_screen(TARGET_COLS, TARGET_ROWS, 29);
        let mut t = 1.0;
        let mut toggle = false;
        group.bench_function("full_redraw", |b| {
            b.iter_batched(
                || {
                    toggle = !toggle;
                    if toggle { &screen_a } else { &screen_b }
                },
                |payload| {
                    runtime.input_mut().push(payload);
                    t += 1.0 / 60.0;
                    run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
                },
                BatchSize::SmallInput,
            );
        });
    }

    // 5. Cursor blink toggle (0.5s steps force BgCacheKey invalidation every frame)
    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness_with_blink(true);
        let mut t = 1.0;
        group.bench_function("cursor_blink_toggle", |b| {
            b.iter(|| {
                t += 0.5;
                run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
            });
        });
    }

    // 6. Active 1k-line selection on idle frames
    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let history = build_truecolor_scrollback_lines(1_200, TARGET_COLS);
        runtime.input_mut().push(&history);
        run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, 1.0);
        {
            let term = runtime.terminal_mut();
            term.scroll_display(1_000);
            term.selection_start(0, 0, SelectionKind::Stream);
            term.scroll_to_bottom();
            term.selection_update(usize::from(TARGET_ROWS - 1), usize::from(TARGET_COLS - 1));
            term.selection_finish();
            assert!(
                term.selection_text().is_some_and(|s| s.len() > 100_000),
                "active selection must span > 100 KB"
            );
        }
        let mut t = 1.5;
        group.bench_function("active_selection_1k_lines", |b| {
            b.iter(|| {
                t += 1.0 / 60.0;
                run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
            });
        });
    }

    // 7. 2,500 OSC 133 shell-integration zones on idle frames
    #[cfg(feature = "shell-integration")]
    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let zones_payload = build_osc133_zones_payload(2_500);
        runtime.input_mut().push(&zones_payload);
        let screen = build_truecolor_screen(TARGET_COLS, TARGET_ROWS, 3);
        runtime.input_mut().push(&screen);
        run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, 1.0);
        assert!(
            runtime.terminal().shell_zones().len() >= 2_500,
            "expected >= 2500 shell zones"
        );
        let mut t = 1.5;
        group.bench_function("osc133_2500_zones_idle", |b| {
            b.iter(|| {
                t += 1.0 / 60.0;
                run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
            });
        });
    }

    // 8. 10k scrollback viewport scrub (+/- 5 lines per frame)
    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let history = build_truecolor_scrollback_lines(10_500, TARGET_COLS);
        runtime.input_mut().push(&history);
        run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, 1.0);
        assert_eq!(runtime.terminal().scrollback_len(), 10_000);
        runtime.terminal_mut().scroll_display(5_000);
        let mut t = 1.5;
        let mut up = true;
        group.bench_function("scrollback_scrub_10k", |b| {
            b.iter(|| {
                let delta = if up { 5 } else { -5 };
                up = !up;
                runtime.terminal_mut().scroll_display(delta);
                t += 1.0 / 60.0;
                run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
            });
        });
    }

    group.finish();
}

fn bench_frame_render_utf8(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_render_200x60_utf8");
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(4));
    group.throughput(Throughput::Elements(
        u64::from(TARGET_COLS) * u64::from(TARGET_ROWS),
    ));

    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let screen_a = build_utf8_wide_screen(TARGET_ROWS, 0);
        let screen_b = build_utf8_wide_screen(TARGET_ROWS, 1);
        runtime.input_mut().push(&screen_a);
        run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, 1.0);
        let mut t = 1.5;
        let mut toggle = false;
        group.sample_size(30);
        group.bench_function("utf8_wide_full_redraw", |b| {
            b.iter_batched(
                || {
                    toggle = !toggle;
                    if toggle { &screen_b } else { &screen_a }
                },
                |payload| {
                    runtime.input_mut().push(payload);
                    t += 1.0 / 60.0;
                    run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
                },
                BatchSize::SmallInput,
            );
        });
    }

    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let init = build_utf8_wide_screen(TARGET_ROWS, 0);
        let row_a = build_utf8_wide_single_row(30, 0);
        let row_b = build_utf8_wide_single_row(30, 1);
        runtime.input_mut().push(&init);
        run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, 1.0);
        let mut t = 1.5;
        let mut toggle = false;
        group.sample_size(50);
        group.bench_function("utf8_wide_1_dirty_row", |b| {
            b.iter_batched(
                || {
                    toggle = !toggle;
                    if toggle { &row_a } else { &row_b }
                },
                |payload| {
                    runtime.input_mut().push(payload);
                    t += 1.0 / 60.0;
                    run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

#[cfg(feature = "vector-glyphs")]
fn bench_frame_render_tui(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_render_200x60_tui");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(4));
    group.throughput(Throughput::Elements(
        u64::from(TARGET_COLS) * u64::from(TARGET_ROWS),
    ));

    let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
    let dash_a = build_vector_glyphs_dashboard(TARGET_ROWS, 0);
    let dash_b = build_vector_glyphs_dashboard(TARGET_ROWS, 1);
    runtime.input_mut().push(&dash_a);
    run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, 1.0);

    let mut t = 1.5;
    let mut toggle = false;
    group.bench_function("vector_glyphs_dashboard", |b| {
        b.iter_batched(
            || {
                toggle = !toggle;
                if toggle { &dash_b } else { &dash_a }
            },
            |payload| {
                runtime.input_mut().push(payload);
                t += 1.0 / 60.0;
                run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

#[cfg(not(feature = "vector-glyphs"))]
fn bench_frame_render_tui(_c: &mut Criterion) {}

fn build_bold_and_italic_screen(cols: u16, rows: u16, seed: u8) -> Vec<u8> {
    let mut out = String::with_capacity(usize::from(cols) * usize::from(rows) * 16);
    out.push_str("\x1b[H");
    for r in 0..rows {
        for c in 0..cols {
            let sgr = match (r + c + u16::from(seed)) % 3 {
                0 => "\x1b[1m",
                1 => "\x1b[3m",
                _ => "\x1b[1;3m",
            };
            let ch = (b'A' + ((c as u8).wrapping_add(seed)) % 26) as char;
            out.push_str(sgr);
            out.push(ch);
        }
        out.push_str("\x1b[0m");
        if r + 1 < rows {
            out.push_str("\r\n");
        }
    }
    out.into_bytes()
}

#[cfg(feature = "search")]
fn build_search_2500_payload(rows: usize) -> Vec<u8> {
    let mut out = String::with_capacity(rows * 210);
    for r in 0..rows {
        for c in 0..42 {
            if write!(out, "HIT{c:02}").is_err() {
                // `String`'s `fmt::Write` impl is infallible.
            }
        }
        if r + 1 < rows {
            out.push_str("\r\n");
        }
    }
    out.into_bytes()
}

#[cfg(feature = "kitty-graphics")]
fn build_kitty_placeholder_grid_payload(cols: u16, rows: u16) -> Vec<u8> {
    let mut out = String::with_capacity(usize::from(cols) * usize::from(rows) * 6 + 256);
    // 2x2 RGBA white pixel: base64("/////////////////////w==")
    if write!(
        out,
        "\x1b_Ga=T,i=1,p=1,U=1,f=32,s=2,v=2,c={cols},r={rows},q=2;/////////////////////w==\x1b\\\x1b[H\x1b[38;2;0;0;1m"
    )
    .is_err()
    {
        // `String`'s `fmt::Write` impl is infallible.
    }
    for r in 0..rows {
        let row_diacritic = egui_xterm_core::features::inline_images::KITTY_DIACRITICS
            .get(usize::from(r))
            .copied()
            .unwrap_or('\u{0305}');
        out.push('\u{10EEEE}');
        out.push(row_diacritic);
        out.push('\u{0305}');
        for _ in 1..cols {
            out.push('\u{10EEEE}');
        }
        if r + 1 < rows {
            out.push_str("\r\n");
        }
    }
    out.push_str("\x1b[0m");
    out.into_bytes()
}

fn bench_frame_render_m5(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_render_200x60_m5");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(4));
    group.throughput(Throughput::Elements(
        u64::from(TARGET_COLS) * u64::from(TARGET_ROWS),
    ));

    // 1. SGR 1 (bold) + SGR 3 (italic) full 200x60 redraw.
    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let screen_a = build_bold_and_italic_screen(TARGET_COLS, TARGET_ROWS, 1);
        let screen_b = build_bold_and_italic_screen(TARGET_COLS, TARGET_ROWS, 2);
        runtime.input_mut().push(&screen_a);
        run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, 1.0);
        let mut t = 1.5;
        let mut toggle = false;
        group.bench_function("bold_and_italic_full_redraw", |b| {
            b.iter_batched(
                || {
                    toggle = !toggle;
                    if toggle { &screen_b } else { &screen_a }
                },
                |payload| {
                    runtime.input_mut().push(payload);
                    t += 1.0 / 60.0;
                    run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
                },
                BatchSize::SmallInput,
            );
        });
    }

    // 2. Full 200x60 redraw with >= 2,500 active search matches (`O(log M)` partition_point).
    #[cfg(feature = "search")]
    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let payload = build_search_2500_payload(100);
        runtime.input_mut().push(&payload);
        run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, 1.0);
        let query = egui_xterm::SearchQuery::literal("HIT");
        let match_count = runtime.terminal_mut().search(&query).map_or(0, |m| m.len());
        assert!(
            match_count >= 2_500,
            "expected >= 2,500 search matches, got {match_count}"
        );
        let mut t = 1.5;
        group.bench_function("search_2500_matches_redraw", |b| {
            b.iter(|| {
                runtime.config_mut().theme.scheme.background[0] ^= 1;
                t += 1.0 / 60.0;
                run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
            });
        });
    }

    // 3. Kitty `U+10EEEE` virtual placement placeholder 200x60 grid coalescing.
    #[cfg(feature = "kitty-graphics")]
    {
        let (ctx, mut runtime, screen_rect) = setup_200x60_harness();
        let payload = build_kitty_placeholder_grid_payload(TARGET_COLS, TARGET_ROWS);
        runtime.input_mut().push(&payload);
        run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, 1.0);
        let mut t = 1.5;
        group.bench_function("kitty_placeholder_grid", |b| {
            b.iter(|| {
                runtime.config_mut().theme.scheme.background[0] ^= 1;
                t += 1.0 / 60.0;
                run_single_frame_and_tessellate(&ctx, &mut runtime, screen_rect, t);
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_frame_render_stages,
    bench_frame_render,
    bench_frame_render_utf8,
    bench_frame_render_tui,
    bench_frame_render_m5,
);
criterion_main!(benches);
