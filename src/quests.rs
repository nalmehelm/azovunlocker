//! Discord quests: discovery, relevance checks, enrollment, video and game tasks.
//!
//! Every request goes through [`call`]: it follows Discord's rate-limit headers (waits
//! *before* a bucket runs out), waits out 429s and retries, spaces enrollments apart;
//! one cached `/quests/@me` is shared by all workers and the UI.

// Closures passed to `call` return ureq's (large) error type.
#![allow(clippy::result_large_err)]

use crate::games::Game;
use crate::http;
use crate::manager::{self, LaunchInfo, Manager, Origin, QuestTask, TaskKind, TaskState};
use serde::Deserialize;
use serde_json::Value;
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const API: &str = "https://discord.com/api/v9";
const DEFAULT_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const DONE_TEXT: &str = "✔ Завершено у Discord (100%)";
/// Pause between two enrollments (all workers together): enroll has the strictest limit.
const ENROLL_GAP: f64 = 2.0;
/// How old a cached `/quests/@me` may be for status checks.
const STATUS_MAX_AGE: Duration = Duration::from_secs(25);
/// How often a game task asks Discord for progress.
const GAME_CHECK_EVERY: Duration = Duration::from_secs(30);

type Mgr = Arc<Mutex<Manager>>;

#[derive(Clone, Copy)]
pub enum RewardKind {
    Orbs,
    Cosmetic,
    InGame,
}

#[derive(Clone)]
pub struct DiscoveredQuest {
    pub id: String,
    pub title: String,
    /// Discord application id of the quest game — the same id as in the detectable list.
    pub app_id: Option<String>,
    pub game_title: Option<String>,
    pub is_video_desktop: bool,
    pub is_video_mobile: bool,
    pub is_game: bool,
    pub target_seconds: u64,
    pub reward_kind: RewardKind,
    pub reward_label: String,
    pub completed: bool,
    pub enrolled: bool,
    /// Unix seconds.
    pub starts_at: Option<i64>,
    pub expires_at: Option<i64>,
}

impl DiscoveredQuest {
    pub fn expired(&self) -> bool {
        self.expires_at.is_some_and(|e| now_unix() >= e)
    }

    /// Already started and not expired yet.
    pub fn is_active(&self) -> bool {
        !self.expired() && self.starts_at.is_none_or(|s| now_unix() >= s)
    }
}

#[derive(Clone)]
pub struct QuestRunConfig {
    pub token: String,
    pub do_desktop_video: bool,
    pub do_mobile_video: bool,
    pub do_play_game: bool,
    pub concurrency: usize,
    pub auto_continue_all: bool,
    pub max_tasks: usize,
    pub filter_orbs: bool,
    pub filter_cosmetics: bool,
    pub filter_ingame: bool,
}

#[derive(Deserialize)]
struct QuestsResponse {
    #[serde(default)]
    quests: Vec<QuestItem>,
    /// Discord temporarily refuses new enrollments for this account.
    #[serde(default)]
    quest_enrollment_blocked_until: Option<String>,
    /// Discord temporarily closed quests for this account altogether.
    #[serde(default)]
    quest_access_suspended_until: Option<String>,
}

/// Quests for the UI plus an account-level restriction message, if one is in effect.
pub struct QuestOverview {
    pub quests: Vec<DiscoveredQuest>,
    pub restriction: Option<String>,
}

#[derive(Deserialize)]
struct QuestItem {
    id: String,
    #[serde(default)]
    config: Option<QuestConfig>,
    #[serde(default)]
    user_status: Option<QuestUserStatus>,
}

#[derive(Deserialize)]
struct QuestConfig {
    #[serde(default)]
    messages: Option<QuestMessages>,
    #[serde(default)]
    rewards_config: Option<QuestRewardsConfig>,
    #[serde(default)]
    task_config_v2: Option<Value>,
    #[serde(default)]
    task_config: Option<Value>,
    /// Kept as a raw value so an unexpected shape doesn't break parsing of the whole list.
    #[serde(default)]
    application: Option<Value>,
    #[serde(default)]
    starts_at: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
}

#[derive(Deserialize)]
struct QuestRewardsConfig {
    #[serde(default)]
    rewards: Vec<QuestRewardItem>,
}

#[derive(Deserialize)]
struct QuestRewardItem {
    #[serde(rename = "type")]
    reward_type: Option<i32>,
    #[serde(default)]
    orb_quantity: Option<u32>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    messages: Option<QuestRewardMessages>,
}

#[derive(Deserialize)]
struct QuestRewardMessages {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    name_with_article: Option<String>,
}

#[derive(Deserialize)]
struct QuestMessages {
    #[serde(default)]
    quest_name: Option<String>,
    #[serde(default)]
    game_title: Option<String>,
}

#[derive(Deserialize, Clone)]
struct QuestUserStatus {
    #[serde(default)]
    enrolled_at: Option<String>,
    #[serde(default)]
    completed_at: Option<String>,
    /// The reward was already taken — the quest is done for good.
    #[serde(default)]
    claimed_at: Option<String>,
    #[serde(default)]
    stream_progress_seconds: Option<f64>,
    #[serde(default)]
    progress: Option<Value>,
}

impl QuestUserStatus {
    fn is_enrolled(&self) -> bool {
        self.enrolled_at.as_ref().is_some_and(|s| !s.trim().is_empty())
    }

    fn is_completed(&self) -> bool {
        [&self.completed_at, &self.claimed_at].iter().any(|t| t.as_ref().is_some_and(|s| !s.trim().is_empty()))
    }

    /// Highest accumulated seconds from the progress dictionary or stream_progress_seconds.
    fn get_seconds(&self) -> f64 {
        let mut max_sec = self.stream_progress_seconds.unwrap_or(0.0);
        let mut take = |n: Option<f64>| {
            if let Some(n) = n {
                max_sec = max_sec.max(n);
            }
        };
        match &self.progress {
            Some(Value::Object(obj)) => {
                for v in obj.values() {
                    take(v.as_f64().or_else(|| v.get("value").and_then(Value::as_f64)));
                }
            }
            Some(p) => take(p.as_f64()),
            None => {}
        }
        max_sec
    }

    /// Seconds Discord counted for one task (`progress.<TASK>.value`), or the best
    /// guess from the whole status when that task isn't listed.
    fn task_seconds(&self, task: &str) -> f64 {
        self.progress
            .as_ref()
            .and_then(|p| p.get(task))
            .and_then(|v| v.as_f64().or_else(|| v.get("value").and_then(Value::as_f64)))
            .unwrap_or_else(|| self.get_seconds())
    }

