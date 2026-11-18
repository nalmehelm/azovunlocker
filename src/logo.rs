//! Embedded pictures. All of them are prepared at build time (build.rs), so the
//! program spends no time decoding at runtime.

/// Window and taskbar icon, 64×64 RGBA (azovchan, from assets/azovchan.png).
pub const ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/icon64.rgba"));
pub const ICON_SIZE: u32 = 64;

/// Top-bar pictures (white background already removed), PLUSH_SIZE×PLUSH_SIZE RGBA.
pub const AZOV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/azovchan.rgba"));
pub const MIGUEL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/miguelchan.rgba"));
pub const PLUSH_SIZE: usize = 160;
