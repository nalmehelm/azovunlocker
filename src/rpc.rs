//! Discord Rich Presence over the local IPC pipe, for games Discord knows without an exe
//! (console, SDK, launcher-only games): we connect as the game's application id and set an
//! activity, so the desktop client shows "Playing <game>" and counts it like a real session.
//!
//! The pipe is a plain file handle. Reads never block: we wait with `PeekNamedPipe` until
//! the bytes are there (with a timeout), so a silent Discord can't hang the program.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::windows::io::AsRawHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
/// Longest wait for an answer from Discord.
const TIMEOUT: Duration = Duration::from_secs(5);

fn pipe_name(i: u32) -> String {
    format!(r"\\.\pipe\discord-ipc-{i}")
}

/// Is a desktop Discord listening? Checked without taking a pipe instance.
pub fn available() -> bool {
    use windows_sys::Win32::{Foundation::GetLastError, System::Pipes::WaitNamedPipeW};
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    (0..10).any(|i| {
        let name: Vec<u16> = pipe_name(i).encode_utf16().chain([0]).collect();
        // Busy (all instances taken) still means it exists.
        unsafe { WaitNamedPipeW(name.as_ptr(), 1) != 0 || GetLastError() != ERROR_FILE_NOT_FOUND }
    })
}

/// One game's presence. Dropping it clears the activity and closes the pipe.
pub struct Session {
    pipe: File,
}

impl Session {
    /// Connects as application `client_id` and shows it as being played since now.
    pub fn start(client_id: &str) -> Result<Self, String> {
        let pipe = (0..10)
            .find_map(|i| OpenOptions::new().read(true).write(true).open(pipe_name(i)).ok())
            .ok_or("Discord не запущено (немає RPC)")?;
        let mut s = Self { pipe };
        s.send(OP_HANDSHAKE, &serde_json::json!({ "v": 1, "client_id": client_id }))?;
        match s.recv()? {
            (OP_FRAME, v) if v["evt"] == "READY" => {}
            (_, v) => return Err(error_text(&v, "Discord не прийняв з'єднання")),
        }
        let start = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        s.send(OP_FRAME, &activity(serde_json::json!({ "timestamps": { "start": start }, "instance": true })))?;
        match s.recv()? {
            (OP_FRAME, v) if v["evt"] != "ERROR" => Ok(s),
            (_, v) => Err(error_text(&v, "Discord відхилив активність")),
        }
    }

    /// False once Discord has closed the connection (restarted, quit).
    pub fn alive(&self) -> bool {
        self.peek().is_some()
    }

    /// Bytes waiting in the pipe, or None if it's broken.
    fn peek(&self) -> Option<u32> {
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;
        let mut avail = 0u32;
        let ok = unsafe {
            PeekNamedPipe(self.pipe.as_raw_handle() as _, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut())
        };
        (ok != 0).then_some(avail)
    }

    fn send(&mut self, op: u32, body: &serde_json::Value) -> Result<(), String> {
        let body = body.to_string();
        let mut buf = Vec::with_capacity(8 + body.len());
        buf.extend_from_slice(&op.to_le_bytes());
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(body.as_bytes());
        self.pipe.write_all(&buf).map_err(|e| format!("RPC: {e}"))
    }

    /// Reads exactly `n` bytes once they have arrived (never blocks past [`TIMEOUT`]).
    fn read_exact(&mut self, n: usize, until: Instant) -> Result<Vec<u8>, String> {
        loop {
            match self.peek() {
                None => return Err("Discord закрив з'єднання".into()),
                Some(a) if a as usize >= n => break,
                Some(_) if Instant::now() >= until => return Err("Discord не відповідає".into()),
                Some(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        let mut buf = vec![0; n];
        self.pipe.read_exact(&mut buf).map_err(|e| format!("RPC: {e}"))?;
        Ok(buf)
    }

    fn recv(&mut self) -> Result<(u32, serde_json::Value), String> {
        let until = Instant::now() + TIMEOUT;
        let h = self.read_exact(8, until)?;
        let op = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
        if len > 1 << 16 {
            return Err("RPC: завеликий пакет".into());
        }
        let body = self.read_exact(len, until)?;
        Ok((op, serde_json::from_slice(&body).unwrap_or_default()))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Discord drops the activity with the connection anyway; clearing first makes it
        // disappear at once instead of after Discord notices the closed pipe.
        let _ = self.send(OP_FRAME, &activity(serde_json::Value::Null));
        let _ = self.send(OP_CLOSE, &serde_json::json!({}));
    }
}

/// SET_ACTIVITY command for our process.
fn activity(a: serde_json::Value) -> serde_json::Value {
    static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    serde_json::json!({
        "cmd": "SET_ACTIVITY",
        "args": { "pid": std::process::id(), "activity": a },
        "nonce": n.to_string(),
    })
}

/// Discord's error message from an ERROR / close frame.
fn error_text(v: &serde_json::Value, fallback: &str) -> String {
    v.pointer("/data/message")
        .or_else(|| v.get("message"))
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| fallback.to_string(), |m| format!("{fallback}: {m}"))
}
