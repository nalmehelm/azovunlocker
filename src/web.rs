//! Hidden Discord web client (system WebView2) for automatic stream watching.
//!
//! Runs as a separate process of our own exe (`DU_WEB=1`): one hidden WebView2 browser with a
//! page per account, each in its own profile. The token goes into the page's localStorage
//! before Discord's scripts start, so it logs in without a form.
//!
//! The automation does not press buttons (their labels depend on the language and change
//! with updates): every tick [`STEP_JS`] reads Discord's own stores and calls its own actions
//! (join the DM call, start / watch the stream). Its answer is printed to stdout, where the
//! main program reads it. The "screen" being streamed is a canvas inside the page, so
//! nothing appears on the user's desktop and the microphone is never opened.

use std::{io::Write, sync::mpsc, time::Duration};
use webview2_com::{Microsoft::Web::WebView2::Win32::*, *};
use windows::{
    core::{w, Interface, PCWSTR},
    Win32::{
        Foundation::{E_POINTER, HWND, LPARAM, LRESULT, RECT, WPARAM},
        System::{
            Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
            LibraryLoader::GetModuleHandleW,
        },
        UI::WindowsAndMessaging::*,
    },
};

pub const ROLE_ENV: &str = "DU_WEB";
pub const ALT_TOKEN_ENV: &str = "DU_WEB_ALT";
pub const MAIN_TOKEN_ENV: &str = "DU_WEB_MAIN";
pub const DIR_ENV: &str = "DU_WEB_DIR";

/// Check interval while setting up, and once everything runs.
const FAST: Duration = Duration::from_secs(2);
const SLOW: Duration = Duration::from_secs(10);
/// Discord's web client grows over hours; a reload brings it back to its start size. The
/// second page is offset by half of this, so the two never reload together.
const RELOAD_EVERY: Duration = Duration::from_secs(4 * 3600);

/// Runs in every page before Discord's scripts: logs in with the token and replaces the
/// capture devices. Screen share gets a 1 fps canvas with a clock (a still picture costs
/// almost nothing to encode and send), the microphone is silence.
const INIT_JS: &str = r#"if (location.hostname === 'discord.com') {
try { localStorage.setItem('token', JSON.stringify(__TOKEN__)); } catch (e) {}
// Keep the WebRTC connections, to check that video frames really flow.
if (window.RTCPeerConnection) {
  const PC = window.RTCPeerConnection, pcs = window.__duPCs = [];
  window.RTCPeerConnection = function (...a) { const pc = new PC(...a); pcs.push(pc); return pc; };
  window.RTCPeerConnection.prototype = PC.prototype;
  Object.setPrototypeOf(window.RTCPeerConnection, PC);
}
const md = navigator.mediaDevices;
if (md) {
  const canvas = (w, h) => {
    const c = document.createElement('canvas'); c.width = w; c.height = h;
    const g = c.getContext('2d'); const t0 = Date.now();
    const draw = () => {
      const s = Math.floor((Date.now() - t0) / 1000), p = n => String(n).padStart(2, '0');
      g.fillStyle = '#100e20'; g.fillRect(0, 0, w, h); g.textAlign = 'center';
      g.fillStyle = '#8088ea'; g.font = `bold ${h / 12}px Segoe UI`; g.fillText('azovunlocker', w / 2, h / 2 - h / 20);
      g.fillStyle = '#d3d6f6'; g.font = `${h / 9}px Segoe UI`;
      g.fillText(`${p(Math.floor(s / 3600))}:${p(Math.floor(s / 60) % 60)}:${p(s % 60)}`, w / 2, h / 2 + h / 8);
    };
    draw(); setInterval(draw, 1000);
    return c.captureStream(1);
  };
  const silence = () => { const a = new AudioContext(); return a.createMediaStreamDestination().stream; };
  // Discord keeps screen sources by their track label; a canvas track has none.
  md.getDisplayMedia = async () => {
    const s = canvas(640, 360);
    s.getVideoTracks().forEach(t => Object.defineProperty(t, 'label', { value: 'azovstream-' + Date.now() }));
    return s;
  };
  md.getUserMedia = async (c) => {
    const out = new MediaStream();
    if (c && c.audio) silence().getAudioTracks().forEach(t => out.addTrack(t));
    if (c && c.video) canvas(320, 180).getVideoTracks().forEach(t => out.addTrack(t));
    return out;
  };
}
}"#;

