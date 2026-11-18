use crate::bg::Background;
use crate::games::{self, Game};
use crate::logo;
use crate::manager::{self, Manager, QuestTask, TaskState};
use crate::quests::{self, DiscoveredQuest, QuestOverview};
use eframe::egui::{
    self, epaint::RectShape, pos2, vec2, Align, Align2, Color32, ColorImage, CornerRadius, FontFamily,
    FontId, Id, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, TextStyle, TextureHandle,
    TextureOptions, Vec2,
};
use eframe::{egui_glow, glow};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::{atomic::{AtomicBool, Ordering}, mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

// ------------------------------------------------------------------ palette

// "Indigo" palette: periwinkle and violet on dark ink-blue, lavender highlights.
const ACCENT: Color32 = Color32::from_rgb(128, 136, 234);
/// Darker periwinkle — for fills with white text (contrast).
const ACCENT_DEEP: Color32 = Color32::from_rgb(88, 94, 204);
/// Violet — the second accent (DEMO tags, selection counter, warnings).
const HOT: Color32 = Color32::from_rgb(160, 112, 222);
/// Lavender glint — the gleam running over the logo text.
const GLINT: Color32 = Color32::from_rgb(211, 214, 246);
const TAB_PAD: f32 = 32.0;
const TAB_GAP: f32 = 4.0;

const GREEN: Color32 = Color32::from_rgb(94, 242, 181);
const DIM: Color32 = Color32::from_rgb(168, 170, 200);

fn fam(name: &str) -> FontFamily {
    FontFamily::Name(name.into())
}

fn lerp_c(a: Color32, b: Color32, k: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Cyclic gradient blurple → pink → cyan → blurple.
fn grad(t: f32) -> Color32 {
    const S: [Color32; 4] = [ACCENT, HOT, GLINT, ACCENT];
    let x = t.rem_euclid(1.0) * 3.0;
    let i = (x as usize).min(2);
    lerp_c(S[i], S[i + 1], x - i as f32)
}

fn ease_out(k: f32) -> f32 {
    1.0 - (1.0 - k.clamp(0.0, 1.0)).powi(3)
}

// ------------------------------------------------------------------ settings

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    animate: bool,
    fps: u32,
    scale: f32,
    pause_unfocused: bool,
    mouse_glow: bool,
    play_minutes: u32,
    discord_token: String,
    quest_do_desktop: bool,
    quest_do_mobile: bool,
    quest_do_game: bool,
    quest_concurrency: usize,
    quest_auto_continue_all: bool,
    quest_limit: usize,
    quest_filter_orbs: bool,
    quest_filter_cosmetics: bool,
    quest_filter_ingame: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            animate: true,
            fps: 30,
            scale: 0.5,
            pause_unfocused: true,
            mouse_glow: true,
            play_minutes: 0,
            discord_token: String::new(),
            quest_do_desktop: true,
            quest_do_mobile: true,
            quest_do_game: true,
            quest_concurrency: 2,
            quest_auto_continue_all: true,
            quest_limit: 3,
            quest_filter_orbs: true,
            quest_filter_cosmetics: true,
            quest_filter_ingame: true,
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Page {
    Games,
    Quests,
    Active,
    Settings,
}

type LoadResult = Result<Vec<Game>, String>;

pub struct App {
    mgr: Arc<Mutex<Manager>>,
    games: Arc<Vec<Game>>,
    loading: Option<mpsc::Receiver<LoadResult>>,
    status: String,
    bg: Option<Arc<Mutex<Background>>>,
    tex: TextureHandle,
    /// Top-bar pictures (prepared at build time, see build.rs).
    azov_tex: TextureHandle,
    miguel_tex: TextureHandle,
    /// Fonts awaiting vertical correction (done on the first frame).
    font_defs: Option<egui::FontDefinitions>,
    born: Instant,
    t: f32,
    last_frame: Instant,
    mouse: Pos2,
    page: Page,
    page_since: Instant,
    query: String,
    filtered: Vec<usize>,
    filter_dirty: bool,
    only_demo: bool,
    selected: HashSet<usize>,
    confirm_reset: bool,
    s: Settings,
    cancel_quests: Arc<AtomicBool>,
    /// True while a quest queue is being executed (set by `quests::run_all_quests`).
    quests_running: Arc<AtomicBool>,
    loading_quests: Option<mpsc::Receiver<Result<QuestOverview, String>>>,
    /// Account-level restriction reported by Discord (enrollment blocked etc.).
    quests_notice: Option<String>,
    /// The list was requested at least once for the current token (no auto-refetch loop).
    quests_fetched: bool,
    quests_error: Option<String>,
    available_quests: Vec<DiscoveredQuest>,
    quests_view: Option<QuestsView>,
    show_token_in_settings: bool,
    /// The token was edited; the quest list is reloaded when the field loses focus.
    token_dirty: bool,
    /// settings.json as last written — settings are saved whenever they change.
    saved_settings: String,
    last_settings_check: Instant,
    shot: Option<Shot>,
}

/// Ready-made content of the Quests tab, so the list isn't filtered and formatted
/// on every frame.
struct QuestsView {
    built: Instant,
    summary: String,
    rows: Vec<QuestRow>,
    videos: usize,
    games: usize,
    /// Not completed (upper bound of the "how many" slider).
    open: usize,
}

struct QuestRow {
    title: String,
    reward: String,
    left: String,
    enrolled: bool,
}

impl QuestsView {
    fn build(quests: &[DiscoveredQuest]) -> Self {
        let now = quests::now_unix();
        let actual: Vec<&DiscoveredQuest> = quests.iter().filter(|q| !q.completed && q.is_active()).collect();
        let expired = quests.iter().filter(|q| !q.completed && q.expired()).count();
        let is_video = |q: &DiscoveredQuest| q.is_video_desktop || q.is_video_mobile;
        let mut summary = format!("· активних: {}", actual.len());
        if expired > 0 {
            summary += &format!(", прострочених: {expired}");
        }
        Self {
            built: Instant::now(),
            summary,
            videos: actual.iter().filter(|q| is_video(q)).count(),
            games: actual.iter().filter(|q| q.is_game).count(),
            open: quests.iter().filter(|q| !q.completed).count(),
            rows: actual
                .iter()
                .map(|q| QuestRow {
                    title: format!("{} {}", if is_video(q) { "🎬" } else { "🎮" }, q.title),
                    reward: q.reward_label.clone(),
                    left: q.expires_at.map(|e| format!("ще {}", quests::fmt_left(e - now))).unwrap_or_default(),
                    enrolled: q.enrolled,
                })
                .collect(),
        }
    }
}

/// Hidden test mode, read once from the environment (see `App::debug_shot`).
struct Shot {
    path: String,
    page: Option<Page>,
    select: Option<usize>,
}

impl Shot {
    fn from_env() -> Option<Self> {
        let path = std::env::var("DU_SHOT").ok()?;
        let page = match std::env::var("DU_PAGE").as_deref() {
            Ok("quests") => Some(Page::Quests),
            Ok("active") => Some(Page::Active),
            Ok("settings") => Some(Page::Settings),
            _ => None,
        };
        let select = std::env::var("DU_SELECT").ok().and_then(|s| s.parse().ok());
        Some(Self { path, page, select })
    }
}

fn start_load_quests(token: String) -> mpsc::Receiver<Result<QuestOverview, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(quests::fetch_quests(&token));
    });
    rx
}

fn start_load(force: bool) -> mpsc::Receiver<LoadResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(games::load(force));
    });
    rx
}

// ------------------------------------------------------------------ style & fonts

/// Our fonts and the family in which each one comes first (used for measuring).
const FONT_PROBES: [(&str, &str); 5] = [
    ("segoe", ""),
    ("segoe_sb", "bold"),
    ("segoe_black", "title"),
    ("bahn", "display"),
    ("cascadia", "mono"),
];

