//! Game processes: start, stop, time accounting.
//! Driven by a background thread that wakes once per second, so timers work even
//! when the window is minimized and the UI isn't redrawing.

use crate::games::{base_dir, data_path, Game};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    os::windows::{io::AsRawHandle, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;

/// Locks the manager. A panic in another thread (poisoned mutex) doesn't take the whole
/// program down with it: the data is still consistent enough to keep going.
pub fn lock(m: &Mutex<Manager>) -> MutexGuard<'_, Manager> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Job object: all child processes die with the program (even if it's killed).
pub struct Job(windows_sys::Win32::Foundation::HANDLE);
unsafe impl Send for Job {}

impl Job {
    pub fn new() -> Option<Self> {
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            );
            Some(Job(h))
        }
    }
    pub fn add(&self, c: &Child) {
        unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(self.0, c.as_raw_handle() as _);
        }
    }
}

/// What a quest task does — decides its icon and label on the "Active" page.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Video,
    MobileVideo,
    Game,
}

impl TaskKind {
    pub fn icon(self) -> &'static str {
        match self {
            TaskKind::Video => "🎬",
            TaskKind::MobileVideo => "📱",
            TaskKind::Game => "🎮",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Video => "Відео (ПК)",
            TaskKind::MobileVideo => "Відео з телефону",
            TaskKind::Game => "Гра",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Waiting for a free worker.
    Queued,
    /// Being checked, enrolled or executed right now.
    Active,
    Done,
    /// Not done on purpose (expired, already done, enrollment blocked…).
    Skipped,
    /// Stopped by the user.
    Stopped,
    Failed,
}

impl TaskState {
    /// Still has work ahead (counts as "running", can be stopped).
    pub fn is_live(self) -> bool {
        matches!(self, TaskState::Queued | TaskState::Active)
    }
}

/// One quest on the "Active" page: a single card from the queue to the result.
#[derive(Clone)]
pub struct QuestTask {
    /// Quest id.
    pub id: String,
    pub title: String,
    pub reward: String,
    pub kind: TaskKind,
    pub state: TaskState,
    pub progress: f32,
    pub detail: String,
    /// When the work on it began (None while queued).
    pub started: Option<Instant>,
}

/// Who started a game.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// From the "Games" tab: listed on "Active".
    Manual,
    /// For quests: shown on their cards, closed when no quest needs it.
    Quest,
    /// By the unique-games farm: shown as one farm card, not logged one by one.
    Farm,
}

/// How a running game shows up in Discord.
enum Backend {
    /// A dummy process at the game's exe path.
    Process(Child),
    /// Rich Presence for a game without an exe.
    Rpc(crate::rpc::Session),
}

/// A launch made ready by [`prepare`] (the slow part, done without the manager lock).
pub enum Prepared {
    Exe(PathBuf),
    Rpc(crate::rpc::Session),
}

pub struct Run {
    /// Game id, not an index: the game list can be reloaded while the game runs.
    pub id: String,
    pub name: String,
    pub exe_key: String,
    pub origin: Origin,
    backend: Backend,
    pub started: Instant,
    pub until: Option<Instant>,
    credited: u64,
}

/// Everything needed to launch a game, copied out of the list so that the file work
/// in [`prepare_exe`] can happen without holding the manager lock.
pub struct LaunchInfo {
    pub id: String,
    pub name: String,
    pub exe: String,
    pub args: Option<String>,
    pub exe_key: String,
    pub rpc: bool,
}

impl LaunchInfo {
    pub fn of(g: &Game) -> Self {
        Self {
            id: g.id.clone(),
            name: g.name.clone(),
            exe: g.exe.clone(),
            args: g.args.clone(),
            exe_key: g.exe_key.clone(),
            rpc: g.rpc,
        }
    }
}

/// Deletes `games/<id>` (hard links, so no real data). Errors are ignored: a file of a
/// process that is just exiting may still be locked, it'll be replaced next time.
pub fn remove_game_dir(id: &str) {
    let _ = fs::remove_dir_all(base_dir().join("games").join(id));
}

/// Gets a launch ready: the dummy exe on disk, or the Rich Presence connection.
/// Slow-ish (disk, pipe), so call it without the manager lock when possible.
pub fn prepare(g: &LaunchInfo) -> Result<Prepared, String> {
    if g.rpc {
        crate::rpc::Session::start(&g.id).map(Prepared::Rpc)
    } else {
        prepare_exe(g).map(Prepared::Exe)
    }
}

