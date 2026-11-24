//! High-performance 4-stage terminal renderer (`bg_mesh`, coalesced `text_mesh`,
//! cached `overlay_mesh` + `PathShape`, decoupled cursor `Shape::rect_filled`, and inline images).

use std::collections::HashMap;
#[cfg(any(
    feature = "search",
    feature = "hyperlinks",
    feature = "shell-integration"
))]
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use egui::Vec2;
use egui::emath::GuiRounding as _;
use egui::epaint::text::VariationCoords;
use egui::epaint::{Mesh, PathShape, TextOptions, TextShape, Vertex, WHITE_UV};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Color32, FontFamily, FontId, Galley, Pos2, Rect, Stroke, TextureId, Ui, pos2, vec2};
#[cfg(feature = "kitty-graphics")]
use egui_xterm_core::ImagePlacement;
#[cfg(feature = "search")]
use egui_xterm_core::SearchMatch;
#[cfg(feature = "shell-integration")]
use egui_xterm_core::ShellZonesView;
#[cfg(feature = "kitty-graphics")]
use egui_xterm_core::features::inline_images::{
    KITTY_PLACEHOLDER_CHAR, KittyPlaceholderCell, decode_placeholder_cell,
};
use egui_xterm_core::{
    CellFlags, ColorScheme, CursorShape, PackedColor, RenderSnapshot, Terminal, Theme,
    UnderlineStyle,
};

use crate::config::TerminalConfig;
use crate::layout::{FrameLayout, snap_px, snap_rect, snap_size_at_least_1px};