    /// Share of the target Discord counted, and whether the quest is done.
    fn progress(&self, target_seconds: u64) -> (f32, bool) {
        if self.is_completed() {
            return (1.0, true);
        }
        let pct = (self.get_seconds() / target_seconds.max(1) as f64).clamp(0.0, 1.0) as f32;
        (pct, pct >= 1.0)
    }
}

// ------------------------------------------------------------------ time

pub fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Days since 1970-01-01 for a civil date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// RFC 3339 timestamp ("2026-11-30T23:00:00.000+00:00" / "...Z") → unix seconds.
fn parse_ts(s: &str) -> Option<i64> {
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let rest = s.get(19..)?;
    let rest = rest.strip_prefix('.').map_or(rest, |r| r.trim_start_matches(|c: char| c.is_ascii_digit()));
    let offset = match rest {
        "" | "Z" | "z" => 0,
        r => {
            let sign = if r.starts_with('-') { -1 } else { 1 };
            let oh = r.get(1..3)?.parse::<i64>().ok()?;
            let om = r.get(4..6).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
            sign * (oh * 3600 + om * 60)
        }
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset)
}

pub fn fmt_left(secs: i64) -> String {
    match secs {
        s if s >= 86_400 => format!("{} дн", s / 86_400),
        s if s >= 3600 => format!("{} год", s / 3600),
        s => format!("{} хв", (s / 60).max(1)),
    }
}

// ------------------------------------------------------------------ rate-limited API calls

enum ApiError {
    Status(u16, String),
    Net(String),
    Stopped,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Status(c, b) => {
                let b: String = b.chars().take(160).collect();
                write!(f, "HTTP {c}: {b}")
            }
            ApiError::Net(e) => write!(f, "{e}"),
            ApiError::Stopped => write!(f, "зупинено"),
        }
    }
}

/// What a request may do while it waits: notice a stop, tell the user it's waiting.
struct Ctx<'a> {
    stopped: &'a dyn Fn() -> bool,
    on_wait: &'a dyn Fn(f64),
}

fn never() -> bool {
    false
}
fn ignore(_: f64) {}
const NO_CTX: Ctx<'static> = Ctx { stopped: &never, on_wait: &ignore };

/// Key of the "global" limit (a 429 with `global: true` blocks every route).
const GLOBAL: &str = "*";

/// Per-bucket "not before" moments.
fn gates() -> MutexGuard<'static, HashMap<String, Instant>> {
    static G: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    G.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// Pushes a bucket's "not before" moment at least `secs` into the future.
fn block(bucket: &str, secs: f64) {
    let until = Instant::now() + Duration::from_secs_f64(secs.clamp(0.0, 600.0));
    let mut g = gates();
    let e = g.entry(bucket.to_string()).or_insert(until);
    if *e < until {
        *e = until;
    }
}

/// Takes the bucket if it is free (and reserves `gap` seconds for the next caller);
/// otherwise returns how long to wait. Check and reservation happen under one lock,
/// so two workers can't slip through together.
fn try_acquire(bucket: &str, gap: f64) -> Duration {
    let now = Instant::now();
    let mut g = gates();
    let wait = [bucket, GLOBAL]
        .iter()
        .filter_map(|b| g.get(*b))
        .map(|t| t.saturating_duration_since(now))
        .max()
        .unwrap_or_default();
    if wait.is_zero() && gap > 0.0 {
        g.insert(bucket.to_string(), now + Duration::from_secs_f64(gap));
    }
    wait
}

/// Sends a request through the limiter: waits for the bucket, reads Discord's
/// X-RateLimit headers (so the next call waits before the limit is hit), and on a 429
/// waits `retry_after` and repeats (up to 5 times). Returns the response body.
fn call(
    bucket: &str,
    gap: f64,
    ctx: &Ctx,
    send: impl Fn() -> Result<ureq::Response, ureq::Error>,
) -> Result<String, ApiError> {
    for _ in 0..6 {
        loop {
            if (ctx.stopped)() {
                return Err(ApiError::Stopped);
            }
            let w = try_acquire(bucket, gap);
            if w.is_zero() {
                break;
            }
            if w > Duration::from_secs(2) {
                (ctx.on_wait)(w.as_secs_f64());
            }
            std::thread::sleep(w.min(Duration::from_secs(1)));
        }
        let (resp, code) = match send() {
            Ok(r) => (r, None),
            Err(ureq::Error::Status(c, r)) => (r, Some(c)),
            Err(e) => return Err(ApiError::Net(e.to_string())),
        };
        let header = |n: &str| resp.header(n).and_then(|v| v.trim().parse::<f64>().ok());
        // The bucket is used up: wait for its reset before the next request, not after a 429.
        if header("x-ratelimit-remaining") == Some(0.0) {
            if let Some(reset) = header("x-ratelimit-reset-after") {
                block(bucket, reset);
            }
        }
        let retry_hdr = header("retry-after");
        let body = resp.into_string().unwrap_or_default();
        match code {
            None => return Ok(body),
            Some(429) => {
                let v: Option<Value> = serde_json::from_str(&body).ok();
                let wait = v
                    .as_ref()
                    .and_then(|v| v.get("retry_after"))
                    .and_then(Value::as_f64)
                    .or(retry_hdr)
                    .unwrap_or(5.0);
                let global = v.as_ref().and_then(|v| v.get("global")).and_then(Value::as_bool) == Some(true);
                block(if global { GLOBAL } else { bucket }, wait + 0.25);
                (ctx.on_wait)(wait);
            }
            Some(c) => return Err(ApiError::Status(c, body)),
        }
    }
    Err(ApiError::Status(429, "Discord rate limit".into()))
}

/// Build number of the current Discord web client, used if discord.com can't be reached.
const FALLBACK_BUILD: u64 = 675_174;

/// Standard base64 (for the X-Super-Properties header).
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = |i: usize| c.get(i).copied().unwrap_or(0) as u32;
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Current web client build number from the public discord.com/app page
/// (`"BUILD_NUMBER":"…"`), fetched once per program run.
fn build_number() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| {
        let html = http::agent()
            .get("https://discord.com/app")
            .set("User-Agent", DEFAULT_UA)
            .timeout(Duration::from_secs(10))
            .call()
            .ok()
            .and_then(|r| r.into_string().ok())
            .unwrap_or_default();
        let digits = html.split("BUILD_NUMBER\":\"").nth(1).map(|s| s.chars().take_while(char::is_ascii_digit).collect::<String>());
        digits.and_then(|d| d.parse().ok()).unwrap_or(FALLBACK_BUILD)
    })
}

