//! Shared plumbing for resident helper processes: a per-purpose singleton
//! lock, a detached spawn that never holds a hook's stdio open, and the
//! machine hostname.
//!
//! The singleton is an `flock` on a state-dir file. `flock` belongs to the
//! open file description, so it dies with the holder and cannot go stale.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Resident daemons truncate their log at spawn once it grows past this.
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// A held singleton lock; released when dropped or when the process exits.
pub struct Singleton {
    _file: File,
}

pub fn state_dir() -> Result<PathBuf, String> {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "HERDR_PLUGIN_STATE_DIR not set".to_string())
}

/// Take `path`'s lock without blocking. `None` means another process holds
/// it. The holder's pid is written for diagnostics only.
pub fn acquire(path: &Path) -> Result<Option<Singleton>, String> {
    let mut file = open_lock(path)?;
    if !try_lock(&file) {
        return Ok(None);
    }
    let _ = file.set_len(0);
    let _ = writeln!(file, "pid {}", std::process::id()).and_then(|_| file.flush());
    Ok(Some(Singleton { _file: file }))
}

/// Whether some process currently holds `path`'s lock. The probe lock is
/// released before returning so a spawned daemon can take it.
pub fn is_held(path: &Path) -> Result<bool, String> {
    let probe = open_lock(path)?;
    Ok(!try_lock(&probe))
}

/// Start `herdr-cast <arguments>` outside the caller's process group with
/// stdin and stdout detached, so a hook's pipes close when the hook exits.
/// Stderr goes to `log` (appended, truncated first when oversized) or to
/// `/dev/null`.
pub fn spawn_detached(arguments: &[&str], log: Option<&Path>) -> Result<(), String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("failed to resolve the herdr-cast path: {error}"))?;
    let stderr = match log {
        Some(path) => Stdio::from(open_log(path)?),
        None => Stdio::null(),
    };
    Command::new(executable)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|error| {
            format!(
                "failed to spawn herdr-cast {}: {error}",
                arguments.join(" ")
            )
        })?;
    Ok(())
}

/// The machine hostname as the kernel reports it, trimmed.
pub fn hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    let result =
        unsafe { libc::gethostname(buffer.as_mut_ptr().cast::<libc::c_char>(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let host = String::from_utf8_lossy(&buffer[..end]).trim().to_string();
    (!host.is_empty()).then_some(host)
}

/// The first DNS label, matching how the Space sidebar shortens hosts.
pub fn short_host(host: &str) -> Option<String> {
    host.split('.')
        .next()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_string)
}

fn open_lock(path: &Path) -> Result<File, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| format!("failed to open lock {}: {error}", path.display()))
}

fn try_lock(file: &File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

fn open_log(path: &Path) -> Result<File, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let oversized = std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES);
    OpenOptions::new()
        .create(true)
        .append(!oversized)
        .write(true)
        .truncate(oversized)
        .open(path)
        .map_err(|error| format!("failed to open log {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cast-daemon-{label}-{}", std::process::id()))
    }

    #[test]
    fn a_held_lock_excludes_a_second_holder_until_dropped() {
        let path = temp_path("lock");
        let _ = std::fs::remove_file(&path);
        let first = acquire(&path).unwrap();
        assert!(first.is_some(), "the first holder takes the lock");
        assert!(
            acquire(&path).unwrap().is_none(),
            "a second holder is rejected"
        );
        assert!(is_held(&path).unwrap());
        drop(first);
        assert!(!is_held(&path).unwrap(), "dropping releases the lock");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn oversized_logs_are_truncated_when_reopened() {
        let path = temp_path("log");
        std::fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        drop(open_log(&path).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        std::fs::write(&path, b"keep").unwrap();
        let mut file = open_log(&path).unwrap();
        file.write_all(b" more").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep more");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn short_host_keeps_only_the_first_label() {
        assert_eq!(
            short_host("buildbox.lab.internal").as_deref(),
            Some("buildbox")
        );
        assert_eq!(short_host("buildbox").as_deref(), Some("buildbox"));
        assert_eq!(short_host(""), None);
        assert_eq!(short_host("   "), None);
    }
}
