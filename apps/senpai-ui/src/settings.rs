//! The choices the reader makes once and expects to stay made: which
//! view a PDF opens in, and how far the wheel scrolls.
//!
//! One file, `key = value` a line, in the same plain-text spirit as the
//! other things kept between runs (see [`crate::config`]). Parsing is
//! forgiving in one direction only: a line it does not understand is
//! skipped and that setting keeps its default, so a file from a newer
//! version, or one edited by hand and mistyped, costs a preference
//! rather than the launch.

use crate::config;
use crate::scroll::WheelStep;

/// Name of the file these live in.
const STORE: &str = "settings";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Whether PDFs open as one continuous column rather than a page at
    /// a time.
    pub continuous: bool,
    pub wheel_step: WheelStep,
}

impl Settings {
    pub fn load() -> Self {
        config::read(STORE).map(|t| parse(&t)).unwrap_or_default()
    }

    /// Writes the settings out. Called by whoever changed one, not by
    /// anything in here, for the reason `NavKeys::save` gives: a setter
    /// that writes to disk cannot be tested without writing to the
    /// reader's real settings.
    pub fn save(&self) {
        config::write(STORE, &serialize(self));
    }
}

fn serialize(settings: &Settings) -> String {
    format!(
        "view = {}\nwheel = {}\n",
        if settings.continuous {
            "continuous"
        } else {
            "paged"
        },
        settings.wheel_step.name(),
    )
}

fn parse(text: &str) -> Settings {
    let mut settings = Settings::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match (key.trim(), value.trim()) {
            ("view", "continuous") => settings.continuous = true,
            ("view", "paged") => settings.continuous = false,
            ("wheel", name) => {
                if let Some(step) = WheelStep::from_name(name) {
                    settings.wheel_step = step;
                }
            }
            _ => {}
        }
    }
    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_saved_means_paged_and_a_fine_wheel() {
        assert_eq!(
            parse(""),
            Settings {
                continuous: false,
                wheel_step: WheelStep::Fine,
            }
        );
    }

    #[test]
    fn what_is_written_is_what_is_read_back() {
        for continuous in [false, true] {
            for wheel_step in WheelStep::ALL {
                let settings = Settings {
                    continuous,
                    wheel_step,
                };
                assert_eq!(parse(&serialize(&settings)), settings);
            }
        }
    }

    /// A mistyped or unknown line loses that one preference, not the
    /// others and not the launch.
    #[test]
    fn a_line_it_does_not_understand_is_skipped() {
        let text = "view = sideways\nnonsense\nfuture = 3\nwheel = fast\n";
        assert_eq!(
            parse(text),
            Settings {
                continuous: false,
                wheel_step: WheelStep::Fast,
            }
        );
    }

    #[test]
    fn spacing_and_line_endings_do_not_matter() {
        assert!(parse("view=continuous\r\n").continuous);
        assert!(parse("  view   =   continuous  ").continuous);
    }

    #[test]
    fn a_later_line_wins() {
        assert!(!parse("view = continuous\nview = paged\n").continuous);
    }
}
