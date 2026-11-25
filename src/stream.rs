//! Automatic stream watching with two accounts.
//!
//! One hidden browser ([`crate::web`]) runs as a child process with a Discord page per
//! account: the second account ("alt") joins a DM call with the main account and streams a
//! still picture, the main account joins the same call and watches it. Only the time the
//! main account actually receives the video counts toward the set duration; if the browser
//! dies it is started again.

use crate::games::data_path;
use crate::manager::Job;
use std::{
    io::{BufRead, BufReader},
    os::windows::process::CommandExt,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const FILE: &str = "stream.json";
/// A browser that died is started again after this pause.
const RESTART_AFTER: Duration = Duration::from_secs(15);

struct Run {
    /// Bumped on every start, so threads of an old run stop touching the new one.
    id: u64,
    alt_token: String,
    main_token: String,
    child: Option<Child>,
    /// Open while the browser runs: "quit" (or closing it) makes it leave the call and exit.
    stdin: Option<ChildStdin>,
    /// When the browser died (for the restart pause).
    died: Option<Instant>,
    alt: String,
    main: String,
    /// Seconds the main account has watched in this run.
    watched: u64,
    target: Option<u64>,
}

#[derive(Default)]
struct Inner {
    run: Option<Run>,
    next_id: u64,
    /// Seconds watched in earlier runs (saved in stream.json).
    total: u64,
    /// The browser dies together with the program, even if it's killed.
    job: Option<Job>,
}

/// Shared between the UI, the timer thread and the stdout reader.
#[derive(Clone)]
pub struct StreamCtl(Arc<Mutex<Inner>>);

/// What the UI shows.
pub struct Status {
    pub running: bool,
    pub watched: u64,
    pub left: Option<u64>,
    /// All watched time, including the current run.
    pub total: u64,
    pub alt: String,
    pub main: String,
}

/// The user id is the first part of a token, in base64.
pub fn token_user_id(token: &str) -> Option<String> {
    let part = token.trim().trim_matches('"').split('.').next()?;
    let mut bits = 0u32;
    let mut n = 0u32;
    let mut out = Vec::new();
    for c in part.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        bits = bits << 6 | v as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    let id = String::from_utf8(out).ok()?;
    (id.len() >= 15 && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

/// Human-readable state of one client.
pub fn describe(state: &str) -> String {
    let s = match state {
        "" | "started" => "запуск",
        "loading" => "завантаження Discord",
        "login" => "токен не підходить",
        "adding-friend" => "надсилаю заявку в друзі",
        "accepting-friend" => "приймаю заявку в друзі",
        "waiting-friend" => "чекаю, поки приймуть заявку в друзі",
        "blocked" => "акаунти заблокували один одного",
        "captcha" => "Discord просить капчу на заявку в друзі: додайте акаунти в друзі вручну один раз",
        s if s.starts_with("friend-error ") => return format!("не вдалося додати в друзі: {}", &s[13..]),
        "opening-dm" => "відкриваю ЛС",
        "waiting-dm" => "чекаю ЛС від другого акаунта",
        "joining" => "заходжу в дзвінок",
        "go-live" => "вмикаю трансляцію",
        "streaming" => "транслює",
        "waiting-stream" => "чекаю трансляцію",
        "opening-stream" => "відкриваю трансляцію",
        "watching" => "дивиться",
        "no-video" => "немає відео, перезапускаю трансляцію",
        "busy" => "сторінка зайнята",
        "restart" => "перезапуск",
        s if s.starts_with("loading ") => "завантаження Discord",
        s if s.starts_with("error ") => return format!("помилка: {}", &s[6..]),
        s if s.starts_with("js ") => return format!("помилка скрипта: {}", &s[3..]),
        s => s,
    };
    s.to_string()
}

impl StreamCtl {
    pub fn new() -> Self {
        let total = crate::games::read_data(FILE)
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v["total_secs"].as_u64())
            .unwrap_or(0);
        Self(Arc::new(Mutex::new(Inner { total, job: Job::new(), ..Default::default() })))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn status(&self) -> Status {
        let g = self.lock();
        let r = g.run.as_ref();
        let watched = r.map_or(0, |r| r.watched);
        Status {
            running: r.is_some(),
            watched,
            left: r.and_then(|r| r.target).map(|t| t.saturating_sub(watched)),
            total: g.total + watched,
            alt: r.map_or(String::new(), |r| r.alt.clone()),
            main: r.map_or(String::new(), |r| r.main.clone()),
        }
    }

    /// Starts the browser; `secs` of watching (None = until stopped).
    pub fn start(&self, secs: Option<u64>, main_token: &str, alt_token: &str) -> Result<(), String> {
        let main_id = token_user_id(main_token).ok_or("Токен основного акаунта (Налаштування) не схожий на токен")?;
        let alt_id = token_user_id(alt_token).ok_or("Токен другого акаунта не схожий на токен")?;
        if main_id == alt_id {
            return Err("Потрібні два різні акаунти".into());
        }
        let mut g = self.lock();
        if g.run.is_some() {
            return Ok(());
        }
        // Folders of the old one-browser-per-account layout (each held a full browser profile).
        for old in ["web\\alt", "web\\main"] {
            let _ = std::fs::remove_dir_all(data_path(old));
        }
        g.next_id += 1;
        let clean = |t: &str| t.trim().trim_matches('"').to_string();
        let mut run = Run {
            id: g.next_id,
            alt_token: clean(alt_token),
            main_token: clean(main_token),
            child: None,
            stdin: None,
            died: None,
            alt: String::new(),
            main: String::new(),
            watched: 0,
            target: secs,
        };
        spawn(self, &mut run, g.job.as_ref())?;
        let id = run.id;
        g.run = Some(run);
        drop(g);
        self.timer(id);
        Ok(())
    }

    /// Once a second: counts watched time, stops at the target, restarts a dead browser.
    fn timer(&self, id: u64) {
        let ctl = self.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            let mut guard = ctl.lock();
            let g = &mut *guard;
            let Some(r) = g.run.as_mut().filter(|r| r.id == id) else { return };
            if r.main == "watching" {
                r.watched += 1;
            }
            if r.target.is_some_and(|t| r.watched >= t) {
                drop(guard);
                ctl.stop();
                return;
            }
            if let Some(ch) = r.child.as_mut() {
                if matches!(ch.try_wait(), Ok(Some(_))) {
                    r.child = None;
                    r.stdin = None;
                    r.died = Some(Instant::now());
                    for s in [&mut r.alt, &mut r.main] {
                        if !s.starts_with("error") {
                            *s = "restart".into();
                        }
                    }
                }
            }
            if r.child.is_none() && r.died.is_some_and(|d| d.elapsed() >= RESTART_AFTER) {
                if let Err(e) = spawn(&ctl, r, g.job.as_ref()) {
                    r.alt = format!("error {e}");
                    r.died = Some(Instant::now());
                }
            }
        });
    }

    /// Stops in the background (the browser takes a moment to leave the call).
    pub fn stop(&self) {
        if let Some(f) = self.end() {
            std::thread::spawn(f);
        }
    }

    /// Stops and waits until the browser is gone (on program exit).
    pub fn stop_wait(&self) {
        if let Some(f) = self.end() {
            f();
        }
    }

    /// Ends the run, adds the watched time to the total; returns what closes the browser.
    fn end(&self) -> Option<impl FnOnce() + Send + 'static> {
        let mut g = self.lock();
        let mut r = g.run.take()?;
        g.total += r.watched;
        let total = g.total;
        drop(g);
        let (child, stdin) = (r.child.take(), r.stdin.take());
        Some(move || {
            // Written outside the lock: the disk flush must not stall the UI.
            let _ = crate::games::write_data(FILE, serde_json::json!({ "total_secs": total }).to_string());
            close(child, stdin);
        })
    }
}

