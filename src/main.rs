#![windows_subsystem = "windows"]
//! Discord Unlocker ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â makes Discord detect games so that unique games,
//! game hours and demo hours are counted.
//!
//! Discord detects games by the exe path of a process. For each game we create a
//! hard link to ourselves at `games/<id>/<path from the Discord list>` and start it
//! with an env var; that copy just sleeps (0% CPU, ~150 KB RAM, no window).

mod bg;
mod farm;
mod games;
mod http;
mod logo;
mod manager;
mod quests;
mod rpc;
mod stream;
mod ui;
mod web;

pub const DUMMY_ENV: &str = "DU_DUMMY";
pub const TITLE_ENV: &str = "DU_TITLE";

/// Dummy "game": a minimized tool window. Discord only sees processes that own a
/// window, while this one isn't shown on screen or on the taskbar.
fn dummy() -> ! {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let class: Vec<u16> = "DUGame\0".encode_utf16().collect();
        let title: Vec<u16> = std::env::var(TITLE_ENV)
            .unwrap_or_else(|_| "Game".into())
            .encode_utf16()
            .chain([0])
            .collect();
        let hinst = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(DefWindowProcW),
            hInstance: hinst,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            320,
            200,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
        // Message loop: blocks in GetMessage, 0% CPU.
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
        std::process::exit(0);
    }
}

fn main() -> eframe::Result {
    if std::env::var_os(DUMMY_ENV).is_some() {
        dummy();
    }
    if std::env::var_os(web::ROLE_ENV).is_some() {
        web::host();
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("azovunlocker")
            .with_inner_size([1040.0, 680.0])
            .with_min_inner_size([900.0, 560.0])
            .with_transparent(false)
            .with_icon(std::sync::Arc::new(eframe::egui::IconData {
                rgba: logo::ICON.to_vec(),
                width: logo::ICON_SIZE,
                height: logo::ICON_SIZE,
            })),
        vsync: false,
        multisampling: 0,
        depth_buffer: 0,
        stencil_buffer: 0,
        ..Default::default()
    };
    eframe::run_native(
        "azovunlocker",
        options,
        Box::new(|cc| Ok(Box::new(ui::App::new(cc)))),
    )
}