const MAX_CLUSTER_CACHE_ENTRIES: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GlobalRenderKey {
    cols: u16,
    rows: u16,
    ppp_bits: u32,
    font_size_bits: u32,
    raw_cell_w_bits: u32,
    cell_w_bits: u32,
    cell_h_bits: u32,
    is_alt_screen: bool,
    reverse_video: bool,
    background_opacity_bits: u32,
    theme_epoch: u64,
    palette_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BgLayoutKey {
    cols: u16,
    rows: usize,
    origin_x_bits: u32,
    origin_y_bits: u32,
    cell_w_bits: u32,
    cell_h_bits: u32,
    ppp_bits: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BgRun {
    start_col: u16,
    end_col: u16,
    color: Color32,
}

#[derive(Debug, Clone, Copy)]
struct RowDecoration {
    start_col: u16,
    end_col: u16,
    underline: UnderlineStyle,
    underline_color: Color32,
    strikethrough: bool,
    overline: bool,
    fg_color: Color32,
}

#[cfg(feature = "vector-glyphs")]
#[derive(Debug, Clone, Copy)]
struct VectorGlyphCell {
    col: u16,
    ch: char,
    color: Color32,
}

const BLINK_MASK: CellFlags = CellFlags::BLINK_SLOW.union(CellFlags::BLINK_RAPID);

#[inline]
fn is_cell_text_hidden(cell: egui_xterm_core::Cell, blink_hidden: bool) -> bool {
    cell.flags.contains(CellFlags::HIDDEN) || (blink_hidden && cell.flags.intersects(BLINK_MASK))
}

#[cfg(feature = "kitty-graphics")]
#[inline]
fn is_kitty_placeholder_char(ch: char) -> bool {
    ch == KITTY_PLACEHOLDER_CHAR
}

#[cfg(not(feature = "kitty-graphics"))]
#[inline]
fn is_kitty_placeholder_char(_ch: char) -> bool {
    false
}

#[cfg(feature = "vector-glyphs")]
#[inline]
fn is_vector_glyph_char(ch: char) -> bool {
    is_vector_glyph(ch)
}

#[cfg(not(feature = "vector-glyphs"))]
#[inline]
fn is_vector_glyph_char(_ch: char) -> bool {
    false
}

#[derive(Debug, Clone, Default)]
struct CachedRowSlot {
    // Stage 1 (Background)
    bg_valid: bool,
    bg_row_gen: u64,
    bg_sel_range: Option<(usize, usize)>,
    bg_search_sig: u64,
    bg_runs: Vec<BgRun>,
    bg_vert_range: std::ops::Range<usize>,

    // Stage 2 (Text)
    text_valid: bool,
    text_row_gen: u64,
    text_sel_range: Option<(usize, usize)>,
    text_cursor_col: Option<u16>,
    has_blink: bool,
    blink_hidden: bool,
    start_col: u16,
    text_vertices: Vec<Vertex>,
    text_indices: Vec<u32>,
    text_is_standard_quads: bool,
    text_mesh_vert_range: std::ops::Range<usize>,
    text_mesh_idx_range: std::ops::Range<usize>,

    // Stage 3 (Overlay: decorations + vector glyphs)
    overlay_valid: bool,
    overlay_row_gen: u64,
    overlay_sel_range: Option<(usize, usize)>,
    overlay_cursor_col: Option<u16>,
    overlay_has_blink: bool,
    overlay_blink_hidden: bool,
    decorations: Vec<RowDecoration>,
    #[cfg(feature = "vector-glyphs")]
    vector_cells: Vec<VectorGlyphCell>,
    overlay_vertices: Vec<Vertex>,
    overlay_indices: Vec<u32>,
    curly_paths: Vec<PathShape>,
    overlay_mesh_vert_range: std::ops::Range<usize>,
    overlay_mesh_idx_range: std::ops::Range<usize>,
}

impl CachedRowSlot {
    fn invalidate_all(&mut self) {
        self.bg_valid = false;
        self.text_valid = false;
        self.overlay_valid = false;
    }
}

#[derive(Debug, Clone, PartialEq)]
enum BoldRenderMode {
    FontFamily(FontFamily),
    VariableWght(f32),
    FauxBold(f32),
}

#[derive(Debug, Clone, Copy)]
struct ClusterQuad {
    pos: [Pos2; 4],
    uv: [Pos2; 4],
    is_color: bool,
}

#[derive(Debug, Clone)]
enum ClusterQuads {
    Empty,
    Single(ClusterQuad),
    Multi(Vec<ClusterQuad>),
}

#[derive(Debug, Clone)]
struct CachedCluster {
    glyph_count: usize,
    standalone_quads: Option<ClusterQuads>,
}

/// Active IME composition preedit state rendered inline at the cursor.
#[derive(Debug, Clone, Default)]
pub(crate) struct ImePreeditState {
    pub text: String,
    pub active_range_chars: Option<std::ops::Range<usize>>,
}

#[derive(Clone, Copy)]
struct AsciiGlyphEntry {
    has_quad: bool,
    rect_min: Pos2,
    rect_size: Vec2,
    uv_min: Pos2,
    uv_max: Pos2,
}

struct AsciiGlyphCache {
    entries: [Option<AsciiGlyphEntry>; 95],
    unsupported: [bool; 95],
    bold_entries: [Option<AsciiGlyphEntry>; 95],
    bold_unsupported: [bool; 95],
}

impl Default for AsciiGlyphCache {
    fn default() -> Self {
        Self {
            entries: [None; 95],
            unsupported: [false; 95],
            bold_entries: [None; 95],
            bold_unsupported: [false; 95],
        }
    }
}

impl AsciiGlyphCache {
    fn clear(&mut self) {
        self.entries = [None; 95];
        self.unsupported = [false; 95];
        self.bold_entries = [None; 95];
        self.bold_unsupported = [false; 95];
    }
}

#[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
const MAX_CACHED_IMAGE_TEXTURES: usize = 256;

#[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
struct CachedImageTexture {
    handle: egui::TextureHandle,
    width: u32,
    height: u32,
    revision: u64,
    last_used_frame: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayMeshKey {
    global: GlobalRenderKey,
    origin_x_bits: u32,
    origin_y_bits: u32,
    top_abs_line: u64,
    row_count: usize,
    shell_zones_sig: u64,
    hovered_links_sig: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextMeshKey {
    global: GlobalRenderKey,
    origin_x_bits: u32,
    origin_y_bits: u32,
    top_abs_line: u64,
    row_count: usize,
    tex_size: [usize; 2],
}

struct ContextualCell {
    text_byte_start: usize,
    text_byte_end: usize,
    x_rel: f32,
    fg_c32: Color32,
    is_bold: bool,
    is_italic: bool,
    glyph_count: usize,
}

#[derive(Default)]
struct ContextualRunScratch {
    active: bool,
    start_x: f32,
    text: String,
    cells: Vec<ContextualCell>,
    clusters: Vec<(f32, usize)>,
}

impl ContextualRunScratch {
    fn clear(&mut self) {
        self.active = false;
        self.start_x = 0.0;
        self.text.clear();
        self.cells.clear();
        self.clusters.clear();
    }
}

/// Persistent per-runtime 4-stage cached renderer.
pub(crate) struct TerminalRenderer {
    bg_mesh: Arc<Mesh>,
    last_bg_layout_key: Option<BgLayoutKey>,
    bg_initialized: bool,
    text_mesh: Arc<Mesh>,
    last_text_key: Option<TextMeshKey>,
    overlay_mesh: Arc<Mesh>,
    last_overlay_key: Option<OverlayMeshKey>,
    cached_curly_paths: Vec<PathShape>,
    last_theme: Option<Theme>,
    theme_epoch: u64,
    scratch_cluster: String,
    scratch_bg_runs: Vec<BgRun>,
    scratch_text_vertices: Vec<Vertex>,
    scratch_overlay_vertices: Vec<Vertex>,
    scratch_ctx_run: ContextualRunScratch,
    scratch_shapes: Vec<egui::Shape>,
    col_x_cache: Vec<f32>,
    row_y_cache: Vec<f32>,
    slots: Vec<CachedRowSlot>,
    last_global_key: Option<GlobalRenderKey>,
    last_top_abs_line: u64,
    last_atlas_fill_ratio: f32,
    last_atlas_height: usize,
    atlas_exhausted_repaint_sent: bool,
    last_text_options: Option<TextOptions>,
    atlas_sentinel_galley: Option<Arc<Galley>>,
    bold_mode: Option<BoldRenderMode>,
    cluster_cache: HashMap<String, [Option<CachedCluster>; 4]>,
    ascii_glyph_cache: AsciiGlyphCache,
    // Track DECSCUSR vs user config overrides for cursor shape & blink.
    last_term_cursor_shape: Option<CursorShape>,
    last_cfg_cursor_shape: CursorShape,
    effective_cursor_shape: CursorShape,
    last_term_cursor_blink: Option<bool>,
    last_cfg_cursor_blink: bool,
    effective_cursor_blink: bool,
    last_input_time_secs: f64,
    bell_until_secs: Option<f64>,
    #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
    image_textures: HashMap<u32, CachedImageTexture>,
    #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
    image_frame_seq: u64,
    #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
    scratch_images_behind: Vec<(i32, Mesh)>,
    #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
    scratch_images_above: Vec<(i32, Mesh)>,
}

impl Default for TerminalRenderer {
    fn default() -> Self {
        Self {
            bg_mesh: Arc::new(Mesh::default()),
            last_bg_layout_key: None,
            bg_initialized: false,
            text_mesh: Arc::new(Mesh::default()),
            last_text_key: None,
            overlay_mesh: Arc::new(Mesh::default()),
            last_overlay_key: None,
            cached_curly_paths: Vec::new(),
            last_theme: None,
            theme_epoch: 0,
            scratch_cluster: String::with_capacity(16),
            scratch_bg_runs: Vec::new(),
            scratch_text_vertices: Vec::new(),
            scratch_overlay_vertices: Vec::new(),
            scratch_ctx_run: ContextualRunScratch::default(),
            scratch_shapes: Vec::new(),
            col_x_cache: Vec::new(),
            row_y_cache: Vec::new(),
            slots: Vec::new(),
            last_global_key: None,
            last_top_abs_line: 0,
            last_atlas_fill_ratio: 0.0,
            last_atlas_height: 0,
            atlas_exhausted_repaint_sent: false,
            last_text_options: None,
            atlas_sentinel_galley: None,
            bold_mode: None,
            cluster_cache: HashMap::new(),
            ascii_glyph_cache: AsciiGlyphCache::default(),
            last_term_cursor_shape: None,
            last_cfg_cursor_shape: CursorShape::Block,
            effective_cursor_shape: CursorShape::Block,
            last_term_cursor_blink: None,
            last_cfg_cursor_blink: true,
            effective_cursor_blink: true,
            last_input_time_secs: f64::NEG_INFINITY,
            bell_until_secs: None,
            #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
            image_textures: HashMap::new(),
            #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
            image_frame_seq: 0,
            #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
            scratch_images_behind: Vec::new(),
            #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
            scratch_images_above: Vec::new(),
        }
    }
}

impl TerminalRenderer {
    /// Records user interaction time so cursor blink stays momentarily solid while typing.
    pub(crate) fn note_input_activity(&mut self, now_secs: f64) {
        self.last_input_time_secs = now_secs;
    }

    /// Triggers a visual bell flash decaying over `duration_ms`.
    pub(crate) fn trigger_visual_bell(&mut self, now_secs: f64, duration_ms: u32) {
        if duration_ms == 0 || !now_secs.is_finite() {
            self.bell_until_secs = None;
            return;
        }
        let duration_secs = f64::from(duration_ms) / 1000.0;
        self.bell_until_secs = Some(now_secs + duration_secs);
    }

    pub(crate) fn effective_cursor_shape(
        &mut self,
        term_shape: CursorShape,
        cfg_shape: CursorShape,
    ) -> CursorShape {
        match self.last_term_cursor_shape {
            None => {
                self.last_term_cursor_shape = Some(term_shape);
                self.last_cfg_cursor_shape = cfg_shape;
                self.effective_cursor_shape = cfg_shape;
            }
            Some(prev_term) => {
                if term_shape != prev_term {
                    self.last_term_cursor_shape = Some(term_shape);
                    self.effective_cursor_shape = term_shape;
                } else if cfg_shape != self.last_cfg_cursor_shape {
                    self.last_cfg_cursor_shape = cfg_shape;
                    self.effective_cursor_shape = cfg_shape;
                }
            }
        }
        self.effective_cursor_shape
    }

    pub(crate) fn effective_cursor_blink(&mut self, term_blink: bool, cfg_blink: bool) -> bool {
        match self.last_term_cursor_blink {
            None => {
                self.last_term_cursor_blink = Some(term_blink);
                self.last_cfg_cursor_blink = cfg_blink;
                self.effective_cursor_blink = cfg_blink;
            }
            Some(prev_term) => {
                if term_blink != prev_term {
                    self.last_term_cursor_blink = Some(term_blink);
                    self.effective_cursor_blink = term_blink;
                } else if cfg_blink != self.last_cfg_cursor_blink {
                    self.last_cfg_cursor_blink = cfg_blink;
                    self.effective_cursor_blink = cfg_blink;
                }
            }
        }
        self.effective_cursor_blink
    }

    /// Executes the 4-stage render pipeline with explicit visual bell color and opacity, returning
    /// the cursor anchor rect (in local layer coordinates) for IME positioning.
    #[expect(clippy::too_many_arguments, reason = "per-frame render context")]
    pub(crate) fn render_with_bell_style(
        &mut self,
        ui: &mut Ui,
        layout: &FrameLayout,
        config: &TerminalConfig,
        terminal: &mut Terminal,
        now_secs: f64,
        has_focus: bool,
        ime_preedit: &ImePreeditState,
        #[cfg(feature = "hyperlinks")] hovered_link_spans: &[egui_xterm_core::HyperlinkSpan],
        bell_color: Option<Color32>,
        bell_opacity: f32,
    ) -> Rect {
        let origin = layout.term_rect.min;
        let cell_w = layout.cell_size.x;
        let cell_h = layout.cell_size.y;
        let ppp = layout.pixels_per_point;

        if self.last_theme.as_ref() != Some(&config.theme) {
            self.last_theme = Some(config.theme.clone());
            self.theme_epoch = self.theme_epoch.wrapping_add(1);
        }

        // Read snapshot, search matches, and shell zones once (all shared `&Terminal` borrows).
        #[cfg(feature = "search")]
        let search_matches = terminal.search_matches();
        #[cfg(feature = "search")]
        let active_search = terminal.active_search_match().copied();
        #[cfg(feature = "shell-integration")]
        let shell_zones = terminal.shell_zones();
        #[cfg(feature = "kitty-graphics")]
        let all_image_placements = terminal.image_placements();

        let (
            dirty_rows,
            default_fg,
            default_bg,
            cursor_c32,
            cursor_pos,
            cursor_visible,
            cursor_shape,
            cursor_blink_on,
            block_cursor_active,
            ime_cursor_rect_base,
        ) = {
            let snapshot = terminal.active_snapshot();
            let cursor_pos = (snapshot.cursor.col, snapshot.cursor.row);
            let cursor_visible = snapshot.cursor.visible;
            let term_cursor_shape = snapshot.cursor.style.shape;
            let term_cursor_blink = snapshot.cursor.style.blink;

            let cursor_shape = self.effective_cursor_shape(term_cursor_shape, config.cursor_shape);
            let cursor_blink = self.effective_cursor_blink(term_cursor_blink, config.cursor_blink);

            // Compute deterministic blink phase from egui frame time.
            let cursor_blink_on = if !has_focus {
                false
            } else if !cursor_blink {
                true
            } else {
                let raw_period = config.cursor_config.blink_period_secs;
                let period = if raw_period.is_finite() {
                    raw_period.clamp(0.2, 60.0)
                } else {
                    1.0
                };
                let half = period * 0.5;
                let safe_now = if now_secs.is_finite() { now_secs } else { 0.0 };
                let recent_input = self.last_input_time_secs.is_finite()
                    && (safe_now - self.last_input_time_secs) < half;
                if recent_input {
                    true
                } else {
                    let phase = (safe_now / half).floor();
                    let is_on = (phase as i64).rem_euclid(2) == 0;
                    let next_toggle = ((phase + 1.0) * half - safe_now).clamp(0.01, half);
                    if next_toggle.is_finite() {
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_secs_f64(next_toggle));
                    }
                    is_on
                }
            };

            let (text_blink_on, next_text_blink_toggle) = if config.enable_blink {
                let safe_now = if now_secs.is_finite() { now_secs } else { 0.0 };
                let half = 0.5_f64;
                let phase = (safe_now / half).floor();
                let is_on = (phase as i64).rem_euclid(2) == 0;
                let next_toggle = ((phase + 1.0) * half - safe_now).clamp(0.01, half);
                (is_on, next_toggle)
            } else {
                (true, 0.0)
            };
            let blink_hidden = config.enable_blink && !text_blink_on;

            let block_cursor_active = cursor_visible
                && has_focus
                && cursor_blink_on
                && cursor_shape == CursorShape::Block;

            let (default_fg, default_bg) = resolve_default_pair(&config.theme.scheme, &snapshot);
            let default_bg = egui::Color32::from_rgba_unmultiplied(
                default_bg.r(),
                default_bg.g(),
                default_bg.b(),
                ((default_bg.a() as f32 * config.background_opacity.clamp(0.0, 1.0)).round()) as u8,
            );
            let cursor_c32 = resolve_cursor_color(&config.theme.scheme, &snapshot);

            let global_key = GlobalRenderKey {
                cols: snapshot.cols,
                rows: snapshot.rows,
                ppp_bits: ppp.to_bits(),
                font_size_bits: layout.effective_font_size.to_bits(),
                raw_cell_w_bits: layout.raw_cell_w.to_bits(),
                cell_w_bits: cell_w.to_bits(),
                cell_h_bits: cell_h.to_bits(),
                is_alt_screen: snapshot.is_alt_screen,
                reverse_video: snapshot.modes.reverse_video,
                background_opacity_bits: config.background_opacity.to_bits(),
                theme_epoch: self.theme_epoch,
                palette_epoch: snapshot.palette_epoch,
            };

            // Pre-Stage 1: Unified slot resizing, global invalidation, and scroll rotation.
            let row_count = snapshot.visible_rows.len();
            let mut slots_size_changed = false;
            if self.slots.len() != row_count {
                self.slots.resize_with(row_count, CachedRowSlot::default);
                for slot in &mut self.slots {
                    slot.invalidate_all();
                }
                slots_size_changed = true;
            }

            let global_changed = self.last_global_key.as_ref() != Some(&global_key);
            let metrics_changed = self.last_global_key.is_some_and(|prev| {
                prev.ppp_bits != global_key.ppp_bits
                    || prev.font_size_bits != global_key.font_size_bits
                    || prev.raw_cell_w_bits != global_key.raw_cell_w_bits
                    || prev.cell_w_bits != global_key.cell_w_bits
                    || prev.cell_h_bits != global_key.cell_h_bits
            });
            let scrolled = snapshot.top_abs_line.0 != self.last_top_abs_line;

            if global_changed {
                if metrics_changed {
                    self.cluster_cache.clear();
                    self.ascii_glyph_cache.clear();
                    self.bold_mode = None;
                }
                for slot in &mut self.slots {
                    slot.invalidate_all();
                }
                self.last_global_key = Some(global_key);
                self.last_top_abs_line = snapshot.top_abs_line.0;
            } else if scrolled {
                let delta = snapshot.top_abs_line.0 as i64 - self.last_top_abs_line as i64;
                let len = self.slots.len();
                if !slots_size_changed && len > 0 && delta > 0 && (delta as usize) < len {
                    let d = delta as usize;
                    self.slots.rotate_left(d);
                    for slot in &mut self.slots[len - d..] {
                        slot.invalidate_all();
                    }
                } else if !slots_size_changed && len > 0 && delta < 0 && ((-delta) as usize) < len {
                    let d = (-delta) as usize;
                    self.slots.rotate_right(d);
                    for slot in &mut self.slots[..d] {
                        slot.invalidate_all();
                    }
                } else {
                    for slot in &mut self.slots {
                        slot.invalidate_all();
                    }
                }
                self.last_top_abs_line = snapshot.top_abs_line.0;
            }

            // Stage 1: Per-row BgRun cache + incremental/1-row-patched bg_mesh (cursor decoupled).
            let cols_usize = usize::from(snapshot.cols);
            let bg_layout_key = BgLayoutKey {
                cols: snapshot.cols,
                rows: row_count,
                origin_x_bits: origin.x.to_bits(),
                origin_y_bits: origin.y.to_bits(),
                cell_w_bits: cell_w.to_bits(),
                cell_h_bits: cell_h.to_bits(),
                ppp_bits: ppp.to_bits(),
            };
            let bg_layout_changed = self.last_bg_layout_key != Some(bg_layout_key);
            if bg_layout_changed {
                self.last_bg_layout_key = Some(bg_layout_key);
                self.col_x_cache.clear();
                self.col_x_cache
                    .extend((0..=cols_usize).map(|c| snap_px(origin.x + c as f32 * cell_w, ppp)));
                self.row_y_cache.clear();
                self.row_y_cache
                    .extend((0..=row_count).map(|r| snap_px(origin.y + r as f32 * cell_h, ppp)));
            }

            #[cfg(any(feature = "search", feature = "shell-integration"))]
            let retained_top = snapshot
                .scrollback_len
                .saturating_sub(snapshot.display_offset);
            #[cfg(feature = "search")]
            let visible_matches = if search_matches.is_empty() {
                &[][..]
            } else {
                let retained_end = retained_top.saturating_add(row_count);
                let lo = search_matches.partition_point(|m| m.end_line < retained_top);
                let hi = lo + search_matches[lo..].partition_point(|m| m.line < retained_end);
                &search_matches[lo..hi]
            };
            #[cfg(feature = "search")]
            let mut match_cursor = 0usize;

            let mut bg_changed_rows = 0usize;
            let mut last_changed_bg_row = 0usize;
            let mut bg_length_changed = false;

            for (vp_row, row) in snapshot.visible_rows.iter().enumerate() {
                let row_gen = row.generation();
                let bg_sel = snapshot.selection.and_then(|sel| {
                    let line = egui_xterm_core::AbsLine(snapshot.top_abs_line.0 + vp_row as u64);
                    sel.row_col_range(line, row)
                });

                #[cfg(feature = "search")]
                let (retained_line, row_matches, row_search_sig) = if visible_matches.is_empty() {
                    (retained_top + vp_row, &[][..], 0u64)
                } else {
                    let r_line = retained_top + vp_row;
                    while match_cursor < visible_matches.len()
                        && visible_matches[match_cursor].end_line < r_line
                    {
                        match_cursor += 1;
                    }
                    let mut row_end = match_cursor;
                    while row_end < visible_matches.len() && visible_matches[row_end].line <= r_line
                    {
                        row_end += 1;
                    }
                    let r_matches = &visible_matches[match_cursor..row_end];
                    let sig = if r_matches.is_empty() {
                        0
                    } else {
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        r_matches.hash(&mut h);
                        let active_in_row = active_search.filter(|act| r_matches.contains(act));
                        active_in_row.hash(&mut h);
                        h.finish().max(1)
                    };
                    (r_line, r_matches, sig)
                };
                #[cfg(not(feature = "search"))]
                let row_search_sig: u64 = 0;

                let slot = &mut self.slots[vp_row];
                if slot.bg_valid
                    && slot.bg_row_gen == row_gen
                    && slot.bg_sel_range == bg_sel
                    && slot.bg_search_sig == row_search_sig
                {
                    continue;
                }

                std::mem::swap(&mut self.scratch_bg_runs, &mut slot.bg_runs);
                slot.bg_runs.clear();

                let row_cols = cols_usize.min(row.len());
                let cells = row.cells();
                let mut run_start_col: usize = 0;
                let mut run_color: Option<Color32> = None;
                #[cfg(feature = "search")]
                let mut row_m_idx: usize = 0;

                for (col, &cell) in cells.iter().enumerate().take(row_cols) {
                    let inverse = cell.flags.contains(CellFlags::INVERSE);
                    let mut bg_c32 = if inverse {
                        resolve_fg_color(cell.fg, default_fg, &config.theme, &snapshot, false)
                    } else {
                        resolve_bg_color(cell.bg, default_bg, &config.theme, &snapshot)
                    };

                    if bg_sel.is_some_and(|(c0, c1)| col >= c0 && col <= c1) {
                        bg_c32 = rgb_to_color32(config.theme.scheme.selection_bg);
                    }

                    #[cfg(feature = "search")]
                    if !row_matches.is_empty()
                        && let Some(hl_color) = search_highlight_row_cursor(
                            row_matches,
                            &mut row_m_idx,
                            active_search,
                            retained_line,
                            col as u16,
                        )
                    {
                        bg_c32 = hl_color;
                    }

                    let cell_color = if bg_c32 == default_bg {
                        None
                    } else {
                        Some(bg_c32)
                    };

                    if cell_color != run_color {
                        if let Some(color) = run_color {
                            slot.bg_runs.push(BgRun {
                                start_col: run_start_col as u16,
                                end_col: col as u16,
                                color,
                            });
                        }
                        run_start_col = col;
                        run_color = cell_color;
                    }
                }
                if let Some(color) = run_color {
                    slot.bg_runs.push(BgRun {
                        start_col: run_start_col as u16,
                        end_col: row_cols as u16,
                        color,
                    });
                }

                slot.bg_valid = true;
                slot.bg_row_gen = row_gen;
                slot.bg_sel_range = bg_sel;
                slot.bg_search_sig = row_search_sig;

                if slot.bg_runs != self.scratch_bg_runs {
                    bg_changed_rows += 1;
                    last_changed_bg_row = vp_row;
                    if slot.bg_runs.len() != self.scratch_bg_runs.len() {
                        bg_length_changed = true;
                    }
                }
            }

            let need_full_bg_rebuild = !self.bg_initialized
                || bg_layout_changed
                || global_changed
                || scrolled
                || slots_size_changed
                || bg_length_changed
                || bg_changed_rows > 1;

            if need_full_bg_rebuild {
                self.bg_initialized = true;
                let bg_mesh = Arc::make_mut(&mut self.bg_mesh);
                bg_mesh.texture_id = TextureId::default();
                bg_mesh.vertices.clear();
                bg_mesh.indices.clear();
                let snapped_term_rect = snap_rect(layout.term_rect, ppp);
                if snapped_term_rect.is_positive() {
                    bg_mesh.add_colored_rect(snapped_term_rect, default_bg);
                }
                for (vp_row, slot) in self.slots.iter_mut().enumerate() {
                    let row_y0 = self.row_y_cache[vp_row];
                    let row_y1 = self.row_y_cache[vp_row + 1];
                    let v_start = bg_mesh.vertices.len();
                    if row_y1 > row_y0 {
                        for run in &slot.bg_runs {
                            let x0 = self.col_x_cache[usize::from(run.start_col)];
                            let x1 = self.col_x_cache[usize::from(run.end_col)];
                            if x1 > x0 {
                                bg_mesh.add_colored_rect(
                                    Rect::from_min_max(pos2(x0, row_y0), pos2(x1, row_y1)),
                                    run.color,
                                );
                            }
                        }
                    }
                    slot.bg_vert_range = v_start..bg_mesh.vertices.len();
                }
            } else if bg_changed_rows == 1 {
                let slot = &self.slots[last_changed_bg_row];
                let row_y0 = self.row_y_cache[last_changed_bg_row];
                let row_y1 = self.row_y_cache[last_changed_bg_row + 1];
                let expected_verts = if row_y1 > row_y0 {
                    slot.bg_runs
                        .iter()
                        .filter(|r| {
                            self.col_x_cache[usize::from(r.end_col)]
                                > self.col_x_cache[usize::from(r.start_col)]
                        })
                        .count()
                        * 4
                } else {
                    0
                };
                if slot.bg_vert_range.len() == expected_verts
                    && slot.bg_vert_range.end <= self.bg_mesh.vertices.len()
                {
                    let bg_mesh = Arc::make_mut(&mut self.bg_mesh);
                    let slice = &mut bg_mesh.vertices[slot.bg_vert_range.clone()];
                    let mut v_off = 0usize;
                    if row_y1 > row_y0 {
                        for run in &slot.bg_runs {
                            let x0 = self.col_x_cache[usize::from(run.start_col)];
                            let x1 = self.col_x_cache[usize::from(run.end_col)];
                            if x1 > x0 && v_off + 4 <= slice.len() {
                                write_colored_rect_vertices(
                                    &mut slice[v_off..v_off + 4],
                                    pos2(x0, row_y0),
                                    pos2(x1, row_y1),
                                    run.color,
                                );
                                v_off += 4;
                            }
                        }
                    }
                } else {
                    let bg_mesh = Arc::make_mut(&mut self.bg_mesh);
                    bg_mesh.texture_id = TextureId::default();
                    bg_mesh.vertices.clear();
                    bg_mesh.indices.clear();
                    let snapped_term_rect = snap_rect(layout.term_rect, ppp);
                    if snapped_term_rect.is_positive() {
                        bg_mesh.add_colored_rect(snapped_term_rect, default_bg);
                    }
                    for (vp_row, slot) in self.slots.iter_mut().enumerate() {
                        let row_y0 = self.row_y_cache[vp_row];
                        let row_y1 = self.row_y_cache[vp_row + 1];
                        let v_start = bg_mesh.vertices.len();
                        if row_y1 > row_y0 {
                            for run in &slot.bg_runs {
                                let x0 = self.col_x_cache[usize::from(run.start_col)];
                                let x1 = self.col_x_cache[usize::from(run.end_col)];
                                if x1 > x0 {
                                    bg_mesh.add_colored_rect(
                                        Rect::from_min_max(pos2(x0, row_y0), pos2(x1, row_y1)),
                                        run.color,
                                    );
                                }
                            }
                        }
                        slot.bg_vert_range = v_start..bg_mesh.vertices.len();
                    }
                }
            }

            // Stage 2: Coalesced text_mesh + per-row texel-UV cache + AsciiGlyphCache + cluster_cache.
            let mut dirty_count: u64 = 0;
            let cursor_text_c32 = rgb_to_color32(config.theme.scheme.cursor_text);
            let sel_fg_override = config.theme.scheme.selection_fg.map(rgb_to_color32);
            let font_size = layout.effective_font_size;
            let base_font_id = FontId::new(font_size, FontFamily::Monospace);

            let (
                needs_repaint,
                post_tex_size,
                text_changed_rows,
                last_changed_text_row,
                text_length_changed,
                overlay_dirty_rows,
                mid_loop_reset,
            ) = ui.ctx().fonts_mut(|fonts| {
                let curr_fill = fonts.font_atlas_fill_ratio();
                let curr_tex_size = fonts.font_image_size();
                let curr_height = curr_tex_size[1];
                let curr_opts = *fonts.options();
                let sentinel = fonts.layout_job(LayoutJob::default());
                let sentinel_reset = !self
                    .atlas_sentinel_galley
                    .as_ref()
                    .is_some_and(|prev| Arc::ptr_eq(prev, &sentinel));
                let atlas_reset = sentinel_reset
                    || curr_fill < self.last_atlas_fill_ratio
                    || curr_height < self.last_atlas_height
                    || self.last_text_options.as_ref() != Some(&curr_opts);

                if atlas_reset {
                    self.cluster_cache.clear();
                    self.ascii_glyph_cache.clear();
                    self.last_text_options = Some(curr_opts);
                    self.atlas_sentinel_galley = Some(sentinel);
                    for slot in &mut self.slots {
                        slot.text_valid = false;
                    }
                }

                if self.bold_mode.is_none() || atlas_reset || global_changed {
                    let detected = detect_bold_render_mode(fonts, cell_w, ppp);
                    if self.bold_mode.as_ref() != Some(&detected) {
                        self.bold_mode = Some(detected);
                        self.ascii_glyph_cache.clear();
                        self.cluster_cache.clear();
                        for slot in &mut self.slots {
                            slot.text_valid = false;
                        }
                    }
                }
                let bold_mode = self
                    .bold_mode
                    .clone()
                    .unwrap_or_else(|| detect_bold_render_mode(fonts, cell_w, ppp));

                let mut total_text_changed = 0usize;
                let mut last_text_row = 0usize;
                let mut any_text_len_changed = false;
                let mut total_overlay_dirty = 0usize;
                let mut any_mid_loop_reset = false;
                let mut final_fill = curr_fill;
                let mut final_tex_size = curr_tex_size;

                for attempt in 0..2 {
                    let pass_start_fill = fonts.font_atlas_fill_ratio();
                    let pass_start_height = fonts.font_image_size()[1];
                    total_text_changed = 0;
                    last_text_row = 0;
                    any_text_len_changed = false;
                    total_overlay_dirty = 0;

                    for (vp_row, row) in snapshot.visible_rows.iter().enumerate() {
                        let row_gen = row.generation();
                        let row_sel = if sel_fg_override.is_some() {
                            snapshot.selection.and_then(|sel| {
                                let line = egui_xterm_core::AbsLine(
                                    snapshot.top_abs_line.0 + vp_row as u64,
                                );
                                sel.row_col_range(line, row)
                            })
                        } else {
                            None
                        };
                        let row_cursor_col =
                            if block_cursor_active && usize::from(cursor_pos.1) == vp_row {
                                Some(cursor_pos.0)
                            } else {
                                None
                            };

                        let slot = &mut self.slots[vp_row];
                        let text_hit = slot.text_valid
                            && slot.text_row_gen == row_gen
                            && slot.text_sel_range == row_sel
                            && slot.text_cursor_col == row_cursor_col
                            && (!slot.has_blink || slot.blink_hidden == blink_hidden);

                        let overlay_cursor_match = row_cursor_col.filter(|&cc| {
                            let dec_hit = slot
                                .decorations
                                .iter()
                                .any(|d| cc >= d.start_col && cc < d.end_col);
                            #[cfg(feature = "vector-glyphs")]
                            let vec_hit = slot.vector_cells.iter().any(|v| v.col == cc);
                            #[cfg(not(feature = "vector-glyphs"))]
                            let vec_hit = false;
                            dec_hit || vec_hit
                        });
                        let overlay_hit = slot.overlay_valid
                            && slot.overlay_row_gen == row_gen
                            && slot.overlay_sel_range == row_sel
                            && slot.overlay_cursor_col == overlay_cursor_match
                            && (!slot.overlay_has_blink
                                || slot.overlay_blink_hidden == blink_hidden);

                        if text_hit && overlay_hit {
                            continue;
                        }

                        let cells = row.cells();
                        let row_cols = usize::from(snapshot.cols).min(cells.len());
                        let scan_end = row.trimmed_len().min(row_cols);

                        // Compute exclusive end column of visible text.
                        let mut row_text_end = 0usize;
                        for col in (0..scan_end).rev() {
                            let c = cells[col];
                            if c.is_wide_spacer() || c.is_wrap_spacer() || c.is_sized_spacer() {
                                continue;
                            }
                            if is_kitty_placeholder_char(c.codepoint) {
                                continue;
                            }
                            let has_comb = c.extra_id != 0
                                && snapshot
                                    .cell_extra(c.extra_id)
                                    .is_some_and(|e| !e.combining.is_empty());
                            if !has_comb && is_vector_glyph_char(c.codepoint) {
                                continue;
                            }
                            if c.codepoint != ' ' || has_comb {
                                row_text_end = col + 1;
                                break;
                            }
                        }

                        let mut first_text_col: Option<u16> = None;
                        let mut ascii_fast_eligible = !text_hit && row_text_end > 0;
                        let mut row_has_blink_text = false;
                        let mut row_has_blink_overlay = false;
                        let mut active_deco: Option<RowDecoration> = None;

                        if !overlay_hit {
                            slot.decorations.clear();
                            #[cfg(feature = "vector-glyphs")]
                            slot.vector_cells.clear();
                        }

                        for (col, &cell) in cells.iter().enumerate().take(row_cols) {
                            if cell.is_wide_spacer()
                                || cell.is_wrap_spacer()
                                || cell.is_sized_spacer()
                            {
                                if cell.is_wide_spacer()
                                    && col < row_text_end
                                    && first_text_col.is_some()
                                {
                                    ascii_fast_eligible = false;
                                }
                                continue;
                            }
                            let col_u16 = col as u16;
                            let is_blinking = cell.flags.intersects(BLINK_MASK)
                                && !cell.flags.contains(CellFlags::HIDDEN);
                            if is_blinking {
                                row_has_blink_text = true;
                            }
                            let is_hidden = is_cell_text_hidden(cell, blink_hidden);

                            if is_kitty_placeholder_char(cell.codepoint) {
                                if !overlay_hit && let Some(prev) = active_deco.take() {
                                    slot.decorations.push(prev);
                                }
                                continue;
                            }

                            let extra = if cell.extra_id != 0 {
                                snapshot.cell_extra(cell.extra_id)
                            } else {
                                None
                            };
                            let has_combining = extra.is_some_and(|e| !e.combining.is_empty());
                            let has_text_sizing = extra.is_some_and(|e| e.text_sizing.is_some());

                            let is_vec = !has_combining
                                && !has_text_sizing
                                && is_vector_glyph_char(cell.codepoint);

                            if !overlay_hit {
                                let underline_style = cell.underline_style();
                                let strikethrough = cell.flags.contains(CellFlags::STRIKETHROUGH);
                                let overline = cell.flags.contains(CellFlags::OVERLINE);
                                let has_deco_attrs = underline_style != UnderlineStyle::None
                                    || strikethrough
                                    || overline;
                                if is_blinking && (has_deco_attrs || is_vec) {
                                    row_has_blink_overlay = true;
                                }
                                let has_deco = !is_hidden && has_deco_attrs;
                                let emit_vec = !is_hidden && is_vec;

                                if has_deco || emit_vec {
                                    let fg_c32 = resolve_cell_fg(
                                        cell,
                                        col,
                                        row_cursor_col,
                                        cursor_text_c32,
                                        sel_fg_override,
                                        row_sel,
                                        default_fg,
                                        default_bg,
                                        &config.theme,
                                        &snapshot,
                                        blink_hidden,
                                    );
                                    if has_deco {
                                        let cell_span_cols: u16 =
                                            if cell.is_wide_lead() { 2 } else { 1 };
                                        let ul_color = if let Some(ul_c) =
                                            extra.and_then(|e| e.underline_color)
                                        {
                                            resolve_fg_color(
                                                ul_c,
                                                default_fg,
                                                &config.theme,
                                                &snapshot,
                                                false,
                                            )
                                        } else {
                                            fg_c32
                                        };
                                        let end_col = col_u16
                                            .saturating_add(cell_span_cols)
                                            .min(snapshot.cols);
                                        match &mut active_deco {
                                            Some(prev)
                                                if prev.end_col == col_u16
                                                    && prev.underline == underline_style
                                                    && prev.underline_color == ul_color
                                                    && prev.strikethrough == strikethrough
                                                    && prev.overline == overline
                                                    && prev.fg_color == fg_c32 =>
                                            {
                                                prev.end_col = end_col;
                                            }
                                            _ => {
                                                if let Some(prev) = active_deco.take() {
                                                    slot.decorations.push(prev);
                                                }
                                                active_deco = Some(RowDecoration {
                                                    start_col: col_u16,
                                                    end_col,
                                                    underline: underline_style,
                                                    underline_color: ul_color,
                                                    strikethrough,
                                                    overline,
                                                    fg_color: fg_c32,
                                                });
                                            }
                                        }
                                    } else if let Some(prev) = active_deco.take() {
                                        slot.decorations.push(prev);
                                    }

                                    #[cfg(feature = "vector-glyphs")]
                                    if emit_vec {
                                        slot.vector_cells.push(VectorGlyphCell {
                                            col: col_u16,
                                            ch: cell.codepoint,
                                            color: fg_c32,
                                        });
                                        continue;
                                    }
                                } else if let Some(prev) = active_deco.take() {
                                    slot.decorations.push(prev);
                                }
                            }

                            if is_vec {
                                continue;
                            }

                            if text_hit || col >= row_text_end {
                                continue;
                            }
                            if first_text_col.is_none() {
                                if cell.codepoint == ' ' && !has_combining && !has_text_sizing {
                                    continue;
                                }
                                first_text_col = Some(col_u16);
                            }
                            if ascii_fast_eligible
                                && (has_combining
                                    || has_text_sizing
                                    || cell.is_wide_lead()
                                    || !(' '..='~').contains(&cell.codepoint))
                            {
                                ascii_fast_eligible = false;
                            }
                        }

                        if !overlay_hit {
                            if let Some(prev) = active_deco.take() {
                                slot.decorations.push(prev);
                            }
                            slot.overlay_valid = true;
                            slot.overlay_row_gen = row_gen;
                            slot.overlay_sel_range = row_sel;
                            slot.overlay_cursor_col = row_cursor_col.filter(|&cc| {
                                let dec_hit = slot
                                    .decorations
                                    .iter()
                                    .any(|d| cc >= d.start_col && cc < d.end_col);
                                #[cfg(feature = "vector-glyphs")]
                                let vec_hit = slot.vector_cells.iter().any(|v| v.col == cc);
                                #[cfg(not(feature = "vector-glyphs"))]
                                let vec_hit = false;
                                dec_hit || vec_hit
                            });
                            slot.overlay_has_blink = row_has_blink_overlay;
                            slot.overlay_blink_hidden = blink_hidden;
                            total_overlay_dirty += 1;

                            // Tessellate this dirty row's overlay into row-local vertices.
                            std::mem::swap(
                                &mut self.scratch_overlay_vertices,
                                &mut slot.overlay_vertices,
                            );
                            slot.overlay_vertices.clear();
                            slot.overlay_indices.clear();
                            slot.curly_paths.clear();
                            tessellate_slot_overlay(slot, cell_w, cell_h, ppp);
                        }

                        if text_hit {
                            continue;
                        }

                        dirty_count += 1;
                        slot.text_valid = true;
                        slot.text_row_gen = row_gen;
                        slot.text_sel_range = row_sel;
                        slot.text_cursor_col = row_cursor_col;
                        slot.has_blink = row_has_blink_text;
                        slot.blink_hidden = blink_hidden;

                        std::mem::swap(&mut self.scratch_text_vertices, &mut slot.text_vertices);
                        let prev_idx_len = slot.text_indices.len();
                        let prev_was_quads = slot.text_is_standard_quads;
                        let prev_start_col = slot.start_col;
                        slot.text_vertices.clear();
                        slot.text_indices.clear();
                        slot.text_is_standard_quads = true;

                        let Some(start_col) = first_text_col else {
                            slot.start_col = 0;
                            if slot.text_vertices != self.scratch_text_vertices
                                || prev_idx_len != 0
                                || prev_start_col != 0
                            {
                                total_text_changed += 1;
                                last_text_row = vp_row;
                                if slot.text_vertices.len() != self.scratch_text_vertices.len()
                                    || prev_idx_len != 0
                                {
                                    any_text_len_changed = true;
                                }
                            }
                            continue;
                        };

                        slot.start_col = start_col;
                        let anchor_idx = usize::from(start_col);

                        let fast_ok = ascii_fast_eligible
                            && build_ascii_row_mesh(
                                &mut self.ascii_glyph_cache,
                                fonts,
                                slot,
                                &cells[anchor_idx..row_text_end],
                                start_col,
                                row_cursor_col,
                                cursor_text_c32,
                                sel_fg_override,
                                row_sel,
                                default_fg,
                                default_bg,
                                &config.theme,
                                &snapshot,
                                &base_font_id,
                                &bold_mode,
                                cell_w,
                                cell_h,
                                ppp,
                                blink_hidden,
                            );

                        if !fast_ok {
                            build_slow_row_mesh(
                                &mut self.ascii_glyph_cache,
                                &mut self.cluster_cache,
                                &mut self.scratch_cluster,
                                &mut self.scratch_ctx_run,
                                fonts,
                                slot,
                                cells,
                                row_cols,
                                anchor_idx,
                                row_text_end,
                                start_col,
                                row_cursor_col,
                                cursor_text_c32,
                                sel_fg_override,
                                row_sel,
                                default_fg,
                                default_bg,
                                &config.theme,
                                &snapshot,
                                &base_font_id,
                                &bold_mode,
                                font_size,
                                cell_w,
                                cell_h,
                                ppp,
                                blink_hidden,
                            );
                        }

                        if slot.text_vertices != self.scratch_text_vertices
                            || slot.text_indices.len() != prev_idx_len
                            || slot.text_is_standard_quads != prev_was_quads
                            || slot.start_col != prev_start_col
                        {
                            total_text_changed += 1;
                            last_text_row = vp_row;
                            if slot.text_vertices.len() != self.scratch_text_vertices.len()
                                || slot.text_indices.len() != prev_idx_len
                                || !prev_was_quads
                                || !slot.text_is_standard_quads
                            {
                                any_text_len_changed = true;
                            }
                        }
                    }

                    final_fill = fonts.font_atlas_fill_ratio();
                    final_tex_size = fonts.font_image_size();
                    let reset_this_pass =
                        final_fill < pass_start_fill || final_tex_size[1] < pass_start_height;
                    if reset_this_pass {
                        any_mid_loop_reset = true;
                        self.cluster_cache.clear();
                        self.ascii_glyph_cache.clear();
                        self.atlas_sentinel_galley = None;
                        for slot in &mut self.slots {
                            slot.text_valid = false;
                        }
                        if attempt == 0 {
                            continue;
                        }
                    }
                    break;
                }

                let newly_exhausted = final_fill >= 1.0 && !self.atlas_exhausted_repaint_sent;
                let mut needs_repaint = false;
                if any_mid_loop_reset || newly_exhausted {
                    needs_repaint = true;
                    if final_fill >= 1.0 {
                        self.atlas_exhausted_repaint_sent = true;
                    }
                }
                if final_fill < 1.0 {
                    self.atlas_exhausted_repaint_sent = false;
                }
                self.last_atlas_fill_ratio = final_fill;
                self.last_atlas_height = final_tex_size[1];

                (
                    needs_repaint,
                    final_tex_size,
                    total_text_changed,
                    last_text_row,
                    any_text_len_changed,
                    total_overlay_dirty,
                    any_mid_loop_reset,
                )
            });

            if needs_repaint {
                ui.ctx().request_repaint();
            }
            if config.enable_blink
                && self
                    .slots
                    .iter()
                    .take(usize::from(snapshot.rows))
                    .any(|s| s.text_valid && s.has_blink)
                && next_text_blink_toggle.is_finite()
            {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs_f64(
                        next_text_blink_toggle,
                    ));
            }

            // Assemble / normalize / patch coalesced `text_mesh: Arc<Mesh>`.
            let text_key = TextMeshKey {
                global: global_key,
                origin_x_bits: origin.x.to_bits(),
                origin_y_bits: origin.y.to_bits(),
                top_abs_line: snapshot.top_abs_line.0,
                row_count,
                tex_size: post_tex_size,
            };
            let uv_norm = if post_tex_size[0] > 0 && post_tex_size[1] > 0 {
                vec2(1.0 / post_tex_size[0] as f32, 1.0 / post_tex_size[1] as f32)
            } else {
                Vec2::ZERO
            };

            let force_text_rebuild = self.last_text_key != Some(text_key)
                || mid_loop_reset
                || text_length_changed
                || text_changed_rows > 1;

            if force_text_rebuild {
                self.last_text_key = Some(text_key);
                let text_mesh = Arc::make_mut(&mut self.text_mesh);
                text_mesh.texture_id = TextureId::default();
                text_mesh.vertices.clear();
                text_mesh.indices.clear();
                for (vp_row, slot) in self.slots.iter_mut().enumerate() {
                    let v_start = text_mesh.vertices.len();
                    let i_start = text_mesh.indices.len();
                    if !slot.text_vertices.is_empty() {
                        let row_origin = vec2(
                            snap_px(origin.x + f32::from(slot.start_col) * cell_w, ppp),
                            snap_px(origin.y + vp_row as f32 * cell_h, ppp),
                        );
                        let base_v = v_start as u32;
                        text_mesh
                            .vertices
                            .extend(slot.text_vertices.iter().map(|v| Vertex {
                                pos: v.pos + row_origin,
                                uv: pos2(v.uv.x * uv_norm.x, v.uv.y * uv_norm.y),
                                color: v.color,
                            }));
                        text_mesh
                            .indices
                            .extend(slot.text_indices.iter().map(|&idx| base_v + idx));
                    }
                    slot.text_mesh_vert_range = v_start..text_mesh.vertices.len();
                    slot.text_mesh_idx_range = i_start..text_mesh.indices.len();
                }
            } else if text_changed_rows == 1 {
                let slot = &self.slots[last_changed_text_row];
                if slot.text_vertices.len() == slot.text_mesh_vert_range.len()
                    && slot.text_indices.len() == slot.text_mesh_idx_range.len()
                    && slot.text_mesh_vert_range.end <= self.text_mesh.vertices.len()
                    && slot.text_mesh_idx_range.end <= self.text_mesh.indices.len()
                {
                    let text_mesh = Arc::make_mut(&mut self.text_mesh);
                    let row_origin = vec2(
                        snap_px(origin.x + f32::from(slot.start_col) * cell_w, ppp),
                        snap_px(origin.y + last_changed_text_row as f32 * cell_h, ppp),
                    );
                    for (dst, src) in text_mesh.vertices[slot.text_mesh_vert_range.clone()]
                        .iter_mut()
                        .zip(&slot.text_vertices)
                    {
                        dst.pos = src.pos + row_origin;
                        dst.uv = pos2(src.uv.x * uv_norm.x, src.uv.y * uv_norm.y);
                        dst.color = src.color;
                    }
                } else {
                    let text_mesh = Arc::make_mut(&mut self.text_mesh);
                    text_mesh.texture_id = TextureId::default();
                    text_mesh.vertices.clear();
                    text_mesh.indices.clear();
                    for (vp_row, slot) in self.slots.iter_mut().enumerate() {
                        let v_start = text_mesh.vertices.len();
                        let i_start = text_mesh.indices.len();
                        if !slot.text_vertices.is_empty() {
                            let row_origin = vec2(
                                snap_px(origin.x + f32::from(slot.start_col) * cell_w, ppp),
                                snap_px(origin.y + vp_row as f32 * cell_h, ppp),
                            );
                            let base_v = v_start as u32;
                            text_mesh
                                .vertices
                                .extend(slot.text_vertices.iter().map(|v| Vertex {
                                    pos: v.pos + row_origin,
                                    uv: pos2(v.uv.x * uv_norm.x, v.uv.y * uv_norm.y),
                                    color: v.color,
                                }));
                            text_mesh
                                .indices
                                .extend(slot.text_indices.iter().map(|&idx| base_v + idx));
                        }
                        slot.text_mesh_vert_range = v_start..text_mesh.vertices.len();
                        slot.text_mesh_idx_range = i_start..text_mesh.indices.len();
                    }
                }
            }

            // Stage 3: Cached `overlay_mesh: Arc<Mesh>` + cached `PathShape` for SGR 4:3 curly underlines.
            #[cfg(feature = "shell-integration")]
            let shell_zones_sig = compute_shell_zones_sig(
                &snapshot,
                shell_zones,
                retained_top,
                retained_top.saturating_add(row_count),
            );
            #[cfg(not(feature = "shell-integration"))]
            let shell_zones_sig: u64 = 0;

            #[cfg(feature = "hyperlinks")]
            let hovered_links_sig = {
                if hovered_link_spans.is_empty() {
                    0u64
                } else {
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    hovered_link_spans.hash(&mut h);
                    h.finish().max(1)
                }
            };
            #[cfg(not(feature = "hyperlinks"))]
            let hovered_links_sig: u64 = 0;

            let overlay_key = OverlayMeshKey {
                global: global_key,
                origin_x_bits: origin.x.to_bits(),
                origin_y_bits: origin.y.to_bits(),
                top_abs_line: snapshot.top_abs_line.0,
                row_count,
                shell_zones_sig,
                hovered_links_sig,
            };

            let overlay_key_changed = self.last_overlay_key != Some(overlay_key);
            if overlay_key_changed || overlay_dirty_rows > 0 {
                self.last_overlay_key = Some(overlay_key);
                let snapped_origin_x = snap_px(origin.x, ppp);
                let overlay_mesh = Arc::make_mut(&mut self.overlay_mesh);
                overlay_mesh.texture_id = TextureId::default();
                overlay_mesh.vertices.clear();
                overlay_mesh.indices.clear();
                self.cached_curly_paths.clear();

                for (vp_row, slot) in self.slots.iter_mut().enumerate() {
                    let v_start = overlay_mesh.vertices.len();
                    let i_start = overlay_mesh.indices.len();
                    let row_offset = vec2(
                        snapped_origin_x,
                        snap_px(origin.y + vp_row as f32 * cell_h, ppp),
                    );

                    if !slot.overlay_vertices.is_empty() {
                        let base_v = v_start as u32;
                        overlay_mesh
                            .vertices
                            .extend(slot.overlay_vertices.iter().map(|v| Vertex {
                                pos: v.pos + row_offset,
                                uv: v.uv,
                                color: v.color,
                            }));
                        overlay_mesh
                            .indices
                            .extend(slot.overlay_indices.iter().map(|&idx| base_v + idx));
                    }

                    for path in &slot.curly_paths {
                        let mut translated = path.clone();
                        for p in &mut translated.points {
                            *p += row_offset;
                        }
                        self.cached_curly_paths.push(translated);
                    }

                    slot.overlay_mesh_vert_range = v_start..overlay_mesh.vertices.len();
                    slot.overlay_mesh_idx_range = i_start..overlay_mesh.indices.len();
                }

                #[cfg(feature = "shell-integration")]
                crate::widgets::prompt_gutter::paint_prompt_gutter(
                    overlay_mesh,
                    &snapshot,
                    shell_zones,
                    origin,
                    layout.cell_size,
                    ppp,
                );

                #[cfg(feature = "hyperlinks")]
                crate::widgets::hyperlink::paint_hyperlink_underlines(
                    overlay_mesh,
                    hovered_link_spans,
                    origin,
                    layout.cell_size,
                    ppp,
                    default_fg,
                );
            }

            #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
            self.prepare_inline_images(
                ui.ctx(),
                &snapshot,
                #[cfg(feature = "kitty-graphics")]
                all_image_placements,
                layout,
                origin,
                cell_w,
                cell_h,
                ppp,
                #[cfg(feature = "kitty-graphics")]
                blink_hidden,
            );

            let base_cursor_rect = cursor_cell_rect(
                origin,
                cursor_pos.0.min(layout.cols.saturating_sub(1)),
                cursor_pos.1.min(layout.rows.saturating_sub(1)),
                cell_w,
                cell_h,
                ppp,
            );
            (
                dirty_count,
                default_fg,
                default_bg,
                cursor_c32,
                cursor_pos,
                cursor_visible,
                cursor_shape,
                cursor_blink_on,
                block_cursor_active,
                base_cursor_rect,
            )
        };

        #[cfg(feature = "metrics")]
        terminal.record_frame_rendered(dirty_rows);
        #[cfg(not(feature = "metrics"))]
        let _unused_dirty_rows = dirty_rows;

        // Stage 4: Submit batched shapes to clipped painter.
        let painter = ui.painter_at(layout.clip_rect);

        self.scratch_shapes.clear();
        if !self.bg_mesh.vertices.is_empty() {
            self.scratch_shapes
                .push(egui::Shape::Mesh(Arc::clone(&self.bg_mesh)));
        }

        #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
        for (_, mesh) in self.scratch_images_behind.drain(..) {
            self.scratch_shapes.push(egui::Shape::Mesh(Arc::new(mesh)));
        }

        // Decoupled solid block cursor drawn behind text so cursor cell glyph stays legible.
        if block_cursor_active && cursor_pos.0 < layout.cols && cursor_pos.1 < layout.rows {
            let c_rect = cursor_cell_rect(origin, cursor_pos.0, cursor_pos.1, cell_w, cell_h, ppp);
            if c_rect.is_positive() {
                self.scratch_shapes
                    .push(egui::Shape::rect_filled(c_rect, 0.0, cursor_c32));
            }
        }

        if !self.text_mesh.vertices.is_empty() {
            self.scratch_shapes
                .push(egui::Shape::Mesh(Arc::clone(&self.text_mesh)));
        }

        if !self.overlay_mesh.vertices.is_empty() {
            self.scratch_shapes
                .push(egui::Shape::Mesh(Arc::clone(&self.overlay_mesh)));
        }
        if !self.cached_curly_paths.is_empty() {
            for path in &self.cached_curly_paths {
                self.scratch_shapes.push(egui::Shape::Path(path.clone()));
            }
        }

        // Decoupled Underline / Bar / unfocused hollow cursor drawn above text/overlay.
        if cursor_visible && cursor_pos.0 < layout.cols && cursor_pos.1 < layout.rows {
            let c_rect = cursor_cell_rect(origin, cursor_pos.0, cursor_pos.1, cell_w, cell_h, ppp);
            if !has_focus {
                if config.cursor_config.hollow_when_unfocused && c_rect.is_positive() {
                    push_hollow_rect_shapes(&mut self.scratch_shapes, c_rect, ppp, cursor_c32);
                }
            } else if cursor_blink_on {
                match cursor_shape {
                    CursorShape::Block => {} // Emitted above before text_mesh.
                    CursorShape::Underline => {
                        let h =
                            snap_size_at_least_1px(config.cursor_config.underline_height_px, ppp);
                        let ul_rect = snap_rect(
                            Rect::from_min_max(
                                pos2(c_rect.min.x, (c_rect.max.y - h).max(c_rect.min.y)),
                                c_rect.max,
                            ),
                            ppp,
                        );
                        if ul_rect.is_positive() {
                            self.scratch_shapes
                                .push(egui::Shape::rect_filled(ul_rect, 0.0, cursor_c32));
                        }
                    }
                    CursorShape::Bar => {
                        let w = snap_size_at_least_1px(config.cursor_config.bar_width_px, ppp);
                        let bar_rect = snap_rect(
                            Rect::from_min_max(
                                c_rect.min,
                                pos2((c_rect.min.x + w).min(c_rect.max.x), c_rect.max.y),
                            ),
                            ppp,
                        );
                        if bar_rect.is_positive() {
                            self.scratch_shapes
                                .push(egui::Shape::rect_filled(bar_rect, 0.0, cursor_c32));
                        }
                    }
                }
            }
        }

        #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
        for (_, mesh) in self.scratch_images_above.drain(..) {
            self.scratch_shapes.push(egui::Shape::Mesh(Arc::new(mesh)));
        }

        // Render active IME preedit inline at cursor and compute IME cursor_rect.
        let ime_cursor_rect = if !ime_preedit.text.is_empty() {
            let (preedit_galley, preedit_w) = ui.ctx().fonts_mut(|f| {
                let font_id = FontId::new(layout.effective_font_size, FontFamily::Monospace);
                let g = f.layout_no_wrap(ime_preedit.text.clone(), font_id, default_fg);
                let w = g.size().x.max(cell_w);
                (g, w)
            });
            let max_x = layout.term_rect.max.x;
            let px = ime_cursor_rect_base
                .min
                .x
                .min((max_x - preedit_w).max(origin.x));
            let py = ime_cursor_rect_base.min.y;
            let preedit_rect = snap_rect(
                Rect::from_min_max(
                    pos2(px, py),
                    pos2(
                        (px + preedit_w).min(max_x),
                        (py + cell_h).min(layout.term_rect.max.y),
                    ),
                ),
                ppp,
            );
            let char_count = ime_preedit.text.chars().count();
            let active_span_x = ime_preedit.active_range_chars.as_ref().and_then(|range| {
                let start = range.start.min(char_count);
                let end = range.end.min(char_count);
                if start >= end {
                    return None;
                }
                let x0 = preedit_rect.min.x
                    + preedit_galley
                        .pos_from_cursor(egui::text::CCursor::new(start))
                        .min
                        .x;
                let x1 = preedit_rect.min.x
                    + preedit_galley
                        .pos_from_cursor(egui::text::CCursor::new(end))
                        .min
                        .x;
                let clamped_x0 = x0.clamp(preedit_rect.min.x, preedit_rect.max.x);
                let clamped_x1 = x1.clamp(clamped_x0, preedit_rect.max.x);
                (clamped_x1 > clamped_x0).then_some((clamped_x0, clamped_x1))
            });
            if preedit_rect.is_positive() {
                self.scratch_shapes
                    .push(egui::Shape::rect_filled(preedit_rect, 0.0, default_bg));
                let ul_h = snap_size_at_least_1px(1.0, ppp);
                let ul_rect = snap_rect(
                    Rect::from_min_max(
                        pos2(
                            preedit_rect.min.x,
                            (preedit_rect.max.y - ul_h).max(preedit_rect.min.y),
                        ),
                        preedit_rect.max,
                    ),
                    ppp,
                );
                let base_ul_color = if active_span_x.is_some() {
                    Color32::from_rgba_unmultiplied(
                        default_fg.r(),
                        default_fg.g(),
                        default_fg.b(),
                        140,
                    )
                } else {
                    default_fg
                };
                self.scratch_shapes
                    .push(egui::Shape::rect_filled(ul_rect, 0.0, base_ul_color));

                if let Some((ax0, ax1)) = active_span_x {
                    let active_bg = Color32::from_rgba_unmultiplied(
                        default_fg.r(),
                        default_fg.g(),
                        default_fg.b(),
                        36,
                    );
                    let active_bg_rect = snap_rect(
                        Rect::from_min_max(
                            pos2(ax0, preedit_rect.min.y),
                            pos2(ax1, preedit_rect.max.y),
                        ),
                        ppp,
                    );
                    if active_bg_rect.is_positive() {
                        self.scratch_shapes.push(egui::Shape::rect_filled(
                            active_bg_rect,
                            0.0,
                            active_bg,
                        ));
                    }
                    let active_ul_h = snap_size_at_least_1px(2.5, ppp);
                    let active_ul_rect = snap_rect(
                        Rect::from_min_max(
                            pos2(
                                ax0,
                                (preedit_rect.max.y - active_ul_h).max(preedit_rect.min.y),
                            ),
                            pos2(ax1, preedit_rect.max.y),
                        ),
                        ppp,
                    );
                    if active_ul_rect.is_positive() {
                        self.scratch_shapes.push(egui::Shape::rect_filled(
                            active_ul_rect,
                            0.0,
                            default_fg,
                        ));
                    }
                }
            }
            self.scratch_shapes.push(egui::Shape::Text(TextShape::new(
                preedit_rect.min,
                preedit_galley,
                default_fg,
            )));
            let anchor_x = active_span_x.map_or(preedit_rect.max.x, |(_, ax1)| ax1);
            snap_rect(
                Rect::from_min_max(
                    pos2(
                        (anchor_x - 1.0 / ppp).max(preedit_rect.min.x),
                        preedit_rect.min.y,
                    ),
                    pos2(anchor_x, preedit_rect.max.y),
                ),
                ppp,
            )
        } else {
            ime_cursor_rect_base
        };

        if let Some(until) = self.bell_until_secs {
            let duration_secs = f64::from(config.visual_bell_duration_ms) / 1000.0;
            if config.visual_bell_duration_ms == 0
                || !now_secs.is_finite()
                || now_secs >= until
                || duration_secs <= 0.0
            {
                self.bell_until_secs = None;
            } else {
                let remaining = ((until - now_secs) / duration_secs).clamp(0.0, 1.0) as f32;
                let base_color = bell_color.unwrap_or(default_fg);
                let safe_opacity = if bell_opacity.is_finite() {
                    bell_opacity.clamp(0.0, 1.0)
                } else {
                    0.18
                };
                let [base_r, base_g, base_b, base_a] = base_color.to_srgba_unmultiplied();
                let max_alpha = (f32::from(base_a) * safe_opacity).round();
                if max_alpha >= 1.0 {
                    let max_u8 = max_alpha.clamp(1.0, 255.0);
                    let alpha = (remaining * max_u8).round().clamp(1.0, max_u8) as u8;
                    let flash_color =
                        Color32::from_rgba_unmultiplied(base_r, base_g, base_b, alpha);
                    let bell_rect = snap_rect(layout.term_rect, ppp);
                    if bell_rect.is_positive() {
                        self.scratch_shapes.push(egui::Shape::rect_filled(
                            bell_rect,
                            0.0,
                            flash_color,
                        ));
                    }
                }
                let next_step = (until - now_secs).clamp(0.001, 0.016);
                if next_step.is_finite() {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_secs_f64(next_step));
                }
            }
        }

        painter.extend(self.scratch_shapes.drain(..));
        ime_cursor_rect
    }

    #[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
    #[expect(
        clippy::too_many_arguments,
        reason = "inline image layout & texture cache"
    )]
    fn prepare_inline_images(
        &mut self,
        ctx: &egui::Context,
        snapshot: &RenderSnapshot<'_>,
        #[cfg(feature = "kitty-graphics")] all_placements: &[ImagePlacement],
        layout: &FrameLayout,
        origin: Pos2,
        cell_w: f32,
        cell_h: f32,
        ppp: f32,
        #[cfg(feature = "kitty-graphics")] blink_hidden: bool,
    ) {
        self.scratch_images_behind.clear();
        self.scratch_images_above.clear();
        self.image_frame_seq = self.image_frame_seq.wrapping_add(1);
        let frame_seq = self.image_frame_seq;

        let placements = snapshot.image_placements();
        for placement in placements {
            let Some(stored) = snapshot.image_data(placement.image_id) else {
                continue;
            };
            if stored.width == 0 || stored.height == 0 {
                continue;
            }

            let crop_x = placement.src_x.min(stored.width);
            let crop_y = placement.src_y.min(stored.height);
            let crop_w = if placement.src_w == 0 {
                stored.width.saturating_sub(crop_x)
            } else {
                placement.src_w.min(stored.width.saturating_sub(crop_x))
            };
            let crop_h = if placement.src_h == 0 {
                stored.height.saturating_sub(crop_y)
            } else {
                placement.src_h.min(stored.height.saturating_sub(crop_y))
            };
            if crop_w == 0 || crop_h == 0 {
                continue;
            }

            let rel_row = placement.abs_line.0 as i64 - snapshot.top_abs_line.0 as i64;
            let x = origin.x + f32::from(placement.col) * cell_w + f32::from(placement.sub_x) / ppp;
            let y = origin.y + (rel_row as f32) * cell_h + f32::from(placement.sub_y) / ppp;
            let disp_w = if placement.cols > 0 {
                f32::from(placement.cols) * cell_w
            } else {
                (crop_w as f32) / ppp
            };
            let disp_h = if placement.rows > 0 {
                f32::from(placement.rows) * cell_h
            } else {
                (crop_h as f32) / ppp
            };

            let img_w = stored.width as f32;
            let img_h = stored.height as f32;
            let u_min = crop_x as f32 / img_w;
            let v_min = crop_y as f32 / img_h;
            let u_max = (crop_x + crop_w) as f32 / img_w;
            let v_max = (crop_y + crop_h) as f32 / img_h;

            emit_image_quad(
                &mut self.image_textures,
                &mut self.scratch_images_behind,
                &mut self.scratch_images_above,
                ctx,
                stored,
                frame_seq,
                layout.term_rect,
                pos2(x, y),
                egui::vec2(disp_w, disp_h),
                Rect::from_min_max(pos2(u_min, v_min), pos2(u_max, v_max)),
                placement.z_index,
                ppp,
            );
        }

        #[cfg(feature = "kitty-graphics")]
        self.prepare_kitty_placeholders(
            ctx,
            snapshot,
            all_placements,
            layout,
            origin,
            cell_w,
            cell_h,
            ppp,
            blink_hidden,
            frame_seq,
        );

        self.scratch_images_behind.sort_by_key(|(z, _)| *z);
        self.scratch_images_above.sort_by_key(|(z, _)| *z);

        // Evict textures no longer present in the terminal's ImageStore.
        self.image_textures
            .retain(|id, _| snapshot.image_data(*id).is_some());

        // Enforce bounded texture cache cap by evicting least-recently-used entries.
        while self.image_textures.len() > MAX_CACHED_IMAGE_TEXTURES {
            let Some(oldest_id) = self
                .image_textures
                .iter()
                .min_by_key(|(_, entry)| entry.last_used_frame)
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.image_textures.remove(&oldest_id);
        }
    }

    #[cfg(feature = "kitty-graphics")]
    #[expect(
        clippy::too_many_arguments,
        reason = "Kitty U+10EEEE virtual placement run coalescer"
    )]
    fn prepare_kitty_placeholders(
        &mut self,
        ctx: &egui::Context,
        snapshot: &RenderSnapshot<'_>,
        all_placements: &[ImagePlacement],
        layout: &FrameLayout,
        origin: Pos2,
        cell_w: f32,
        cell_h: f32,
        ppp: f32,
        blink_hidden: bool,
        frame_seq: u64,
    ) {
        if !snapshot.is_synchronized_output && !all_placements.iter().any(|p| p.is_virtual) {
            return;
        }

        #[derive(Clone, Copy)]
        struct PlaceholderRun {
            image_id: u32,
            placement_id: u32,
            start_grid_col: u16,
            run_len: u16,
            tile_row: u16,
            tile_col_start: u16,
        }

        let cols_usize = usize::from(snapshot.cols);
        let cell_px_w = (cell_w * ppp).round().max(1.0) as u32;
        let cell_px_h = (cell_h * ppp).round().max(1.0) as u32;

        let mut emit_run = |run: PlaceholderRun, vp_row: usize| {
            let Some(vp) = lookup_virtual_placement(snapshot, run.image_id, run.placement_id)
            else {
                return;
            };
            let Some(stored) = snapshot.image_data(run.image_id) else {
                return;
            };
            if stored.width == 0 || stored.height == 0 {
                return;
            }
            let crop_x = vp.src_x.min(stored.width);
            let crop_y = vp.src_y.min(stored.height);
            let crop_w = if vp.src_w == 0 {
                stored.width.saturating_sub(crop_x)
            } else {
                vp.src_w.min(stored.width.saturating_sub(crop_x))
            };
            let crop_h = if vp.src_h == 0 {
                stored.height.saturating_sub(crop_y)
            } else {
                vp.src_h.min(stored.height.saturating_sub(crop_y))
            };
            if crop_w == 0 || crop_h == 0 {
                return;
            }
            let virt_cols = if vp.cols > 0 {
                vp.cols
            } else {
                crop_w.div_ceil(cell_px_w).clamp(1, u32::from(u16::MAX)) as u16
            };
            let virt_rows = if vp.rows > 0 {
                vp.rows
            } else {
                crop_h.div_ceil(cell_px_h).clamp(1, u32::from(u16::MAX)) as u16
            };
            if run.tile_row >= virt_rows || run.tile_col_start >= virt_cols {
                return;
            }
            let eff_len = run.run_len.min(virt_cols - run.tile_col_start);
            if eff_len == 0 {
                return;
            }

            let img_w = stored.width as f32;
            let img_h = stored.height as f32;
            let u_base = crop_x as f32 / img_w;
            let v_base = crop_y as f32 / img_h;
            let u_span = crop_w as f32 / img_w;
            let v_span = crop_h as f32 / img_h;
            let u_min = u_base + u_span * (f32::from(run.tile_col_start) / f32::from(virt_cols));
            let u_max =
                u_base + u_span * (f32::from(run.tile_col_start + eff_len) / f32::from(virt_cols));
            let v_min = v_base + v_span * (f32::from(run.tile_row) / f32::from(virt_rows));
            let v_max = v_base + v_span * (f32::from(run.tile_row + 1) / f32::from(virt_rows));

            let x = origin.x + f32::from(run.start_grid_col) * cell_w + f32::from(vp.sub_x) / ppp;
            let y = origin.y + (vp_row as f32) * cell_h + f32::from(vp.sub_y) / ppp;
            let disp_w = f32::from(eff_len) * cell_w;

            emit_image_quad(
                &mut self.image_textures,
                &mut self.scratch_images_behind,
                &mut self.scratch_images_above,
                ctx,
                stored,
                frame_seq,
                layout.term_rect,
                pos2(x, y),
                vec2(disp_w, cell_h),
                Rect::from_min_max(pos2(u_min, v_min), pos2(u_max, v_max)),
                vp.z_index,
                ppp,
            );
        };

        for (vp_row, row) in snapshot.visible_rows.iter().enumerate() {
            let cells = row.cells();
            let scan_len = row.trimmed_len().min(cols_usize).min(cells.len());
            let mut prev_ph: Option<KittyPlaceholderCell> = None;
            let mut active_run: Option<PlaceholderRun> = None;

            for (col, &cell) in cells.iter().enumerate().take(scan_len) {
                if cell.codepoint != KITTY_PLACEHOLDER_CHAR
                    || is_cell_text_hidden(cell, blink_hidden)
                {
                    if let Some(run) = active_run.take() {
                        emit_run(run, vp_row);
                    }
                    prev_ph = None;
                    continue;
                }

                let extra = if cell.extra_id != 0 {
                    snapshot.cell_extra(cell.extra_id)
                } else {
                    None
                };
                let Some(raw_ph) = decode_placeholder_cell(&cell, extra) else {
                    if let Some(run) = active_run.take() {
                        emit_run(run, vp_row);
                    }
                    prev_ph = None;
                    continue;
                };

                let mut resolved = raw_ph.resolve_with_left(prev_ph);
                let tile_row = resolved.row.unwrap_or(0);
                let tile_col = resolved.col.unwrap_or(0);
                resolved.row = Some(tile_row);
                resolved.col = Some(tile_col);
                prev_ph = Some(resolved);

                let col_u16 = col as u16;
                match &mut active_run {
                    Some(run)
                        if run.image_id == resolved.image_id
                            && run.placement_id == resolved.placement_id
                            && run.tile_row == tile_row
                            && run.start_grid_col + run.run_len == col_u16
                            && run.tile_col_start + run.run_len == tile_col =>
                    {
                        run.run_len += 1;
                    }
                    _ => {
                        if let Some(run) = active_run.take() {
                            emit_run(run, vp_row);
                        }
                        active_run = Some(PlaceholderRun {
                            image_id: resolved.image_id,
                            placement_id: resolved.placement_id,
                            start_grid_col: col_u16,
                            run_len: 1,
                            tile_row,
                            tile_col_start: tile_col,
                        });
                    }
                }
            }

            if let Some(run) = active_run.take() {
                emit_run(run, vp_row);
            }
        }
    }
}

