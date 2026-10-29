//! Where the reader keeps the handful of things it remembers between
//! runs.
//!
//! Two of them now -- the recent-documents list and the navigation keys
//! -- which is why this is its own module rather than a private helper
//! inside whichever one came first.
//!
//! Everything stored is plain text in a file per setting, deliberately
//! boring, so it can be read, edited or deleted by hand without any
//! tooling and a corrupt one degrades to "no setting" rather than to an
//! error.

use std::env;
use std::fs;
use std::path::PathBuf;

/// `%APPDATA%\senpai\<name>` on Windows, `$XDG_CONFIG_HOME/senpai/<name>`
/// (or `~/.config`) elsewhere.
///
/// Under Wine these resolve to the prefix's own AppData, so a Wine build
/// and a native build keep separate settings -- which is correct for the
/// recent list, whose paths are not interchangeable, and harmless for
/// the rest.
pub fn config_path(name: &str) -> Option<PathBuf> {
    let dir = if cfg!(windows) {
        PathBuf::from(env::var_os("APPDATA")?)
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?
    };
    Some(dir.join("senpai").join(name))
}

/// Reads a setting, or `None` if it has never been written, cannot be
/// read, or there is nowhere to keep it.
pub fn read(name: &str) -> Option<String> {
    fs::read_to_string(config_path(name)?).ok()
}

/// Best-effort: a reader that cannot write its settings is still a
/// working reader, so a failure here is not worth interrupting anyone
/// over.
pub fn write(name: &str, contents: &str) {
    let Some(path) = config_path(name) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(path, contents);
}