/// One automation step. `__ROLE__`: "alt" (joins the call and streams) or "main" (joins and
/// watches); `__PEER__`: the other account's user id. Returns the current state.
const STEP_JS: &str = r#"(async () => {
const role = "__ROLE__", peer = "__PEER__";
if (document.querySelector('input[name="email"],input[name="password"]')) return 'login';
if (!window.webpackChunkdiscord_app) return 'loading';
let D = window.__du;
if (!D) {
  const req = webpackChunkdiscord_app.push([[Symbol()], {}, r => r]); webpackChunkdiscord_app.pop();
  const vals = [];
  for (const m of Object.values(req.c)) {
    const e = m.exports; if (!e || typeof e !== 'object') continue;
    for (const k of Object.keys(e)) {
      let v; try { v = e[k]; if (!v || v.__duNope !== undefined) continue; } catch (_) { continue; }
      vals.push(v);
    }
  }
  const store = n => vals.find(v => { try { return typeof v.getName === 'function' && v.getName() === n; } catch (_) { return false; } });
  const prop = p => vals.find(v => { try { return Object.prototype.hasOwnProperty.call(v, p) && typeof v[p] === 'function'; } catch (_) { return false; } });
  const src = re => {
    for (const v of vals) {
      let fs = [];
      try { fs = typeof v === 'function' ? [v] : typeof v === 'object' ? Object.values(v) : []; } catch (_) {}
      for (const f of fs) {
        if (typeof f !== 'function') continue;
        try { const s = Function.prototype.toString.call(f); if (s.length < 3000 && re.test(s)) return f; } catch (_) {}
      }
    }
  };
  const d = {
    users: store('UserStore'), channels: store('ChannelStore'), selected: store('SelectedChannelStore'),
    streams: store('ApplicationStreamingStore'), rtc: store('StreamRTCConnectionStore'), media: store('MediaEngineStore'),
    rel: store('RelationshipStore'), friend: prop('addRelationship'),
    voice: prop('selectVoiceChannel'), dm: prop('openPrivateChannel'), audio: prop('toggleSelfMute'),
    start: src(/type:"STREAM_START"/), watch: src(/type:"STREAM_WATCH"/), stop: src(/type:"STREAM_STOP"/),
  };
  const missing = Object.keys(d).filter(k => !d[k]);
  // Some modules load late: try again on the next tick.
  if (missing.length) return 'loading ' + missing.join(',');
  D = window.__du = d;
}
if (!D.users.getCurrentUser()) return 'loading';
// Discord only lets friends call each other: become friends first (once, it stays).
// Both sides do the same: send a request, or accept the incoming one; two crossing
// requests become a friendship by themselves. At most one try a minute.
const rt = D.rel.getRelationshipType(peer);
if (rt !== 1) {
  if (rt === 2) return 'blocked';
  const f = window.__duFriend || (window.__duFriend = {});
  if (f.err && Date.now() - f.errAt < 300000) return f.err;
  if (rt === 4) return 'waiting-friend';
  if (!f.at || Date.now() - f.at > 60000) {
    f.at = Date.now(); f.err = null;
    Promise.resolve(D.friend.addRelationship({ userId: peer, context: { location: 'Friends' }, confirmStrangerRequest: true })).catch(e => {
      const b = (e && (e.body || e.data)) || e || {};
      f.err = b.captcha_key || b.captcha_sitekey ? 'captcha' : 'friend-error ' + String(b.message || b.code || e).slice(0, 120);
      f.errAt = Date.now();
    });
  }
  return rt === 3 ? 'accepting-friend' : 'adding-friend';
}
const ch =D.channels.getDMFromUserId(peer);
if (!ch) {
  if (role === 'alt') { D.dm.openPrivateChannel({ recipientIds: [peer], navigateToChannel: true }); return 'opening-dm'; }
  return 'waiting-dm';
}
if (D.selected.getVoiceChannelId() !== ch) { D.voice.selectVoiceChannel(ch); return 'joining'; }
// Nobody needs to hear anything: stay muted.
if (!D.media.isSelfMute()) D.audio.toggleSelfMute({ playSoundEffect: false });
// Video frames sent (alt) or decoded (main) over all connections. "Flowing" = the count
// grew in the last 30 s; a fresh stream gets the same 30 s to start.
const frames = async () => {
  let n = 0;
  for (const pc of window.__duPCs || []) {
    if (pc.connectionState === 'closed') continue;
    try {
      (await pc.getStats()).forEach(s => {
        if (s.kind !== 'video') return;
        if (role === 'alt' && s.type === 'outbound-rtp') n += s.framesEncoded || 0;
        if (role === 'main' && s.type === 'inbound-rtp') n += s.framesDecoded || 0;
      });
    } catch (_) {}
  }
  return n;
};
const flowing = async () => {
  const n = await frames(), h = window.__duFlow || (window.__duFlow = { n: -1, at: Date.now() });
  if (n > h.n) { h.n = n; h.at = Date.now(); }
  return Date.now() - h.at < 30000;
};
const fresh = () => { window.__duFlow = null; };
if (role === 'alt') {
  const mine = D.streams.getCurrentUserActiveStream();
  if (mine) {
    if (await flowing()) return 'streaming';
    // A stream without a picture: end it, the next step starts a new one.
    D.stop(`call:${ch}:${D.users.getCurrentUser().id}`);
    fresh();
    return 'no-video';
  }
  // As the web client does it: pick a "screen" (our canvas), then go live with it.
  const g = window.__duGoLive || 0;
  if (Date.now() - g < 15000) return 'go-live';
  window.__duGoLive = Date.now();
  fresh();
  const source = await D.media.getMediaEngine().getDesktopSource({ width: 640, height: 360 }, false);
  D.start(null, ch, { pid: null, sourceId: source, sourceName: null });
  return 'go-live';
}
const s = D.streams.getAllActiveStreamsForChannel(ch).find(x => x.ownerId === peer);
if (!s) { fresh(); return 'waiting-stream'; }
const key = `call:${ch}:${peer}`;
if (D.rtc.getAllActiveStreamKeys().includes(key)) {
  if (await flowing()) return 'watching';
  // Connected but no picture (e.g. a stale stream after a restart): leave and watch again.
  D.stop(key);
  fresh();
  return 'no-video';
}
fresh();
D.watch(s, {});
return 'opening-stream';
})()"#;