/// Looks up a virtual (`U=1`) inline image placement directly via
/// [`RenderSnapshot::virtual_placement`].
#[cfg(feature = "kitty-graphics")]
fn lookup_virtual_placement(
    snapshot: &RenderSnapshot<'_>,
    image_id: u32,
    placement_id: u32,
) -> Option<ImagePlacement> {
    snapshot.virtual_placement(image_id, placement_id)
}

#[cfg(any(feature = "kitty-graphics", feature = "sixel"))]
#[expect(clippy::too_many_arguments, reason = "shared image mesh quad emitter")]
fn emit_image_quad(
    image_textures: &mut HashMap<u32, CachedImageTexture>,
    behind: &mut Vec<(i32, Mesh)>,
    above: &mut Vec<(i32, Mesh)>,
    ctx: &egui::Context,
    stored: &egui_xterm_core::StoredImage,
    frame_seq: u64,
    term_rect: Rect,
    dest_pos: Pos2,
    dest_size: Vec2,
    src_uv: Rect,
    z_index: i32,
    ppp: f32,
) {
    let dest_rect = snap_rect(Rect::from_min_size(dest_pos, dest_size), ppp);
    let clipped_rect = dest_rect.intersect(term_rect);
    if !clipped_rect.is_positive() || !dest_rect.is_positive() {
        return;
    }

    let dw = dest_rect.width().max(f32::EPSILON);
    let dh = dest_rect.height().max(f32::EPSILON);
    let u0 = egui::lerp(
        src_uv.min.x..=src_uv.max.x,
        (clipped_rect.min.x - dest_rect.min.x) / dw,
    );
    let u1 = egui::lerp(
        src_uv.min.x..=src_uv.max.x,
        (clipped_rect.max.x - dest_rect.min.x) / dw,
    );
    let v0 = egui::lerp(
        src_uv.min.y..=src_uv.max.y,
        (clipped_rect.min.y - dest_rect.min.y) / dh,
    );
    let v1 = egui::lerp(
        src_uv.min.y..=src_uv.max.y,
        (clipped_rect.max.y - dest_rect.min.y) / dh,
    );
    let uv_rect = Rect::from_min_max(pos2(u0, v0), pos2(u1, v1));

    let expected_bytes = (stored.width as usize)
        .saturating_mul(stored.height as usize)
        .saturating_mul(4);
    let rgba = stored.active_rgba();
    if rgba.len() != expected_bytes {
        return;
    }

    let max_side = ctx.input(|i| i.max_texture_side);
    let build_color_image = || -> Option<egui::ColorImage> {
        let w = stored.width as usize;
        let h = stored.height as usize;
        if w == 0 || h == 0 || max_side == 0 {
            return None;
        }
        if w <= max_side && h <= max_side {
            return Some(egui::ColorImage::from_rgba_unmultiplied([w, h], rgba));
        }
        let scale_num = max_side;
        let scale_den = w.max(h);
        let dst_w = ((w as u128 * scale_num as u128) / scale_den as u128).clamp(1, max_side as u128)
            as usize;
        let dst_h = ((h as u128 * scale_num as u128) / scale_den as u128).clamp(1, max_side as u128)
            as usize;
        let mut scaled = vec![0u8; dst_w.saturating_mul(dst_h).saturating_mul(4)];
        for dy in 0..dst_h {
            let sy = (dy.saturating_mul(h) / dst_h).min(h - 1);
            for dx in 0..dst_w {
                let sx = (dx.saturating_mul(w) / dst_w).min(w - 1);
                let src_idx = (sy * w + sx) * 4;
                let dst_idx = (dy * dst_w + dx) * 4;
                if let (Some(src_px), Some(dst_px)) = (
                    rgba.get(src_idx..src_idx + 4),
                    scaled.get_mut(dst_idx..dst_idx + 4),
                ) {
                    dst_px.copy_from_slice(src_px);
                }
            }
        }
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [dst_w, dst_h],
            &scaled,
        ))
    };

    match image_textures.get_mut(&stored.id) {
        Some(entry) => {
            entry.last_used_frame = frame_seq;
            if entry.revision != stored.revision
                || entry.width != stored.width
                || entry.height != stored.height
            {
                let Some(color_image) = build_color_image() else {
                    return;
                };
                entry.handle.set(color_image, egui::TextureOptions::LINEAR);
                entry.width = stored.width;
                entry.height = stored.height;
                entry.revision = stored.revision;
            }
        }
        None => {
            let Some(color_image) = build_color_image() else {
                return;
            };
            let handle = ctx.load_texture(
                format!("egui_xterm_img_{}", stored.id),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            image_textures.insert(
                stored.id,
                CachedImageTexture {
                    handle,
                    width: stored.width,
                    height: stored.height,
                    revision: stored.revision,
                    last_used_frame: frame_seq,
                },
            );
        }
    }

    if let Some(entry) = image_textures.get(&stored.id) {
        let mut mesh = Mesh::with_texture(entry.handle.id());
        mesh.add_rect_with_uv(clipped_rect, uv_rect, Color32::WHITE);
        if z_index < 0 {
            behind.push((z_index, mesh));
        } else {
            above.push((z_index, mesh));
        }
    }
}