/// Prepares `games/<id>/<exe>`: a hard link to ourselves (0 bytes on disk), or a copy.
fn prepare_exe(g: &LaunchInfo) -> Result<PathBuf, String> {
    let dest = base_dir().join("games").join(&g.id).join(&g.exe);
    let me = env::current_exe().map_err(|e| e.to_string())?;
    // An old version of the program may be sitting at the path — replace it.
    let stale = match (fs::metadata(&dest), fs::metadata(&me)) {
        (Ok(a), Ok(b)) => a.len() != b.len() || a.modified().ok() != b.modified().ok(),
        (Err(_), _) => true,
        _ => false,
    };
    if stale {
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        if dest.exists() {
            fs::remove_file(&dest).map_err(|e| format!("старий exe зайнятий іншим процесом ({e})"))?;
        }
        if fs::hard_link(&me, &dest).is_err() {
            fs::copy(&me, &dest).map_err(|e| format!("не вдалося створити exe: {e}"))?;
        }
    }
    Ok(dest)
}

pub struct Manager {
    pub games: Arc<Vec<Game>>,
    pub runs: Vec<Run>,
    pub active_tasks: Vec<QuestTask>,
    /// Tasks the user stopped: their workers must not bring them back to life.
    cancelled_tasks: HashSet<String>,
    /// Game id -> number of quest tasks currently using it.
    quest_games: HashMap<String, usize>,
    /// id -> seconds played.
    pub played: HashMap<String, u64>,
    pub log: VecDeque<String>,
    job: Option<Job>,
    dirty: bool,
    last_save: Instant,
}