fn say(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{s}");
    let _ = out.flush();
}

/// Posted by the stdin thread: leave the call and exit.
const WM_QUIT_GRACEFUL: u32 = WM_APP + 1;
const QUIT_TIMER: usize = 9;
/// Leaves the voice call (the stream ends with it).
const LEAVE_JS: &str = "try { window.__du && window.__du.voice.selectVoiceChannel(null); } catch (e) {}";

extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

pub fn host() -> ! {
    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            say(&format!("error {e}"));
            1
        }
    };
    std::process::exit(code)
}

fn werr<E: std::fmt::Debug>(e: E) -> String {
    format!("{e:?}")
}

/// One account's page.
struct Page {
    role: &'static str,
    // Keeps the page alive.
    _controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    /// `Runtime.evaluate` parameters of its step.
    params: String,
    last: String,
    interval: Duration,
    /// Last (re)load: the page is reloaded every [RELOAD_EVERY].
    loaded: std::time::Instant,
}

/// Creates one page in its own profile (separate storage, so a separate login) of the
/// shared browser, logs it in with `token` and opens Discord.
fn page(environment: &ICoreWebView2Environment, hwnd: HWND, role: &'static str, token: &str, peer: &str) -> std::result::Result<Page, String> {
    let env10: ICoreWebView2Environment10 = environment.cast().map_err(|_| "WebView2 застарий: оновіть Microsoft Edge WebView2".to_string())?;
    let options = unsafe { env10.CreateCoreWebView2ControllerOptions() }.map_err(werr)?;
    let name = CoTaskMemPWSTR::from(role);
    unsafe { options.SetProfileName(*name.as_ref().as_pcwstr()) }.map_err(werr)?;
    let controller = {
        let (tx, rx) = mpsc::channel();
        CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
            Box::new(move |h| unsafe {
                env10.CreateCoreWebView2ControllerWithOptions(hwnd, &options, &h).map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |err, c| {
                err?;
                let _ = tx.send(c.ok_or_else(|| windows::core::Error::from(E_POINTER)));
                Ok(())
            }),
        )
        .map_err(werr)?;
        rx.recv().map_err(werr)?.map_err(werr)?
    };

    let webview = unsafe {
        controller.SetBounds(RECT { left: 0, top: 0, right: 1000, bottom: 640 }).map_err(werr)?;
        // Nobody looks at these pages: an invisible WebView draws nothing at all, while the
        // browser arguments keep its timers, network and WebRTC at full speed.
        // DU_WEB_DEBUG keeps it drawn, for screenshots.
        controller.SetIsVisible(std::env::var_os("DU_WEB_DEBUG").is_some() && std::env::var_os("DU_WEB_HIDE").is_none()).map_err(werr)?;
        let wv = controller.CoreWebView2().map_err(werr)?;
        if let Ok(s) = wv.Settings() {
            let _ = s.SetAreDefaultScriptDialogsEnabled(false);
            let _ = s.SetIsStatusBarEnabled(false);
        }
        // Microphone / screen: the page only ever gets our silent and canvas streams.
        let mut tok = 0;
        wv.add_PermissionRequested(
            &PermissionRequestedEventHandler::create(Box::new(|_, args| {
                if let Some(a) = args {
                    a.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)?;
                }
                Ok(())
            })),
            &mut tok,
        )
        .map_err(werr)?;
        // Nobody looks at the page: let the browser give memory back as hard as it can.
        if let Ok(w19) = wv.cast::<ICoreWebView2_19>() {
            let _ = w19.SetMemoryUsageTargetLevel(COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW);
        }
        // ...and load nothing that is only for the eyes (avatars, emoji, GIFs, fonts, media
        // files), nor Discord's analytics. Each of these costs memory, CPU and traffic.
        let w22 = wv.cast::<ICoreWebView2_22>().ok();
        for (uri, ctx) in [
            ("*", COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE),
            ("*", COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA),
            ("*", COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT),
            ("*", COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING),
            ("*://discord.com/api/v*/science*", COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL),
            ("*://discord.com/api/v*/metrics*", COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL),
            ("*sentry.io/*", COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL),
        ] {
            let u = CoTaskMemPWSTR::from(uri);
            let u = *u.as_ref().as_pcwstr();
            let _ = match &w22 {
                Some(w) => w.AddWebResourceRequestedFilterWithRequestSourceKinds(u, ctx, COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL),
                None => wv.AddWebResourceRequestedFilter(u, ctx),
            };
        }
        let env = environment.clone();
        let mut tok = 0;
        wv.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                if let Some(a) = args {
                    let r = env.CreateWebResourceResponse(None::<&windows::Win32::System::Com::IStream>, 204, w!("No Content"), w!(""))?;
                    a.SetResponse(&r)?;
                }
                Ok(())
            })),
            &mut tok,
        )
        .map_err(werr)?;
        wv
    };

    let init = INIT_JS.replace("__TOKEN__", &serde_json::to_string(token).map_err(werr)?);
    {
        let wv = webview.clone();
        AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
            Box::new(move |h| unsafe {
                let js = CoTaskMemPWSTR::from(init.as_str());
                wv.AddScriptToExecuteOnDocumentCreated(*js.as_ref().as_pcwstr(), &h).map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(|err, _| err),
        )
        .map_err(werr)?;
    }
    unsafe {
        let u = CoTaskMemPWSTR::from("https://discord.com/channels/@me");
        webview.Navigate(*u.as_ref().as_pcwstr()).map_err(werr)?;
    }
    // Through the DevTools protocol with userGesture, so the page treats the step like a
    // user action (some media APIs require one).
    let params = serde_json::json!({
        "expression": STEP_JS.replace("__ROLE__", role).replace("__PEER__", peer),
        "userGesture": true,
        "awaitPromise": true,
        "returnByValue": true,
    })
    .to_string();
    Ok(Page { role, _controller: controller, webview, params, last: String::new(), interval: FAST, loaded: std::time::Instant::now() })
}