/// Asks the browser to leave the call and exit; kills it if it doesn't within 3 s.
fn close(child: Option<Child>, stdin: Option<ChildStdin>) {
    use std::io::Write;
    let Some(mut ch) = child else { return };
    if let Some(mut s) = stdin {
        let _ = s.write_all(b"quit\n");
    }
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if matches!(ch.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = ch.kill();
    let _ = ch.wait();
}
/// Starts the browser and a thread that reads its "<role> <state>" lines.
fn spawn(ctl: &StreamCtl, run: &mut Run, job: Option<&Job>) -> Result<(), String> {
    let dir = data_path("web");
    let _ = std::fs::create_dir_all(&dir);
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .env(crate::web::ROLE_ENV, "1")
        .env(crate::web::ALT_TOKEN_ENV, &run.alt_token)
        .env(crate::web::MAIN_TOKEN_ENV, &run.main_token)
        .env(crate::web::DIR_ENV, &dir)
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("не вдалося запустити браузер: {e}"))?;
    if let Some(j) = job {
        j.add(&child);
    }
    let out = child.stdout.take();
    run.stdin = child.stdin.take();
    run.child = Some(child);
    run.died = None;
    run.alt = "started".into();
    run.main = "started".into();
    let (ctl, id) = (ctl.clone(), run.id);
    if let Some(out) = out {
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let (who, state) = line.split_once(' ').unwrap_or((line.as_str(), ""));
                let state = state.trim().to_string();
                let mut g = ctl.lock();
                let Some(r) = g.run.as_mut().filter(|r| r.id == id) else { return };
                match who {
                    "alt" => r.alt = state,
                    "main" => r.main = state,
                    // The browser itself failed: show it on both lines.
                    "error" => {
                        r.alt = format!("error {state}");
                        r.main = r.alt.clone();
                    }
                    _ => {}
                }
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::token_user_id;

    #[test]
    fn user_id_from_token() {
        // "1516832660444680212" in base64 without padding, as in real tokens.
        assert_eq!(token_user_id("MTUxNjgzMjY2MDQ0NDY4MDIxMg.GabcDe.xyz").as_deref(), Some("1516832660444680212"));
        assert_eq!(token_user_id("\"MTUxNjgzMjY2MDQ0NDY4MDIxMg.a.b\"").as_deref(), Some("1516832660444680212"));
        assert_eq!(token_user_id("not a token"), None);
        assert_eq!(token_user_id(""), None);
    }
}
