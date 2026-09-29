//! Forward host-local state to the active Herdr client.
//!
//! The pasteboard is the first source. Keep the watcher and transport here so
//! future sources can share the same forwarding lifecycle.

#[cfg(target_os = "macos")]
mod pasteboard;

pub fn start() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return pasteboard::start();

    #[cfg(not(target_os = "macos"))]
    Ok(())
}

pub fn daemon() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return pasteboard::daemon();

    #[cfg(not(target_os = "macos"))]
    Ok(())
}