/// `X-Super-Properties`: the client description every official Discord client sends with
/// each request (the same as the web client in Chrome). Without it Discord treats the
/// program as an old client and leaves video quests out of `/quests/@me`.
fn super_properties() -> &'static str {
    static P: OnceLock<String> = OnceLock::new();
    P.get_or_init(|| {
        let props = serde_json::json!({
            "os": "Windows",
            "browser": "Chrome",
            "device": "",
            "system_locale": "uk",
            "browser_user_agent": DEFAULT_UA,
            "browser_version": "124.0.0.0",
            "os_version": "10",
            "referrer": "",
            "referring_domain": "",
            "referrer_current": "",
            "referring_domain_current": "",
            "release_channel": "stable",
            "client_build_number": build_number(),
            "client_event_source": null,
        });
        base64(props.to_string().as_bytes())
    })
}

fn with_headers(req: ureq::Request, token: &str) -> ureq::Request {
    req.set("Authorization", token)
        .set("User-Agent", DEFAULT_UA)
        .set("X-Super-Properties", super_properties())
        .set("Origin", "https://discord.com")
        .set("Referer", "https://discord.com/channels/@me")
        .set("X-Discord-Locale", "uk")
}

/// A quest as Discord reports it: parsed info plus the raw progress status.
#[derive(Clone)]
struct QuestState {
    quest: DiscoveredQuest,
    status: Option<QuestUserStatus>,
}

/// Parsed `/quests/@me`.
struct QuestList {
    states: Vec<QuestState>,
    /// Unix seconds (may already be in the past — compare with the current time).
    enroll_blocked_until: Option<i64>,
    suspended_until: Option<i64>,
}

impl QuestList {
    fn left(t: Option<i64>) -> Option<i64> {
        t.map(|t| t - now_unix()).filter(|&l| l > 0)
    }

    fn suspended(&self) -> bool {
        Self::left(self.suspended_until).is_some()
    }

    fn enroll_blocked(&self) -> bool {
        Self::left(self.enroll_blocked_until).is_some()
    }

    /// Human-readable account restriction, if one is in effect.
    fn restriction(&self) -> Option<String> {
        if let Some(l) = Self::left(self.suspended_until) {
            return Some(format!("Discord призупинив доступ до квестів ще на {}", fmt_left(l)));
        }
        Self::left(self.enroll_blocked_until)
            .map(|l| format!("Discord заблокував прийняття нових квестів ще на {}", fmt_left(l)))
    }
}

/// One parsed `/quests/@me` shared by all workers and the UI.
struct MeCache {
    token: String,
    at: Instant,
    list: Arc<QuestList>,
}

/// `/quests/@me`, taken from the shared cache when it is at most `max_age` old.
/// The list is parsed once per request, not on every lookup. The cache lock is held
/// during the request, so workers that need the list at the same moment wait for one
/// request instead of sending one each.
fn fetch_list(token: &str, max_age: Duration, ctx: &Ctx) -> Result<Arc<QuestList>, String> {
    static CACHE: OnceLock<Mutex<Option<MeCache>>> = OnceLock::new();
    let mut c = CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = c.as_ref().filter(|m| m.token == token && m.at.elapsed() <= max_age) {
        return Ok(m.list.clone());
    }
    let url = format!("{API}/quests/@me");
    let body = call("me", 0.0, ctx, || with_headers(http::agent().get(&url), token).call())
        .map_err(|e| format!("Помилка запиту: {e}"))?;
    let parsed: QuestsResponse = serde_json::from_str(&body).map_err(|e| format!("Помилка JSON: {e}"))?;
    let list = Arc::new(QuestList {
        enroll_blocked_until: parsed.quest_enrollment_blocked_until.as_deref().and_then(parse_ts),
        suspended_until: parsed.quest_access_suspended_until.as_deref().and_then(parse_ts),
        states: parsed
            .quests
            .into_iter()
            .map(|q| QuestState { status: q.user_status.clone(), quest: discover(q) })
            .collect(),
    });
    *c = Some(MeCache { token: token.to_string(), at: Instant::now(), list: list.clone() });
    Ok(list)
}

/// Current state of one quest (`None` — Discord doesn't list it any more).
fn quest_state(token: &str, quest_id: &str, max_age: Duration, ctx: &Ctx) -> Result<Option<QuestState>, String> {
    Ok(fetch_list(token, max_age, ctx)?.states.iter().find(|s| s.quest.id == quest_id).cloned())
}

/// Real progress status of one quest, if it could be fetched.
fn fetch_status(token: &str, quest_id: &str, max_age: Duration, ctx: &Ctx) -> Option<QuestUserStatus> {
    quest_state(token, quest_id, max_age, ctx).ok()??.status
}

/// Status from a response: either the status itself or `{ user_status: … }`.
fn parse_status(text: &str) -> Option<QuestUserStatus> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v.get("enrolled_at").is_some() || v.get("completed_at").is_some() || v.get("progress").is_some() {
        serde_json::from_value(v).ok()
    } else {
        serde_json::from_value(v.get("user_status")?.clone()).ok()
    }
}

// ------------------------------------------------------------------ parsing

/// Removes `pat` (ASCII, lowercase) from `s` ignoring case.
fn strip_ci(s: &str, pat: &str) -> String {
    let mut out = s.to_string();
    // to_ascii_lowercase keeps byte offsets, so the index is valid in `out`.
    while let Some(i) = out.to_ascii_lowercase().find(pat) {
        out.replace_range(i..i + pat.len(), "");
    }
    out.trim().to_string()
}

