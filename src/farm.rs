//! "Unique games" farm: runs games that aren't in the stats yet, batch by batch, so the
//! number of different games played keeps growing. Discord counts a game after a short
//! session (~15 s is enough); the badge itself updates with a delay of up to a day.

use crate::manager::{self, prepare, remove_game_dir, LaunchInfo, Manager, Origin};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Pause between two process launches, so Discord's process watcher sees each one.
const SPAWN_GAP: Duration = Duration::from_millis(30);
/// Pause between batches.
const BATCH_GAP: Duration = Duration::from_millis(300);
/// How often the farm checks its batch.
const POLL: Duration = Duration::from_millis(200);

/// A game counts once it has been played this long.
pub fn is_counted(played_secs: u64, hold_secs: u64) -> bool {
    played_secs >= hold_secs
}

#[derive(Clone, Copy)]
pub struct FarmConfig {
    /// How many new games to count in this run.
    pub target: usize,
    /// Games running at the same time.
    pub batch: usize,
    /// Seconds each game is held.
    pub hold_secs: u64,
    pub only_demo: bool,
    pub skip_adult: bool,
}

impl FarmConfig {
    /// Approximate time of one batch.
    pub fn batch_secs(&self) -> f32 {
        self.hold_secs as f32 + 0.6 + self.batch as f32 * SPAWN_GAP.as_secs_f32()
    }
}

/// The batch running right now (for the UI).
#[derive(Default, Clone)]
pub struct Batch {
    pub number: usize,
    pub names: Vec<String>,
    pub ends: Option<Instant>,
}

/// Shared between the farm thread and the UI.
#[derive(Default)]
pub struct FarmState {
    pub running: AtomicBool,
    pub cancel: AtomicBool,
    /// Games counted in the current/last run.
    pub done: AtomicUsize,
    pub target: AtomicUsize,
    pub batch: Mutex<Batch>,
    pub started: Mutex<Option<Instant>>,
}

impl FarmState {
    pub fn batch(&self) -> Batch {
        self.batch.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_batch(&self, b: Batch) {
        *self.batch.lock().unwrap_or_else(|e| e.into_inner()) = b;
    }
}

/// Up to `n` games that aren't counted, aren't running and don't share an exe name with a
/// running game or with each other. Rich Presence games (no exe) only when `rpc_ok`: they
/// need a running desktop Discord. Starts at a random place in the list so every run
/// doesn't go through the same alphabetical start.
fn pick(m: &Manager, cfg: &FarmConfig, skip: &HashSet<String>, n: usize, rpc_ok: bool) -> Vec<LaunchInfo> {
    let games = &m.games;
    if games.is_empty() {
        return Vec::new();
    }
    let mut busy: HashSet<&str> = m.runs.iter().map(|r| r.exe_key.as_str()).collect();
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as usize);
    let start = seed % games.len();
    let mut out = Vec::with_capacity(n);
    for k in 0..games.len() {
        if out.len() >= n {
            break;
        }
        let g = &games[(start + k) % games.len()];
        if (cfg.only_demo && !g.demo)
            || (cfg.skip_adult && g.adult)
            || skip.contains(&g.id)
            || is_counted(m.played.get(&g.id).copied().unwrap_or(0), cfg.hold_secs)
            || m.is_running_id(&g.id)
            || (g.rpc && !rpc_ok)
            // Rich Presence games have no exe to clash on.
            || (!g.rpc && !busy.insert(g.exe_key.as_str()))
        {
            continue;
        }
        out.push(LaunchInfo::of(g));
    }
    out
}

pub fn start(cfg: FarmConfig, mgr: Arc<Mutex<Manager>>, st: Arc<FarmState>) {
    if st.running.swap(true, Ordering::SeqCst) {
        return;
    }
    st.cancel.store(false, Ordering::SeqCst);
    st.done.store(0, Ordering::SeqCst);
    st.target.store(cfg.target, Ordering::SeqCst);
    *st.started.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    std::thread::spawn(move || {
        run(cfg, &mgr, &st);
        st.set_batch(Batch::default());
        st.running.store(false, Ordering::SeqCst);
    });
}