/// Vertical font correction. egui centers widget text by the font's line box,
/// which includes the font's own ascent/descent padding — it differs between
/// Segoe, Bahnschrift and Cascadia, so text "floats" up or down.
/// We measure where the capital letters actually sit and shift the glyphs with a
/// `FontTweak` so that their middle lands exactly in the middle of the line. After
/// that, any egui widget (checkboxes, buttons, fields, sliders) is centered by itself.
fn tune_fonts(ctx: &egui::Context, mut defs: egui::FontDefinitions) {
    const PROBE: f32 = 40.0;
    for (key, family) in FONT_PROBES {
        let Some(data) = defs.font_data.get(key).cloned() else { continue };
        let family = match family {
            "" => FontFamily::Proportional,
            "mono" => FontFamily::Monospace,
            name => fam(name),
        };
        let g = ctx.fonts(|f| f.layout_no_wrap("HН0".into(), FontId::new(PROBE, family), Color32::WHITE));
        let shift = g.size().y / 2.0 - optical_center(&g); // >0 — letters too high, move them down
        let mut fd = (*data).clone();
        fd.tweak.y_offset_factor += shift / PROBE;
        defs.font_data.insert(key.into(), Arc::new(fd));
    }
    ctx.set_fonts(defs);
}

fn setup_style(ctx: &egui::Context) -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    let base = fonts.families[&FontFamily::Proportional].clone();
    let mono_base = fonts.families[&FontFamily::Monospace].clone();
    let mut have = Vec::new();
    for (key, file) in [
        ("segoe", "segoeui.ttf"),
        ("segoe_sb", "seguisb.ttf"),
        ("segoe_black", "seguibl.ttf"),
        ("bahn", "bahnschrift.ttf"),
        ("cascadia", "CascadiaMono.ttf"),
    ] {
        if let Ok(b) = std::fs::read(format!("C:\\Windows\\Fonts\\{file}")) {
            fonts.font_data.insert(key.into(), Arc::new(egui::FontData::from_owned(b)));
            have.push(key);
        }
    }
    // A family is the available fonts from the list + standard fallbacks (Cyrillic, emoji).
    let chain = |want: &[&str], tail: &[String]| -> Vec<String> {
        want.iter().filter(|k| have.contains(k)).map(|k| k.to_string()).chain(tail.iter().cloned()).collect()
    };
    fonts.families.insert(FontFamily::Proportional, chain(&["segoe"], &base));
    let mut mono_tail = mono_base.clone();
    mono_tail.extend(base.iter().cloned());
    fonts.families.insert(FontFamily::Monospace, chain(&["cascadia"], &mono_tail));
    fonts.families.insert(fam("bold"), chain(&["segoe_sb", "segoe"], &base));
    fonts.families.insert(fam("display"), chain(&["bahn", "segoe_sb"], &base));
    fonts.families.insert(fam("title"), chain(&["segoe_black", "segoe_sb"], &base));
    // Afacad (SIL OFL) is embedded into the exe — used for the logo.
    for (key, bytes) in [
        ("afacad_b", &include_bytes!("../assets/fonts/Afacad-700.ttf")[..]),
        ("afacad_sb", &include_bytes!("../assets/fonts/Afacad-600.ttf")[..]),
    ] {
        fonts.font_data.insert(key.into(), Arc::new(egui::FontData::from_static(bytes)));
    }
    let mut logo = vec!["afacad_b".to_string()];
    logo.extend(base.iter().cloned());
    fonts.families.insert(fam("logo"), logo);
    let mut logo_sb = vec!["afacad_sb".to_string()];
    logo_sb.extend(base.iter().cloned());
    fonts.families.insert(fam("logo_sb"), logo_sb);
    ctx.set_fonts(fonts.clone());

    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = Color32::from_rgba_unmultiplied(22, 20, 42, 235);
    v.extreme_bg_color = Color32::from_rgba_unmultiplied(0, 0, 0, 110);
    v.faint_bg_color = Color32::from_rgba_unmultiplied(255, 255, 255, 8);
    v.selection.bg_fill = ACCENT_DEEP;
    v.selection.stroke = Stroke::new(1.0f32, Color32::WHITE);
    v.hyperlink_color = ACCENT;
    let r = CornerRadius::same(9);
    for (w, a) in [
        (&mut v.widgets.inactive, 16u8),
        (&mut v.widgets.hovered, 34),
        (&mut v.widgets.active, 60),
        (&mut v.widgets.open, 34),
    ] {
        w.corner_radius = r;
        w.weak_bg_fill = Color32::from_rgba_unmultiplied(255, 255, 255, a);
        w.bg_fill = Color32::from_rgba_unmultiplied(255, 255, 255, a);
        // Borders are semi-transparent black, not white: calmer and doesn't draw the eye.
        w.bg_stroke = Stroke::new(1.0f32, Color32::from_black_alpha(90 + a));
    }
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0f32, Color32::from_black_alpha(80));
    v.widgets.inactive.fg_stroke = Stroke::new(1.0f32, Color32::from_rgb(232, 233, 250));
    ctx.set_visuals(v);

    ctx.style_mut(|s| {
        s.spacing.item_spacing = Vec2::new(10.0, 8.0);
        s.spacing.button_padding = Vec2::new(14.0, 7.0);
        s.spacing.interact_size.y = 30.0;
        s.text_styles.insert(TextStyle::Body, FontId::proportional(15.0));
        s.text_styles.insert(TextStyle::Button, FontId::new(15.0, fam("display")));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(12.5));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(14.0));
        s.text_styles.insert(TextStyle::Heading, FontId::new(30.0, fam("display")));
    });
    fonts
}

/// 256×1 texture of the bright gradient (for progress bars): with an offset in UV
/// coordinates it gives a "flowing" gradient on any rounded shape, at the cost of one rectangle.
fn tex_bright(ctx: &egui::Context) -> TextureHandle {
    let n = 256;
    let pixels = (0..n).map(|i| grad(i as f32 / n as f32)).collect();
    ctx.load_texture("grad", ColorImage { size: [n, 1], pixels }, TextureOptions::LINEAR_REPEAT)
}

// ------------------------------------------------------------------ widgets

fn glass() -> egui::Frame {
    egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(14, 12, 30, 150))
        .stroke(Stroke::new(1.0f32, Color32::from_black_alpha(110)))
        .corner_radius(18)
        .inner_margin(Margin::same(18))
}

/// Optical center of the first line, relative to the galley top: the midpoint between
/// the top of the letters and the baseline. Descenders (у, р, д) and the font's
/// internal ascent/descent padding don't count — that's why different fonts
/// (Bahnschrift, Segoe, emoji) end up exactly level with each other.
fn optical_center(g: &egui::Galley) -> f32 {
    // Everything is taken from the actual letter images (uv_rect), not the logical
    // baseline: FontTweak shifts only the images, and mixing the two leaks half the shift.
    let Some(row) = g.rows.first() else { return g.size().y / 2.0 };
    let mut top = f32::INFINITY;
    let mut bottoms: Vec<f32> = Vec::with_capacity(row.glyphs.len());
    for gl in &row.glyphs {
        if gl.uv_rect.size.y > 0.0 {
            let y0 = gl.pos.y + gl.uv_rect.offset.y;
            top = top.min(y0);
            bottoms.push(y0 + gl.uv_rect.size.y);
        }
    }
    if bottoms.is_empty() {
        return g.size().y / 2.0;
    }
    // Baseline = median of the letter bottoms: most letters stand on it,
    // and descenders (у, р, д) and the raised middle dot don't skew the result.
    bottoms.sort_unstable_by(f32::total_cmp);
    let base = bottoms[bottoms.len() / 2];
    (top + base) / 2.0
}

/// Like `Painter::text`, but vertically centered by the letters (only for LEFT/CENTER/RIGHT_CENTER).
fn text_c(p: &egui::Painter, pos: Pos2, align: Align2, text: impl ToString, font: FontId, color: Color32) -> Rect {
    let g = p.layout_no_wrap(text.to_string(), font, Color32::PLACEHOLDER);
    let x = match align.x() {
        Align::Min => pos.x,
        Align::Center => pos.x - g.size().x / 2.0,
        Align::Max => pos.x - g.size().x,
    };
    let origin = pos2(x, pos.y - optical_center(&g));
    let rect = Rect::from_min_size(origin, g.size());
    p.galley(origin, g, color);
    rect
}