fn parse_reward(cfg: &Option<QuestConfig>) -> (RewardKind, String) {
    if let Some(c) = cfg {
        if let Some(rc) = &c.rewards_config {
            for r in &rc.rewards {
                if let Some(qty) = r.orb_quantity.filter(|&q| q > 0) {
                    return (RewardKind::Orbs, format!("🔮 {qty} Orbs"));
                }
                let raw_name = r
                    .messages
                    .as_ref()
                    .and_then(|m| m.name.clone().or_else(|| m.name_with_article.clone()))
                    .or_else(|| r.name.clone())
                    .unwrap_or_default();
                let lower = raw_name.to_lowercase();
                let or_raw = |clean: String| if clean.is_empty() { raw_name.clone() } else { clean };
                let cosmetic = || {
                    if lower.contains("decoration") {
                        let name = or_raw(strip_ci(&strip_ci(&raw_name, "avatar decoration"), "decoration"));
                        format!("🎨 Рамка аватара «{name}»")
                    } else if lower.contains("profile effect") {
                        format!("🎨 Ефект профілю «{}»", or_raw(strip_ci(&raw_name, "profile effect")))
                    } else if lower.contains("nameplate") {
                        format!("🎨 Табличка «{}»", or_raw(strip_ci(&raw_name, "nameplate")))
                    } else {
                        format!("🎨 Косметика «{raw_name}»")
                    }
                };
                let item = || format!("🎁 Предмет «{raw_name}»");
                // Discord reward types: 1 = game code, 2 = in-game item, 3 = Discord collectible
                // (avatar decoration, profile effect, nameplate), 4 = Orbs, 5 = Nitro trial.
                match r.reward_type {
                    Some(4) => return (RewardKind::Orbs, format!("🔮 {raw_name}")),
                    Some(3) => return (RewardKind::Cosmetic, cosmetic()),
                    Some(1 | 2 | 5) if !raw_name.is_empty() => return (RewardKind::InGame, item()),
                    _ => {}
                }
                // Unknown type: guess by name.
                if lower.contains("orb") {
                    return (RewardKind::Orbs, format!("🔮 {raw_name}"));
                }
                if ["avatar decoration", "profile effect", "nameplate", "collectible"].iter().any(|p| lower.contains(p)) {
                    return (RewardKind::Cosmetic, cosmetic());
                }
                if !raw_name.is_empty() {
                    return (RewardKind::InGame, item());
                }
            }
        }
        let orb_quest = c
            .messages
            .as_ref()
            .and_then(|m| m.quest_name.as_ref())
            .is_some_and(|n| n.to_lowercase().contains("orb"));
        if orb_quest {
            return (RewardKind::Orbs, "🔮 Discord Orbs".into());
        }
    }
    (RewardKind::InGame, "🎁 Внутрішньоігровий предмет".into())
}

#[derive(Default)]
struct Tasks {
    video: bool,
    mobile: bool,
    game: bool,
    /// The config had a task list at all (even with types we don't support).
    any: bool,
    target: Option<u64>,
}

/// Task types: WATCH_VIDEO, WATCH_VIDEO_ON_MOBILE, PLAY_ON_DESKTOP. STREAM_ON_DESKTOP,
/// PLAY_ACTIVITY, PLAY_ON_XBOX… can't be done by just running an exe and are skipped.
fn scan_tasks(val: &Option<Value>, t: &mut Tasks) {
    let Some(tasks) = val.as_ref().and_then(|v| v.get("tasks")).and_then(Value::as_object) else { return };
    for (k, v) in tasks {
        t.any = true;
        let ku = k.to_uppercase();
        let flag = if ku.contains("VIDEO") {
            if ku.contains("MOBILE") { &mut t.mobile } else { &mut t.video }
        } else if ku.contains("PLAY_ON_DESKTOP") {
            &mut t.game
        } else {
            continue;
        };
        *flag = true;
        if let Some(s) = v.get("target").or_else(|| v.get("seconds")).and_then(Value::as_u64) {
            t.target = Some(t.target.map_or(s, |x| x.max(s)));
        }
    }
}

fn discover(q: QuestItem) -> DiscoveredQuest {
    let messages = q.config.as_ref().and_then(|c| c.messages.as_ref());
    let game_title = messages.and_then(|m| m.game_title.clone());
    let title = messages
        .and_then(|m| m.quest_name.clone())
        .or_else(|| game_title.clone())
        .unwrap_or_else(|| format!("Квест {}", q.id));
    let app_id = q
        .config
        .as_ref()
        .and_then(|c| c.application.as_ref())
        .and_then(|a| a.get("id"))
        .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| n.to_string())));
    let ts = |f: fn(&QuestConfig) -> &Option<String>| q.config.as_ref().and_then(|c| f(c).as_deref()).and_then(parse_ts);
    let (starts_at, expires_at) = (ts(|c| &c.starts_at), ts(|c| &c.expires_at));

    let mut t = Tasks::default();
    if let Some(cfg) = &q.config {
        scan_tasks(&cfg.task_config_v2, &mut t);
        if !t.any {
            scan_tasks(&cfg.task_config, &mut t);
        }
    }
    // Old quests without a task list: a quest with a game is a "play" quest.
    if !t.any && game_title.is_some() {
        t.game = true;
    }
    let default_target = if t.game { 15 * 60 } else { 30 };
    let (reward_kind, reward_label) = parse_reward(&q.config);
    let status = q.user_status.as_ref();

    DiscoveredQuest {
        completed: status.is_some_and(|u| u.is_completed()),
        enrolled: status.is_some_and(|u| u.is_enrolled()),
        id: q.id,
        title,
        app_id,
        game_title,
        is_video_desktop: t.video,
        is_video_mobile: t.mobile,
        is_game: t.game,
        target_seconds: t.target.unwrap_or(default_target),
        reward_kind,
        reward_label,
        starts_at,
        expires_at,
    }
}

/// Fetches and parses all quests for the current account (always a fresh request).
pub fn fetch_quests(token: &str) -> Result<QuestOverview, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("Токен Discord не вказано".into());
    }
    let list = fetch_list(token, Duration::ZERO, &NO_CTX)?;
    Ok(QuestOverview {
        quests: list.states.iter().map(|s| s.quest.clone()).collect(),
        restriction: list.restriction(),
    })
}

// ------------------------------------------------------------------ relevance & enrollment

/// Is the quest still worth doing right now? `Ok(enrolled)` or `Err(reason to skip)`.
fn check_relevance(q: &DiscoveredQuest, token: &str, ctx: &Ctx) -> Result<bool, String> {
    if q.expired() {
        return Err("термін дії квесту минув".into());
    }
    let list = fetch_list(token, STATUS_MAX_AGE, ctx)?;
    if list.suspended() {
        return Err(list.restriction().unwrap_or_default());
    }
    let cur = list
        .states
        .iter()
        .find(|s| s.quest.id == q.id)
        .map(|s| s.quest.clone())
        .ok_or("квест більше не доступний у Discord")?;
    if cur.expired() {
        return Err("термін дії квесту минув".into());
    }
    if !cur.is_active() {
        return Err("квест ще не почався".into());
    }
    if cur.completed {
        return Err("вже виконано".into());
    }
    if let Some(e) = cur.expires_at {
        let left = e - now_unix();
        // Games and videos both run in real time: the whole duration must fit before the deadline.
        let need = if q.is_game { q.target_seconds.max(15 * 60) } else { q.target_seconds };
        if left < need as i64 {
            return Err(format!("до кінця квесту {} — не встигнемо виконати", fmt_left(left)));
        }
    }
    // Enrolling now would only fail (and may extend the block) — skip without a request.
    if !cur.enrolled && list.enroll_blocked() {
        return Err(list.restriction().unwrap_or_default());
    }
    Ok(cur.enrolled)
}