fn run(cfg: FarmConfig, mgr: &Mutex<Manager>, st: &FarmState) {
    let cancelled = || st.cancel.load(Ordering::Relaxed);
    let hold = Duration::from_secs(cfg.hold_secs);
    manager::lock(mgr).msg(format!("🏆 Фарм: {} ігор, по {} одночасно, {} с", cfg.target, cfg.batch, cfg.hold_secs));
    // Games that didn't start or didn't count — not retried in this run.
    let mut skip = HashSet::new();
    let mut number = 0;
    let mut rpc_warned = false;

    while !cancelled() {
        let done = st.done.load(Ordering::Relaxed);
        if done >= cfg.target {
            break;
        }
        // Checked every batch: Discord may be started (or closed) while the farm runs.
        let rpc_ok = crate::rpc::available();
        if !rpc_ok && !rpc_warned {
            rpc_warned = true;
            manager::lock(mgr).msg("🏆 Discord не запущено: ігри без exe (RPC) поки пропускаються");
        }
        let batch = pick(&manager::lock(mgr), &cfg, &skip, cfg.batch.min(cfg.target - done), rpc_ok);
        if batch.is_empty() {
            manager::lock(mgr).msg("🏆 Незарахованих ігор більше немає");
            break;
        }

        let mut ids = Vec::with_capacity(batch.len());
        let mut names = Vec::with_capacity(batch.len());
        for info in batch {
            if cancelled() {
                break;
            }
            let (id, name) = (info.id.clone(), info.name.clone());
            let prepared = prepare(&info); // disk / pipe work without the lock
            // The ticker's deadline is only a safety net: the farm stops the batch itself.
            if manager::lock(mgr).launch(info, prepared, Some(hold + Duration::from_secs(5)), Origin::Farm) {
                ids.push(id);
                names.push(name);
            } else {
                skip.insert(id);
            }
            std::thread::sleep(SPAWN_GAP);
        }
        if ids.is_empty() {
            continue;
        }
        number += 1;
        // Counted from the last launch, so every game of the batch gets the full time.
        let ends = Instant::now() + hold + Duration::from_millis(300);
        st.set_batch(Batch { number, names, ends: Some(ends) });

        while !cancelled() && Instant::now() < ends {
            std::thread::sleep(POLL);
            // Stopped by hand ("Stop all" on the Active tab).
            if !ids.iter().any(|id| manager::lock(mgr).is_running_id(id)) {
                break;
            }
        }

        let mut m = manager::lock(mgr);
        for id in &ids {
            m.stop_id(id);
        }
        let mut counted = 0;
        for id in &ids {
            if is_counted(m.played.get(id).copied().unwrap_or(0), cfg.hold_secs) {
                counted += 1;
            } else {
                skip.insert(id.clone());
            }
            if !m.is_running_id(id) {
                remove_game_dir(id);
            }
        }
        drop(m);
        st.done.fetch_add(counted, Ordering::Relaxed);
        std::thread::sleep(BATCH_GAP);
    }

    let done = st.done.load(Ordering::Relaxed);
    let mut m = manager::lock(mgr);
    m.msg(if cancelled() { format!("■ Фарм зупинено: {done} ігор") } else { format!("✨ Фарм завершено: {done} ігор") });
    m.save();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::games::Game;

    fn game(id: &str, exe_key: &str, adult: bool) -> Game {
        Game {
            id: id.into(),
            name: id.into(),
            name_lc: id.into(),
            exe: format!("{id}/{exe_key}"),
            args: None,
            exe_key: exe_key.into(),
            demo: false,
            adult,
            rpc: exe_key.is_empty(),
        }
    }

    #[test]
    fn batch_picking() {
        let mut m = Manager::new();
        m.played.clear();
        m.games = Arc::new(vec![
            game("a", "game.exe", false),
            game("b", "game.exe", false), // same exe name as "a"
            game("c", "c.exe", true),     // 18+
            game("d", "d.exe", false),    // already counted
            game("e", "e.exe", false),
        ]);
        m.played.insert("d".into(), 20);
        let cfg = FarmConfig { target: 10, batch: 10, hold_secs: 15, only_demo: false, skip_adult: true };
        let mut ids: Vec<String> = pick(&m, &cfg, &HashSet::new(), 10, true).into_iter().map(|g| g.id).collect();
        ids.sort();
        // One of a/b (random start), plus e.
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"e".to_string()));
        assert!(ids[0] == "a" || ids[0] == "b");
        let skip: HashSet<String> = ["e".to_string()].into();
        assert_eq!(pick(&m, &cfg, &skip, 10, true).len(), 1);
    }

    #[test]
    fn rpc_games() {
        let mut m = Manager::new();
        m.played.clear();
        // No exe: never clash with each other, but need a running Discord.
        m.games = Arc::new(vec![game("r1", "", false), game("r2", "", false), game("x", "x.exe", false)]);
        let cfg = FarmConfig { target: 10, batch: 10, hold_secs: 15, only_demo: false, skip_adult: true };
        assert_eq!(pick(&m, &cfg, &HashSet::new(), 10, true).len(), 3);
        let ids: Vec<String> = pick(&m, &cfg, &HashSet::new(), 10, false).into_iter().map(|g| g.id).collect();
        assert_eq!(ids, ["x"]);
    }
}
