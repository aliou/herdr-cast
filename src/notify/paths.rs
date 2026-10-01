//! Where the bundled `HerdrNotify*.app` binaries live. Plugin-context code
//! (hooks) resolves them from `HERDR_PLUGIN_ROOT`/cwd; client-context code
//! (`forward-notify`, the bridge host daemon) resolves them from the
//! canonicalized executable or falls back to the plugin root.

use std::fs;
use std::path::PathBuf;

pub(crate) struct Paths {
    pub(crate) state: PathBuf,
    pub(crate) assets: PathBuf,
}

impl Paths {
    pub(crate) fn from_environment() -> Result<Self, String> {
        let root = std::env::var_os("HERDR_PLUGIN_ROOT")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .ok_or_else(|| "plugin root is unavailable".to_string())?;
        let state = state_dir();
        let assets = root.join("assets");
        Ok(Self { state, assets })
    }

    /// The app bundle rendering notifications for a given status variant
    /// (`None` = the neutral bundle for pass-through and Herdr's own
    /// toasts).
    pub(crate) fn app(&self, variant: Option<&str>) -> PathBuf {
        match variant {
            None => self.assets.join("HerdrNotify.app"),
            Some(variant) => self.assets.join(format!("HerdrNotify-{variant}.app")),
        }
    }

    pub(crate) fn notifier(&self, variant: Option<&str>) -> PathBuf {
        self.app(variant).join("Contents/MacOS/terminal-notifier")
    }

    /// Resolve the bundled app for `forward-notify`, which runs in client
    /// context (invoked via PATH) and cannot rely on `HERDR_PLUGIN_ROOT`.
    /// The Nix package installs `bin/herdr-cast` and
    /// `libexec/HerdrNotify.app` side by side, so the app lives two
    /// directories up from the resolved executable. Canonicalizing matters:
    /// the binary is usually reached through the `~/.local/bin/herdr-cast`
    /// symlink, and `current_exe` may report the link path.
    pub(crate) fn from_executable() -> Result<Self, String> {
        let exe = std::env::current_exe()
            .map_err(|error| format!("failed to locate herdr-cast executable: {error}"))?;
        let exe = fs::canonicalize(&exe).unwrap_or(exe);
        let prefix = exe
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or_else(|| format!("unexpected executable location: {}", exe.display()))?;
        let assets = prefix.join("libexec");
        Ok(Self {
            state: state_dir(),
            assets,
        })
    }

    /// Client-context resolution for code that runs outside a plugin hook's
    /// working directory: the packaged `libexec` bundles when present, else
    /// the plugin root's `assets`.
    pub(crate) fn for_client() -> Result<Self, String> {
        let packaged = Self::from_executable()?;
        if packaged.notifier(None).is_file() {
            return Ok(packaged);
        }
        Self::from_environment()
    }
}

pub(crate) fn state_dir() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("herdr-cast"))
}