/// Enrolls the quest and makes sure Discord really recorded it (`enrolled_at`).
/// Enrollments of all workers go one by one, `ENROLL_GAP` seconds apart.
fn ensure_enrolled(q: &DiscoveredQuest, token: &str, ctx: &Ctx) -> Result<(), String> {
    let url = format!("{API}/quests/{}/enroll", q.id);
    let mut last_err = None;
    for loc in [1, 2] {
        let payload = serde_json::json!({ "location": loc }).to_string();
        let res = call("enroll", ENROLL_GAP, ctx, || {
            with_headers(http::agent().post(&url), token)
                .set("Content-Type", "application/json")
                .send_string(&payload)
        });
        match res {
            Ok(body) => {
                if parse_status(&body).is_some_and(|s| s.is_enrolled()) {
                    return Ok(());
                }
                last_err = None;
                break; // accepted, but the body didn't say so — verify below
            }
            Err(ApiError::Status(_, b)) if b.contains("ALREADY_ENROLLED") || b.contains("already enrolled") => {
                last_err = None;
                break;
            }
            Err(ApiError::Stopped) => return Err("зупинено".into()),
            // Rejected with location 1 — try location 2.
            Err(e) => last_err = Some(e.to_string()),
        }
    }
    if let Some(e) = last_err {
        return Err(format!("Discord відхилив прийняття ({e})"));
    }
    // Ask Discord directly (fresh, not cached) whether the quest is now taken.
    match quest_state(token, &q.id, Duration::ZERO, ctx)? {
        Some(cur) if cur.quest.enrolled => Ok(()),
        Some(_) => Err("Discord не підтвердив прийняття завдання".into()),
        None => Err("квест більше не доступний у Discord".into()),
    }
}

// ------------------------------------------------------------------ queue

/// Resets the "running" flag when the queue thread ends, however it ends.
struct Busy(Arc<AtomicBool>);
impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Which task of the quest will be done with these settings. Doing one task completes
/// the quest, so one is enough: desktop video, then mobile video, then the game.
fn pick_kind(q: &DiscoveredQuest, cfg: &QuestRunConfig) -> Option<TaskKind> {
    if cfg.do_desktop_video && q.is_video_desktop {
        Some(TaskKind::Video)
    } else if cfg.do_mobile_video && q.is_video_mobile {
        Some(TaskKind::MobileVideo)
    } else if cfg.do_play_game && q.is_game {
        Some(TaskKind::Game)
    } else {
        None
    }
}

/// Runs all requested quest automations in the background, updating Manager tasks and log.
/// `running` stays true while the queue works; a second call during that time does nothing.
pub fn run_all_quests(config: QuestRunConfig, mgr: Mgr, cancel: Arc<AtomicBool>, running: Arc<AtomicBool>) {
    if running.swap(true, Ordering::Relaxed) {
        manager::lock(&mgr).msg("ℹ Черга квестів вже виконується");
        return;
    }
    let busy = Busy(running);
    std::thread::spawn(move || {
        let _busy = busy;
        let token = config.token.trim().to_string();
        if token.is_empty() {
            manager::lock(&mgr).msg("⚠ Вкажіть токен Discord у Налаштуваннях");
            return;
        }
        manager::lock(&mgr).reset_cancelled();

        // The UI usually loaded the list a moment ago — no need to ask Discord again.
        let list = match fetch_list(&token, Duration::from_secs(5), &NO_CTX) {
            Ok(l) => l,
            Err(e) => {
                manager::lock(&mgr).msg(format!("❌ Не вдалося отримати квести: {e}"));
                return;
            }
        };
        if let Some(r) = list.restriction() {
            manager::lock(&mgr).msg(format!("⛔ {r}"));
            if list.suspended() {
                return;
            }
        }
        let enroll_blocked = list.enroll_blocked();
        let active = list.states.iter().filter(|s| s.quest.is_active() && !s.quest.completed);
        let (n_active, n_video) = active.fold((0, 0), |(a, v), s| {
            (a + 1, v + usize::from(s.quest.is_video_desktop || s.quest.is_video_mobile))
        });
        manager::lock(&mgr).msg(format!("📋 Квестів: {n_active} активних, відео: {n_video}"));

        let (mut eligible, inactive): (Vec<_>, Vec<_>) = list
            .states
            .iter()
            .map(|s| s.quest.clone())
            .filter(|q| !q.completed)
            // While enrolling is blocked only already accepted quests can be done.
            .filter(|q| q.enrolled || !enroll_blocked)
            .filter(|q| match q.reward_kind {
                RewardKind::Orbs => config.filter_orbs,
                RewardKind::Cosmetic => config.filter_cosmetics,
                RewardKind::InGame => config.filter_ingame,
            })
            .filter(|q| pick_kind(q, &config).is_some())
            .partition(|q| q.is_active());
        if !inactive.is_empty() {
            manager::lock(&mgr).msg(format!("⏭ Неактуальних пропущено: {}", inactive.len()));
        }
        if eligible.is_empty() {
            manager::lock(&mgr).msg("ℹ Немає квестів під обрані фільтри");
            return;
        }
        // The ones that end soonest go first; without a deadline — last.
        eligible.sort_by_key(|q| q.expires_at.unwrap_or(i64::MAX));
        if !config.auto_continue_all && config.max_tasks > 0 {
            eligible.truncate(config.max_tasks);
        }
        let mut jobs: Vec<(DiscoveredQuest, TaskKind)> =
            eligible.into_iter().filter_map(|q| pick_kind(&q, &config).map(|k| (q, k))).collect();

        let num_workers = config.concurrency.clamp(1, 5);
        {
            // Every quest gets its card right away, so the whole queue is visible.
            let mut m = manager::lock(&mgr);
            m.msg(format!("🚀 Завдань: {} · одночасно: {num_workers}", jobs.len()));
            for (q, kind) in &jobs {
                m.update_task(QuestTask {
                    id: q.id.clone(),
                    title: q.title.clone(),
                    reward: q.reward_label.clone(),
                    kind: *kind,
                    state: TaskState::Queued,
                    progress: 0.0,
                    detail: "У черзі".into(),
                    started: None,
                });
            }
        }

        jobs.reverse(); // so pop() takes the first items in order
        let queue = Arc::new(Mutex::new(jobs));
        let handles: Vec<_> = (0..num_workers)
            .map(|_| {
                let (queue, mgr, cancel, token) = (queue.clone(), mgr.clone(), cancel.clone(), token.clone());
                std::thread::spawn(move || {
                    while !cancel.load(Ordering::Relaxed) {
                        let next = queue.lock().unwrap_or_else(|e| e.into_inner()).pop();
                        let Some((q, kind)) = next else { break };
                        execute_quest(&q, kind, &token, &mgr, &cancel);
                    }
                })
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }

        let msg = if cancel.load(Ordering::Relaxed) { "■ Чергу зупинено" } else { "✨ Чергу виконано" };
        manager::lock(&mgr).msg(msg);
    });
}

fn execute_quest(q: &DiscoveredQuest, kind: TaskKind, token: &str, mgr: &Mgr, cancel: &AtomicBool) {
    let card = TaskCard::new(mgr, cancel, q, kind);
    if card.stopped() {
        return; // stopped while it was still in the queue
    }
    card.set(0.0, "Перевірка актуальності…");
    let enrolled = match card.with_ctx(|ctx| check_relevance(q, token, ctx)) {
        Ok(e) => e,
        Err(reason) => {
            if !card.stopped() {
                card.skip(format!("Пропущено: {reason}"));
                manager::lock(mgr).msg(format!("⏭ «{}»: {reason}", q.title));
            }
            return;
        }
    };
    if !enrolled {
        card.set(0.0, "Прийняття завдання…");
        if let Err(e) = card.with_ctx(|ctx| ensure_enrolled(q, token, ctx)) {
            if !card.stopped() {
                card.fail(format!("Не прийнято: {e}"));
                manager::lock(mgr).msg(format!("⚠ «{}»: {e}", q.title));
            }
            return;
        }
        manager::lock(mgr).msg(format!("✔ Прийнято: «{}»", q.title));
    }
    match kind {
        TaskKind::Game => run_game_step(q, &card, token, mgr),
        TaskKind::Video => run_video_step(q, false, &card, token, mgr),
        TaskKind::MobileVideo => run_video_step(q, true, &card, token, mgr),
    }
}

/// The quest's card on the "Active" page; writes go through [`Manager::update_task`]
/// (which ignores them once the user stopped the task).
struct TaskCard<'a> {
    mgr: &'a Mgr,
    cancel: &'a AtomicBool,
    id: String,
    title: String,
    reward: String,
    kind: TaskKind,
    progress: Cell<f32>,
    started: Cell<Option<Instant>>,
}

