use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashSet,
    env, fs, io,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

const API: &str = "https://discord.com/api/v9/applications/detectable";
/// Slim cache of the list. The name carries the format version: a cache of an older
/// format lacks fields (arguments, 18+ flag, games without an exe) and is re-downloaded.
const CACHE: &str = "detectable_v3.json";
const OLD_CACHES: [&str; 2] = ["detectable.json", "detectable_v2.json"];
/// ESRB "Adults Only".
const ESRB_ADULTS_ONLY: u64 = 5;

#[derive(Deserialize)]
struct RawApp {
    id: String,
    name: String,
    executables: Option<Vec<RawExe>>,
    #[serde(default)]
    themes: Vec<String>,
    #[serde(default)]
    content_classification: Option<Value>,
    /// Only present in our own slim cache.
    #[serde(default)]
    adult: bool,
}

#[derive(Deserialize)]
struct RawExe {
    os: String,
    name: String,
    #[serde(default)]
    is_launcher: bool,
    #[serde(default)]
    arguments: Option<String>,
}

impl RawApp {
    fn is_adult(&self) -> bool {
        self.adult
            || self.themes.iter().any(|t| t.eq_ignore_ascii_case("erotic"))
            || self
                .content_classification
                .as_ref()
                .and_then(|c| c.pointer("/agency_ratings/esrb/rating"))
                .and_then(Value::as_u64)
                == Some(ESRB_ADULTS_ONLY)
    }
}