/// Text with a "wave": each letter bobs and shimmers with the gradient, plus a soft glow.
fn wave_text(ui: &mut egui::Ui, text: &str, font: FontId, t: f32) {
    const AMP: f32 = 3.0;
    const SPACING: f32 = 2.0;
    const ALPHA: f32 = 0.55;
    let glyphs: Vec<_> = text
        .chars()
        .map(|c| ui.painter().layout_no_wrap(c.to_string(), font.clone(), Color32::PLACEHOLDER))
        .collect();
    let n = glyphs.len().max(1) as f32;
    let w: f32 = glyphs.iter().map(|g| g.size().x + SPACING).sum::<f32>() - SPACING;
    let h = glyphs.first().map_or(font.size, |g| g.size().y) + AMP * 2.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w.max(1.0), h), Sense::hover());
    let p = ui.painter();
    let mut x = rect.left();
    for (i, g) in glyphs.into_iter().enumerate() {
        let fi = i as f32;
        let dy = (t * 2.4 - fi * 0.5).sin() * AMP;
        let col = grad(fi / n * 0.5 - t * 0.07).gamma_multiply(ALPHA);
        let pos = pos2(x, rect.top() + AMP + dy);
        for o in [vec2(-1.6, 0.0), vec2(1.6, 0.0), vec2(0.0, -1.6), vec2(0.0, 1.6)] {
            p.galley(pos + o, g.clone(), col.gamma_multiply(0.16));
        }
        x += g.size().x + SPACING;
        p.galley(pos, g, col);
    }
}