impl<'a> TaskCard<'a> {
    fn new(mgr: &'a Mgr, cancel: &'a AtomicBool, q: &DiscoveredQuest, kind: TaskKind) -> Self {
        Self {
            mgr,
            cancel,
            id: q.id.clone(),
            title: q.title.clone(),
            reward: q.reward_label.clone(),
            kind,
            progress: Cell::new(0.0),
            started: Cell::new(None),
        }
    }

    fn put(&self, state: TaskState, progress: f32, detail: impl Into<String>) {
        self.progress.set(progress);
        manager::lock(self.mgr).update_task(QuestTask {
            id: self.id.clone(),
            title: self.title.clone(),
            reward: self.reward.clone(),
            kind: self.kind,
            state,
            progress,
            detail: detail.into(),
            started: self.started.get(),
        });
    }

    /// The task is being worked on.
    fn set(&self, progress: f32, detail: impl Into<String>) {
        if self.started.get().is_none() {
            self.started.set(Some(Instant::now()));
        }
        self.put(TaskState::Active, progress, detail);
    }

    /// Discord confirmed the quest.
    fn done(&self) {
        self.put(TaskState::Done, 1.0, DONE_TEXT);
        manager::lock(self.mgr).msg(format!("✅ Завершено: «{}» [{}]", self.title, self.reward));
    }

    /// Ends with an error, keeping the progress it reached.
    fn fail(&self, detail: impl Into<String>) {
        self.put(TaskState::Failed, self.progress.get(), detail);
    }

    /// Not worth doing (expired, already done…).
    fn skip(&self, detail: impl Into<String>) {
        self.put(TaskState::Skipped, self.progress.get(), detail);
    }

    /// Stopped globally ("Stop all") or with this card's own Stop button.
    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || manager::lock(self.mgr).is_task_cancelled(&self.id)
    }

    /// Runs `f` with a request context bound to this card: a stop interrupts waiting,
    /// and rate-limit pauses are shown on the card.
    fn with_ctx<R>(&self, f: impl FnOnce(&Ctx) -> R) -> R {
        let stopped = || self.stopped();
        let on_wait = |secs: f64| self.set(self.progress.get(), format!("Ліміт Discord, очікування {secs:.0} с…"));
        f(&Ctx { stopped: &stopped, on_wait: &on_wait })
    }

    /// Sleeps in one-second steps; returns true if the task was stopped meanwhile.
    fn sleep(&self, secs: f64) -> bool {
        let end = Instant::now() + Duration::from_secs_f64(secs.max(0.0));
        while Instant::now() < end {
            if self.stopped() {
                return true;
            }
            std::thread::sleep(end.saturating_duration_since(Instant::now()).min(Duration::from_secs(1)));
        }
        self.stopped()
    }
}

/// How far ahead of the real time since enrollment Discord accepts a video timestamp.
const VIDEO_AHEAD: u64 = 10;
/// Seconds of video reported per request.
const VIDEO_STEP: u64 = 7;
/// Requests in a row that Discord didn't count before the task gives up.
const VIDEO_MAX_STALLS: u32 = 20;

/// The next video timestamp to report, or `None` while the clock doesn't allow it yet.
/// Discord counts video progress only up to the time passed since enrollment
/// (plus a few seconds), so a video can't be "watched" faster than it plays.
fn next_video_timestamp(done: u64, target: u64, since_enroll: u64) -> Option<u64> {
    let next = (done + VIDEO_STEP).min(target);
    (next <= since_enroll + VIDEO_AHEAD).then_some(next)
}