impl Manager {
    pub fn new() -> Self {
        let played = crate::games::read_data("played.json")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            games: Arc::new(Vec::new()),
            runs: Vec::new(),
            active_tasks: Vec::new(),
            cancelled_tasks: HashSet::new(),
            quest_games: HashMap::new(),
            played,
            log: VecDeque::new(),
            job: Job::new(),
            dirty: false,
            last_save: Instant::now(),
        }
    }

    pub fn msg(&mut self, s: impl Into<String>) {
        if self.log.len() >= 60 {
            self.log.pop_front();
        }
        self.log.push_back(s.into());
    }

    pub fn played_secs(&self, idx: usize) -> u64 {
        self.games.get(idx).and_then(|g| self.played.get(&g.id)).copied().unwrap_or(0)
    }

    pub fn is_running_id(&self, id: &str) -> bool {
        self.runs.iter().any(|r| r.id == id)
    }

    pub fn is_running(&self, idx: usize) -> bool {
        self.games.get(idx).is_some_and(|g| self.is_running_id(&g.id))
    }

    /// Starts a game from the list by index (prepares the exe under the lock).
    pub fn start(&mut self, idx: usize, dur: Option<Duration>) -> bool {
        let Some(g) = self.games.get(idx) else { return false };
        let info = LaunchInfo::of(g);
        if self.is_running_id(&info.id) {
            return true;
        }
        let prepared = prepare(&info);
        self.launch(info, prepared, dur, Origin::Manual)
    }

    /// Starts a launch made ready by [`prepare`] and registers the run.
    pub fn launch(&mut self, info: LaunchInfo, prepared: Result<Prepared, String>, dur: Option<Duration>, origin: Origin) -> bool {
        if self.is_running_id(&info.id) {
            return true;
        }
        let res = prepared.and_then(|p| match p {
            Prepared::Rpc(s) => Ok(Backend::Rpc(s)),
            Prepared::Exe(dest) => {
                let mut cmd = Command::new(&dest);
                if let Some(a) = &info.args {
                    // Passed verbatim: Discord compares the command line as a string.
                    cmd.raw_arg(a);
                }
                cmd.current_dir(dest.parent().unwrap_or(Path::new(".")))
                    .env(crate::DUMMY_ENV, "1")
                    .env(crate::TITLE_ENV, &info.name)
                    .creation_flags(CREATE_NO_WINDOW | IDLE_PRIORITY_CLASS)
                    .spawn()
                    .map_err(|e| e.to_string())
                    .map(|child| {
                        if let Some(j) = &self.job {
                            j.add(&child);
                        }
                        Backend::Process(child)
                    })
            }
        });
        match res {
            Ok(backend) => {
                let now = Instant::now();
                if origin != Origin::Farm {
                    self.msg(format!("▶ {}", info.name));
                }
                self.runs.push(Run {
                    id: info.id,
                    name: info.name,
                    exe_key: info.exe_key,
                    origin,
                    backend,
                    started: now,
                    until: dur.map(|d| now + d),
                    credited: 0,
                });
                true
            }
            Err(e) => {
                self.msg(format!("✖ {}: {e}", info.name));
                false
            }
        }
    }

    fn credit(&mut self, i: usize) {
        let r = &mut self.runs[i];
        let e = r.started.elapsed().as_secs();
        let add = e - r.credited;
        if add > 0 {
            r.credited = e;
            *self.played.entry(r.id.clone()).or_insert(0) += add;
            self.dirty = true;
        }
    }

    /// Stops the run with index `i` in `runs`.
    pub fn stop(&mut self, i: usize) {
        if i >= self.runs.len() {
            return;
        }
        self.credit(i);
        let r = self.runs.swap_remove(i);
        if let Backend::Process(mut child) = r.backend {
            let _ = child.kill();
            let _ = child.wait();
        } // An Rpc session clears the presence when dropped.
        if r.origin != Origin::Farm {
            self.msg(format!("■ {}", r.name));
        }
    }

    /// Stops the game with this id, if it is running.
    pub fn stop_id(&mut self, id: &str) {
        if let Some(i) = self.runs.iter().position(|r| r.id == id) {
            self.stop(i);
        }
    }

    /// Adds or replaces a task. Updates for tasks the user stopped are ignored.
    pub fn update_task(&mut self, task: QuestTask) {
        if self.cancelled_tasks.contains(&task.id) {
            return;
        }
        if let Some(pos) = self.active_tasks.iter().position(|t| t.id == task.id) {
            self.active_tasks[pos] = task;
        } else {
            self.active_tasks.push(task);
        }
    }

    pub fn cancel_task(&mut self, id: &str) {
        self.cancelled_tasks.insert(id.to_string());
        if let Some(t) = self.active_tasks.iter_mut().find(|t| t.id == id && t.state.is_live()) {
            t.state = TaskState::Stopped;
            t.detail = "Зупинено вручну".into();
        }
    }

    /// Quest tasks that still have work ahead (queued or running).
    pub fn live_tasks(&self) -> usize {
        self.active_tasks.iter().filter(|t| t.state.is_live()).count()
    }

    /// Games started by hand (quest games are shown on their quest cards).
    pub fn manual_runs(&self) -> impl Iterator<Item = (usize, &Run)> {
        self.runs.iter().enumerate().filter(|(_, r)| r.origin == Origin::Manual)
    }

    /// A quest task starts using a game (several quests may share one).
    pub fn hold_game(&mut self, id: &str) {
        *self.quest_games.entry(id.to_string()).or_insert(0) += 1;
    }

    /// A quest task is done with a game. When no other quest needs it, a game that was
    /// started for quests is closed; one started by hand keeps running.
    pub fn release_game(&mut self, id: &str) {
        let left = self.quest_games.get_mut(id).map_or(0, |n| {
            *n = n.saturating_sub(1);
            *n
        });
        if left == 0 {
            self.quest_games.remove(id);
            if self.runs.iter().any(|r| r.id == id && r.origin == Origin::Quest) {
                self.stop_id(id);
            }
        }
    }

    pub fn is_task_cancelled(&self, id: &str) -> bool {
        self.cancelled_tasks.contains(id)
    }

    /// Called when a new quest queue starts: earlier stops don't apply to it.
    pub fn reset_cancelled(&mut self) {
        self.cancelled_tasks.clear();
    }

    /// Removes finished cards (done, skipped, stopped, failed).
    pub fn clear_completed_tasks(&mut self) {
        self.active_tasks.retain(|t| t.state.is_live());
    }

    pub fn stop_all(&mut self) {
        while !self.runs.is_empty() {
            self.stop(self.runs.len() - 1);
        }
        let ids: Vec<String> = self.active_tasks.iter().map(|t| t.id.clone()).collect();
        for id in ids {
            self.cancel_task(&id);
        }
        self.save();
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        let mut i = self.runs.len();
        while i > 0 {
            i -= 1;
            self.credit(i);
            let r = &mut self.runs[i];
            let expired = r.until.is_some_and(|u| now >= u);
            let dead = match &mut r.backend {
                Backend::Process(c) => matches!(c.try_wait(), Ok(Some(_))),
                Backend::Rpc(s) => !s.alive(),
            };
            if expired || dead {
                self.stop(i);
            }
        }
        // Save every 30 s, and right away once the last game has stopped.
        if self.dirty && (self.runs.is_empty() || self.last_save.elapsed() > Duration::from_secs(30)) {
            self.save();
        }
    }

    pub fn save(&mut self) {
        if let Ok(s) = serde_json::to_string(&self.played) {
            let _ = fs::write(data_path("played.json"), s);
        }
        self.dirty = false;
        self.last_save = Instant::now();
    }

    pub fn reset_stats(&mut self) {
        self.played.clear();
        self.dirty = true;
        self.save();
    }
}

pub fn spawn_ticker(m: Arc<Mutex<Manager>>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let mut m = lock(&m);
        if !m.runs.is_empty() || m.dirty {
            m.tick();
        }
    });
}