#[inline]
fn write_colored_rect_vertices(slice: &mut [Vertex], min: Pos2, max: Pos2, color: Color32) {
    if slice.len() < 4 {
        return;
    }
    slice[0] = Vertex {
        pos: min,
        uv: WHITE_UV,
        color,
    };
    slice[1] = Vertex {
        pos: pos2(max.x, min.y),
        uv: WHITE_UV,
        color,
    };
    slice[2] = Vertex {
        pos: pos2(min.x, max.y),
        uv: WHITE_UV,
        color,
    };
    slice[3] = Vertex {
        pos: max,
        uv: WHITE_UV,
        color,
    };
}

#[inline]
fn push_text_quad(
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    pos: [Pos2; 4],
    uv: [Pos2; 4],
    color: Color32,
) {
    let base = vertices.len() as u32;
    indices.extend_from_slice(&[base, base + 1, base + 2, base + 2, base + 1, base + 3]);
    vertices.extend_from_slice(&[
        Vertex {
            pos: pos[0],
            uv: uv[0],
            color,
        },
        Vertex {
            pos: pos[1],
            uv: uv[1],
            color,
        },
        Vertex {
            pos: pos[2],
            uv: uv[2],
            color,
        },
        Vertex {
            pos: pos[3],
            uv: uv[3],
            color,
        },
    ]);
}

#[inline]
fn detect_bold_render_mode(
    fonts: &egui::epaint::FontsView<'_>,
    cell_w: f32,
    ppp: f32,
) -> BoldRenderMode {
    let bold_family = FontFamily::Name("monospace_bold".into());
    if fonts
        .definitions()
        .families
        .get(&bold_family)
        .is_some_and(|list| !list.is_empty())
    {
        return BoldRenderMode::FontFamily(bold_family);
    }

    if let Some(primary_name) = fonts
        .definitions()
        .families
        .get(&FontFamily::Monospace)
        .and_then(|list| list.first())
        && let Some(font_data) = fonts.definitions().font_data.get(primary_name)
    {
        for axis in font_data.variation_axes() {
            if axis.tag.to_be_bytes() == *b"wght" && axis.range.max >= 600.0 {
                let target_wght = 700.0_f32.clamp(axis.range.min, axis.range.max);
                return BoldRenderMode::VariableWght(target_wght);
            }
        }
    }

    let bold_dx = snap_size_at_least_1px((cell_w * 0.08).min(1.0), ppp);
    BoldRenderMode::FauxBold(bold_dx)
}