/// Video quest (WATCH_VIDEO / WATCH_VIDEO_ON_MOBILE). The video "plays" in real time from
/// the moment of enrollment, continuing from the progress Discord already has. The quest
/// counts as done only when Discord confirms it.
fn run_video_step(q: &DiscoveredQuest, mobile: bool, card: &TaskCard, token: &str, mgr: &Mgr) {
    let task = if mobile { "WATCH_VIDEO_ON_MOBILE" } else { "WATCH_VIDEO" };
    let target = q.target_seconds.max(1);
    let pct = |s: u64| (s as f32 / target as f32).min(1.0);
    let watching = |done: u64| format!("Перегляд: {done} / {target} с ({:.0}%)", pct(done) * 100.0);
    card.set(0.0, "Синхронізація з Discord…");
    manager::lock(mgr).msg(format!("🎬 «{}»", q.title));

    // Where Discord thinks we are: when the quest was accepted and how much is already
    // counted (the cache is fresh enough: the relevance check has just loaded it).
    let status = match card.with_ctx(|ctx| quest_state(token, &q.id, STATUS_MAX_AGE, ctx)) {
        Ok(Some(s)) => s.status,
        Ok(None) => return card.fail("Квест більше не доступний у Discord"),
        Err(e) => return card.fail(format!("Не вдалося отримати статус: {e}")),
    };
    if status.as_ref().is_some_and(|s| s.is_completed()) {
        return card.done();
    }
    let enrolled_at = status.as_ref().and_then(|s| s.enrolled_at.as_deref()).and_then(parse_ts).unwrap_or_else(now_unix);
    let mut done = status.as_ref().map_or(0, |s| s.task_seconds(task) as u64);

    let url = format!("{API}/quests/{}/video-progress", q.id);
    let bucket = format!("video:{}", q.id);
    let (mut failures, mut stalls) = (0, 0);
    loop {
        if card.stopped() {
            return;
        }
        if q.expired() {
            return card.fail("Термін дії квесту минув");
        }
        let since_enroll = (now_unix() - enrolled_at).max(0) as u64;
        let Some(next) = next_video_timestamp(done, target, since_enroll) else {
            // Ahead of real time — Discord wouldn't count it; wait for the clock.
            card.set(pct(done), watching(done));
            if card.sleep(1.0) {
                return;
            }
            continue;
        };

        let payload = format!(r#"{{"timestamp":{next}}}"#);
        let res = card.with_ctx(|ctx| {
            call(&bucket, 0.0, ctx, || {
                with_headers(http::agent().post(&url), token).set("Content-Type", "application/json").send_string(&payload)
            })
        });
        match res {
            Ok(body) => {
                failures = 0;
                let st = parse_status(&body);
                if st.as_ref().is_some_and(|s| s.is_completed()) {
                    return card.done();
                }
                // Trust Discord's own number when it sends one.
                let counted = st.map_or(next, |s| s.task_seconds(task) as u64);
                if counted > done {
                    done = counted;
                    stalls = 0;
                } else {
                    stalls += 1;
                    if stalls >= VIDEO_MAX_STALLS {
                        return card.fail("Discord не зараховує перегляд");
                    }
                }
            }
            Err(ApiError::Stopped) => return,
            // 4xx (except 429, handled in `call`) won't fix itself on retry.
            Err(e @ ApiError::Status(400..=499, _)) => return card.fail(format!("Помилка Discord API: {e}")),
            Err(e) => {
                failures += 1;
                if failures >= 3 {
                    return card.fail(format!("Помилка Discord API: {e}"));
                }
                card.set(card.progress.get(), format!("Помилка, повтор {failures}/3: {e}"));
                if card.sleep(3.0) {
                    return;
                }
                continue;
            }
        }

        if done >= target {
            // Everything is sent: wait for Discord to mark the quest completed.
            for _ in 0..4 {
                card.set(1.0, "Перегляд завершено, очікування підтвердження Discord…");
                if card.with_ctx(|ctx| fetch_status(token, &q.id, Duration::ZERO, ctx)).is_some_and(|s| s.is_completed()) {
                    return card.done();
                }
                if card.sleep(5.0) {
                    return;
                }
            }
            return card.fail("Перегляд надіслано, але Discord не підтвердив виконання");
        }
        card.set(pct(done), watching(done));
        if card.sleep(1.0) {
            return;
        }
    }
}

/// Finds the quest game in the detectable list: by application id, then by exact
/// name, then by the longest game name contained in the quest/game title.
fn find_game(games: &[Game], q: &DiscoveredQuest) -> Option<usize> {
    if let Some(id) = &q.app_id {
        if let Some(i) = games.iter().position(|g| &g.id == id) {
            return Some(i);
        }
    }
    let names: Vec<String> =
        [q.game_title.as_deref(), Some(q.title.as_str())].into_iter().flatten().map(str::to_lowercase).collect();
    if let Some(i) = games.iter().position(|g| names.contains(&g.name_lc)) {
        return Some(i);
    }
    // Very short names ("Go", "Ace") would match almost any title.
    let long_enough = |s: &str| s.chars().count() >= 4;
    let contained = games
        .iter()
        .enumerate()
        .filter(|(_, g)| long_enough(&g.name_lc) && names.iter().any(|n| n.contains(&g.name_lc)))
        .max_by_key(|(_, g)| g.name_lc.len())
        .map(|(i, _)| i);
    if contained.is_some() {
        return contained;
    }
    // The list name may be longer than the quest's ("Game (Steam)"): take the closest one.
    let gt = q.game_title.as_ref().map(|s| s.to_lowercase()).filter(|s| long_enough(s))?;
    games
        .iter()
        .enumerate()
        .filter(|(_, g)| g.name_lc.contains(&gt))
        .min_by_key(|(_, g)| g.name_lc.len())
        .map(|(i, _)| i)
}

/// A quest's claim on a running game; released (and the game closed if it was the
/// last claim) however the game task ends.
struct GameHold<'a> {
    mgr: &'a Mgr,
    id: &'a str,
}

impl Drop for GameHold<'_> {
    fn drop(&mut self) {
        manager::lock(self.mgr).release_game(self.id);
    }
}

