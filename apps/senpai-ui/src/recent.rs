//! A most-recently-used document list, persisted as plain text.
//!
//! This exists because of what the file dialog actually is. On Windows
//! rfd opens the shell's Common Item Dialog -- the real Explorer picker,
//! with its sidebar, search and places. Under Wine that COM class is
//! Wine's own reimplementation, which is a much plainer, much smaller
//! window, and no option on our side switches it: the dialog is chosen
//! by whatever is implementing the shell, not by the caller.
//!
//! So rather than fight the picker, the aim here is to need it less
//! often: the list below puts documents one click away in the Files
//! menu, and its newest entry also decides which directory the dialog
//! opens in, so a browse starts where the last one ended instead of at
//! the drive root.
//!
//! The format is deliberately boring -- one absolute path per line, UTF-8,
//! newest first -- so it can be read, edited or deleted by hand without
//! any tooling, and a corrupt file degrades to "no history" rather than
//! to an error.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// How many entries are kept on disk.
pub const CAPACITY: usize = 10;

#[derive(Default)]
pub struct Recent {
    paths: Vec<PathBuf>,
}

impl Recent {
    /// Reads the list, dropping entries that no longer resolve to a file.
    ///
    /// Pruning on load means a document on a drive that happens to be
    /// unmounted falls off the list rather than sitting in the menu as a
    /// entry that only reports an error when clicked. The trade is that
    /// such an entry is then gone for good; "it is in the menu" staying
    /// equivalent to "it opens" is worth more than that.
    pub fn load() -> Self {
        let paths = store_path()
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|text| {
                let mut paths = parse(&text);
                paths.retain(|p| p.is_file());
                paths
            })
            .unwrap_or_default();
        Self { paths }
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Records a document as the most recent one and writes the list out.
    pub fn record(&mut self, path: &Path) {
        promote(&mut self.paths, absolute(path));
        self.save();
    }

    /// The directory the file dialog should start in: the newest entry
    /// whose directory still exists. Falling through to an older entry
    /// matters when the newest document has since been moved -- the
    /// alternative is dumping the user at the drive root.
    pub fn start_dir(&self) -> Option<PathBuf> {
        self.paths
            .iter()
            .filter_map(|p| p.parent())
            .find(|dir| dir.is_dir())
            .map(Path::to_path_buf)
    }

    /// Best-effort: a reader that cannot write its history is still a
    /// working reader, so a failure here is not worth interrupting
    /// anyone over.
    fn save(&self) {
        let Some(path) = store_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let mut text = String::new();
        for p in &self.paths {
            text.push_str(&p.to_string_lossy());
            text.push('\n');
        }
        let _ = fs::write(path, text);
    }
}

/// Moves `path` to the front, removing any earlier occurrence of it, and
/// caps the list at [`CAPACITY`].
///
/// Split out from [`Recent::record`] so the ordering rules can be tested
/// without touching the filesystem.
fn promote(paths: &mut Vec<PathBuf>, path: PathBuf) {
    paths.retain(|p| p != &path);
    paths.insert(0, path);
    paths.truncate(CAPACITY);
}

/// One path per line; blank lines and duplicates are dropped. Anything
/// else is taken at face value -- a path is whatever the platform says
/// it is, and second-guessing it here would reject valid ones.
fn parse(text: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let path = PathBuf::from(line);
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// `std::path::absolute` rather than `canonicalize`: it is purely
/// lexical, so it works for a path whose file was just deleted, and on
/// Windows it does not hand back a `\\?\` verbatim path.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `%APPDATA%\senpai\recent` on Windows, `$XDG_CONFIG_HOME/senpai/recent`
/// (or `~/.config`) elsewhere.
///
/// Under Wine these resolve to the prefix's own AppData, so a Wine build
/// and a native build keep separate lists -- which is correct, since
/// their paths are not interchangeable anyway.
fn store_path() -> Option<PathBuf> {
    let dir = if cfg!(windows) {
        PathBuf::from(env::var_os("APPDATA")?)
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?
    };
    Some(dir.join("senpai").join("recent"))
}

/// Where to point the dialog when there is no history at all: the
/// user's documents, then their downloads, then their home. Each is
/// checked for existence so a locale or a trimmed-down profile that
/// lacks one falls through instead of landing nowhere.
pub fn default_start_dir() -> Option<PathBuf> {
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)?;
    for candidate in ["Documents", "Downloads"] {
        let dir = home.join(candidate);
        if dir.is_dir() {
            return Some(dir);
        }
    }
    home.is_dir().then_some(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn promote_moves_an_existing_entry_to_the_front() {
        let mut paths = vec![p("/a.pdf"), p("/b.pdf"), p("/c.pdf")];
        promote(&mut paths, p("/c.pdf"));
        assert_eq!(paths, vec![p("/c.pdf"), p("/a.pdf"), p("/b.pdf")]);
    }

    #[test]
    fn promote_does_not_duplicate() {
        let mut paths = vec![p("/a.pdf")];
        promote(&mut paths, p("/a.pdf"));
        assert_eq!(paths, vec![p("/a.pdf")]);
    }

    #[test]
    fn promote_caps_the_list() {
        let mut paths: Vec<PathBuf> = (0..CAPACITY).map(|i| p(&format!("/{i}.pdf"))).collect();
        promote(&mut paths, p("/new.pdf"));
        assert_eq!(paths.len(), CAPACITY);
        assert_eq!(paths[0], p("/new.pdf"));
        // The oldest entry, not an arbitrary one, is what gets dropped.
        assert!(!paths.contains(&p(&format!("/{}.pdf", CAPACITY - 1))));
    }

    #[test]
    fn parse_skips_blanks_and_duplicates() {
        let text = "/a.pdf\n\n  /b.pdf  \n/a.pdf\n";
        assert_eq!(parse(text), vec![p("/a.pdf"), p("/b.pdf")]);
    }

    #[test]
    fn parse_of_junk_yields_no_entries_rather_than_failing() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n   \n").is_empty());
    }

    #[test]
    fn start_dir_skips_entries_whose_directory_is_gone() {
        let here = std::env::current_dir().unwrap();
        let recent = Recent {
            paths: vec![
                p("/nonexistent-dir-for-test/gone.pdf"),
                here.join("real.pdf"),
            ],
        };
        assert_eq!(recent.start_dir(), Some(here));
    }

    #[test]
    fn start_dir_is_none_without_history() {
        assert_eq!(Recent::default().start_dir(), None);
    }
}