pub struct Game {
    pub id: String,
    pub name: String,
    pub name_lc: String,
    /// Relative exe path, e.g. "game/bin/game.exe".
    pub exe: String,
    /// Command line Discord expects for this exe (some games are only matched with it).
    pub args: Option<String>,
    /// Lowercase file name of the exe: two games with the same one can't be told apart
    /// by Discord when they run at the same time.
    pub exe_key: String,
    pub demo: bool,
    pub adult: bool,
    /// Discord knows no Windows exe for it: played through Rich Presence ([`crate::rpc`]),
    /// `exe` and `exe_key` are empty.
    pub rpc: bool,
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

/// Reads a data file next to the exe. A UTF-8 BOM (Notepad and PowerShell add one) is
/// dropped: JSON parsers reject it, and the file would be silently reset to defaults.
pub fn read_data(file: &str) -> Option<String> {
    let s = fs::read_to_string(data_path(file)).ok()?;
    Some(match s.strip_prefix('\u{feff}') {
        Some(t) => t.to_string(),
        None => s,
    })
}

/// Saves the list in Discord's own format, but only with the fields we use:
/// ~1 MB instead of ~13 MB, so every next start parses it many times faster.
fn save_cache(games: &[Game]) -> Result<(), String> {
    let slim: Vec<_> = games
        .iter()
        .map(|g| {
            let exes = if g.rpc {
                serde_json::json!([])
            } else {
                let mut exe = serde_json::json!({ "os": "win32", "name": g.exe });
                if let Some(a) = &g.args {
                    exe["arguments"] = a.clone().into();
                }
                serde_json::json!([exe])
            };
            let mut app = serde_json::json!({ "id": g.id, "name": g.name, "executables": exes });
            if g.adult {
                app["adult"] = true.into();
            }
            app
        })
        .collect();
    let text = serde_json::to_string(&slim).map_err(|e| e.to_string())?;
    fs::write(data_path(CACHE), text).map_err(|e| format!("не вдалося зберегти: {e}"))?;
    for old in OLD_CACHES {
        let _ = fs::remove_file(data_path(old));
    }
    Ok(())
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

/// Safe relative exe path: no "..", drive letters or characters Windows rejects;
/// ".exe" is added when the name has no extension.
fn clean_exe(name: &str) -> Option<String> {
    let n = name.trim().trim_start_matches('>').trim().replace('\\', "/");
    let mut parts = Vec::new();
    for c in Path::new(&n).components() {
        let Component::Normal(p) = c else { continue };
        let p: String = p
            .to_string_lossy()
            .chars()
            .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') && !c.is_control())
            .collect();
        let p = p.trim_end_matches(['.', ' ']);
        if !p.is_empty() {
            parts.push(p.to_string());
        }
    }
    let last = parts.last_mut()?;
    if !last.contains('.') {
        last.push_str(".exe");
    }
    Some(parts.join("/"))
}

fn parse(json: &str) -> Result<Vec<Game>, String> {
    let apps: Vec<RawApp> = serde_json::from_str(json).map_err(|e| format!("пошкоджений JSON: {e}"))?;
    let mut seen = HashSet::with_capacity(apps.len());
    let mut out: Vec<Game> = apps
        .into_iter()
        .filter_map(|a| {
            let adult = a.is_adult();
            let exes: Vec<RawExe> = a.executables.unwrap_or_default().into_iter().filter(|e| e.os == "win32").collect();
            // The game itself first; a launcher only when that's all Discord knows.
            // No usable Windows exe: the game is played through Rich Presence instead.
            let found = exes
                .iter()
                .filter(|e| !e.is_launcher)
                .chain(exes.iter().filter(|e| e.is_launcher))
                .find_map(|e| clean_exe(&e.name).map(|x| (x, e.arguments.clone().filter(|s| !s.trim().is_empty()))));
            if !seen.insert(a.id.clone()) {
                return None;
            }
            let name_lc = a.name.to_lowercase();
            let demo = name_lc.contains("demo") || name_lc.contains("демо");
            let rpc = found.is_none();
            let (exe, args) = found.unwrap_or_default();
            let exe_key = exe.rsplit('/').next().unwrap_or(&exe).to_lowercase();
            Some(Game { id: a.id, name: a.name, name_lc, exe, args, exe_key, demo, adult, rpc })
        })
        .collect();
    out.sort_unstable_by(|a, b| a.name_lc.cmp(&b.name_lc));
    out.shrink_to_fit();
    Ok(out)
}

pub fn load(force_download: bool) -> Result<Vec<Game>, String> {
    if force_download || !data_path(CACHE).exists() {
        return download();
    }
    let s = read_data(CACHE).ok_or("не вдається прочитати кеш")?;
    parse(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_paths() {
        assert_eq!(clean_exe("Terraria/Terraria.exe").as_deref(), Some("Terraria/Terraria.exe"));
        assert_eq!(clean_exe(">javaw.exe").as_deref(), Some("javaw.exe"));
        assert_eq!(clean_exe("bin\\game").as_deref(), Some("bin/game.exe"));
        assert_eq!(clean_exe("../c:/x/ga?me.exe").as_deref(), Some("c/x/game.exe"));
        assert_eq!(clean_exe("").as_deref(), None);
    }

    #[test]
    fn parse_list() {
        let json = r#"[
            {"id":"1","name":"B Game","executables":[{"os":"win32","name":"launcher.exe","is_launcher":true},{"os":"win32","name":"b/b.exe","arguments":"-x"}]},
            {"id":"2","name":"A Demo","executables":[{"os":"linux","name":"a"},{"os":"win32","name":"a/run","is_launcher":true}],"themes":["Erotic"]},
            {"id":"3","name":"No exe","executables":[{"os":"darwin","name":"c"}]},
            {"id":"4","name":"Console","executables":[]},
            {"id":"5","name":"Sdk"}
        ]"#;
        let g = parse(json).unwrap();
        assert_eq!(g.len(), 5);
        assert_eq!((g[0].exe.as_str(), g[0].demo, g[0].adult, g[0].rpc), ("a/run.exe", true, true, false));
        assert_eq!((g[1].exe.as_str(), g[1].args.as_deref(), g[1].exe_key.as_str()), ("b/b.exe", Some("-x"), "b.exe"));
        // Without a Windows exe: Rich Presence games.
        assert!(g[2..].iter().all(|g| g.rpc && g.exe.is_empty() && g.exe_key.is_empty()));
    }
}