fn run_game_step(q: &DiscoveredQuest, card: &TaskCard, token: &str, mgr: &Mgr) {
    let found = {
        let m = manager::lock(mgr);
        find_game(&m.games, q).map(|gi| LaunchInfo::of(&m.games[gi]))
    };
    let Some(info) = found else {
        return card.fail("Гру не знайдено в базі ігор");
    };
    let (game_id, game_name) = (info.id.clone(), info.name.clone());
    let target = q.target_seconds.max(15 * 60);
    let limit = target + 300;
    let minutes = target as f32 / 60.0;

    // Several quests may share one game: it is closed when the last of them lets it go
    // (and never when the user started it by hand).
    let already_running = {
        let mut m = manager::lock(mgr);
        m.hold_game(&game_id);
        m.is_running_id(&game_id)
    };
    let _hold = GameHold { mgr, id: &game_id };
    if !already_running {
        card.set(0.0, format!("Запуск «{game_name}»…"));
        let prepared = manager::prepare(&info); // disk / pipe work without the lock
        let dur = Duration::from_secs(limit + 30);
        if !manager::lock(mgr).launch(info, prepared, Some(dur), Origin::Quest) {
            return card.fail(format!("Не вдалося запустити «{game_name}»"));
        }
    }
    card.set(0.0, format!("«{game_name}» · синхронізація з Discord…"));
    manager::lock(mgr).msg(format!("🎮 «{game_name}» → «{}»", q.title));

    let started = Instant::now();
    let mut last_check = Instant::now();
    let mut real_pct = 0.0f32;
    let mut done = false;
    while started.elapsed().as_secs() < limit {
        if card.stopped() {
            break;
        }
        if q.expired() {
            return card.fail("Термін дії квесту минув");
        }
        if !manager::lock(mgr).is_running_id(&game_id) {
            return card.fail(format!("«{game_name}» закрито до завершення квесту"));
        }
        // Real progress from Discord (the @me cache is shared with other workers).
        if last_check.elapsed() >= GAME_CHECK_EVERY {
            last_check = Instant::now();
            match card.with_ctx(|ctx| quest_state(token, &q.id, STATUS_MAX_AGE, ctx)) {
                Ok(None) => return card.fail("Квест зник з Discord (закінчився або знятий)"),
                Ok(Some(cur)) if !cur.quest.enrolled && !cur.quest.completed => {
                    return card.fail("Discord вважає завдання не прийнятим");
                }
                Ok(Some(QuestState { status: Some(st), .. })) => {
                    let (pct, d) = st.progress(target);
                    real_pct = pct;
                    if d {
                        done = true;
                        break;
                    }
                }
                _ => {} // network error or no status yet — try again next time
            }
        }
        std::thread::sleep(Duration::from_secs(1));

        let el = started.elapsed().as_secs_f32();
        let shown = real_pct.max((el / target as f32).clamp(0.0, 1.0));
        let discord =
            if real_pct > 0.0 { format!("Discord {:.0}%", real_pct * 100.0) } else { "очікування Discord".to_string() };
        card.set(shown, format!("«{game_name}» · {:.0}/{minutes:.0} хв · {discord}", el / 60.0));
    }

    let stopped = card.stopped();
    if !done && !stopped {
        done = card
            .with_ctx(|ctx| fetch_status(token, &q.id, Duration::ZERO, ctx))
            .is_some_and(|s| s.progress(target).1);
    }
    if stopped {
        // The card is already marked "Stopped".
    } else if done {
        card.done();
    } else {
        card.fail("Час вийшов, але Discord не підтвердив виконання");
        manager::lock(mgr).msg(format!("⚠ Discord не підтвердив квест «{}»", q.title));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("2026-11-17T12:00:00+00:00"), Some(1_794_916_800));
        assert_eq!(parse_ts("2026-11-17T12:00:00.123456+00:00"), Some(1_794_916_800));
        assert_eq!(parse_ts("2026-11-17T14:00:00+02:00"), Some(1_794_916_800));
        assert_eq!(parse_ts("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(parse_ts("garbage"), None);
    }

    #[test]
    fn base64_encoding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(br#"{"os":"Windows"}"#), "eyJvcyI6IldpbmRvd3MifQ==");
    }

    #[test]
    fn video_pacing() {
        // Right after enrollment only the first step fits into the allowance.
        assert_eq!(next_video_timestamp(0, 60, 0), Some(7));
        assert_eq!(next_video_timestamp(7, 60, 0), None);
        // Never ahead of real time + VIDEO_AHEAD; never past the target.
        assert_eq!(next_video_timestamp(7, 60, 4), Some(14));
        assert_eq!(next_video_timestamp(56, 60, 100), Some(60));
        // Resuming long after enrollment goes at full step speed.
        assert_eq!(next_video_timestamp(20, 60, 1000), Some(27));
    }

    #[test]
    fn task_progress() {
        let st: QuestUserStatus = serde_json::from_str(
            r#"{"enrolled_at":"2026-11-17T12:00:00Z","progress":{"WATCH_VIDEO":{"value":42}}}"#,
        )
        .unwrap();
        assert_eq!(st.task_seconds("WATCH_VIDEO"), 42.0);
        // Unknown task — fall back to the best value in the status.
        assert_eq!(st.task_seconds("WATCH_VIDEO_ON_MOBILE"), 42.0);
        assert!(!st.is_completed());
        let done: QuestUserStatus = serde_json::from_str(r#"{"claimed_at":"2026-11-17T12:00:00Z"}"#).unwrap();
        assert!(done.is_completed());
    }

    #[test]
    fn reward_kinds() {
        let kind = |json: &str| {
            let cfg: QuestConfig = serde_json::from_str(&format!(r#"{{"rewards_config":{{"rewards":[{json}]}}}}"#)).unwrap();
            parse_reward(&Some(cfg))
        };
        assert!(matches!(kind(r#"{"type":4,"orb_quantity":700}"#).0, RewardKind::Orbs));
        let (k, label) = kind(r#"{"type":3,"messages":{"name":"Cool Avatar Decoration"}}"#);
        assert!(matches!(k, RewardKind::Cosmetic));
        assert_eq!(label, "🎨 Рамка аватара «Cool»");
        // In-game items, even with "badge"/"skin" in the name, are not cosmetics.
        assert!(matches!(kind(r#"{"type":1,"messages":{"name":"Golden Badge Skin"}}"#).0, RewardKind::InGame));
        assert!(matches!(kind(r#"{"type":2,"messages":{"name":"Weapon Bundle"}}"#).0, RewardKind::InGame));
        // Unknown type — guessed by name.
        assert!(matches!(kind(r#"{"messages":{"name":"Fancy Avatar Decoration"}}"#).0, RewardKind::Cosmetic));
        assert!(matches!(kind(r#"{"messages":{"name":"Epic Sword"}}"#).0, RewardKind::InGame));
    }
}