fn run() -> std::result::Result<(), String> {
    let env = |k: &str| std::env::var(k).map_err(|_| format!("{k} not set"));
    let alt_token = env(ALT_TOKEN_ENV)?;
    let main_token = env(MAIN_TOKEN_ENV)?;
    let dir = env(DIR_ENV)?;
    let id = |t: &str| crate::stream::token_user_id(t).ok_or("bad token".to_string());
    let (alt_id, main_id) = (id(&alt_token)?, id(&main_token)?);

    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().map_err(werr)? };

    // Parent window: off-screen, not in the taskbar, never takes focus.
    let hwnd = unsafe {
        let hinst = GetModuleHandleW(None).map_err(werr)?;
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: w!("DUWeb"), ..Default::default() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("DUWeb"),
            w!("DUWeb"),
            WS_POPUP,
            -32000,
            -32000,
            1000,
            640,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .map_err(werr)?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        hwnd
    };

    // One browser for both accounts: the browser, GPU, network, audio and storage
    // processes are shared, only the page processes are per account.
    // The pages are never drawn. Audio and network run inside the browser process instead
    // of their own; V8 keeps its heap small. Tried and rejected (measured): --disable-gpu
    // (-15 MB but +40% CPU: video coding moves to the CPU), --lite-mode (no WebAssembly,
    // which Discord's call encryption needs), background timer throttling (no CPU gain,
    // slower start).
    let args = "--mute-audio --use-fake-ui-for-media-stream --disable-background-timer-throttling \
                --disable-renderer-backgrounding --disable-backgrounding-occluded-windows \
                --disable-features=CalculateNativeWinOcclusion,AudioServiceOutOfProcess \
                --enable-features=NetworkServiceInProcess2 --no-first-run --disable-extensions \
                --disable-component-update --js-flags=--optimize-for-size"
        .to_string();
    // For debugging: DU_WEB_DEBUG=<port> opens the DevTools protocol on localhost.
    let args = match std::env::var("DU_WEB_DEBUG") {
        Ok(p) => format!("{args} --remote-debugging-port={p} {}", std::env::var("DU_WEB_ARGS").unwrap_or_default()),
        Err(_) => args,
    };
    let options = CoreWebView2EnvironmentOptions::default();
    unsafe { options.set_additional_browser_arguments(args) };
    let options: ICoreWebView2EnvironmentOptions = options.into();

    let environment = {
        let (tx, rx) = mpsc::channel();
        let dir = CoTaskMemPWSTR::from(dir.as_str());
        CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
            Box::new(move |h| unsafe {
                CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), *dir.as_ref().as_pcwstr(), &options, &h)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |err, env| {
                err?;
                let _ = tx.send(env.ok_or_else(|| windows::core::Error::from(E_POINTER)));
                Ok(())
            }),
        )
        .map_err(werr)?;
        rx.recv().map_err(werr)?.map_err(|e| format!("WebView2: {e:?}"))?
    };

    let mut pages = [page(&environment, hwnd, "alt", &alt_token, &main_id)?, page(&environment, hwnd, "main", &main_token, &alt_id)?];
    // Offset the second page's reload schedule by half a period.
    if let Some(t) = std::time::Instant::now().checked_sub(RELOAD_EVERY / 2) {
        pages[1].loaded = t;
    }
    for (i, p) in pages.iter().enumerate() {
        say(&format!("{} started", p.role));
        unsafe { SetTimer(Some(hwnd), i + 1, p.interval.as_millis() as u32, None) };
    }

    // Stop request: "quit" on stdin, or stdin closed because the program is gone.
    {
        let h = hwnd.0 as isize;
        std::thread::spawn(move || {
            let mut line = String::new();
            while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) && !line.contains("quit") {
                line.clear();
            }
            unsafe {
                let _ = PostMessageW(Some(HWND(h as _)), WM_QUIT_GRACEFUL, WPARAM(0), LPARAM(0));
            }
        });
    }

    let (tx, rx) = mpsc::channel::<(usize, String)>();
    let mut msg = MSG::default();
    loop {
        if unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 <= 0 {
            return Ok(());
        }
        if msg.message == WM_QUIT_GRACEFUL {
            // Leave the call properly, so Discord doesn't keep a ghost session that would
            // fight with the next start; then exit.
            for p in &pages {
                let js = CoTaskMemPWSTR::from(LEAVE_JS);
                let _ = unsafe {
                    p.webview.ExecuteScript(*js.as_ref().as_pcwstr(), &ExecuteScriptCompletedHandler::create(Box::new(|_, _| Ok(()))))
                };
            }
            unsafe {
                let _ = KillTimer(Some(hwnd), 1);
                let _ = KillTimer(Some(hwnd), 2);
                SetTimer(Some(hwnd), QUIT_TIMER, 1500, None);
            }
            continue;
        }
        if msg.message == WM_TIMER && msg.hwnd == hwnd && msg.wParam.0 == QUIT_TIMER {
            return Ok(());
        }
        if msg.message == WM_TIMER && msg.hwnd == hwnd && (1..=2).contains(&msg.wParam.0) {
            let i = msg.wParam.0 - 1;
            if pages[i].loaded.elapsed() >= RELOAD_EVERY {
                pages[i].loaded = std::time::Instant::now();
                let _ = unsafe { pages[i].webview.Reload() };
                continue;
            }
            let tx = tx.clone();
            let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |err, json| {
                let state = if err.is_err() {
                    "busy".to_string()
                } else {
                    let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
                    match v["result"]["value"].as_str() {
                        Some(s) => s.to_string(),
                        None => match v["exceptionDetails"]["exception"]["description"].as_str() {
                            Some(e) => format!("js {}", e.lines().next().unwrap_or(e)),
                            None => "busy".to_string(),
                        },
                    }
                };
                let _ = tx.send((i, state));
                Ok(())
            }));
            unsafe {
                let m = CoTaskMemPWSTR::from("Runtime.evaluate");
                let p = CoTaskMemPWSTR::from(pages[i].params.as_str());
                let _ = pages[i].webview.CallDevToolsProtocolMethod(*m.as_ref().as_pcwstr(), *p.as_ref().as_pcwstr(), &handler);
            }
        } else {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        while let Ok((i, state)) = rx.try_recv() {
            let p = &mut pages[i];
            if state != p.last {
                say(&format!("{} {state}", p.role));
                p.last = state.clone();
            }
            // Settled: check rarely. Anything else: check often.
            let want = if matches!(state.as_str(), "streaming" | "watching") { SLOW } else { FAST };
            if want != p.interval {
                p.interval = want;
                unsafe { SetTimer(Some(hwnd), i + 1, want.as_millis() as u32, None) };
            }
        }
    }
}