#[cfg(feature = "shell-integration")]
fn compute_shell_zones_sig(
    snapshot: &RenderSnapshot<'_>,
    zones: ShellZonesView<'_>,
    retained_top: usize,
    retained_end: usize,
) -> u64 {
    if zones.is_empty() || snapshot.rows == 0 || snapshot.cols == 0 || snapshot.is_alt_screen {
        return 0;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut count = 0usize;
    for z in zones.in_line_range(retained_top, retained_end) {
        z.start_line.hash(&mut h);
        z.end_line.hash(&mut h);
        z.kind.hash(&mut h);
        z.exit_code.hash(&mut h);
        count += 1;
    }
    if count == 0 {
        0
    } else {
        count.hash(&mut h);
        h.finish().max(1)
    }
}

fn tessellate_slot_overlay(slot: &mut CachedRowSlot, cell_w: f32, cell_h: f32, ppp: f32) {
    let mut row_mesh = Mesh {
        vertices: std::mem::take(&mut slot.overlay_vertices),
        indices: std::mem::take(&mut slot.overlay_indices),
        texture_id: TextureId::default(),
    };

    #[cfg(feature = "vector-glyphs")]
    for vcell in &slot.vector_cells {
        let cell_rect = Rect::from_min_size(
            pos2(f32::from(vcell.col) * cell_w, 0.0),
            Vec2::new(cell_w, cell_h),
        );
        tessellate_vector_glyph(&mut row_mesh, vcell.ch, cell_rect, vcell.color, ppp);
    }

    let line_thick = snap_size_at_least_1px((cell_h * 0.07).max(1.0), ppp);
    for deco in &slot.decorations {
        let x0 = snap_px(f32::from(deco.start_col) * cell_w, ppp);
        let x1 = snap_px(f32::from(deco.end_col) * cell_w, ppp);
        if x1 <= x0 {
            continue;
        }

        if deco.strikethrough {
            let cy = snap_px(cell_h * 0.5, ppp);
            let r = snap_rect(
                Rect::from_min_max(pos2(x0, cy), pos2(x1, cy + line_thick)),
                ppp,
            );
            if r.is_positive() {
                row_mesh.add_colored_rect(r, deco.fg_color);
            }
        }

        if deco.overline {
            let r = snap_rect(Rect::from_min_max(pos2(x0, 0.0), pos2(x1, line_thick)), ppp);
            if r.is_positive() {
                row_mesh.add_colored_rect(r, deco.fg_color);
            }
        }

        paint_underline(
            &mut row_mesh,
            &mut slot.curly_paths,
            deco.underline,
            x0,
            x1,
            0.0,
            cell_h,
            line_thick,
            ppp,
            deco.underline_color,
        );
    }

    slot.overlay_vertices = row_mesh.vertices;
    slot.overlay_indices = row_mesh.indices;
}

#[inline]
fn needs_contextual_shaping(cluster: &str) -> bool {
    cluster.chars().any(|ch| {
        matches!(
            ch,
            '\u{0590}'..='\u{08FF}'
                | '\u{0900}'..='\u{109F}'
                | '\u{1780}'..='\u{17FF}'
                | '\u{AA60}'..='\u{AA7F}'
                | '\u{FB1D}'..='\u{FDFF}'
                | '\u{FE70}'..='\u{FEFF}'
        )
    })
}

fn snap_slow_galley_clusters(galley: &mut Arc<Galley>, clusters: &[(f32, usize)]) {
    if galley.rows.len() != 1 {
        return;
    }
    let Some(first_row) = galley.rows.first() else {
        return;
    };
    let total_glyphs: usize = clusters.iter().map(|&(_, g)| g).sum();
    if first_row.row.glyphs.len() != total_glyphs {
        return;
    }
    let mut g_idx = 0usize;
    let mut needs_adjust = false;
    {
        let glyphs = &first_row.row.glyphs;
        for &(target_x, glyph_count) in clusters {
            if glyph_count > 0
                && let Some(g) = glyphs.get(g_idx)
                && (g.pos.x - target_x).abs() > 1e-4
            {
                needs_adjust = true;
                break;
            }
            g_idx += glyph_count;
        }
    }
    if !needs_adjust {
        return;
    }

    let galley_mut = Arc::make_mut(galley);
    let Some(placed_row) = galley_mut.rows.first_mut() else {
        return;
    };
    let row_mut = Arc::make_mut(&mut placed_row.row);
    let glyph_vertex_end = row_mut.visuals.glyph_vertex_range.end;
    let num_glyphs = row_mut.glyphs.len();
    let mut g_idx = 0usize;
    let mut max_glyph_x = 0.0_f32;
    for &(target_x, glyph_count) in clusters {
        if glyph_count == 0 {
            continue;
        }
        let Some(anchor_glyph) = row_mut.glyphs.get(g_idx) else {
            break;
        };
        let dx = target_x - anchor_glyph.pos.x;
        if dx.abs() > 1e-4 {
            for i in g_idx..(g_idx + glyph_count) {
                let Some(curr_glyph) = row_mut.glyphs.get(i) else {
                    break;
                };
                let v_start = curr_glyph.first_vertex as usize;
                let v_end = if i + 1 < num_glyphs {
                    row_mut
                        .glyphs
                        .get(i + 1)
                        .map_or(glyph_vertex_end, |g| g.first_vertex as usize)
                } else {
                    glyph_vertex_end
                };
                if let Some(mut_glyph) = row_mut.glyphs.get_mut(i) {
                    mut_glyph.pos.x += dx;
                }
                if v_start < v_end && v_end <= row_mut.visuals.mesh.vertices.len() {
                    for v in &mut row_mut.visuals.mesh.vertices[v_start..v_end] {
                        v.pos.x += dx;
                    }
                }
            }
        }
        for i in g_idx..(g_idx + glyph_count) {
            if let Some(g) = row_mut.glyphs.get(i) {
                max_glyph_x = max_glyph_x.max(g.max_x());
            }
        }
        g_idx += glyph_count;
    }

    let mesh_bounds = row_mut.visuals.mesh.calc_bounds();
    row_mut.visuals.mesh_bounds = mesh_bounds;
    row_mut.size.x = row_mut.size.x.max(max_glyph_x).round_ui();
    galley_mut.mesh_bounds = mesh_bounds.translate(placed_row.pos.to_vec2());
    galley_mut.rect = placed_row.rect().round_ui();
}

#[inline]
#[expect(clippy::too_many_arguments, reason = "cell fg resolver")]
fn resolve_cell_fg(
    cell: egui_xterm_core::Cell,
    col: usize,
    row_cursor_col: Option<u16>,
    cursor_text_c32: Color32,
    sel_fg_override: Option<Color32>,
    row_sel: Option<(usize, usize)>,
    default_fg: Color32,
    default_bg: Color32,
    theme: &Theme,
    snapshot: &RenderSnapshot<'_>,
    blink_hidden: bool,
) -> Color32 {
    if is_cell_text_hidden(cell, blink_hidden) {
        return Color32::TRANSPARENT;
    }
    let is_bold = cell.flags.contains(CellFlags::BOLD);
    let is_dim = cell.flags.contains(CellFlags::DIM);
    let inverse = cell.flags.contains(CellFlags::INVERSE);

    let mut fg_c32 = if row_cursor_col == Some(col as u16) {
        cursor_text_c32
    } else if let (Some(s_fg), Some((s0, s1))) = (sel_fg_override, row_sel)
        && col >= s0
        && col <= s1
    {
        s_fg
    } else if inverse {
        if cell.bg.is_default()
            && let Some(rev_rgb) = snapshot.special_colors[3]
        {
            rgb_to_color32(rev_rgb)
        } else {
            resolve_bg_color(cell.bg, default_bg, theme, snapshot)
        }
    } else if cell.fg.is_default()
        && let Some(special_rgb) = resolve_special_fg_override(cell, snapshot)
    {
        rgb_to_color32(special_rgb)
    } else {
        resolve_fg_color(cell.fg, default_fg, theme, snapshot, is_bold)
    };

    if is_dim {
        fg_c32 = Color32::from_rgba_unmultiplied(
            ((u16::from(fg_c32.r()) * 2) / 3) as u8,
            ((u16::from(fg_c32.g()) * 2) / 3) as u8,
            ((u16::from(fg_c32.b()) * 2) / 3) as u8,
            fg_c32.a(),
        );
    }
    fg_c32
}

#[inline]
fn resolve_special_fg_override(
    cell: egui_xterm_core::Cell,
    snapshot: &RenderSnapshot<'_>,
) -> Option<[u8; 3]> {
    if cell.flags.contains(CellFlags::BOLD)
        && let Some(rgb) = snapshot.special_colors[0]
    {
        return Some(rgb);
    }
    if cell.underline_style().is_underlined()
        && let Some(rgb) = snapshot.special_colors[1]
    {
        return Some(rgb);
    }
    if cell.flags.intersects(BLINK_MASK)
        && let Some(rgb) = snapshot.special_colors[2]
    {
        return Some(rgb);
    }
    if cell.flags.contains(CellFlags::ITALIC)
        && let Some(rgb) = snapshot.special_colors[4]
    {
        return Some(rgb);
    }
    None
}

#[inline]
fn make_probe_format(
    base_font_id: &FontId,
    bold_mode: &BoldRenderMode,
    use_bold_font: bool,
    is_italic: bool,
    color: Color32,
) -> TextFormat {
    let mut font_id = base_font_id.clone();
    let mut coords = VariationCoords::default();
    if use_bold_font {
        match bold_mode {
            BoldRenderMode::FontFamily(family) => {
                font_id.family = family.clone();
            }
            BoldRenderMode::VariableWght(wght) => {
                coords.push(u32::from_be_bytes(*b"wght"), *wght);
            }
            BoldRenderMode::FauxBold(_) => {}
        }
    }
    TextFormat {
        font_id,
        color,
        italics: is_italic,
        coords,
        extra_letter_spacing: 0.0,
        ..Default::default()
    }
}

#[inline]
fn get_or_probe_ascii_entry(
    cache: &mut AsciiGlyphCache,
    fonts: &mut egui::epaint::FontsView<'_>,
    ch: char,
    base_font_id: &FontId,
    bold_mode: &BoldRenderMode,
    cell_h: f32,
    use_bold_font: bool,
) -> Option<AsciiGlyphEntry> {
    let idx = (ch as u32).checked_sub(0x20)? as usize;
    if idx >= 95 {
        return None;
    }
    let (entries, unsupported) = if use_bold_font {
        (&mut cache.bold_entries, &mut cache.bold_unsupported)
    } else {
        (&mut cache.entries, &mut cache.unsupported)
    };
    if unsupported[idx] {
        return None;
    }
    if let Some(entry) = entries[idx] {
        return Some(entry);
    }

    let mut buf = [0u8; 4];
    let s = ch.encode_utf8(&mut buf);
    let mut job = LayoutJob {
        wrap: TextWrapping {
            max_width: f32::INFINITY,
            ..Default::default()
        },
        first_row_min_height: cell_h,
        break_on_newline: false,
        keep_trailing_whitespace: true,
        ..Default::default()
    };
    job.append(
        s,
        0.0,
        make_probe_format(
            base_font_id,
            bold_mode,
            use_bold_font,
            false,
            Color32::WHITE,
        ),
    );
    let g = fonts.layout_job(job);
    if g.rows.len() != 1 {
        unsupported[idx] = true;
        return None;
    }
    let Some(first_row) = g.rows.first() else {
        unsupported[idx] = true;
        return None;
    };
    let row = &first_row.row;
    if row.glyphs.len() != 1 {
        unsupported[idx] = true;
        return None;
    }
    let Some(glyph) = row.glyphs.first().copied() else {
        unsupported[idx] = true;
        return None;
    };
    if glyph.is_color {
        unsupported[idx] = true;
        return None;
    }
    let mesh = &row.visuals.mesh;
    let entry = if mesh.vertices.is_empty() && mesh.indices.is_empty() {
        AsciiGlyphEntry {
            has_quad: false,
            rect_min: Pos2::ZERO,
            rect_size: Vec2::ZERO,
            uv_min: Pos2::ZERO,
            uv_max: Pos2::ZERO,
        }
    } else if let [v0, _, _, v3] = mesh.vertices.as_slice()
        && mesh.indices.len() == 6
        && mesh.texture_id == TextureId::default()
    {
        AsciiGlyphEntry {
            has_quad: true,
            rect_min: v0.pos,
            rect_size: v3.pos - v0.pos,
            uv_min: v0.uv,
            uv_max: v3.uv,
        }
    } else {
        unsupported[idx] = true;
        return None;
    };

    entries[idx] = Some(entry);
    Some(entry)
}

#[inline]
fn emit_ascii_entry_quad(
    slot: &mut CachedRowSlot,
    entry: AsciiGlyphEntry,
    x_off: f32,
    fg_c32: Color32,
    is_italic: bool,
    faux_bold_dx: Option<f32>,
) {
    if !entry.has_quad || fg_c32.a() == 0 {
        return;
    }
    let left_top = pos2(x_off + entry.rect_min.x, entry.rect_min.y);
    let right_bottom = left_top + entry.rect_size;
    let top_dx = if is_italic {
        entry.rect_size.y * 0.25
    } else {
        0.0
    };
    let uv = [
        entry.uv_min,
        pos2(entry.uv_max.x, entry.uv_min.y),
        pos2(entry.uv_min.x, entry.uv_max.y),
        entry.uv_max,
    ];
    push_text_quad(
        &mut slot.text_vertices,
        &mut slot.text_indices,
        [
            pos2(left_top.x + top_dx, left_top.y),
            pos2(right_bottom.x + top_dx, left_top.y),
            pos2(left_top.x, right_bottom.y),
            right_bottom,
        ],
        uv,
        fg_c32,
    );
    if let Some(dx) = faux_bold_dx {
        push_text_quad(
            &mut slot.text_vertices,
            &mut slot.text_indices,
            [
                pos2(left_top.x + top_dx + dx, left_top.y),
                pos2(right_bottom.x + top_dx + dx, left_top.y),
                pos2(left_top.x + dx, right_bottom.y),
                pos2(right_bottom.x + dx, right_bottom.y),
            ],
            uv,
            fg_c32,
        );
    }
}

#[expect(clippy::too_many_arguments, reason = "direct ASCII row mesh fast path")]
fn build_ascii_row_mesh(
    cache: &mut AsciiGlyphCache,
    fonts: &mut egui::epaint::FontsView<'_>,
    slot: &mut CachedRowSlot,
    slice: &[egui_xterm_core::Cell],
    start_col: u16,
    row_cursor_col: Option<u16>,
    cursor_text_c32: Color32,
    sel_fg_override: Option<Color32>,
    row_sel: Option<(usize, usize)>,
    default_fg: Color32,
    default_bg: Color32,
    theme: &Theme,
    snapshot: &RenderSnapshot<'_>,
    base_font_id: &FontId,
    bold_mode: &BoldRenderMode,
    cell_w: f32,
    cell_h: f32,
    ppp: f32,
    blink_hidden: bool,
) -> bool {
    if slice.is_empty() {
        return false;
    }

    let is_faux_bold = matches!(bold_mode, BoldRenderMode::FauxBold(_));
    // Verify all visible ASCII characters can be served by `AsciiGlyphCache` before mutating slot.
    for &cell in slice {
        if cell.is_wide_spacer()
            || cell.is_wrap_spacer()
            || cell.is_sized_spacer()
            || is_kitty_placeholder_char(cell.codepoint)
            || is_vector_glyph_char(cell.codepoint)
        {
            continue;
        }
        if cell.codepoint == ' ' || is_cell_text_hidden(cell, blink_hidden) {
            continue;
        }
        let use_bold_font = cell.flags.contains(CellFlags::BOLD) && !is_faux_bold;
        if get_or_probe_ascii_entry(
            cache,
            fonts,
            cell.codepoint,
            base_font_id,
            bold_mode,
            cell_h,
            use_bold_font,
        )
        .is_none()
        {
            return false;
        }
    }

    slot.text_vertices.reserve(slice.len() * 4);
    slot.text_indices.reserve(slice.len() * 6);
    slot.text_is_standard_quads = true;

    let faux_bold_shift = match *bold_mode {
        BoldRenderMode::FauxBold(dx) => Some(dx),
        BoldRenderMode::FontFamily(_) | BoldRenderMode::VariableWght(_) => None,
    };

    for (offset, &cell) in slice.iter().enumerate() {
        if cell.is_wide_spacer()
            || cell.is_wrap_spacer()
            || cell.is_sized_spacer()
            || is_kitty_placeholder_char(cell.codepoint)
            || is_vector_glyph_char(cell.codepoint)
        {
            continue;
        }
        if cell.codepoint == ' ' || is_cell_text_hidden(cell, blink_hidden) {
            continue;
        }

        let col = usize::from(start_col) + offset;
        let is_bold = cell.flags.contains(CellFlags::BOLD);
        let is_italic = cell.flags.contains(CellFlags::ITALIC);
        let fg_c32 = resolve_cell_fg(
            cell,
            col,
            row_cursor_col,
            cursor_text_c32,
            sel_fg_override,
            row_sel,
            default_fg,
            default_bg,
            theme,
            snapshot,
            blink_hidden,
        );
        if fg_c32.a() == 0 {
            continue;
        }

        let use_bold_font = is_bold && !is_faux_bold;
        let idx = (cell.codepoint as usize).saturating_sub(0x20);
        let entry = if use_bold_font {
            cache.bold_entries.get(idx).copied().flatten()
        } else {
            cache.entries.get(idx).copied().flatten()
        };
        let Some(entry) = entry else {
            return false;
        };
        if !entry.has_quad {
            continue;
        }

        let x_off = snap_px(offset as f32 * cell_w, ppp);
        let faux_dx = if is_bold { faux_bold_shift } else { None };
        emit_ascii_entry_quad(slot, entry, x_off, fg_c32, is_italic, faux_dx);
    }

    true
}

#[inline]
fn extract_cluster_from_galley(galley: &Galley) -> CachedCluster {
    let Some(placed) = galley.rows.first() else {
        return CachedCluster {
            glyph_count: 0,
            standalone_quads: Some(ClusterQuads::Empty),
        };
    };
    let row = &placed.row;
    let glyph_count = row.glyphs.len();
    if galley.rows.len() != 1 || row.visuals.mesh.texture_id != TextureId::default() {
        return CachedCluster {
            glyph_count,
            standalone_quads: None,
        };
    }

    let mesh = &row.visuals.mesh;
    let visible_glyphs: Vec<_> = row
        .glyphs
        .iter()
        .filter(|g| !g.uv_rect.is_nothing())
        .collect();
    if mesh.vertices.is_empty() && visible_glyphs.is_empty() {
        return CachedCluster {
            glyph_count,
            standalone_quads: Some(ClusterQuads::Empty),
        };
    }
    if mesh.vertices.len() != visible_glyphs.len() * 4
        || mesh.indices.len() != visible_glyphs.len() * 6
    {
        return CachedCluster {
            glyph_count,
            standalone_quads: None,
        };
    }

    let mut quads = Vec::with_capacity(visible_glyphs.len());
    for (q_idx, glyph) in visible_glyphs.iter().enumerate() {
        let base_v = q_idx * 4;
        let Some(verts) = mesh.vertices.get(base_v..base_v + 4) else {
            return CachedCluster {
                glyph_count,
                standalone_quads: None,
            };
        };
        let base_i = q_idx * 6;
        let Some(idxs) = mesh.indices.get(base_i..base_i + 6) else {
            return CachedCluster {
                glyph_count,
                standalone_quads: None,
            };
        };
        let b = base_v as u32;
        if idxs != [b, b + 1, b + 2, b + 2, b + 1, b + 3] {
            return CachedCluster {
                glyph_count,
                standalone_quads: None,
            };
        }
        quads.push(ClusterQuad {
            pos: [verts[0].pos, verts[1].pos, verts[2].pos, verts[3].pos],
            uv: [verts[0].uv, verts[1].uv, verts[2].uv, verts[3].uv],
            is_color: glyph.is_color,
        });
    }

    let cq = match quads.as_slice() {
        [] => ClusterQuads::Empty,
        [single] => ClusterQuads::Single(*single),
        _ => ClusterQuads::Multi(quads),
    };
    CachedCluster {
        glyph_count,
        standalone_quads: Some(cq),
    }
}

#[inline]
fn emit_cluster_quads(
    slot: &mut CachedRowSlot,
    quads: &ClusterQuads,
    x_off: f32,
    fg_c32: Color32,
    faux_bold_dx: Option<f32>,
) {
    let mut push_one = |quad: &ClusterQuad| {
        let color = if quad.is_color {
            Color32::from_white_alpha(fg_c32.a())
        } else {
            fg_c32
        };
        push_text_quad(
            &mut slot.text_vertices,
            &mut slot.text_indices,
            [
                pos2(quad.pos[0].x + x_off, quad.pos[0].y),
                pos2(quad.pos[1].x + x_off, quad.pos[1].y),
                pos2(quad.pos[2].x + x_off, quad.pos[2].y),
                pos2(quad.pos[3].x + x_off, quad.pos[3].y),
            ],
            quad.uv,
            color,
        );
        if !quad.is_color
            && let Some(dx) = faux_bold_dx
        {
            push_text_quad(
                &mut slot.text_vertices,
                &mut slot.text_indices,
                [
                    pos2(quad.pos[0].x + x_off + dx, quad.pos[0].y),
                    pos2(quad.pos[1].x + x_off + dx, quad.pos[1].y),
                    pos2(quad.pos[2].x + x_off + dx, quad.pos[2].y),
                    pos2(quad.pos[3].x + x_off + dx, quad.pos[3].y),
                ],
                quad.uv,
                color,
            );
        }
    };

    match quads {
        ClusterQuads::Empty => {}
        ClusterQuads::Single(q) => push_one(q),
        ClusterQuads::Multi(list) => {
            for q in list {
                push_one(q);
            }
        }
    }
}

fn append_galley_to_slot(
    slot: &mut CachedRowSlot,
    galley: &Galley,
    offset: Vec2,
    faux_bold_dx: Option<f32>,
) {
    let Some(placed) = galley.rows.first() else {
        return;
    };
    let row = &placed.row;
    let mesh = &row.visuals.mesh;
    if mesh.vertices.is_empty() || mesh.texture_id != TextureId::default() {
        return;
    }

    let row_has_color = row.glyphs.iter().any(|g| g.is_color);
    let glyph_range = &row.visuals.glyph_vertex_range;
    let is_color_vert = |vertex_idx: usize| -> bool {
        if !row_has_color || !glyph_range.contains(&vertex_idx) {
            return false;
        }
        let g_idx = row
            .glyphs
            .partition_point(|g| g.first_vertex as usize <= vertex_idx);
        g_idx
            .checked_sub(1)
            .and_then(|idx| row.glyphs.get(idx))
            .is_some_and(|g| g.is_color && !g.uv_rect.is_nothing())
    };

    let base_v = slot.text_vertices.len() as u32;
    let vert_start = glyph_range.start.min(mesh.vertices.len());
    let vert_end = glyph_range.end.min(mesh.vertices.len());
    if vert_start >= vert_end {
        return;
    }

    // Check if vertices/indices form standard 4-vertex quads.
    let v_count = vert_end - vert_start;
    let is_quad_layout = v_count.is_multiple_of(4) && mesh.indices.len() == (v_count / 4) * 6;
    if is_quad_layout {
        for q_idx in 0..(v_count / 4) {
            let v_off = vert_start + q_idx * 4;
            let Some(verts) = mesh.vertices.get(v_off..v_off + 4) else {
                break;
            };
            let is_col = is_color_vert(v_off);
            let color = if is_col {
                Color32::from_white_alpha(verts[0].color.a())
            } else {
                verts[0].color
            };
            push_text_quad(
                &mut slot.text_vertices,
                &mut slot.text_indices,
                [
                    verts[0].pos + offset,
                    verts[1].pos + offset,
                    verts[2].pos + offset,
                    verts[3].pos + offset,
                ],
                [verts[0].uv, verts[1].uv, verts[2].uv, verts[3].uv],
                color,
            );
            if !is_col && let Some(dx) = faux_bold_dx {
                let shift = vec2(dx, 0.0);
                push_text_quad(
                    &mut slot.text_vertices,
                    &mut slot.text_indices,
                    [
                        verts[0].pos + offset + shift,
                        verts[1].pos + offset + shift,
                        verts[2].pos + offset + shift,
                        verts[3].pos + offset + shift,
                    ],
                    [verts[0].uv, verts[1].uv, verts[2].uv, verts[3].uv],
                    color,
                );
            }
        }
    } else {
        slot.text_is_standard_quads = false;
        let orig_base = vert_start as u32;
        let orig_end = vert_end as u32;
        for (i, v) in mesh.vertices[vert_start..vert_end].iter().enumerate() {
            let abs_i = vert_start + i;
            let color = if is_color_vert(abs_i) {
                Color32::from_white_alpha(v.color.a())
            } else {
                v.color
            };
            slot.text_vertices.push(Vertex {
                pos: v.pos + offset,
                uv: v.uv,
                color,
            });
        }
        for &idx in &mesh.indices {
            if idx >= orig_base && idx < orig_end {
                slot.text_indices.push(base_v + (idx - orig_base));
            }
        }
    }
}

fn flush_contextual_run(
    ctx_run: &mut ContextualRunScratch,
    fonts: &mut egui::epaint::FontsView<'_>,
    slot: &mut CachedRowSlot,
    base_font_id: &FontId,
    bold_mode: &BoldRenderMode,
    cell_h: f32,
) {
    if !ctx_run.active || ctx_run.cells.is_empty() {
        ctx_run.clear();
        return;
    }

    let is_faux_bold = matches!(bold_mode, BoldRenderMode::FauxBold(_));
    let faux_bold_shift = match *bold_mode {
        BoldRenderMode::FauxBold(dx) => Some(dx),
        BoldRenderMode::FontFamily(_) | BoldRenderMode::VariableWght(_) => None,
    };

    // Split on `is_bold` transitions under FauxBold so double-stamping stays exact per run.
    let mut slice_start = 0usize;
    while slice_start < ctx_run.cells.len() {
        let run_bold = ctx_run.cells[slice_start].is_bold;
        let mut slice_end = slice_start + 1;
        while slice_end < ctx_run.cells.len() && ctx_run.cells[slice_end].is_bold == run_bold {
            slice_end += 1;
        }
        let seg_cells = &ctx_run.cells[slice_start..slice_end];
        let seg_origin_x = ctx_run.start_x + seg_cells[0].x_rel;
        let use_bold_font = run_bold && !is_faux_bold;

        let mut job = LayoutJob {
            wrap: TextWrapping {
                max_width: f32::INFINITY,
                ..Default::default()
            },
            first_row_min_height: cell_h,
            break_on_newline: false,
            keep_trailing_whitespace: true,
            ..Default::default()
        };
        let mut cursor_x = 0.0_f32;
        ctx_run.clusters.clear();
        for c in seg_cells {
            let local_x = (ctx_run.start_x + c.x_rel) - seg_origin_x;
            let leading_space = local_x - cursor_x;
            let fmt = make_probe_format(
                base_font_id,
                bold_mode,
                use_bold_font,
                c.is_italic,
                c.fg_c32,
            );
            job.append(
                &ctx_run.text[c.text_byte_start..c.text_byte_end],
                leading_space,
                fmt,
            );
            cursor_x = local_x;
            ctx_run.clusters.push((local_x, c.glyph_count));
        }
        let mut galley = fonts.layout_job(job);
        snap_slow_galley_clusters(&mut galley, &ctx_run.clusters);
        let faux_dx = if run_bold { faux_bold_shift } else { None };
        append_galley_to_slot(slot, &galley, vec2(seg_origin_x, 0.0), faux_dx);

        slice_start = slice_end;
    }

    ctx_run.clear();
}

#[expect(clippy::too_many_arguments, reason = "slow-path cell mesh builder")]
fn build_slow_row_mesh(
    ascii_cache: &mut AsciiGlyphCache,
    cluster_cache: &mut HashMap<String, [Option<CachedCluster>; 4]>,
    scratch_cluster: &mut String,
    scratch_ctx_run: &mut ContextualRunScratch,
    fonts: &mut egui::epaint::FontsView<'_>,
    slot: &mut CachedRowSlot,
    cells: &[egui_xterm_core::Cell],
    row_cols: usize,
    anchor_idx: usize,
    row_text_end: usize,
    start_col: u16,
    row_cursor_col: Option<u16>,
    cursor_text_c32: Color32,
    sel_fg_override: Option<Color32>,
    row_sel: Option<(usize, usize)>,
    default_fg: Color32,
    default_bg: Color32,
    theme: &Theme,
    snapshot: &RenderSnapshot<'_>,
    base_font_id: &FontId,
    bold_mode: &BoldRenderMode,
    font_size: f32,
    cell_w: f32,
    cell_h: f32,
    ppp: f32,
    blink_hidden: bool,
) {
    slot.text_vertices.clear();
    slot.text_indices.clear();
    slot.text_is_standard_quads = true;
    scratch_ctx_run.clear();

    let is_faux_bold = matches!(bold_mode, BoldRenderMode::FauxBold(_));
    let faux_bold_shift = match *bold_mode {
        BoldRenderMode::FauxBold(dx) => Some(dx),
        BoldRenderMode::FontFamily(_) | BoldRenderMode::VariableWght(_) => None,
    };

    for (col, &cell) in cells.iter().enumerate().take(row_text_end).skip(anchor_idx) {
        if cell.is_wide_spacer()
            || cell.is_wrap_spacer()
            || cell.is_sized_spacer()
            || is_kitty_placeholder_char(cell.codepoint)
        {
            continue;
        }

        let extra = if cell.extra_id != 0 {
            snapshot.cell_extra(cell.extra_id)
        } else {
            None
        };
        let has_combining = extra.is_some_and(|e| !e.combining.is_empty());
        let text_sizing = extra.and_then(|e| e.text_sizing);
        if !has_combining && text_sizing.is_none() && is_vector_glyph_char(cell.codepoint) {
            continue;
        }

        let col_u16 = col as u16;
        let is_bold = cell.flags.contains(CellFlags::BOLD);
        let is_italic = cell.flags.contains(CellFlags::ITALIC);
        let fg_c32 = resolve_cell_fg(
            cell,
            col,
            row_cursor_col,
            cursor_text_c32,
            sel_fg_override,
            row_sel,
            default_fg,
            default_bg,
            theme,
            snapshot,
            blink_hidden,
        );
        if fg_c32.a() == 0 {
            continue;
        }

        let target_x = snap_px(f32::from(col_u16 - start_col) * cell_w, ppp);
        let use_bold_font = is_bold && !is_faux_bold;
        let faux_dx = if is_bold { faux_bold_shift } else { None };

        // Case A: OSC 66 text sizing cell (`scale`, fractional `n/d`, explicit `width`, alignment).
        if let Some(ts) = text_sizing
            && (ts.scale > 1
                || (ts.frac_num > 0 && ts.frac_den > ts.frac_num)
                || ts.width > 0
                || ts.h_align != 0
                || ts.v_align != 0)
        {
            flush_contextual_run(
                scratch_ctx_run,
                fonts,
                slot,
                base_font_id,
                bold_mode,
                cell_h,
            );

            scratch_cluster.clear();
            if let Some(ext) = extra {
                ext.write_grapheme(cell.codepoint, scratch_cluster);
            } else {
                scratch_cluster.push(cell.codepoint);
            }

            let footprint_cols = if ts.width > 0 {
                usize::from(ts.width).saturating_mul(usize::from(ts.scale.max(1)))
            } else {
                1 + cells[col + 1..row_cols]
                    .iter()
                    .take_while(|c| c.is_sized_spacer())
                    .count()
            };
            let frac = if ts.frac_num > 0 && ts.frac_den > ts.frac_num {
                f32::from(ts.frac_num) / f32::from(ts.frac_den)
            } else {
                1.0
            };
            let scaled_font_size = (font_size * f32::from(ts.scale.max(1)) * frac).max(1.0);
            let mut sized_font = base_font_id.clone();
            sized_font.size = scaled_font_size;
            let mut job = LayoutJob {
                wrap: TextWrapping {
                    max_width: f32::INFINITY,
                    ..Default::default()
                },
                break_on_newline: false,
                keep_trailing_whitespace: true,
                ..Default::default()
            };
            job.append(
                scratch_cluster,
                0.0,
                make_probe_format(&sized_font, bold_mode, use_bold_font, is_italic, fg_c32),
            );
            let sized_galley = fonts.layout_job(job);
            let g_size = sized_galley.intrinsic_size();
            let box_w = footprint_cols as f32 * cell_w;
            let box_h = f32::from(ts.scale.max(1)) * cell_h;
            let align_dx = match ts.h_align {
                1 => (box_w - g_size.x).max(0.0),
                2 => (box_w - g_size.x).max(0.0) * 0.5,
                _ => 0.0,
            };
            let align_dy = match ts.v_align {
                1 => (box_h - g_size.y).max(0.0),
                2 => (box_h - g_size.y).max(0.0) * 0.5,
                _ => 0.0,
            };
            let pos = vec2(snap_px(target_x + align_dx, ppp), snap_px(align_dy, ppp));
            append_galley_to_slot(slot, &sized_galley, pos, faux_dx);
            continue;
        }

        // Case B: Plain ASCII non-italic/italic cell via `AsciiGlyphCache`.
        if !has_combining && (' '..='~').contains(&cell.codepoint) {
            if cell.codepoint == ' ' {
                continue;
            }
            if let Some(entry) = get_or_probe_ascii_entry(
                ascii_cache,
                fonts,
                cell.codepoint,
                base_font_id,
                bold_mode,
                cell_h,
                use_bold_font,
            ) {
                flush_contextual_run(
                    scratch_ctx_run,
                    fonts,
                    slot,
                    base_font_id,
                    bold_mode,
                    cell_h,
                );
                emit_ascii_entry_quad(slot, entry, target_x, fg_c32, is_italic, faux_dx);
                continue;
            }
        }

        // Case C: Non-ASCII / combining grapheme cluster (zero-alloc `cluster_cache` hit or probe).
        scratch_cluster.clear();
        if let Some(ext) = extra {
            ext.write_grapheme(cell.codepoint, scratch_cluster);
        } else {
            scratch_cluster.push(cell.codepoint);
        }

        let style_idx = (usize::from(use_bold_font) << 1) | usize::from(is_italic);
        let is_contextual = needs_contextual_shaping(scratch_cluster);

        let cached_opt = if !is_contextual {
            cluster_cache
                .get(scratch_cluster.as_str())
                .and_then(|arr| arr[style_idx].clone())
        } else {
            None
        };

        let cluster_entry = if let Some(hit) = cached_opt {
            hit
        } else {
            let mut probe_job = LayoutJob {
                wrap: TextWrapping {
                    max_width: f32::INFINITY,
                    ..Default::default()
                },
                first_row_min_height: cell_h,
                break_on_newline: false,
                keep_trailing_whitespace: true,
                ..Default::default()
            };
            probe_job.append(
                scratch_cluster,
                0.0,
                make_probe_format(
                    base_font_id,
                    bold_mode,
                    use_bold_font,
                    is_italic,
                    Color32::WHITE,
                ),
            );
            let probe_galley = fonts.layout_job(probe_job);
            let extracted = extract_cluster_from_galley(&probe_galley);
            if !is_contextual {
                if cluster_cache.len() >= MAX_CLUSTER_CACHE_ENTRIES {
                    cluster_cache.clear();
                }
                let arr = cluster_cache
                    .entry(scratch_cluster.clone())
                    .or_insert_with(|| [None, None, None, None]);
                arr[style_idx] = Some(extracted.clone());
            }
            extracted
        };

        if !is_contextual && let Some(ref quads) = cluster_entry.standalone_quads {
            flush_contextual_run(
                scratch_ctx_run,
                fonts,
                slot,
                base_font_id,
                bold_mode,
                cell_h,
            );
            emit_cluster_quads(slot, quads, target_x, fg_c32, faux_dx);
        } else {
            if !scratch_ctx_run.active {
                scratch_ctx_run.active = true;
                scratch_ctx_run.start_x = target_x;
            }
            let text_byte_start = scratch_ctx_run.text.len();
            scratch_ctx_run.text.push_str(scratch_cluster);
            let text_byte_end = scratch_ctx_run.text.len();
            scratch_ctx_run.cells.push(ContextualCell {
                text_byte_start,
                text_byte_end,
                x_rel: target_x - scratch_ctx_run.start_x,
                fg_c32,
                is_bold,
                is_italic,
                glyph_count: cluster_entry.glyph_count,
            });
        }
    }

    flush_contextual_run(
        scratch_ctx_run,
        fonts,
        slot,
        base_font_id,
        bold_mode,
        cell_h,
    );
}

#[inline]
fn rgb_to_color32(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

#[inline]
fn resolve_default_pair(
    theme_scheme: &ColorScheme,
    snapshot: &RenderSnapshot<'_>,
) -> (Color32, Color32) {
    let fg_rgb = if snapshot.palette_overrides.foreground {
        snapshot.palette.foreground
    } else {
        theme_scheme.foreground
    };
    let bg_rgb = if snapshot.palette_overrides.background {
        snapshot.palette.background
    } else {
        theme_scheme.background
    };
    if snapshot.modes.reverse_video {
        (rgb_to_color32(bg_rgb), rgb_to_color32(fg_rgb))
    } else {
        (rgb_to_color32(fg_rgb), rgb_to_color32(bg_rgb))
    }
}

#[inline]
fn resolve_cursor_color(theme_scheme: &ColorScheme, snapshot: &RenderSnapshot<'_>) -> Color32 {
    let rgb = if snapshot.palette_overrides.cursor {
        snapshot.palette.cursor
    } else {
        theme_scheme.cursor
    };
    rgb_to_color32(rgb)
}

#[inline]
fn resolve_indexed_rgb(idx: u8, theme: &Theme, snapshot: &RenderSnapshot<'_>) -> [u8; 3] {
    if usize::from(idx) < 16 {
        if snapshot.palette_overrides.ansi[usize::from(idx)] {
            snapshot.palette.ansi[usize::from(idx)]
        } else {
            theme.scheme.ansi[usize::from(idx)]
        }
    } else if let Some(rgb) = snapshot.extended_palette[usize::from(idx - 16)] {
        rgb
    } else {
        ColorScheme::xterm_256_color(idx, &theme.scheme.ansi)
    }
}

#[inline]
fn resolve_fg_color(
    packed: PackedColor,
    default_fg: Color32,
    theme: &Theme,
    snapshot: &RenderSnapshot<'_>,
    is_bold: bool,
) -> Color32 {
    match packed {
        PackedColor::Default => default_fg,
        PackedColor::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        PackedColor::Indexed(idx) => {
            let eff_idx = if is_bold && theme.bold_is_bright && idx < 8 {
                idx + 8
            } else {
                idx
            };
            rgb_to_color32(resolve_indexed_rgb(eff_idx, theme, snapshot))
        }
    }
}

#[inline]
fn resolve_bg_color(
    packed: PackedColor,
    default_bg: Color32,
    theme: &Theme,
    snapshot: &RenderSnapshot<'_>,
) -> Color32 {
    match packed {
        PackedColor::Default => default_bg,
        PackedColor::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        PackedColor::Indexed(idx) => rgb_to_color32(resolve_indexed_rgb(idx, theme, snapshot)),
    }
}

#[cfg(feature = "search")]
#[inline]
fn search_highlight_row_cursor(
    row_matches: &[SearchMatch],
    row_m_idx: &mut usize,
    active: Option<SearchMatch>,
    retained_line: usize,
    col: u16,
) -> Option<Color32> {
    while let Some(&m) = row_matches.get(*row_m_idx) {
        if m.end_line == retained_line && col >= m.end_col {
            *row_m_idx += 1;
        } else {
            break;
        }
    }
    let mut matched = false;
    let mut is_active = false;
    for &m in &row_matches[*row_m_idx..] {
        if m.line == retained_line && col < m.start_col {
            break;
        }
        let in_span = if m.line == m.end_line {
            col >= m.start_col && col < m.end_col
        } else if retained_line == m.line {
            col >= m.start_col
        } else if retained_line == m.end_line {
            col < m.end_col
        } else {
            true
        };
        if in_span {
            matched = true;
            if active == Some(m) {
                is_active = true;
                break;
            }
        }
    }
    if is_active {
        Some(Color32::from_rgb(255, 165, 0))
    } else if matched {
        Some(Color32::from_rgb(130, 100, 30))
    } else {
        None
    }
}

#[cfg(all(test, feature = "search"))]
fn search_highlight_at(
    matches: &[SearchMatch],
    active: Option<SearchMatch>,
    retained_line: usize,
    col: u16,
) -> Option<Color32> {
    let lo = matches.partition_point(|m| m.end_line < retained_line);
    let hi = lo + matches[lo..].partition_point(|m| m.line <= retained_line);
    let mut idx = 0usize;
    search_highlight_row_cursor(&matches[lo..hi], &mut idx, active, retained_line, col)
}

fn cursor_cell_rect(origin: Pos2, col: u16, row: u16, cell_w: f32, cell_h: f32, ppp: f32) -> Rect {
    let min_x = snap_px(origin.x + f32::from(col) * cell_w, ppp);
    let min_y = snap_px(origin.y + f32::from(row) * cell_h, ppp);
    let max_x = snap_px(origin.x + f32::from(col + 1) * cell_w, ppp);
    let max_y = snap_px(origin.y + f32::from(row + 1) * cell_h, ppp);
    snap_rect(
        Rect::from_min_max(pos2(min_x, min_y), pos2(max_x, max_y)),
        ppp,
    )
}

fn push_hollow_rect_shapes(shapes: &mut Vec<egui::Shape>, rect: Rect, ppp: f32, color: Color32) {
    let t = snap_size_at_least_1px(1.0, ppp);
    let top = snap_rect(
        Rect::from_min_max(rect.min, pos2(rect.max.x, (rect.min.y + t).min(rect.max.y))),
        ppp,
    );
    let bottom = snap_rect(
        Rect::from_min_max(pos2(rect.min.x, (rect.max.y - t).max(rect.min.y)), rect.max),
        ppp,
    );
    let left = snap_rect(
        Rect::from_min_max(rect.min, pos2((rect.min.x + t).min(rect.max.x), rect.max.y)),
        ppp,
    );
    let right = snap_rect(
        Rect::from_min_max(pos2((rect.max.x - t).max(rect.min.x), rect.min.y), rect.max),
        ppp,
    );
    for edge in [top, bottom, left, right] {
        if edge.is_positive() {
            shapes.push(egui::Shape::rect_filled(edge, 0.0, color));
        }
    }
}

#[expect(clippy::too_many_arguments, reason = "underline geometry helper")]
fn paint_underline(
    overlay_mesh: &mut Mesh,
    paths: &mut Vec<PathShape>,
    style: UnderlineStyle,
    x0: f32,
    x1: f32,
    row_y: f32,
    cell_h: f32,
    thick: f32,
    ppp: f32,
    color: Color32,
) {
    let y_bot = snap_px(row_y + cell_h, ppp);
    let y_base = snap_px((y_bot - thick * 1.5).max(row_y), ppp);

    match style {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => {
            let r = snap_rect(
                Rect::from_min_max(pos2(x0, y_base), pos2(x1, (y_base + thick).min(y_bot))),
                ppp,
            );
            if r.is_positive() {
                overlay_mesh.add_colored_rect(r, color);
            }
        }
        UnderlineStyle::Double => {
            let y1 = snap_px((y_bot - thick).max(row_y), ppp);
            let y0 = snap_px((y1 - thick * 2.0).max(row_y), ppp);
            let r0 = snap_rect(
                Rect::from_min_max(pos2(x0, y0), pos2(x1, (y0 + thick).min(y1))),
                ppp,
            );
            let r1 = snap_rect(
                Rect::from_min_max(pos2(x0, y1), pos2(x1, (y1 + thick).min(y_bot))),
                ppp,
            );
            if r0.is_positive() {
                overlay_mesh.add_colored_rect(r0, color);
            }
            if r1.is_positive() {
                overlay_mesh.add_colored_rect(r1, color);
            }
        }
        UnderlineStyle::Dotted => {
            let step = snap_size_at_least_1px(thick * 2.0, ppp);
            let mut x = x0;
            while x < x1 {
                let dx1 = snap_px((x + thick).min(x1), ppp);
                let r = snap_rect(
                    Rect::from_min_max(pos2(x, y_base), pos2(dx1, (y_base + thick).min(y_bot))),
                    ppp,
                );
                if r.is_positive() {
                    overlay_mesh.add_colored_rect(r, color);
                }
                x += step;
            }
        }
        UnderlineStyle::Dashed => {
            let dash = snap_size_at_least_1px(thick * 4.0, ppp);
            let gap = snap_size_at_least_1px(thick * 2.0, ppp);
            let mut x = x0;
            while x < x1 {
                let dx1 = snap_px((x + dash).min(x1), ppp);
                let r = snap_rect(
                    Rect::from_min_max(pos2(x, y_base), pos2(dx1, (y_base + thick).min(y_bot))),
                    ppp,
                );
                if r.is_positive() {
                    overlay_mesh.add_colored_rect(r, color);
                }
                x += dash + gap;
            }
        }
        UnderlineStyle::Curly => {
            let amp = (cell_h * 0.08).max(1.0);
            let wave_len = (cell_h * 0.35).max(4.0);
            let steps = (((x1 - x0) / (wave_len * 0.25)).ceil() as usize).clamp(4, 512);
            let mut points = Vec::with_capacity(steps + 1);
            let mid_y = (y_bot - amp - thick * 0.5).max(row_y);
            for i in 0..=steps {
                let t = i as f32 / steps as f32;
                let x = x0 + (x1 - x0) * t;
                let phase = ((x - x0) / wave_len) * std::f32::consts::TAU;
                let y = mid_y + phase.sin() * amp;
                points.push(pos2(x, y));
            }
            paths.push(PathShape::line(points, Stroke::new(thick, color)));
        }
    }
}

#[cfg(feature = "vector-glyphs")]
#[inline]
const fn is_vector_glyph(ch: char) -> bool {
    matches!(
        ch,
        '\u{2500}'..='\u{257F}'
            | '\u{2580}'..='\u{259F}'
            | '\u{2800}'..='\u{28FF}'
            | '\u{E0B0}'..='\u{E0D4}'
    )
}

#[cfg(feature = "vector-glyphs")]
fn tessellate_vector_glyph(mesh: &mut Mesh, ch: char, rect: Rect, color: Color32, ppp: f32) {
    let r = snap_rect(rect, ppp);
    if !r.is_positive() {
        return;
    }
    match ch {
        '\u{2580}'..='\u{259F}' => tessellate_block_element(mesh, ch, r, color, ppp),
        '\u{2800}'..='\u{28FF}' => tessellate_braille(mesh, ch, r, color, ppp),
        '\u{2500}'..='\u{257F}' => tessellate_box_drawing(mesh, ch, r, color, ppp),
        '\u{E0B0}'..='\u{E0D4}' => tessellate_powerline(mesh, ch, r, color, ppp),
        _ => {}
    }
}

#[cfg(feature = "vector-glyphs")]
fn tessellate_block_element(mesh: &mut Mesh, ch: char, r: Rect, color: Color32, ppp: f32) {
    let add = |mesh: &mut Mesh, fx0: f32, fy0: f32, fx1: f32, fy1: f32, c: Color32| {
        let sub = snap_rect(
            Rect::from_min_max(
                pos2(r.min.x + r.width() * fx0, r.min.y + r.height() * fy0),
                pos2(r.min.x + r.width() * fx1, r.min.y + r.height() * fy1),
            ),
            ppp,
        );
        if sub.is_positive() {
            mesh.add_colored_rect(sub, c);
        }
    };

    match ch {
        '\u{2580}' => add(mesh, 0.0, 0.0, 1.0, 0.5, color),
        '\u{2581}' => add(mesh, 0.0, 0.875, 1.0, 1.0, color),
        '\u{2582}' => add(mesh, 0.0, 0.75, 1.0, 1.0, color),
        '\u{2583}' => add(mesh, 0.0, 0.625, 1.0, 1.0, color),
        '\u{2584}' => add(mesh, 0.0, 0.5, 1.0, 1.0, color),
        '\u{2585}' => add(mesh, 0.0, 0.375, 1.0, 1.0, color),
        '\u{2586}' => add(mesh, 0.0, 0.25, 1.0, 1.0, color),
        '\u{2587}' => add(mesh, 0.0, 0.125, 1.0, 1.0, color),
        '\u{2588}' => add(mesh, 0.0, 0.0, 1.0, 1.0, color),
        '\u{2589}' => add(mesh, 0.0, 0.0, 0.875, 1.0, color),
        '\u{258A}' => add(mesh, 0.0, 0.0, 0.75, 1.0, color),
        '\u{258B}' => add(mesh, 0.0, 0.0, 0.625, 1.0, color),
        '\u{258C}' => add(mesh, 0.0, 0.0, 0.5, 1.0, color),
        '\u{258D}' => add(mesh, 0.0, 0.0, 0.375, 1.0, color),
        '\u{258E}' => add(mesh, 0.0, 0.0, 0.25, 1.0, color),
        '\u{258F}' => add(mesh, 0.0, 0.0, 0.125, 1.0, color),
        '\u{2590}' => add(mesh, 0.5, 0.0, 1.0, 1.0, color),
        '\u{2591}' => {
            let c = Color32::from_rgba_unmultiplied(
                color.r(),
                color.g(),
                color.b(),
                (u16::from(color.a()) / 4) as u8,
            );
            add(mesh, 0.0, 0.0, 1.0, 1.0, c);
        }
        '\u{2592}' => {
            let c = Color32::from_rgba_unmultiplied(
                color.r(),
                color.g(),
                color.b(),
                (u16::from(color.a()) / 2) as u8,
            );
            add(mesh, 0.0, 0.0, 1.0, 1.0, c);
        }
        '\u{2593}' => {
            let c = Color32::from_rgba_unmultiplied(
                color.r(),
                color.g(),
                color.b(),
                ((u16::from(color.a()) * 3) / 4) as u8,
            );
            add(mesh, 0.0, 0.0, 1.0, 1.0, c);
        }
        '\u{2594}' => add(mesh, 0.0, 0.0, 1.0, 0.125, color),
        '\u{2595}' => add(mesh, 0.875, 0.0, 1.0, 1.0, color),
        // Quadrant blocks U+2596..=U+259F: bitflags (BL, BR, TL, TR)
        '\u{2596}' => add(mesh, 0.0, 0.5, 0.5, 1.0, color),
        '\u{2597}' => add(mesh, 0.5, 0.5, 1.0, 1.0, color),
        '\u{2598}' => add(mesh, 0.0, 0.0, 0.5, 0.5, color),
        '\u{2599}' => {
            add(mesh, 0.0, 0.0, 0.5, 1.0, color);
            add(mesh, 0.5, 0.5, 1.0, 1.0, color);
        }
        '\u{259A}' => {
            add(mesh, 0.0, 0.0, 0.5, 0.5, color);
            add(mesh, 0.5, 0.5, 1.0, 1.0, color);
        }
        '\u{259B}' => {
            add(mesh, 0.0, 0.0, 1.0, 0.5, color);
            add(mesh, 0.0, 0.5, 0.5, 1.0, color);
        }
        '\u{259C}' => {
            add(mesh, 0.0, 0.0, 1.0, 0.5, color);
            add(mesh, 0.5, 0.5, 1.0, 1.0, color);
        }
        '\u{259D}' => add(mesh, 0.5, 0.0, 1.0, 0.5, color),
        '\u{259E}' => {
            add(mesh, 0.5, 0.0, 1.0, 0.5, color);
            add(mesh, 0.0, 0.5, 0.5, 1.0, color);
        }
        '\u{259F}' => {
            add(mesh, 0.5, 0.0, 1.0, 0.5, color);
            add(mesh, 0.0, 0.5, 1.0, 1.0, color);
        }
        _ => {}
    }
}

#[cfg(feature = "vector-glyphs")]
fn tessellate_braille(mesh: &mut Mesh, ch: char, r: Rect, color: Color32, ppp: f32) {
    let bits = (ch as u32) - 0x2800;
    if bits == 0 {
        return;
    }
    // Unicode Braille dot positions: (col 0..=1, row 0..=3)
    const DOTS: [(u32, f32, f32); 8] = [
        (0, 0.25, 0.125), // dot 1
        (1, 0.25, 0.375), // dot 2
        (2, 0.25, 0.625), // dot 3
        (3, 0.75, 0.125), // dot 4
        (4, 0.75, 0.375), // dot 5
        (5, 0.75, 0.625), // dot 6
        (6, 0.25, 0.875), // dot 7
        (7, 0.75, 0.875), // dot 8
    ];
    let dot_w = snap_size_at_least_1px((r.width() * 0.26).max(1.0), ppp);
    let dot_h = snap_size_at_least_1px((r.height() * 0.13).max(1.0), ppp);
    for (bit, fx, fy) in DOTS {
        if (bits & (1 << bit)) != 0 {
            let cx = r.min.x + r.width() * fx;
            let cy = r.min.y + r.height() * fy;
            let dot = snap_rect(
                Rect::from_center_size(pos2(cx, cy), Vec2::new(dot_w, dot_h)),
                ppp,
            );
            if dot.is_positive() {
                mesh.add_colored_rect(dot, color);
            }
        }
    }
}

#[cfg(feature = "vector-glyphs")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineWeight {
    None,
    Light,
    Heavy,
    Double,
}

#[cfg(feature = "vector-glyphs")]
fn tessellate_box_drawing(mesh: &mut Mesh, ch: char, r: Rect, color: Color32, ppp: f32) {
    let light = snap_size_at_least_1px((r.width() * 0.14).max(1.0), ppp);
    let heavy = snap_size_at_least_1px(light * 2.0, ppp);
    let cx = snap_px(r.center().x - light * 0.5, ppp);
    let cy = snap_px(r.center().y - light * 0.5, ppp);

    // Handle diagonals and rounded corners first.
    match ch {
        '\u{2571}' => {
            // Diagonal upper-right to lower-left
            add_quad(
                mesh,
                pos2(r.max.x - light * 0.5, r.min.y),
                pos2(r.max.x, r.min.y + light * 0.5),
                pos2(r.min.x + light * 0.5, r.max.y),
                pos2(r.min.x, r.max.y - light * 0.5),
                color,
            );
            return;
        }
        '\u{2572}' => {
            // Diagonal upper-left to lower-right
            add_quad(
                mesh,
                pos2(r.min.x, r.min.y + light * 0.5),
                pos2(r.min.x + light * 0.5, r.min.y),
                pos2(r.max.x, r.max.y - light * 0.5),
                pos2(r.max.x - light * 0.5, r.max.y),
                color,
            );
            return;
        }
        '\u{2573}' => {
            tessellate_box_drawing(mesh, '\u{2571}', r, color, ppp);
            tessellate_box_drawing(mesh, '\u{2572}', r, color, ppp);
            return;
        }
        '\u{256D}' => {
            // Arc down and right
            draw_box_arms(
                mesh,
                r,
                cx,
                cy,
                light,
                heavy,
                ppp,
                color,
                LineWeight::None,
                LineWeight::Light,
                LineWeight::None,
                LineWeight::Light,
            );
            return;
        }
        '\u{256E}' => {
            // Arc down and left
            draw_box_arms(
                mesh,
                r,
                cx,
                cy,
                light,
                heavy,
                ppp,
                color,
                LineWeight::Light,
                LineWeight::None,
                LineWeight::None,
                LineWeight::Light,
            );
            return;
        }
        '\u{256F}' => {
            // Arc up and left
            draw_box_arms(
                mesh,
                r,
                cx,
                cy,
                light,
                heavy,
                ppp,
                color,
                LineWeight::Light,
                LineWeight::None,
                LineWeight::Light,
                LineWeight::None,
            );
            return;
        }
        '\u{2570}' => {
            // Arc up and right
            draw_box_arms(
                mesh,
                r,
                cx,
                cy,
                light,
                heavy,
                ppp,
                color,
                LineWeight::None,
                LineWeight::Light,
                LineWeight::Light,
                LineWeight::None,
            );
            return;
        }
        _ => {}
    }

    // Dashed horizontal / vertical lines.
    if matches!(
        ch,
        '\u{2504}' | '\u{2505}' | '\u{2508}' | '\u{2509}' | '\u{254C}' | '\u{254D}'
    ) {
        let thick = if matches!(ch, '\u{2505}' | '\u{2509}' | '\u{254D}') {
            heavy
        } else {
            light
        };
        let count = match ch {
            '\u{254C}' | '\u{254D}' => 2,
            '\u{2504}' | '\u{2505}' => 3,
            _ => 4,
        };
        let seg_w = r.width() / (count as f32 * 2.0);
        for i in 0..count {
            let x0 = snap_px(r.min.x + (i as f32 * 2.0 + 0.25) * seg_w, ppp);
            let x1 = snap_px((x0 + seg_w).min(r.max.x), ppp);
            let dash = snap_rect(Rect::from_min_max(pos2(x0, cy), pos2(x1, cy + thick)), ppp);
            if dash.is_positive() {
                mesh.add_colored_rect(dash, color);
            }
        }
        return;
    }
    if matches!(
        ch,
        '\u{2506}' | '\u{2507}' | '\u{250A}' | '\u{250B}' | '\u{254E}' | '\u{254F}'
    ) {
        let thick = if matches!(ch, '\u{2507}' | '\u{250B}' | '\u{254F}') {
            heavy
        } else {
            light
        };
        let count = match ch {
            '\u{254E}' | '\u{254F}' => 2,
            '\u{2506}' | '\u{2507}' => 3,
            _ => 4,
        };
        let seg_h = r.height() / (count as f32 * 2.0);
        for i in 0..count {
            let y0 = snap_px(r.min.y + (i as f32 * 2.0 + 0.25) * seg_h, ppp);
            let y1 = snap_px((y0 + seg_h).min(r.max.y), ppp);
            let dash = snap_rect(Rect::from_min_max(pos2(cx, y0), pos2(cx + thick, y1)), ppp);
            if dash.is_positive() {
                mesh.add_colored_rect(dash, color);
            }
        }
        return;
    }

    let (left, right, up, down) = box_arms_for_char(ch);
    draw_box_arms(
        mesh, r, cx, cy, light, heavy, ppp, color, left, right, up, down,
    );
}

#[cfg(feature = "vector-glyphs")]
#[expect(clippy::too_many_arguments, reason = "4-arm box drawing primitive")]
fn draw_box_arms(
    mesh: &mut Mesh,
    r: Rect,
    cx: f32,
    cy: f32,
    light: f32,
    heavy: f32,
    ppp: f32,
    color: Color32,
    left: LineWeight,
    right: LineWeight,
    up: LineWeight,
    down: LineWeight,
) {
    let gap = light;
    let draw_h = |mesh: &mut Mesh, x0: f32, x1: f32, w: LineWeight| match w {
        LineWeight::None => {}
        LineWeight::Light => {
            let b = snap_rect(Rect::from_min_max(pos2(x0, cy), pos2(x1, cy + light)), ppp);
            if b.is_positive() {
                mesh.add_colored_rect(b, color);
            }
        }
        LineWeight::Heavy => {
            let top = snap_px(cy - (heavy - light) * 0.5, ppp);
            let b = snap_rect(
                Rect::from_min_max(pos2(x0, top), pos2(x1, top + heavy)),
                ppp,
            );
            if b.is_positive() {
                mesh.add_colored_rect(b, color);
            }
        }
        LineWeight::Double => {
            let top1 = snap_px(cy - gap, ppp);
            let top2 = snap_px(cy + gap, ppp);
            let b1 = snap_rect(
                Rect::from_min_max(pos2(x0, top1), pos2(x1, top1 + light)),
                ppp,
            );
            let b2 = snap_rect(
                Rect::from_min_max(pos2(x0, top2), pos2(x1, top2 + light)),
                ppp,
            );
            if b1.is_positive() {
                mesh.add_colored_rect(b1, color);
            }
            if b2.is_positive() {
                mesh.add_colored_rect(b2, color);
            }
        }
    };

    let draw_v = |mesh: &mut Mesh, y0: f32, y1: f32, w: LineWeight| match w {
        LineWeight::None => {}
        LineWeight::Light => {
            let b = snap_rect(Rect::from_min_max(pos2(cx, y0), pos2(cx + light, y1)), ppp);
            if b.is_positive() {
                mesh.add_colored_rect(b, color);
            }
        }
        LineWeight::Heavy => {
            let left_x = snap_px(cx - (heavy - light) * 0.5, ppp);
            let b = snap_rect(
                Rect::from_min_max(pos2(left_x, y0), pos2(left_x + heavy, y1)),
                ppp,
            );
            if b.is_positive() {
                mesh.add_colored_rect(b, color);
            }
        }
        LineWeight::Double => {
            let lx1 = snap_px(cx - gap, ppp);
            let lx2 = snap_px(cx + gap, ppp);
            let b1 = snap_rect(
                Rect::from_min_max(pos2(lx1, y0), pos2(lx1 + light, y1)),
                ppp,
            );
            let b2 = snap_rect(
                Rect::from_min_max(pos2(lx2, y0), pos2(lx2 + light, y1)),
                ppp,
            );
            if b1.is_positive() {
                mesh.add_colored_rect(b1, color);
            }
            if b2.is_positive() {
                mesh.add_colored_rect(b2, color);
            }
        }
    };

    let mid_x_end = snap_px(cx + light, ppp);
    let mid_y_end = snap_px(cy + light, ppp);
    draw_h(mesh, r.min.x, mid_x_end, left);
    draw_h(mesh, cx, r.max.x, right);
    draw_v(mesh, r.min.y, mid_y_end, up);
    draw_v(mesh, cy, r.max.y, down);
}

#[cfg(feature = "vector-glyphs")]
const fn box_arms_for_char(ch: char) -> (LineWeight, LineWeight, LineWeight, LineWeight) {
    use LineWeight::{Double as D, Heavy as H, Light as L, None as N};
    match ch {
        '\u{2500}' => (L, L, N, N),
        '\u{2501}' => (H, H, N, N),
        '\u{2502}' => (N, N, L, L),
        '\u{2503}' => (N, N, H, H),
        '\u{250C}' => (N, L, N, L),
        '\u{250D}' => (N, H, N, L),
        '\u{250E}' => (N, L, N, H),
        '\u{250F}' => (N, H, N, H),
        '\u{2510}' => (L, N, N, L),
        '\u{2511}' => (H, N, N, L),
        '\u{2512}' => (L, N, N, H),
        '\u{2513}' => (H, N, N, H),
        '\u{2514}' => (N, L, L, N),
        '\u{2515}' => (N, H, L, N),
        '\u{2516}' => (N, L, H, N),
        '\u{2517}' => (N, H, H, N),
        '\u{2518}' => (L, N, L, N),
        '\u{2519}' => (H, N, L, N),
        '\u{251A}' => (L, N, H, N),
        '\u{251B}' => (H, N, H, N),
        '\u{251C}' => (N, L, L, L),
        '\u{251D}' => (N, H, L, L),
        '\u{251E}' => (N, L, H, L),
        '\u{251F}' => (N, L, L, H),
        '\u{2520}' => (N, L, H, H),
        '\u{2521}' => (N, H, H, L),
        '\u{2522}' => (N, H, L, H),
        '\u{2523}' => (N, H, H, H),
        '\u{2524}' => (L, N, L, L),
        '\u{2525}' => (H, N, L, L),
        '\u{2526}' => (L, N, H, L),
        '\u{2527}' => (L, N, L, H),
        '\u{2528}' => (L, N, H, H),
        '\u{2529}' => (H, N, H, L),
        '\u{252A}' => (H, N, L, H),
        '\u{252B}' => (H, N, H, H),
        '\u{252C}' => (L, L, N, L),
        '\u{252D}' => (H, L, N, L),
        '\u{252E}' => (L, H, N, L),
        '\u{252F}' => (H, H, N, L),
        '\u{2530}' => (L, L, N, H),
        '\u{2531}' => (H, L, N, H),
        '\u{2532}' => (L, H, N, H),
        '\u{2533}' => (H, H, N, H),
        '\u{2534}' => (L, L, L, N),
        '\u{2535}' => (H, L, L, N),
        '\u{2536}' => (L, H, L, N),
        '\u{2537}' => (H, H, L, N),
        '\u{2538}' => (L, L, H, N),
        '\u{2539}' => (H, L, H, N),
        '\u{253A}' => (L, H, H, N),
        '\u{253B}' => (H, H, H, N),
        '\u{253C}' => (L, L, L, L),
        '\u{253D}' => (H, L, L, L),
        '\u{253E}' => (L, H, L, L),
        '\u{253F}' => (H, H, L, L),
        '\u{2540}' => (L, L, H, L),
        '\u{2541}' => (L, L, L, H),
        '\u{2542}' => (L, L, H, H),
        '\u{2543}' => (H, L, H, L),
        '\u{2544}' => (L, H, H, L),
        '\u{2545}' => (H, L, L, H),
        '\u{2546}' => (L, H, L, H),
        '\u{2547}' => (H, H, H, L),
        '\u{2548}' => (H, H, L, H),
        '\u{2549}' => (H, L, H, H),
        '\u{254A}' => (L, H, H, H),
        '\u{254B}' => (H, H, H, H),
        '\u{2550}' => (D, D, N, N),
        '\u{2551}' => (N, N, D, D),
        '\u{2552}' => (N, D, N, L),
        '\u{2553}' => (N, L, N, D),
        '\u{2554}' => (N, D, N, D),
        '\u{2555}' => (D, N, N, L),
        '\u{2556}' => (L, N, N, D),
        '\u{2557}' => (D, N, N, D),
        '\u{2558}' => (N, D, L, N),
        '\u{2559}' => (N, L, D, N),
        '\u{255A}' => (N, D, D, N),
        '\u{255B}' => (D, N, L, N),
        '\u{255C}' => (L, N, D, N),
        '\u{255D}' => (D, N, D, N),
        '\u{255E}' => (N, D, L, L),
        '\u{255F}' => (N, L, D, D),
        '\u{2560}' => (N, D, D, D),
        '\u{2561}' => (D, N, L, L),
        '\u{2562}' => (L, N, D, D),
        '\u{2563}' => (D, N, D, D),
        '\u{2564}' => (D, D, N, L),
        '\u{2565}' => (L, L, N, D),
        '\u{2566}' => (D, D, N, D),
        '\u{2567}' => (D, D, L, N),
        '\u{2568}' => (L, L, D, N),
        '\u{2569}' => (D, D, D, N),
        '\u{256A}' => (D, D, L, L),
        '\u{256B}' => (L, L, D, D),
        '\u{256C}' => (D, D, D, D),
        '\u{2574}' => (L, N, N, N),
        '\u{2575}' => (N, N, L, N),
        '\u{2576}' => (N, L, N, N),
        '\u{2577}' => (N, N, N, L),
        '\u{2578}' => (H, N, N, N),
        '\u{2579}' => (N, N, H, N),
        '\u{257A}' => (N, H, N, N),
        '\u{257B}' => (N, N, N, H),
        '\u{257C}' => (L, H, N, N),
        '\u{257D}' => (N, N, L, H),
        '\u{257E}' => (H, L, N, N),
        '\u{257F}' => (N, N, H, L),
        _ => (N, N, N, N),
    }
}

#[cfg(feature = "vector-glyphs")]
fn tessellate_powerline(mesh: &mut Mesh, ch: char, r: Rect, color: Color32, ppp: f32) {
    let mid_y = snap_px(r.center().y, ppp);
    match ch {
        '\u{E0B0}' | '\u{E0B4}' => {
            // Solid right-pointing triangle (or rounded divider)
            add_tri(
                mesh,
                r.left_top(),
                pos2(r.max.x, mid_y),
                r.left_bottom(),
                color,
            );
        }
        '\u{E0B2}' | '\u{E0B6}' => {
            // Solid left-pointing triangle
            add_tri(
                mesh,
                r.right_top(),
                r.right_bottom(),
                pos2(r.min.x, mid_y),
                color,
            );
        }
        '\u{E0B1}' | '\u{E0B5}' => {
            // Thin right-pointing chevron
            let t = snap_size_at_least_1px((r.width() * 0.14).max(1.0), ppp);
            add_quad(
                mesh,
                r.left_top(),
                pos2(r.min.x + t, r.min.y),
                pos2(r.max.x, mid_y),
                pos2(r.max.x - t, mid_y),
                color,
            );
            add_quad(
                mesh,
                pos2(r.max.x - t, mid_y),
                pos2(r.max.x, mid_y),
                pos2(r.min.x + t, r.max.y),
                r.left_bottom(),
                color,
            );
        }
        '\u{E0B3}' | '\u{E0B7}' => {
            // Thin left-pointing chevron
            let t = snap_size_at_least_1px((r.width() * 0.14).max(1.0), ppp);
            add_quad(
                mesh,
                pos2(r.max.x - t, r.min.y),
                r.right_top(),
                pos2(r.min.x + t, mid_y),
                pos2(r.min.x, mid_y),
                color,
            );
            add_quad(
                mesh,
                pos2(r.min.x, mid_y),
                pos2(r.min.x + t, mid_y),
                r.right_bottom(),
                pos2(r.max.x - t, r.max.y),
                color,
            );
        }
        '\u{E0B8}' | '\u{E0BC}' => {
            add_tri(mesh, r.left_top(), r.right_bottom(), r.left_bottom(), color);
        }
        '\u{E0BA}' | '\u{E0BE}' => {
            add_tri(
                mesh,
                r.right_top(),
                r.right_bottom(),
                r.left_bottom(),
                color,
            );
        }
        _ => {
            // Fallback subtle bar for other U+E0B0..E0D4 glyphs
            let t = snap_size_at_least_1px((r.width() * 0.2).max(1.0), ppp);
            let b = snap_rect(
                Rect::from_min_max(r.min, pos2((r.min.x + t).min(r.max.x), r.max.y)),
                ppp,
            );
            if b.is_positive() {
                mesh.add_colored_rect(b, color);
            }
        }
    }
}

#[cfg(feature = "vector-glyphs")]
fn add_tri(mesh: &mut Mesh, a: Pos2, b: Pos2, c: Pos2, color: Color32) {
    let base = mesh.vertices.len() as u32;
    mesh.colored_vertex(a, color);
    mesh.colored_vertex(b, color);
    mesh.colored_vertex(c, color);
    mesh.add_triangle(base, base + 1, base + 2);
}

#[cfg(feature = "vector-glyphs")]
fn add_quad(mesh: &mut Mesh, a: Pos2, b: Pos2, c: Pos2, d: Pos2, color: Color32) {
    let base = mesh.vertices.len() as u32;
    mesh.colored_vertex(a, color);
    mesh.colored_vertex(b, color);
    mesh.colored_vertex(c, color);
    mesh.colored_vertex(d, color);
    mesh.add_triangle(base, base + 1, base + 2);
    mesh.add_triangle(base, base + 2, base + 3);
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{CentralPanel, Context, Frame, RawInput, Shape, ViewportId};
    use egui_xterm_core::CoreConfig;

    fn make_frame_layout(
        ui: &mut Ui,
        cols: u16,
        rows: u16,
        ppp: f32,
        font_size: f32,
    ) -> FrameLayout {
        let font_id = FontId::new(font_size, FontFamily::Monospace);
        let (raw_w, raw_h) = ui
            .ctx()
            .fonts_mut(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));
        let cell_w = snap_size_at_least_1px(raw_w.max(6.0), ppp);
        let cell_h = snap_size_at_least_1px(raw_h.max(12.0), ppp);
        let term_rect = Rect::from_min_size(
            pos2(0.0, 0.0),
            vec2(f32::from(cols) * cell_w, f32::from(rows) * cell_h),
        );
        let response = ui.allocate_rect(term_rect, egui::Sense::click_and_drag());
        FrameLayout {
            response,
            term_rect,
            clip_rect: term_rect,
            cols,
            rows,
            cell_size: vec2(cell_w, cell_h),
            raw_cell_w: raw_w.max(6.0),
            effective_font_size: font_size,
            pixels_per_point: ppp,
            resized_this_frame: false,
            io_error: None,
        }
    }

    fn run_renderer_frame(
        ctx: &Context,
        renderer: &mut TerminalRenderer,
        terminal: &mut Terminal,
        config: &TerminalConfig,
        ppp: f32,
        time: f64,
        bell_style: Option<(Option<Color32>, f32)>,
    ) -> egui::FullOutput {
        let (cols, rows) = terminal.size();
        let mut raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
            time: Some(time),
            focused: true,
            ..Default::default()
        };
        raw.viewports
            .entry(ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(ppp);

        ctx.run_ui(raw, |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let layout = make_frame_layout(ui, cols, rows, ppp, config.font_size);
                #[cfg(feature = "hyperlinks")]
                let hovered: [egui_xterm_core::HyperlinkSpan; 0] = [];
                let (bell_c, bell_op) =
                    bell_style.unwrap_or((config.visual_bell_color, config.visual_bell_opacity));
                let _ = renderer.render_with_bell_style(
                    ui,
                    &layout,
                    config,
                    terminal,
                    time,
                    true,
                    &ImePreeditState::default(),
                    #[cfg(feature = "hyperlinks")]
                    &hovered,
                    bell_c,
                    bell_op,
                );
            });
        })
    }

    #[test]
    fn idle_frames_preserve_coalesced_mesh_arcs() {
        let ctx = Context::default();
        let mut term = Terminal::new(40, 8, CoreConfig::default());
        term.advance(b"\x1b[4mHello\x1b[0m World\r\n\x1b[44;33mColor\x1b[0m\r\n");
        let cfg = TerminalConfig {
            cursor_blink: false,
            ..TerminalConfig::default()
        };
        let mut renderer = TerminalRenderer::default();

        let out1 = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.0, None);
        out1.drop_without_applying_deltas();
        let bg1 = Arc::clone(&renderer.bg_mesh);
        let txt1 = Arc::clone(&renderer.text_mesh);
        let ovl1 = Arc::clone(&renderer.overlay_mesh);
        assert!(!bg1.vertices.is_empty());
        assert!(!txt1.vertices.is_empty());
        assert!(!ovl1.vertices.is_empty());

        let out2 = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.1, None);
        out2.drop_without_applying_deltas();
        assert!(Arc::ptr_eq(&bg1, &renderer.bg_mesh));
        assert!(Arc::ptr_eq(&txt1, &renderer.text_mesh));
        assert!(Arc::ptr_eq(&ovl1, &renderer.overlay_mesh));
    }

    #[test]
    fn single_dirty_row_in_place_patch_matches_full_redraw() {
        let ctx = Context::default();
        let cfg = TerminalConfig {
            cursor_blink: false,
            ..TerminalConfig::default()
        };
        let mut term = Terminal::new(20, 4, CoreConfig::default());
        term.advance(b"\x1b[41mAAAA\x1b[0m\r\n\x1b[42mBBBB\x1b[0m\r\n\x1b[43mCCCC\x1b[0m");

        let mut patched_renderer = TerminalRenderer::default();
        let out1 = run_renderer_frame(&ctx, &mut patched_renderer, &mut term, &cfg, 1.0, 0.0, None);
        out1.drop_without_applying_deltas();

        // Dirty only row 1 with identical run count (1 colored run) and same glyph count (4).
        term.advance(b"\x1b[2;1H\x1b[45;36mZZZZ\x1b[0m");
        let out2 = run_renderer_frame(&ctx, &mut patched_renderer, &mut term, &cfg, 1.0, 0.1, None);
        out2.drop_without_applying_deltas();

        // Compare against a fresh full-redraw renderer on identical final terminal state.
        let mut fresh_renderer = TerminalRenderer::default();
        let out_fresh =
            run_renderer_frame(&ctx, &mut fresh_renderer, &mut term, &cfg, 1.0, 0.1, None);
        out_fresh.drop_without_applying_deltas();

        assert_eq!(
            patched_renderer.bg_mesh.as_ref(),
            fresh_renderer.bg_mesh.as_ref()
        );
        assert_eq!(
            patched_renderer.text_mesh.as_ref(),
            fresh_renderer.text_mesh.as_ref()
        );
        assert_eq!(
            patched_renderer.overlay_mesh.as_ref(),
            fresh_renderer.overlay_mesh.as_ref()
        );
    }

    #[test]
    fn cursor_blink_decoupled_from_bg_and_overlay_meshes() {
        let ctx = Context::default();
        let cfg = TerminalConfig {
            cursor_blink: true,
            cursor_shape: CursorShape::Block,
            ..TerminalConfig::default()
        };
        let mut term = Terminal::new(20, 4, CoreConfig::default());
        term.advance(b"\x1b[4mABCD\x1b[0m\r\n");

        let mut renderer = TerminalRenderer::default();
        // t = 0.0 -> cursor ON
        let out_on = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.0, None);
        let bg_on = Arc::clone(&renderer.bg_mesh);
        let ovl_on = Arc::clone(&renderer.overlay_mesh);
        let rects_on = out_on
            .shapes
            .iter()
            .filter(|cs| {
                matches!(&cs.shape, Shape::Rect(r) if r.fill != Color32::TRANSPARENT || !r.stroke.is_empty())
            })
            .count();
        out_on.drop_without_applying_deltas();
        assert_eq!(rects_on, 1);

        // t = 0.6 -> cursor OFF
        let out_off = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.6, None);
        let rects_off = out_off
            .shapes
            .iter()
            .filter(|cs| {
                matches!(&cs.shape, Shape::Rect(r) if r.fill != Color32::TRANSPARENT || !r.stroke.is_empty())
            })
            .count();
        out_off.drop_without_applying_deltas();
        assert_eq!(rects_off, 0);
        assert!(Arc::ptr_eq(&bg_on, &renderer.bg_mesh));
        assert!(Arc::ptr_eq(&ovl_on, &renderer.overlay_mesh));
    }

    #[cfg(feature = "vector-glyphs")]
    #[test]
    fn vector_glyphs_in_ascii_row_stay_on_ascii_fast_path_and_curly_emits_path() {
        let ctx = Context::default();
        let cfg = TerminalConfig {
            cursor_blink: false,
            ..TerminalConfig::default()
        };
        let mut term = Terminal::new(40, 4, CoreConfig::default());
        term.advance("│ \x1b[32;4:3;58:2::80:220:120mHEALTHY\x1b[0m \u{E0B0} │\r\n".as_bytes());
        let mut renderer = TerminalRenderer::default();
        let out = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.0, None);
        let has_path = out
            .shapes
            .iter()
            .any(|cs| matches!(cs.shape, Shape::Path(_)));
        out.drop_without_applying_deltas();

        assert!(has_path, "SGR 4:3 curly underline must emit PathShape");
        assert!(
            renderer.cluster_cache.is_empty(),
            "ASCII + vector-glyph + SGR 58 row must stay on ASCII fast path without cluster_cache"
        );
        assert!(!renderer.text_mesh.vertices.is_empty());
        assert!(!renderer.overlay_mesh.vertices.is_empty());
        assert!(
            renderer
                .overlay_mesh
                .vertices
                .iter()
                .all(|v| v.uv == WHITE_UV)
        );
    }

    #[test]
    fn atlas_growth_and_set_fonts_renormalize_text_mesh_uvs() {
        let ctx = Context::default();
        let cfg = TerminalConfig {
            cursor_blink: false,
            ..TerminalConfig::default()
        };
        let mut term = Terminal::new(20, 3, CoreConfig::default());
        term.advance(b"HELLO_WORLD");
        let mut renderer = TerminalRenderer::default();

        let out1 = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.0, None);
        out1.drop_without_applying_deltas();
        let tex_size_1 = renderer.last_text_key.expect("text key").tex_size;

        // Grow font atlas height without resetting by rasterizing large glyphs in a sibling label.
        let mut raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
            time: Some(0.1),
            focused: true,
            ..Default::default()
        };
        raw.viewports
            .entry(ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(1.0);
        let out2 = ctx.run_ui(raw, |ui| {
            CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
                let mut extra = String::new();
                for cp in 0x00C0u32..=0x01AFu32 {
                    if let Some(ch) = char::from_u32(cp) {
                        extra.push(ch);
                    }
                }
                ui.label(egui::RichText::new(extra).size(56.0));
                let layout = make_frame_layout(ui, 20, 3, 1.0, cfg.font_size);
                #[cfg(feature = "hyperlinks")]
                let hovered: [egui_xterm_core::HyperlinkSpan; 0] = [];
                let _ = renderer.render_with_bell_style(
                    ui,
                    &layout,
                    &cfg,
                    &mut term,
                    0.1,
                    true,
                    &ImePreeditState::default(),
                    #[cfg(feature = "hyperlinks")]
                    &hovered,
                    cfg.visual_bell_color,
                    cfg.visual_bell_opacity,
                );
            });
        });
        out2.drop_without_applying_deltas();
        let tex_size_2 = renderer.last_text_key.expect("text key 2").tex_size;
        assert!(tex_size_2[1] >= tex_size_1[1]);
        for v in &renderer.text_mesh.vertices {
            assert!(v.uv.x.is_finite() && v.uv.y.is_finite());
            assert!(v.uv.x >= 0.0 && v.uv.x <= 1.0 && v.uv.y >= 0.0 && v.uv.y <= 1.0);
        }

        // Replace fonts completely via `ctx.set_fonts(...)` and verify clean re-normalization.
        let mut defs = egui::FontDefinitions::default();
        if let Some(mono) = defs.families.get(&FontFamily::Monospace).cloned() {
            defs.families
                .insert(FontFamily::Name("monospace_bold".into()), mono);
        }
        ctx.set_fonts(defs);
        let out3 = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.2, None);
        out3.drop_without_applying_deltas();
        assert!(matches!(
            renderer.bold_mode,
            Some(BoldRenderMode::FontFamily(_))
        ));
        for v in &renderer.text_mesh.vertices {
            assert!(v.uv.x >= 0.0 && v.uv.x <= 1.0 && v.uv.y >= 0.0 && v.uv.y <= 1.0);
        }
    }

    #[test]
    fn bold_faux_bold_and_custom_bold_family_and_osc5_and_osc66() {
        let ctx = Context::default();
        let cfg = TerminalConfig {
            cursor_blink: false,
            ..TerminalConfig::default()
        };

        // 1. Default font uses FauxBold (8 verts for bold 'A' vs 4 verts for normal 'A').
        let mut term_normal = Terminal::new(20, 3, CoreConfig::default());
        term_normal.advance(b"A");
        let mut r_normal = TerminalRenderer::default();
        let o1 = run_renderer_frame(&ctx, &mut r_normal, &mut term_normal, &cfg, 1.0, 0.0, None);
        o1.drop_without_applying_deltas();
        assert_eq!(r_normal.text_mesh.vertices.len(), 4);

        let mut term_bold = Terminal::new(20, 3, CoreConfig::default());
        term_bold.advance(b"\x1b[1mA\x1b[0m");
        let mut r_bold = TerminalRenderer::default();
        let o2 = run_renderer_frame(&ctx, &mut r_bold, &mut term_bold, &cfg, 1.0, 0.0, None);
        o2.drop_without_applying_deltas();
        assert!(matches!(
            r_bold.bold_mode,
            Some(BoldRenderMode::FauxBold(_))
        ));
        assert_eq!(r_bold.text_mesh.vertices.len(), 8);

        // 2. Custom `monospace_bold` family emits 4 vertices using bold_entries.
        let ctx_bold_fam = Context::default();
        let mut defs = egui::FontDefinitions::default();
        if let Some(mono) = defs.families.get(&FontFamily::Monospace).cloned() {
            defs.families
                .insert(FontFamily::Name("monospace_bold".into()), mono);
        }
        ctx_bold_fam.set_fonts(defs);
        let mut r_fam = TerminalRenderer::default();
        let o3 = run_renderer_frame(
            &ctx_bold_fam,
            &mut r_fam,
            &mut term_bold,
            &cfg,
            1.0,
            0.0,
            None,
        );
        o3.drop_without_applying_deltas();
        assert!(matches!(
            r_fam.bold_mode,
            Some(BoldRenderMode::FontFamily(_))
        ));
        assert_eq!(r_fam.text_mesh.vertices.len(), 4);

        // 3. OSC 5 special colors (bold & italic) apply to default fg cells.
        let mut term_osc5 = Terminal::new(20, 3, CoreConfig::default());
        term_osc5.advance(b"\x1b]5;0;#123456;4;#654321\x07\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m");
        let mut r_osc5 = TerminalRenderer::default();
        let o4 = run_renderer_frame(&ctx, &mut r_osc5, &mut term_osc5, &cfg, 1.0, 0.0, None);
        o4.drop_without_applying_deltas();
        let b_color = Color32::from_rgb(0x12, 0x34, 0x56);
        let i_color = Color32::from_rgb(0x65, 0x43, 0x21);
        assert!(r_osc5.text_mesh.vertices.iter().any(|v| v.color == b_color));
        assert!(r_osc5.text_mesh.vertices.iter().any(|v| v.color == i_color));

        // 4. OSC 66 text sizing skips SIZED_SPACER and scales glyph taller than 1 cell_h.
        let mut term_osc66 = Terminal::new(20, 4, CoreConfig::default());
        term_osc66.advance(b"\x1b]66;s=2;H\x07");
        let mut r_osc66 = TerminalRenderer::default();
        let o5 = run_renderer_frame(&ctx, &mut r_osc66, &mut term_osc66, &cfg, 1.0, 0.0, None);
        o5.drop_without_applying_deltas();
        assert_eq!(r_osc66.text_mesh.vertices.len(), 4);
        let bounds = r_osc66.text_mesh.calc_bounds();
        assert!(
            bounds.height() > 18.0,
            "s=2 OSC 66 glyph must scale taller than normal 1x cell height, got {}",
            bounds.height()
        );

        // 5. Visual bell style hook renders custom tint and opacity.
        r_normal.trigger_visual_bell(1.0, 200);
        let bell_rgb = Color32::from_rgb(200, 50, 90);
        let o_bell = run_renderer_frame(
            &ctx,
            &mut r_normal,
            &mut term_normal,
            &cfg,
            1.0,
            1.0,
            Some((Some(bell_rgb), 0.5)),
        );
        let bell_rect_color = o_bell.shapes.iter().find_map(|cs| match &cs.shape {
            Shape::Rect(r) if r.fill.a() > 0 => {
                let expected = Color32::from_rgba_unmultiplied(200, 50, 90, r.fill.a());
                (r.fill == expected).then_some(r.fill.a())
            }
            _ => None,
        });
        o_bell.drop_without_applying_deltas();
        let ua = bell_rect_color.expect("custom bell color rect");
        assert!((120..=135).contains(&ua));
    }

    #[test]
    fn cluster_cache_zero_alloc_standalone_and_contextual_shaping() {
        let ctx = Context::default();
        let cfg = TerminalConfig {
            cursor_blink: false,
            ..TerminalConfig::default()
        };
        let mut term = Terminal::new(30, 4, CoreConfig::default());
        term.advance("中文🚀 👩\u{200D}🔬\r\n\u{0628}\u{064A}\r\n".as_bytes());
        let mut renderer = TerminalRenderer::default();
        let out = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.0, None);
        out.drop_without_applying_deltas();
        assert!(!renderer.text_mesh.vertices.is_empty());
        assert!(
            renderer.cluster_cache.contains_key("中") || renderer.cluster_cache.contains_key("🚀")
        );
    }

    #[cfg(feature = "kitty-graphics")]
    #[test]
    fn inline_image_revision_and_kitty_u10eeee_placeholders() {
        let ctx = Context::default();
        let cfg = TerminalConfig {
            cursor_blink: false,
            ..TerminalConfig::default()
        };
        let mut term = Terminal::new(20, 6, CoreConfig::default());
        // Upload 2x2 RGBA white image i=7, virtual placement p=10, c=2, r=2, X=1, Y=2.
        term.advance(
            b"\x1b_Ga=T,i=7,p=10,U=1,X=1,Y=2,c=2,r=2,f=32,s=2,v=2;/////////////////////w==\x1b\\",
        );
        // Row 0: U+10EEEE + row0 + col0 diacritics, followed by bare U+10EEEE (inherits row=0, col=1)
        // Row 1: U+10EEEE + row1 + col0 diacritics, followed by bare U+10EEEE (inherits row=1, col=1)
        // Row 2: missing placement p=99 (safe no-op)
        term.advance(
            "\x1b[38;2;0;0;7m\x1b[58;2;0;0;10m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\r\n\
             \u{10EEEE}\u{030D}\u{0305}\u{10EEEE}\r\n\
             \x1b[58;2;0;0;99m\u{10EEEE}\u{0305}\u{0305}\x1b[0m"
                .as_bytes(),
        );

        let mut renderer = TerminalRenderer::default();
        let out1 = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.0, None);
        let tex_set_1 = out1.textures_delta.set.len();
        let img_meshes_1: Vec<Mesh> = out1
            .shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                Shape::Mesh(m) if m.texture_id != TextureId::default() => Some((**m).clone()),
                _ => None,
            })
            .collect();
        out1.drop_without_applying_deltas();

        assert!(tex_set_1 >= 1, "initial upload must emit texture delta");
        assert!(
            renderer.text_mesh.vertices.is_empty(),
            "U+10EEEE placeholders must suppress text glyph emission"
        );
        assert_eq!(
            img_meshes_1.len(),
            2,
            "2 rows of contiguous placeholders must coalesce into 2 tile meshes"
        );
        let tex_id_1 = img_meshes_1[0].texture_id;
        assert!((img_meshes_1[0].vertices[0].uv.y - 0.0).abs() < 1e-4);
        assert!((img_meshes_1[0].vertices[3].uv.y - 0.5).abs() < 1e-4);
        assert!((img_meshes_1[1].vertices[0].uv.y - 0.5).abs() < 1e-4);
        assert!((img_meshes_1[1].vertices[3].uv.y - 1.0).abs() < 1e-4);

        // Idle frame: no texture re-upload.
        let out2 = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.1, None);
        let tex_set_2 = out2.textures_delta.set.len();
        out2.drop_without_applying_deltas();
        assert_eq!(tex_set_2, 0);

        // Replace i=7 with same 2x2 dimensions & recreate p=10 virtual placement (revision bump):
        // must call TextureHandle::set on SAME TextureId.
        let rev_before = term.image_data(7).expect("img 7").revision;
        term.advance(
            b"\x1b_Ga=T,i=7,p=10,U=1,X=1,Y=2,c=2,r=2,f=32,s=2,v=2;AAAAAAAAAAAAAAAAAAAAAA==\x1b\\",
        );
        let rev_after = term.image_data(7).expect("img 7 after").revision;
        assert!(rev_after > rev_before);

        let out3 = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.2, None);
        let tex_set_3 = out3.textures_delta.set.len();
        let tex_id_3 = out3
            .shapes
            .iter()
            .find_map(|cs| match &cs.shape {
                Shape::Mesh(m) if m.texture_id != TextureId::default() => Some(m.texture_id),
                _ => None,
            })
            .expect("image mesh after revision bump");
        out3.drop_without_applying_deltas();
        assert!(tex_set_3 >= 1, "revision bump must re-upload texture delta");
        assert_eq!(
            tex_id_3, tex_id_1,
            "TextureHandle::set must reuse TextureId"
        );
    }

    #[cfg(feature = "search")]
    #[test]
    fn search_highlight_at_binary_searches_2500_matches() {
        let mut matches = Vec::with_capacity(2500);
        for i in 0..2500usize {
            matches.push(SearchMatch {
                line: i,
                start_col: 2,
                end_col: 6,
                end_line: i,
            });
        }
        let active = Some(matches[1800]);
        assert_eq!(
            search_highlight_at(&matches, active, 1800, 3),
            Some(Color32::from_rgb(255, 165, 0))
        );
        assert_eq!(
            search_highlight_at(&matches, active, 1200, 4),
            Some(Color32::from_rgb(130, 100, 30))
        );
        assert_eq!(search_highlight_at(&matches, active, 1200, 1), None);
    }

    #[cfg(feature = "sixel")]
    #[test]
    fn oversized_sixel_image_clamps_to_max_texture_side_without_panic() {
        let ctx = Context::default();
        let cfg = TerminalConfig {
            cursor_blink: false,
            ..TerminalConfig::default()
        };
        let mut term = Terminal::new(40, 6, CoreConfig::default());
        term.advance(b"\x1bPq!999999~\x1b\\");
        let placements = term.image_placements();
        assert!(!placements.is_empty());
        let stored = term.image_data(placements[0].image_id).expect("sixel img");
        assert!(stored.width > 2048);

        let mut renderer = TerminalRenderer::default();
        let out = run_renderer_frame(&ctx, &mut renderer, &mut term, &cfg, 1.0, 0.0, None);
        let max_side = ctx.input(|i| i.max_texture_side);
        let uploaded_img = out.textures_delta.set.iter().any(|(id, deltas)| {
            *id != TextureId::default()
                && deltas.iter().any(|d| {
                    let [w, h] = d.image.size();
                    w > 0 && h > 0 && w <= max_side && h <= max_side
                })
        });
        out.drop_without_applying_deltas();
        assert!(
            uploaded_img,
            "downsampled sixel texture must be uploaded safely"
        );
    }
}
