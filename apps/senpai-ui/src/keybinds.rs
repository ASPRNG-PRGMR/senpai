//! Navigation keys: the ones that always work, and the ones a reader can
//! assign.
//!
//! Two sets, deliberately. The arrows, Page Up/Down and Space are built
//! in and cannot be reassigned, so a reader who has never opened Options
//! -- or who sits down at someone else's machine -- can always turn a
//! page. Anything a reader binds is *additional*: it never takes a key
//! away, it only adds one.
//!
//! That is what makes the feature safe to offer at all. A rebinding
//! scheme that can leave you with no way to turn a page is a trap, and
//! the usual escape from it is a "reset to defaults" button that you
//! have to find using the very keys you have broken.
//!
//! Bindings are remembered between runs, because a shortcut you have to
//! set again every launch is not a shortcut.

use crate::config;
use eframe::egui::Key;

/// Name of the file these live in. See [`crate::config`].
const STORE: &str = "keys";

/// A direction a reader can ask to move in.
///
/// Left and right turn pages; up and down scroll within what is on
/// screen. In a paged view there is nothing to scroll, so up and down
/// fall through to turning pages rather than doing nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NavTarget {
    Left,
    Right,
    Up,
    Down,
}

impl NavTarget {
    pub const ALL: [Self; 4] = [Self::Left, Self::Right, Self::Up, Self::Down];

    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Up => "Up",
            Self::Down => "Down",
        }
    }

    /// Keys that always mean this direction, whatever else is bound.
    pub fn builtins(self) -> &'static [Key] {
        match self {
            // Space goes with Right because that is what every reader
            // since the first one has done with it.
            Self::Left => &[Key::ArrowLeft, Key::PageUp],
            Self::Right => &[Key::ArrowRight, Key::PageDown, Key::Space],
            Self::Up => &[Key::ArrowUp],
            Self::Down => &[Key::ArrowDown],
        }
    }

    fn field(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// Whether a key is already spoken for by the built-in set.
pub fn is_builtin(key: Key) -> bool {
    NavTarget::ALL.iter().any(|t| t.builtins().contains(&key))
}

/// Whether a key can be assigned at all.
///
/// Escape is excluded because it is how you back out of assigning one,
/// and a built-in is excluded because binding it to a second direction
/// would make one keypress mean two contradictory things.
pub fn is_assignable(key: Key) -> bool {
    key != Key::Escape && !is_builtin(key)
}

/// A ready-made set, for the two schemes people actually ask for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preset {
    /// Home row, for anyone who has spent time in vim.
    Vim,
    /// The movement keys of every game ever shipped.
    Gamer,
}

impl Preset {
    pub fn label(self) -> &'static str {
        match self {
            Self::Vim => "Vim",
            Self::Gamer => "Gamer",
        }
    }

    pub fn keys(self) -> [(NavTarget, Key); 4] {
        match self {
            Self::Vim => [
                (NavTarget::Left, Key::J),
                (NavTarget::Right, Key::L),
                (NavTarget::Up, Key::I),
                (NavTarget::Down, Key::K),
            ],
            Self::Gamer => [
                (NavTarget::Left, Key::A),
                (NavTarget::Right, Key::D),
                (NavTarget::Up, Key::W),
                (NavTarget::Down, Key::S),
            ],
        }
    }
}

/// The reader's own bindings. Empty means "built-ins only".
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct NavKeys {
    left: Option<Key>,
    right: Option<Key>,
    up: Option<Key>,
    down: Option<Key>,
}

impl NavKeys {
    pub fn load() -> Self {
        config::read(STORE).map(|t| parse(&t)).unwrap_or_default()
    }

    pub fn get(self, target: NavTarget) -> Option<Key> {
        match target {
            NavTarget::Left => self.left,
            NavTarget::Right => self.right,
            NavTarget::Up => self.up,
            NavTarget::Down => self.down,
        }
    }

    fn slot(&mut self, target: NavTarget) -> &mut Option<Key> {
        match target {
            NavTarget::Left => &mut self.left,
            NavTarget::Right => &mut self.right,
            NavTarget::Up => &mut self.up,
            NavTarget::Down => &mut self.down,
        }
    }

    /// Assigns `key` to `target`, taking it off whatever else held it.
    ///
    /// Without the taking-off, binding the key that is already Right to
    /// Left leaves one press meaning both, and which one wins depends on
    /// the order the directions happen to be checked in.
    pub fn bind(&mut self, target: NavTarget, key: Key) {
        for other in NavTarget::ALL {
            if other != target && self.get(other) == Some(key) {
                *self.slot(other) = None;
            }
        }
        *self.slot(target) = Some(key);
    }

    pub fn apply(&mut self, preset: Preset) {
        *self = Self::default();
        for (target, key) in preset.keys() {
            *self.slot(target) = Some(key);
        }
    }

    /// Back to built-ins only.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Whether this is exactly `preset`, so the UI can show which one is
    /// in force.
    pub fn matches(self, preset: Preset) -> bool {
        let mut want = Self::default();
        for (target, key) in preset.keys() {
            *want.slot(target) = Some(key);
        }
        self == want
    }