/// Text with a glint: a bright gold "sunbeam" periodically runs across the letters.
/// On launch the letters drop in from above one after another (`intro` — seconds since start).
fn shine_text(ui: &mut egui::Ui, text: &str, font: FontId, t: f32, intro: f32, spacing: f32, base: Color32) {
    let glyphs: Vec<_> = text
        .chars()
        .map(|c| ui.painter().layout_no_wrap(c.to_string(), font.clone(), Color32::PLACEHOLDER))
        .collect();
    let w: f32 = glyphs.iter().map(|g| g.size().x + spacing).sum::<f32>() - spacing;
    let h = glyphs.iter().map(|g| g.size().y).fold(0.0, f32::max);
    let (rect, _) = ui.allocate_exact_size(vec2(w.max(1.0), h), Sense::hover());
    let p = ui.painter();
    // The glint runs once every ~4.5 s.
    let sweep = (t * 0.32).rem_euclid(1.45) - 0.2;
    let mut x = rect.left();
    for (i, g) in glyphs.into_iter().enumerate() {
        let k = ease_out((intro - i as f32 * 0.05) / 0.45);
        let cx = (x + g.size().x / 2.0 - rect.left()) / rect.width().max(1.0);
        let shine = (-(cx - sweep).powi(2) * 60.0).exp();
        let col = lerp_c(base, GLINT, shine * 0.9).gamma_multiply(k);
        let pos = pos2(x, rect.top() - (1.0 - k) * 10.0);
        if shine > 0.05 {
            for o in [vec2(-1.2, 0.0), vec2(1.2, 0.0), vec2(0.0, -1.2), vec2(0.0, 1.2)] {
                p.galley(pos + o, g.clone(), GLINT.gamma_multiply(0.18 * shine * k));
            }
        }
        x += g.size().x + spacing;
        p.galley(pos, g, col);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Btn {
    /// Main action: deep gradient + glow.
    Primary,
    /// Secondary: glass.
    Ghost,
    /// Dangerous action: deep raspberry.
    Danger,
}

/// One button for the whole app. No seams or two-tone halves: a single fill,
/// a thin border and text with a shadow for contrast.
fn button(ui: &mut egui::Ui, text: &str, kind: Btn, enabled: bool) -> egui::Response {
    // Plain "physical" button like in desktop apps: solid fill, thin top
    // highlight, a 1px shadow underneath, sinks by a pixel when pressed. No glow or gradients.
    let (height, pad) = match kind {
        Btn::Primary => (36.0, 18.0),
        _ => (32.0, 14.0),
    };
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::new(14.5, fam("bold")), Color32::PLACEHOLDER);
    let size = vec2(galley.size().x + pad * 2.0, height);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let h = ui.ctx().animate_bool_with_time(resp.id, enabled && resp.hovered(), 0.08);
    let pressed = enabled && resp.is_pointer_button_down_on();
    let radius = 7.0;
    let p = ui.painter();
    let center_text = |r: Rect| pos2(r.center().x - galley.size().x / 2.0, r.center().y - optical_center(&galley));

    if !enabled {
        p.rect_filled(rect, radius, Color32::from_white_alpha(6));
        p.rect_stroke(rect, radius, Stroke::new(1.0f32, Color32::from_black_alpha(80)), egui::StrokeKind::Inside);
        p.galley(center_text(rect), galley, DIM.gamma_multiply(0.5));
        return resp.on_hover_cursor(egui::CursorIcon::NotAllowed);
    }

    // Shadow under the button; when pressed the button "sits down" onto it.
    p.rect_filled(rect.translate(vec2(0.0, 1.0)), radius, Color32::from_black_alpha(110));
    let r = if pressed { rect.translate(vec2(0.0, 1.0)) } else { rect };

    let (fill, border, highlight) = match kind {
        Btn::Primary | Btn::Danger => {
            let (base, hover, down) = if kind == Btn::Primary {
                (ACCENT_DEEP, Color32::from_rgb(106, 112, 222), Color32::from_rgb(72, 76, 180))
            } else {
                (Color32::from_rgb(168, 44, 86), Color32::from_rgb(190, 56, 100), Color32::from_rgb(144, 34, 72))
            };
            let fill = if pressed { down } else { lerp_c(base, hover, h) };
            (fill, Color32::from_black_alpha(80), Color32::from_white_alpha(if pressed { 0 } else { 46 }))
        }
        Btn::Ghost => (
            Color32::from_white_alpha(if pressed { 6 } else { (10.0 + h * 8.0) as u8 }),
            Color32::from_black_alpha((90.0 + h * 40.0) as u8),
            Color32::from_white_alpha(if pressed { 0 } else { 16 }),
        ),
    };
    p.rect_filled(r, radius, fill);
    // Highlight — a single line along the inner top edge (not half the button).
    p.line_segment(
        [pos2(r.left() + radius * 0.8, r.top() + 1.0), pos2(r.right() - radius * 0.8, r.top() + 1.0)],
        Stroke::new(1.0f32, highlight),
    );
    p.rect_stroke(r, radius, Stroke::new(1.0f32, border), egui::StrokeKind::Inside);
    p.galley(center_text(r), galley, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn flow_bar(ui: &mut egui::Ui, frac: f32, tex: &TextureHandle, t: f32, h: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, h / 2.0, Color32::from_white_alpha(14));
    let w = rect.width() * frac.clamp(0.0, 1.0);
    if w > 1.0 {
        let fill = Rect::from_min_size(rect.min, vec2(w.max(h), h));
        // A 0.6-wide window of the gradient texture, sliding with time.
        let off = -t * 0.25;
        let uv = Rect::from_min_max(pos2(off, 0.0), pos2(off + 0.6, 1.0));
        p.add(RectShape::filled(fill, h / 2.0, Color32::WHITE).with_texture(tex.id(), uv));
        p.circle_filled(pos2(fill.right() - h / 2.0, fill.center().y), h * 0.9, Color32::WHITE.gamma_multiply(0.18));
    }
}

fn pulse_dot(ui: &mut egui::Ui, color: Color32, t: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
    let c = rect.center();
    let k = (t * 3.0).sin() * 0.5 + 0.5;
    ui.painter().circle_filled(c, 4.0 + k * 3.0, color.gamma_multiply(0.25 * (1.0 - k) + 0.05));
    ui.painter().circle_filled(c, 3.5, color);
}

fn fmt_secs(s: u64) -> String {
    if s >= 3600 {
        format!("{} год {:02} хв", s / 3600, s / 60 % 60)
    } else if s >= 60 {
        format!("{} хв {:02} с", s / 60, s % 60)
    } else {
        format!("{s} с")
    }
}

fn fmt_clock(s: u64) -> String {
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn minutes_picker(ui: &mut egui::Ui, v: &mut u32) {
    ui.horizontal(|ui| {
        for (m, label) in [(30, "30 хв"), (60, "1 год"), (120, "2 год"), (300, "5 год"), (0, "∞")] {
            if ui.selectable_label(*v == m, label).clicked() {
                *v = m;
            }
        }
        ui.add(egui::DragValue::new(v).range(0..=6000).suffix(" хв"));
    });
}

// ------------------------------------------------------------------ App

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let font_defs = Some(setup_style(&cc.egui_ctx));
        let saved_settings = std::fs::read_to_string(games::data_path("settings.json")).unwrap_or_default();
        let s: Settings = serde_json::from_str(&saved_settings).unwrap_or_default();
        let loading_quests = if !s.discord_token.trim().is_empty() {
            Some(start_load_quests(s.discord_token.clone()))
        } else {
            None
        };
        let mgr = Arc::new(Mutex::new(Manager::new()));
        manager::spawn_ticker(mgr.clone());
        let bg = cc.gl.as_ref().and_then(|gl| Background::new(gl)).map(|b| Arc::new(Mutex::new(b)));
        Self {
            mgr,
            games: Arc::new(Vec::new()),
            loading: Some(start_load(false)),
            status: "Завантаження списку ігор…".into(),
            bg,
            tex: tex_bright(&cc.egui_ctx),
            azov_tex: cc.egui_ctx.load_texture(
                "azovchan",
                ColorImage::from_rgba_unmultiplied([logo::PLUSH_SIZE, logo::PLUSH_SIZE], logo::AZOV),
                TextureOptions::LINEAR,
            ),
            miguel_tex: cc.egui_ctx.load_texture(
                "miguelchan",
                ColorImage::from_rgba_unmultiplied([logo::PLUSH_SIZE, logo::PLUSH_SIZE], logo::MIGUEL),
                TextureOptions::LINEAR,
            ),
            font_defs,
            born: Instant::now(),
            t: 0.0,
            last_frame: Instant::now(),
            mouse: pos2(0.0, 0.0),
            page: Page::Games,
            page_since: Instant::now(),
            query: String::new(),
            filtered: Vec::new(),
            filter_dirty: true,
            only_demo: false,
            selected: HashSet::new(),
            confirm_reset: false,
            s,
            cancel_quests: Arc::new(AtomicBool::new(false)),
            quests_running: Arc::new(AtomicBool::new(false)),
            quests_fetched: loading_quests.is_some(),
            loading_quests,
            quests_error: None,
            quests_notice: None,
            available_quests: Vec::new(),
            quests_view: None,
            show_token_in_settings: false,
            token_dirty: false,
            saved_settings,
            last_settings_check: Instant::now(),
            shot: Shot::from_env(),
        }
    }

    fn trigger_fetch_quests(&mut self) {
        self.quests_fetched = true;
        self.quests_error = None;
        if self.s.discord_token.trim().is_empty() {
            self.available_quests.clear();
        } else {
            self.loading_quests = Some(start_load_quests(self.s.discord_token.clone()));
        }
    }

    /// Writes settings.json if anything changed (checked at most every 2 s).
    fn save_settings(&mut self, force: bool) {
        if !force && self.last_settings_check.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_settings_check = Instant::now();
        if let Ok(t) = serde_json::to_string_pretty(&self.s) {
            if t != self.saved_settings && std::fs::write(games::data_path("settings.json"), &t).is_ok() {
                self.saved_settings = t;
            }
        }
    }

    /// Hidden test mode: `DU_SHOT=file.bmp` [`DU_PAGE=games|quests|active|settings`] [`DU_SELECT=n`]
    /// — after ~3 s the window saves a screenshot of itself and closes. Doesn't touch the screen or the mouse.
    fn debug_shot(&mut self, ctx: &egui::Context) {
        let Some(shot) = &self.shot else { return };
        let (path, page, select) = (shot.path.clone(), shot.page, shot.select);
        let el = ctx.input(|i| i.time);
        if self.loading.is_none() && self.selected.is_empty() {
            if let Some(n) = select {
                self.selected.extend(0..n.min(self.games.len()));
            }
        }
        if let Some(p) = page {
            self.go(p);
        }
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let (w, h) = (img.size[0], img.size[1]);
            let row = (w * 3 + 3) & !3;
            let mut bmp = Vec::with_capacity(54 + row * h);
            let le32 = |v: u32| v.to_le_bytes();
            bmp.extend_from_slice(b"BM");
            bmp.extend_from_slice(&le32((54 + row * h) as u32));
            bmp.extend_from_slice(&[0, 0, 0, 0]);
            bmp.extend_from_slice(&le32(54));
            bmp.extend_from_slice(&le32(40));
            bmp.extend_from_slice(&le32(w as u32));
            bmp.extend_from_slice(&le32(h as u32));
            bmp.extend_from_slice(&[1, 0, 24, 0]);
            bmp.extend_from_slice(&[0; 24]);
            for y in (0..h).rev() {
                for x in 0..w {
                    let c = img.pixels[y * w + x];
                    bmp.extend_from_slice(&[c.b(), c.g(), c.r()]);
                }
                bmp.resize(bmp.len() + (row - w * 3), 0);
            }
            let _ = std::fs::write(&path, bmp);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if el > 3.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        ctx.request_repaint();
    }

    fn go(&mut self, p: Page) {
        if self.page != p {
            self.page = p;
            self.page_since = Instant::now();
        }
    }

    fn poll_loading(&mut self) {
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(Ok(list)) => {
                    self.games = Arc::new(list);
                    self.status = format!("Ігор у базі: {}", self.games.len());
                    manager::lock(&self.mgr).games = self.games.clone();
                    self.selected.clear();
                    self.filter_dirty = true;
                    self.loading = None;
                }
                Ok(Err(e)) => {
                    self.status = format!("Помилка: {e}");
                    self.loading = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.loading = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        if let Some(rx) = &self.loading_quests {
            match rx.try_recv() {
                Ok(Ok(ov)) => {
                    self.quests_view = None;
                    self.available_quests = ov.quests;
                    self.quests_notice = ov.restriction;
                    self.quests_error = None;
                    self.loading_quests = None;
                }
                Ok(Err(e)) => {
                    self.quests_error = Some(e);
                    self.loading_quests = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.loading_quests = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn paint_background(&self, ctx: &egui::Context) {
        let rect = ctx.screen_rect();
        let painter = ctx.layer_painter(egui::LayerId::background());
        match &self.bg {
            Some(bg) => {
                let bg = bg.clone();
                let (t, sc) = (self.t, self.s.scale);
                let m = if self.s.mouse_glow {
                    [self.mouse.x / rect.width().max(1.0), 1.0 - self.mouse.y / rect.height().max(1.0)]
                } else {
                    [-10.0, -10.0]
                };
                painter.add(egui::PaintCallback {
                    rect,
                    callback: Arc::new(egui_glow::CallbackFn::new(move |info, p| {
                        bg.lock().unwrap_or_else(|e| e.into_inner()).paint(p.gl(), &info, t, sc, m);
                    })),
                });
            }
            None => {
                painter.rect_filled(rect, 0.0, Color32::from_rgb(16, 14, 32));
            }
        }
    }

    /// A doll from the top bar: bobs gently, on hover it grows slightly
    /// and sways; on launch it "pops" into place.
    fn plush(&self, ui: &mut egui::Ui, tex: egui::TextureId, key: &str, phase: f32) {
        let (rect, resp) = ui.allocate_exact_size(vec2(68.0, 68.0), Sense::hover());
        let t = self.t;
        let h = ui.ctx().animate_bool_with_time(Id::new(("plush", key)), resp.hovered(), 0.15);
        let pop = ease_out((self.born.elapsed().as_secs_f32() - phase * 0.15) / 0.5);
        let bob = (t * 1.8 + phase * 2.0).sin() * 1.8;
        let size = 68.0 * (0.75 + 0.25 * pop) * (1.0 + 0.07 * h);
        let r = Rect::from_center_size(rect.center() + vec2(0.0, bob - 1.0), vec2(size, size));
        let p = ui.painter();
        // A soft shadow underneath — the doll "stands" on the panel.
        p.add(egui::Shape::ellipse_filled(
            pos2(rect.center().x, rect.bottom() - 3.0),
            vec2(20.0 - bob, 3.5),
            Color32::from_black_alpha((70.0 * pop) as u8),
        ));
        let mut mesh = egui::Mesh::with_texture(tex);
        mesh.add_rect_with_uv(r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE.gamma_multiply(pop));
        mesh.rotate(egui::emath::Rot2::from_angle(h * 0.07 * (t * 7.0).sin()), r.center_bottom());
        ui.painter().add(mesh);
    }

    /// Top bar: azovchan and the name on the left, the tabs exactly in the center, miguelchan on the right.
    fn header(&mut self, ui: &mut egui::Ui, running: usize) {
        let (t, intro) = (self.t, self.born.elapsed().as_secs_f32());
        let (hr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 72.0), Sense::hover());
        let area = |r: Rect, l: Layout| egui::UiBuilder::new().max_rect(r).layout(l);

        let mut left = ui.new_child(area(hr, Layout::left_to_right(Align::Center)));
        left.spacing_mut().item_spacing.x = 10.0;
        self.plush(&mut left, self.azov_tex.id(), "azov", 0.0);
        left.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            shine_text(ui, "azov", FontId::new(25.0, fam("logo")), t, intro, 0.2, Color32::WHITE);
            shine_text(ui, "unlocker", FontId::new(25.0, fam("logo")), t, intro - 0.12, 0.2, ACCENT);
        });

        let mut right = ui.new_child(area(hr, Layout::right_to_left(Align::Center)));
        self.plush(&mut right, self.miguel_tex.id(), "miguel", 1.0);

        let tabs = Rect::from_center_size(hr.center(), vec2(self.tabs_width(ui, running), 40.0));
        let mut mid = ui.new_child(area(tabs, Layout::left_to_right(Align::Center)));
        self.tabs(&mut mid, running);
    }

    fn tab_items(running: usize) -> [(Page, String); 4] {
        [
            (Page::Games, "Ігри та демо".to_string()),
            (Page::Quests, "Квести".to_string()),
            (Page::Active, format!("Активні · {running}")),
            (Page::Settings, "Налаштування".to_string()),
        ]
    }

    fn tab_width(ui: &egui::Ui, label: &str) -> f32 {
        ui.painter().layout_no_wrap(label.to_string(), FontId::new(16.0, fam("display")), Color32::PLACEHOLDER).size().x + TAB_PAD
    }

    fn tabs_width(&self, ui: &egui::Ui, running: usize) -> f32 {
        let items = Self::tab_items(running);
        items.iter().map(|(_, l)| Self::tab_width(ui, l)).sum::<f32>() + TAB_GAP * (items.len() - 1) as f32
    }

    /// Text-only tabs. The highlight slides between them quickly (70 ms) and stretches
    /// to the width of the tab; the active one has a thin accent marker underneath.
    fn tabs(&mut self, ui: &mut egui::Ui, running: usize) {
        let items = Self::tab_items(running);
        let font = FontId::new(16.0, fam("display"));
        let tab_h = 38.0;
        ui.spacing_mut().item_spacing.x = TAB_GAP;
        let slots: Vec<_> = items
            .iter()
            .enumerate()
            .map(|(i, (_, label))| {
                let (rect, resp) = ui.allocate_exact_size(vec2(Self::tab_width(ui, label), tab_h), Sense::click());
                (rect, resp.on_hover_cursor(egui::CursorIcon::PointingHand), Id::new(("tab", i)))
            })
            .collect();
        let idx = items.iter().position(|(p, _)| *p == self.page).unwrap_or(0);
        let target = slots[idx].0;
        let x = ui.ctx().animate_value_with_time(Id::new("tab_x"), target.left(), 0.07);
        let w = ui.ctx().animate_value_with_time(Id::new("tab_w"), target.width(), 0.07);
        let pill = Rect::from_min_size(pos2(x, target.top()), vec2(w, tab_h));
        let p = ui.painter().clone();
        p.rect_filled(pill, 8.0, Color32::from_white_alpha(14));
        p.rect_stroke(pill, 8.0, Stroke::new(1.0f32, Color32::from_black_alpha(80)), egui::StrokeKind::Inside);
        p.rect_filled(Rect::from_center_size(pos2(pill.center().x, pill.bottom() - 1.5), vec2(w * 0.32, 2.0)), 1.0, ACCENT);
        for ((page, label), (rect, resp, id)) in items.into_iter().zip(slots) {
            let hv = ui.ctx().animate_bool_with_time(id, resp.hovered(), 0.08);
            let col = if self.page == page { Color32::WHITE } else { lerp_c(DIM, Color32::WHITE, hv) };
            text_c(&p, rect.center(), Align2::CENTER_CENTER, label, font.clone(), col);
            if resp.clicked() {
                self.go(page);
            }
        }
    }
    fn game_row(&mut self, ui: &mut egui::Ui, gi: usize, m: &Manager, row_h: f32) {
        let games = self.games.clone();
        let g = &games[gi];
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::hover());
        let id = Id::new(("row", gi));
        let resp = ui.interact(rect, id, Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        let h = ui.ctx().animate_bool_with_time(id, resp.hovered(), 0.12);
        if resp.clicked() && !self.selected.remove(&gi) {
            self.selected.insert(gi);
        }
        let sel = self.selected.contains(&gi);
        let p = ui.painter();
        if sel {
            p.rect_filled(rect, 10.0, ACCENT.gamma_multiply(0.22 + 0.08 * h));
            p.rect_filled(Rect::from_min_size(rect.min + vec2(0.0, 8.0), vec2(3.0, rect.height() - 16.0)), 1.5, ACCENT);
        } else if h > 0.0 {
            p.rect_filled(rect, 10.0, Color32::from_white_alpha((h * 14.0) as u8));
        }

        // Avatar: a quiet neutral circle with the first letter; ✔ in the accent color when selected.
        let c = rect.left_center() + vec2(24.0, 0.0);
        p.circle_filled(c, 12.5, if sel { ACCENT_DEEP } else { Color32::from_white_alpha(12) });
        let letter = if sel {
            "✔".to_string()
        } else {
            g.name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into())
        };
        text_c(p, c, Align2::CENTER_CENTER, letter, FontId::new(13.0, fam("display")), if sel { Color32::WHITE } else { DIM });

        let name: String = if g.name.chars().count() > 56 { g.name.chars().take(55).chain(['…']).collect() } else { g.name.clone() };
        let nr = text_c(p, pos2(rect.left() + 46.0 + h * 3.0, rect.center().y), Align2::LEFT_CENTER, name, FontId::proportional(15.0), Color32::WHITE);
        if g.demo {
            let tag = Rect::from_min_size(pos2(nr.right() + 8.0, rect.center().y - 9.0), vec2(42.0, 18.0));
            p.rect_filled(tag, 9.0, HOT.gamma_multiply(0.22));
            text_c(p, tag.center(), Align2::CENTER_CENTER, "DEMO", FontId::new(11.0, fam("display")), HOT);
        }

        let right = pos2(rect.right() - 12.0, rect.center().y);
        if m.is_running(gi) {
            let k = (self.t * 3.0).sin() * 0.5 + 0.5;
            let dot = right - vec2(68.0, 0.0);
            p.circle_filled(dot, 4.0 + k * 3.0, GREEN.gamma_multiply(0.2 * (1.0 - k) + 0.05));
            p.circle_filled(dot, 3.5, GREEN);
            text_c(p, right, Align2::RIGHT_CENTER, "у грі", FontId::new(13.0, fam("display")), GREEN);
        } else {
            let secs = m.played_secs(gi);
            if secs > 0 {
                text_c(p, right, Align2::RIGHT_CENTER, fmt_secs(secs), FontId::monospace(12.5), DIM);
            }
        }
    }

    fn page_games(&mut self, ui: &mut egui::Ui, m: &mut Manager) {
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("🔍  Пошук серед ігор…")
                    .desired_width(340.0)
                    .margin(Margin::symmetric(12, 8)),
            );
            if r.changed() {
                self.filter_dirty = true;
            }
            if ui.checkbox(&mut self.only_demo, "Лише демо").changed() {
                self.filter_dirty = true;
            }
        });
        if self.filter_dirty {
            let q = self.query.to_lowercase();
            self.filtered = (0..self.games.len())
                .filter(|&i| {
                    let g = &self.games[i];
                    (!self.only_demo || g.demo) && g.name_lc.contains(&q)
                })
                .collect();
            self.filter_dirty = false;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("знайдено").small().color(DIM));
            ui.label(RichText::new(self.filtered.len().to_string()).monospace().color(Color32::WHITE));
            ui.label(RichText::new("· вибрано").small().color(DIM));
            ui.label(RichText::new(self.selected.len().to_string()).monospace().color(if self.selected.is_empty() { DIM } else { HOT }));
        });

        let bottom_h = 104.0;
        let row_h = 38.0;
        egui::Frame::new()
            .fill(Color32::from_rgba_unmultiplied(0, 0, 0, 70))
            .corner_radius(14)
            .inner_margin(Margin::same(6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 2.0;
                // Taken out for the duration of drawing (game_row needs &mut self) — no per-frame copy.
                let rows = std::mem::take(&mut self.filtered);
                egui::ScrollArea::vertical()
                    .max_height((ui.available_height() - bottom_h).max(120.0))
                    .auto_shrink([false, false])
                    // Rows are selected by click — a slight drag shouldn't scroll the list.
                    .drag_to_scroll(false)
                    .show_rows(ui, row_h, rows.len(), |ui, range| {
                        for &gi in &rows[range] {
                            self.game_row(ui, gi, m, row_h);
                        }
                    });
                self.filtered = rows;
            });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Час").font(FontId::new(15.0, fam("display"))).color(DIM));
            minutes_picker(ui, &mut self.s.play_minutes);
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let n = self.selected.len();
            if button(ui, &format!("▶   Запустити  ·  {n}"), Btn::Primary, n > 0).clicked() {
                let dur = (self.s.play_minutes > 0).then(|| Duration::from_secs(self.s.play_minutes as u64 * 60));
                for &gi in &self.selected {
                    m.start(gi, dur);
                }
                self.selected.clear();
                self.go(Page::Active);
            }
            if n > 0 && button(ui, "Зняти вибір", Btn::Ghost, true).clicked() {
                self.selected.clear();
            }
        });
    }

    fn page_quests(&mut self, ui: &mut egui::Ui, m: &mut Manager) {
        ui.add_space(8.0);

        // Only once per token: an empty list or an error must not cause a request every frame.
        if !self.quests_fetched && !self.s.discord_token.trim().is_empty() && self.loading_quests.is_none() {
            self.trigger_fetch_quests();
        }
        let quests_running = self.quests_running.load(Ordering::Relaxed);
        // Scrolls when the quest list doesn't leave room for the run button.
        egui::ScrollArea::vertical().id_salt("quests_page").auto_shrink([false, false]).show(ui, |ui| {
            self.quests_panel(ui, m, quests_running);
        });
    }

    fn quests_panel(&mut self, ui: &mut egui::Ui, m: &mut Manager, quests_running: bool) {
        // Built once, not every frame; rebuilt every 30 s because deadlines move.
        let view = match self.quests_view.take() {
            Some(v) if v.built.elapsed() < Duration::from_secs(30) => v,
            _ => QuestsView::build(&self.available_quests),
        };
        glass().show(ui, |ui| {
            ui.set_width(ui.available_width());

            ui.horizontal(|ui| {
                ui.label(RichText::new("⚡").font(FontId::new(22.0, fam("display"))));
                ui.label(
                    RichText::new("Квести Discord")
                        .font(FontId::new(18.0, fam("display")))
                        .color(Color32::WHITE),
                );

                if self.loading_quests.is_some() {
                    ui.spinner();
                } else if let Some(e) = &self.quests_error {
                    ui.label(RichText::new(format!("· {e}")).size(13.0).color(HOT));
                    if ui.button("⟳ Повторити").clicked() {
                        self.trigger_fetch_quests();
                    }
                } else if !self.s.discord_token.trim().is_empty() {
                    ui.label(RichText::new(&view.summary).font(FontId::new(14.0, fam("display"))).color(GREEN));
                    if ui.button("⟳ Оновити").clicked() {
                        self.trigger_fetch_quests();
                    }
                } else {
                    ui.label(RichText::new("· вкажіть токен у Налаштуваннях").size(13.0).color(HOT));
                }
            });

            if let Some(r) = &self.quests_notice {
                ui.label(RichText::new(format!("⛔ {r}")).size(13.0).color(HOT));
            }

            if !view.rows.is_empty() {
                ui.add_space(10.0);
                let video_on = self.s.quest_do_desktop || self.s.quest_do_mobile;
                if view.videos > 0 && !video_on {
                    ui.label(RichText::new(format!("⚠ Відео-квестів: {}, а відео вимкнено", view.videos)).size(12.5).color(HOT));
                }
                if view.games > 0 && !self.s.quest_do_game {
                    ui.label(RichText::new(format!("⚠ Ігрових квестів: {}, а гра вимкнена", view.games)).size(12.5).color(HOT));
                }
                egui::ScrollArea::vertical().id_salt("actual_quests").max_height(112.0).show(ui, |ui| {
                    for r in &view.rows {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&r.title).size(13.5).color(Color32::WHITE));
                            ui.label(RichText::new(&r.reward).size(12.5).color(GLINT));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(&r.left).monospace().size(12.0).color(DIM));
                                if r.enrolled {
                                    ui.label(RichText::new("прийнято").size(12.0).color(GREEN));
                                }
                            });
                        });
                    }
                });
            }

            ui.add_space(14.0);
            ui.label(RichText::new("ЗАВДАННЯ").font(FontId::new(12.0, fam("display"))).color(DIM));
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.s.quest_do_desktop, "🎬 Відео (ПК)");
                ui.add_space(14.0);
                ui.checkbox(&mut self.s.quest_do_mobile, "📱 Відео (телефон)");
                ui.add_space(14.0);
                ui.checkbox(&mut self.s.quest_do_game, "🎮 Гра");
            });

            ui.add_space(14.0);
            ui.label(RichText::new("НАГОРОДА").font(FontId::new(12.0, fam("display"))).color(DIM));
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.s.quest_filter_orbs, "🔮 Orbs");
                ui.add_space(14.0);
                ui.checkbox(&mut self.s.quest_filter_cosmetics, "🎨 Косметика");
                ui.add_space(14.0);
                ui.checkbox(&mut self.s.quest_filter_ingame, "🎁 Ігрові предмети");
            });

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Одночасно").font(FontId::new(14.0, fam("display"))).color(Color32::WHITE));
                ui.add(egui::Slider::new(&mut self.s.quest_concurrency, 1..=5));
                ui.add_space(14.0);
                ui.checkbox(&mut self.s.quest_auto_continue_all, "Усі підряд");
                if !self.s.quest_auto_continue_all {
                    ui.add(egui::Slider::new(&mut self.s.quest_limit, 1..=view.open.max(10)).suffix(" шт."));
                }
            });

            ui.add_space(18.0);
            let has_tasks = self.s.quest_do_desktop || self.s.quest_do_mobile || self.s.quest_do_game;
            let has_rewards = self.s.quest_filter_orbs || self.s.quest_filter_cosmetics || self.s.quest_filter_ingame;
            let can_run = has_tasks && has_rewards && !self.s.discord_token.trim().is_empty() && !quests_running;
            let run_label = if quests_running { "⏳   Виконується…" } else { "▶   Запустити" };

            ui.horizontal(|ui| {
                if button(ui, run_label, Btn::Primary, can_run).clicked() {
                    self.cancel_quests.store(false, Ordering::Relaxed);
                    let cfg = quests::QuestRunConfig {
                        token: self.s.discord_token.clone(),
                        do_desktop_video: self.s.quest_do_desktop,
                        do_mobile_video: self.s.quest_do_mobile,
                        do_play_game: self.s.quest_do_game,
                        concurrency: self.s.quest_concurrency,
                        auto_continue_all: self.s.quest_auto_continue_all,
                        max_tasks: self.s.quest_limit,
                        filter_orbs: self.s.quest_filter_orbs,
                        filter_cosmetics: self.s.quest_filter_cosmetics,
                        filter_ingame: self.s.quest_filter_ingame,
                    };
                    quests::run_all_quests(cfg, self.mgr.clone(), self.cancel_quests.clone(), self.quests_running.clone());
                    self.go(Page::Active);
                }
                if quests_running && button(ui, "■  Зупинити", Btn::Danger, true).clicked() {
                    self.cancel_quests.store(true, Ordering::Relaxed);
                    let ids: Vec<String> = m.active_tasks.iter().map(|t| t.id.clone()).collect();
                    for id in ids {
                        m.cancel_task(&id);
                    }
                }
            });
        });
        self.quests_view = Some(view);
    }

    /// One quest card on the "Active" page. Returns true when its Stop button was pressed.
    fn task_card(&self, ui: &mut egui::Ui, task: &QuestTask) -> bool {
        let (icon, badge, color) = match task.state {
            TaskState::Queued => ("⏳", "в черзі", DIM),
            TaskState::Active => ("", "виконується", ACCENT),
            TaskState::Done => ("✔", "виконано", GREEN),
            TaskState::Skipped => ("⏭", "пропущено", DIM),
            TaskState::Stopped => ("■", "зупинено", HOT),
            TaskState::Failed => ("✖", "помилка", HOT),
        };
        let live = task.state.is_live();
        let active = task.state == TaskState::Active;
        let mut stop = false;
        egui::Frame::new()
            .fill(Color32::from_rgba_unmultiplied(10, 9, 24, if live { 170 } else { 120 }))
            .stroke(Stroke::new(1.0f32, if active { ACCENT.gamma_multiply(0.35) } else { Color32::from_black_alpha(90) }))
            .corner_radius(14)
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Finished-but-not-done cards step back a little.
                if !live && task.state != TaskState::Done {
                    ui.multiply_opacity(0.7);
                }
                ui.horizontal(|ui| {
                    if active {
                        pulse_dot(ui, GREEN, self.t);
                    } else {
                        ui.label(RichText::new(icon).font(FontId::new(16.0, fam("display"))).color(color));
                    }
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&task.title).font(FontId::new(16.0, fam("display"))).color(Color32::WHITE));
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let kind = format!("{} {}", task.kind.icon(), task.kind.label());
                            ui.label(RichText::new(kind).size(12.5).color(DIM));
                            if !task.reward.is_empty() {
                                ui.label(RichText::new("·").size(12.5).color(DIM));
                                ui.label(RichText::new(&task.reward).size(12.5).color(GLINT));
                            }
                        });
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if live && button(ui, "■ Стоп", Btn::Ghost, true).clicked() {
                            stop = true;
                        }
                        let (text, size) =
                            if active { (format!("{:.0}%", task.progress * 100.0), 18.0) } else { (badge.to_string(), 14.0) };
                        ui.label(RichText::new(text).font(FontId::new(size, fam("display"))).color(color));
                        if let Some(s) = task.started.filter(|_| active) {
                            ui.label(RichText::new(fmt_clock(s.elapsed().as_secs())).monospace().size(12.5).color(DIM));
                        }
                    });
                });
                let detail_color = if matches!(task.state, TaskState::Failed | TaskState::Stopped) { HOT } else { DIM };
                ui.label(RichText::new(&task.detail).size(13.0).color(detail_color));
                if active || task.state == TaskState::Done {
                    ui.add_space(2.0);
                    flow_bar(ui, task.progress, &self.tex, self.t, 6.0);
                }
            });
        stop
    }

    fn page_active(&mut self, ui: &mut egui::Ui, m: &mut Manager) {
        // Quest games are shown on their quest cards, not in the process list.
        let has_runs = m.manual_runs().next().is_some();
        let has_tasks = !m.active_tasks.is_empty();

        if has_runs || has_tasks {
            ui.horizontal(|ui| {
                if has_tasks && ui.button("Очистити список завдань").clicked() {
                    m.clear_completed_tasks();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(ui, "■  Зупинити всі", Btn::Danger, true).clicked() {
                        self.cancel_quests.store(true, Ordering::Relaxed);
                        m.stop_all();
                    }
                });
            });
            ui.add_space(8.0);
        }

        if !has_runs && !has_tasks {
            ui.add_space(70.0);
            ui.vertical_centered(|ui| {
                wave_text(ui, "тиша…", FontId::new(34.0, fam("title")), self.t);
                ui.add_space(6.0);
                ui.label(RichText::new("Немає активних ігор або завдань").color(DIM));
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if button(ui, "⚡  До квестів", Btn::Primary, true).clicked() {
                        self.go(Page::Quests);
                    }
                    if button(ui, "🎮  До ігор", Btn::Ghost, true).clicked() {
                        self.go(Page::Games);
                    }
                });
            });
            return;
        }

        let mut remove_task_id = None;
        let mut stop_run_idx = None;

        egui::ScrollArea::vertical().max_height(ui.available_height() - 130.0).show(ui, |ui| {
            // 1. Quests Tasks Section (with percentage, reward details and animated bar)
            if has_tasks {
                let count = |s: TaskState| m.active_tasks.iter().filter(|t| t.state == s).count();
                let summary = [
                    (count(TaskState::Active), "виконується"),
                    (count(TaskState::Queued), "в черзі"),
                    (count(TaskState::Done), "виконано"),
                    (count(TaskState::Skipped) + count(TaskState::Stopped) + count(TaskState::Failed), "не виконано"),
                ]
                .iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, l)| format!("{l}: {n}"))
                .collect::<Vec<_>>()
                .join("  ·  ");
                ui.horizontal(|ui| {
                    ui.label(RichText::new("ЗАВДАННЯ КВЕСТІВ").font(FontId::new(12.0, fam("display"))).color(HOT));
                    ui.label(RichText::new(summary).size(12.0).color(DIM));
                });
                ui.add_space(4.0);

                for task in &m.active_tasks {
                    if self.task_card(ui, task) {
                        remove_task_id = Some(task.id.clone());
                    }
                    ui.add_space(6.0);
                }
                ui.add_space(8.0);
            }

            // 2. Running Game Processes Section
            if has_runs {
                ui.label(RichText::new("АКТИВНІ ПРОЦЕСИ ІГОР").font(FontId::new(12.0, fam("display"))).color(DIM));
                ui.add_space(4.0);

                let e = self.page_since.elapsed().as_secs_f32();
                for (n, (i, r)) in m.manual_runs().enumerate() {
                    let k = ease_out((e - n as f32 * 0.06) / 0.35);
                    ui.add_space((1.0 - k) * 10.0);
                    egui::Frame::new()
                        .fill(Color32::from_rgba_unmultiplied(255, 255, 255, (10.0 * k) as u8))
                        .stroke(Stroke::new(1.0f32, Color32::from_black_alpha((80.0 * k) as u8)))
                        .corner_radius(14)
                        .inner_margin(Margin::symmetric(16, 12))
                        .show(ui, |ui| {
                            ui.multiply_opacity(k);
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                pulse_dot(ui, GREEN, self.t + i as f32);
                                ui.label(RichText::new(&r.name).font(FontId::new(17.0, fam("display"))).color(Color32::WHITE));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if button(ui, "■  Стоп", Btn::Ghost, true).clicked() {
                                        stop_run_idx = Some(i);
                                    }
                                    let el = r.started.elapsed().as_secs();
                                    if let Some(u) = r.until {
                                        let rem = u.saturating_duration_since(Instant::now()).as_secs();
                                        ui.label(RichText::new(format!("−{}", fmt_clock(rem))).monospace().color(DIM));
                                    }
                                    ui.label(RichText::new(fmt_clock(el)).font(FontId::monospace(17.0)).color(Color32::WHITE));
                                });
                            });
                            match r.until {
                                Some(u) => {
                                    let total = (u - r.started).as_secs_f32().max(1.0);
                                    flow_bar(ui, r.started.elapsed().as_secs_f32() / total, &self.tex, self.t, 6.0);
                                }
                                None => flow_bar(ui, 1.0, &self.tex, self.t, 3.0),
                            }
                        });
                    ui.add_space(4.0);
                }
            }
        });

        if let Some(id) = remove_task_id {
            // The worker sees this and stops (and stops the quest game it started).
            m.cancel_task(&id);
        }

        if let Some(i) = stop_run_idx {
            m.stop(i);
            m.save();
        }

        ui.add_space(8.0);
        ui.label(RichText::new("ЖУРНАЛ").font(FontId::new(12.0, fam("display"))).color(DIM));
        egui::ScrollArea::vertical().id_salt("log").stick_to_bottom(true).show(ui, |ui| {
            for l in &m.log {
                ui.label(RichText::new(l).monospace().size(12.0).color(DIM));
            }
        });
    }

    fn page_settings(&mut self, ui: &mut egui::Ui, m: &mut Manager) {
        ui.add_space(4.0);
        egui::Grid::new("set").num_columns(2).spacing([24.0, 14.0]).show(ui, |ui| {
            let lbl = |ui: &mut egui::Ui, s: &str| {
                ui.label(RichText::new(s).font(FontId::new(15.0, fam("display"))).color(DIM));
            };
            lbl(ui, "Токен Discord");
            ui.horizontal(|ui| {
                let te = egui::TextEdit::singleline(&mut self.s.discord_token)
                    .password(!self.show_token_in_settings)
                    .desired_width(280.0)
                    .margin(Margin::symmetric(10, 6))
                    .hint_text("Вставте ваш токен для квестів...");
                let r = ui.add(te);
                // Reload once the field loses focus, not on every typed character.
                if r.changed() {
                    self.token_dirty = true;
                }
                if self.token_dirty && !r.has_focus() {
                    self.token_dirty = false;
                    self.available_quests.clear();
                    self.trigger_fetch_quests();
                }
                let eye = if self.show_token_in_settings { "👁 Сховати" } else { "👁 Показати" };
                if ui.button(eye).clicked() {
                    self.show_token_in_settings = !self.show_token_in_settings;
                }
            });
            ui.end_row();

            lbl(ui, "Анімація");
            ui.checkbox(&mut self.s.animate, "Фон і анімація тексту");
            ui.end_row();

            lbl(ui, "Частота кадрів");
            ui.horizontal(|ui| {
                for f in [15, 30, 60, 120] {
                    ui.selectable_value(&mut self.s.fps, f, RichText::new(format!("{f}")).monospace());
                }
            });
            ui.end_row();

            lbl(ui, "Якість шейдера");
            ui.add(egui::Slider::new(&mut self.s.scale, 0.2..=1.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
            ui.end_row();

            lbl(ui, "Сяйво за мишкою");
            ui.checkbox(&mut self.s.mouse_glow, "Увімкнено");
            ui.end_row();

            lbl(ui, "У фоні");
            ui.checkbox(&mut self.s.pause_unfocused, "Пауза, коли вікно неактивне");
            ui.end_row();
        });
        if self.bg.is_none() {
            ui.label(RichText::new("Шейдери недоступні на цій відеокарті, тому використовується простий фон.").color(HOT));
        }
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            if button(ui, "⟳  Оновити базу ігор", Btn::Ghost, self.loading.is_none()).clicked() {
                self.status = "Завантажую список ігор…".into();
                self.loading = Some(start_load(true));
            }
            if button(ui, "📂  Відкрити теку", Btn::Ghost, true).clicked() {
                let _ = std::process::Command::new("explorer").arg(games::base_dir()).spawn();
            }
            if !self.confirm_reset {
                if button(ui, "Скинути статистику", Btn::Ghost, true).clicked() {
                    self.confirm_reset = true;
                }
            } else {
                if button(ui, "Точно скинути?", Btn::Danger, true).clicked() {
                    m.reset_stats();
                    self.confirm_reset = false;
                }
                if button(ui, "Скасувати", Btn::Ghost, true).clicked() {
                    self.confirm_reset = false;
                }
            }
        });
    }
}

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.035, 0.03, 0.075, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(defs) = self.font_defs.take() {
            tune_fonts(ctx, defs);
            ctx.request_repaint();
        }
        self.poll_loading();
        self.debug_shot(ctx);

        let (focused, minimized, hover) =
            ctx.input(|i| (i.focused, i.viewport().minimized.unwrap_or(false), i.pointer.hover_pos()));
        let animate = self.s.animate && !minimized && (focused || !self.s.pause_unfocused);
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        if animate {
            self.t += dt;
        }
        // The glow smoothly "catches up" with the cursor.
        let screen = ctx.screen_rect();
        let target = hover.unwrap_or(screen.center());
        if self.mouse == Pos2::ZERO {
            self.mouse = target;
        }
        self.mouse += (target - self.mouse) * (dt * 5.0).min(1.0);
        let mouse_moving = (target - self.mouse).length() > 0.5;

        self.paint_background(ctx);

        let mgr = self.mgr.clone();
        let mut m = manager::lock(&mgr);

        // Quest games are part of their quest, so they aren't counted twice.
        let running = m.manual_runs().count() + m.live_tasks();
        let busy = running > 0 || self.loading.is_some();

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(Margin::same(16)))
            .show(ctx, |ui| {
                // The whole interface is one panel: top bar with tabs, below it the page.
                let h = ui.available_height();
                glass().show(ui, |ui| {
                    ui.set_height(h - 36.0);
                    ui.set_width(ui.available_width());
                    ui.vertical(|ui| {
                        self.header(ui, running);
                        ui.add_space(4.0);
                        let (sep, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                        ui.painter().rect_filled(sep, 0.0, Color32::from_black_alpha(100));
                        ui.add_space(16.0);

                        // Content — a centered column at most 860 px wide; at the bottom, a quiet status line.
                        let avail = ui.available_rect_before_wrap();
                        let col_w = avail.width().min(860.0);
                        let footer_h = 20.0;
                        let content = Rect::from_min_size(
                            pos2(avail.center().x - col_w / 2.0, avail.top()),
                            vec2(col_w, (avail.height() - footer_h - 10.0).max(100.0)),
                        );
                        let footer = Rect::from_min_size(pos2(content.left(), avail.bottom() - footer_h), vec2(col_w, footer_h));
                        {
                            let mut cu = ui.new_child(egui::UiBuilder::new().max_rect(content).layout(Layout::top_down(Align::Min)));
                            // Page transition: fade + slight slide.
                            let k = ease_out(self.page_since.elapsed().as_secs_f32() / 0.35);
                            cu.multiply_opacity(k);
                            cu.add_space((1.0 - k) * 14.0);
                            match self.page {
                                Page::Games => self.page_games(&mut cu, &mut m),
                                Page::Quests => self.page_quests(&mut cu, &mut m),
                                Page::Active => self.page_active(&mut cu, &mut m),
                                Page::Settings => self.page_settings(&mut cu, &mut m),
                            }
                        }
                        {
                            let mut fu = ui.new_child(egui::UiBuilder::new().max_rect(footer).layout(Layout::left_to_right(Align::Center)));
                            fu.spacing_mut().item_spacing.x = 6.0;
                            if self.loading.is_some() {
                                fu.spinner();
                            }
                            fu.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(concat!("v", env!("CARGO_PKG_VERSION"))).small().color(DIM.gamma_multiply(0.6)));
                                ui.label(RichText::new(&self.status).small().color(DIM));
                            });
                        }
                        ui.allocate_rect(avail, Sense::hover());
                    });
                });
            });
        drop(m);
        self.save_settings(false);

        // Frame pacing: redraw only as often as needed.
        let intro = self.page_since.elapsed().as_secs_f32() < 2.8;
        if animate {
            ctx.request_repaint_after(Duration::from_secs_f32(1.0 / self.s.fps.max(1) as f32));
        } else if !minimized && (intro || (mouse_moving && self.s.mouse_glow)) {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else if busy && !minimized {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn on_exit(&mut self, gl: Option<&glow::Context>) {
        if let (Some(gl), Some(bg)) = (gl, &self.bg) {
            bg.lock().unwrap_or_else(|e| e.into_inner()).destroy(gl);
        }
        self.cancel_quests.store(true, Ordering::Relaxed);
        manager::lock(&self.mgr).stop_all();
        self.save_settings(true);
    }
}
