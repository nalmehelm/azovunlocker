use serde::Deserialize;
use std::{
    collections::HashSet,
    env, fs, io,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

const API: &str = "https://discord.com/api/v9/applications/detectable";
const CACHE: &str = "detectable.json";

#[derive(Deserialize)]
struct RawApp {
    id: String,
    name: String,
    executables: Option<Vec<RawExe>>,
}

#[derive(Deserialize)]
struct RawExe {
    os: String,
    name: String,
    #[serde(default)]
    is_launcher: bool,
}

pub struct Game {
    pub id: String,
    pub name: String,
    pub name_lc: String,
    /// Relative exe path, e.g. "game/bin/game.exe".
    pub exe: String,
    pub demo: bool,
}

/// Folder of the program's exe (computed once).
pub fn base_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    })
}

pub fn data_path(file: &str) -> PathBuf {
    base_dir().join(file)
}

/// Saves the list in Discord's own format, but only with the fields we use:
/// ~1 MB instead of ~13 MB, so every next start parses it many times faster.
fn save_cache(games: &[Game]) -> Result<(), String> {
    let slim: Vec<_> = games
        .iter()
        .map(|g| serde_json::json!({ "id": g.id, "name": g.name, "executables": [{ "os": "win32", "name": g.exe }] }))
        .collect();
    let text = serde_json::to_string(&slim).map_err(|e| e.to_string())?;
    fs::write(data_path(CACHE), text).map_err(|e| format!("не вдалося зберегти: {e}"))
}

fn download() -> Result<Vec<Game>, String> {
    let mut body = String::with_capacity(16 << 20);
    io::Read::read_to_string(
        &mut crate::http::agent()
            .get(API)
            .set("User-Agent", "Mozilla/5.0")
            .call()
            .map_err(|e| format!("помилка запиту: {e}"))?
            .into_reader(),
        &mut body,
    )
    .map_err(|e| format!("помилка читання: {e}"))?;
    let games = parse(&body)?; // valid before the cache is overwritten
    save_cache(&games)?;
    Ok(games)
}

/// Safe relative exe path (no "..", drive letters, etc.).
fn clean_exe(name: &str) -> Option<String> {
    let n = name.trim_start_matches('>').replace('\\', "/");
    if n.is_empty() || !n.to_ascii_lowercase().ends_with(".exe") {
        return None;
    }
    Path::new(&n)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
        .then_some(n)
}

fn parse(json: &str) -> Result<Vec<Game>, String> {
    let apps: Vec<RawApp> = serde_json::from_str(json).map_err(|e| format!("пошкоджений JSON: {e}"))?;
    let mut seen = HashSet::with_capacity(apps.len());
    let mut out: Vec<Game> = apps
        .into_iter()
        .filter_map(|a| {
            let exe = a
                .executables?
                .into_iter()
                .filter(|e| e.os == "win32" && !e.is_launcher)
                .find_map(|e| clean_exe(&e.name))?;
            if !seen.insert(a.id.clone()) {
                return None;
            }
            let name_lc = a.name.to_lowercase();
            let demo = name_lc.contains("demo") || name_lc.contains("демо");
            Some(Game { id: a.id, name: a.name, name_lc, exe, demo })
        })
        .collect();
    out.sort_unstable_by(|a, b| a.name_lc.cmp(&b.name_lc));
    out.shrink_to_fit();
    Ok(out)
}

pub fn load(force_download: bool) -> Result<Vec<Game>, String> {
    let path = data_path(CACHE);
    if force_download || !path.exists() {
        return download();
    }
    let s = fs::read_to_string(&path).map_err(|e| format!("не вдається прочитати кеш: {e}"))?;
    let games = parse(&s)?;
    // A full-size file from an older version: slim it down once.
    if s.len() > 4 << 20 {
        let _ = save_cache(&games);
    }
    Ok(games)
}