    /// Writes the bindings out.
    ///
    /// Called by the caller after a change rather than by the mutators
    /// themselves, which is the less obvious arrangement and the right
    /// one: a `bind` that writes to disk cannot be tested without
    /// writing to the reader's real settings, and a suite whose tests
    /// run in parallel would then be several threads rewriting one file
    /// -- exactly the race the CLI fixtures had.
    pub fn save(&self) {
        let mut text = String::new();
        for target in NavTarget::ALL {
            if let Some(key) = self.get(target) {
                text.push_str(target.field());
                text.push('=');
                text.push_str(key.name());
                text.push('\n');
            }
        }
        config::write(STORE, &text);
    }
}

/// One `direction=Key` per line. Anything unrecognised is skipped rather
/// than rejected: a file half-written by an older build should cost the
/// bindings it cannot express, not all of them.
fn parse(text: &str) -> NavKeys {
    let mut keys = NavKeys::default();
    for line in text.lines() {
        let Some((field, name)) = line.trim().split_once('=') else {
            continue;
        };
        let Some(target) = NavTarget::ALL.iter().find(|t| t.field() == field) else {
            continue;
        };
        if let Some(key) = Key::from_name(name.trim()) {
            if is_assignable(key) {
                *keys.slot(*target) = Some(key);
            }
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_bound_to_begin_with() {
        let keys = NavKeys::default();
        assert!(NavTarget::ALL.iter().all(|t| keys.get(*t).is_none()));
    }

    /// The whole safety argument: a reader can always turn a page, no
    /// matter what they have done in Options.
    #[test]
    fn every_direction_keeps_a_builtin_whatever_is_bound() {
        let mut keys = NavKeys::default();
        keys.apply(Preset::Vim);
        for target in NavTarget::ALL {
            assert!(
                !target.builtins().is_empty(),
                "{target:?} must keep a key that cannot be taken away"
            );
        }
        assert_eq!(keys.get(NavTarget::Left), Some(Key::J));
    }

    #[test]
    fn binding_a_key_takes_it_off_whatever_else_had_it() {
        let mut keys = NavKeys::default();
        keys.bind(NavTarget::Right, Key::L);
        keys.bind(NavTarget::Left, Key::L);
        assert_eq!(keys.get(NavTarget::Left), Some(Key::L));
        assert_eq!(
            keys.get(NavTarget::Right),
            None,
            "one press must not mean two directions"
        );
    }

    #[test]
    fn the_two_presets_are_what_people_asked_for() {
        let mut keys = NavKeys::default();
        keys.apply(Preset::Vim);
        assert_eq!(keys.get(NavTarget::Left), Some(Key::J));
        assert_eq!(keys.get(NavTarget::Right), Some(Key::L));
        assert_eq!(keys.get(NavTarget::Up), Some(Key::I));
        assert_eq!(keys.get(NavTarget::Down), Some(Key::K));

        keys.apply(Preset::Gamer);
        assert_eq!(keys.get(NavTarget::Left), Some(Key::A));
        assert_eq!(keys.get(NavTarget::Right), Some(Key::D));
        assert_eq!(keys.get(NavTarget::Up), Some(Key::W));
        assert_eq!(keys.get(NavTarget::Down), Some(Key::S));

        assert!(keys.matches(Preset::Gamer));
        assert!(!keys.matches(Preset::Vim));
    }

    #[test]
    fn a_preset_replaces_rather_than_adds_to_what_was_there() {
        let mut keys = NavKeys::default();
        keys.bind(NavTarget::Up, Key::Q);
        keys.apply(Preset::Gamer);
        assert_eq!(keys.get(NavTarget::Up), Some(Key::W));
    }

    #[test]
    fn built_ins_and_escape_cannot_be_assigned() {
        for key in [
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::ArrowUp,
            Key::ArrowDown,
            Key::PageUp,
            Key::PageDown,
            Key::Space,
            Key::Escape,
        ] {
            assert!(!is_assignable(key), "{key:?} should be refused");
        }
        for key in [Key::J, Key::L, Key::A, Key::D, Key::W, Key::S] {
            assert!(is_assignable(key), "{key:?} should be allowed");
        }
    }

    #[test]
    fn bindings_survive_a_round_trip_through_the_file_format() {
        let mut keys = NavKeys::default();
        keys.apply(Preset::Vim);
        let text = {
            let mut t = String::new();
            for target in NavTarget::ALL {
                if let Some(k) = keys.get(target) {
                    t.push_str(&format!("{}={}\n", target.field(), k.name()));
                }
            }
            t
        };
        assert_eq!(parse(&text), keys);
    }

    #[test]
    fn a_damaged_file_costs_only_the_lines_it_damaged() {
        let keys = parse("left=J\nnonsense\nright=NotAKey\nup=I\n= \ndown=K\n");
        assert_eq!(keys.get(NavTarget::Left), Some(Key::J));
        assert_eq!(keys.get(NavTarget::Up), Some(Key::I));
        assert_eq!(keys.get(NavTarget::Down), Some(Key::K));
        assert_eq!(keys.get(NavTarget::Right), None);
    }

    /// A file naming a built-in -- written by hand, or by a build whose
    /// built-in set was different -- must not resurrect the ambiguity.
    #[test]
    fn a_file_cannot_smuggle_in_a_builtin() {
        assert_eq!(parse("left=Space\n"), NavKeys::default());
    }

    #[test]
    fn an_empty_file_means_built_ins_only() {
        assert_eq!(parse(""), NavKeys::default());
        assert_eq!(parse("\n\n   \n"), NavKeys::default());
    }
}
