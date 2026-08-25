//! How far one keyboard scroll step travels.
//!
//! This is desktop2's side of `keybindings.scroll_lines`, the same key the TUI
//! reads, so "one press moves N lines" means the same thing in both clients.
//! Like [`crate::reasoning`], desktop2 cannot depend on the config crates
//! (dependency-boundary gate), so the key is read with the same tolerant line
//! scan: a missing, malformed or out-of-range value falls back to the default
//! rather than failing the window's startup.

/// Lines travelled by one keyboard scroll step when nothing is configured.
///
/// One line, matching `jcode_config_types::DEFAULT_SCROLL_LINES` and the TUI's
/// `LINE_SCROLL_AMOUNT`: a scroll key that moves several lines at once reads as
/// a jump rather than a scroll.
pub const DEFAULT_SCROLL_LINES: f64 = 1.0;

/// The largest step we will honor. A huge value is far more likely to be a typo
/// than an intent, and it would turn every keypress into a blind teleport.
const MAX_SCROLL_LINES: u32 = 200;

/// The effective keyboard scroll step for this process, in body lines.
pub fn keyboard_scroll_lines() -> f64 {
    resolve(read_config_text().as_deref())
}

/// Pure resolver, kept apart from IO so it is testable without touching a real
/// home directory.
pub fn resolve(config_text: Option<&str>) -> f64 {
    config_text
        .and_then(configured_scroll_lines)
        .unwrap_or(DEFAULT_SCROLL_LINES)
}

fn read_config_text() -> Option<String> {
    std::fs::read_to_string(jcode_home_dir()?.join("config.toml")).ok()
}

fn configured_scroll_lines(raw: &str) -> Option<f64> {
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "scroll_lines" {
            continue;
        }
        let value = value
            .split('#')
            .next()
            .unwrap_or_default()
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        // Clamp rather than reject: a 0 would make the scroll keys silently
        // dead, which looks like a broken build rather than a bad setting.
        let parsed = value.parse::<u32>().ok()?;
        return Some(f64::from(parsed.clamp(1, MAX_SCROLL_LINES)));
    }
    None
}

fn jcode_home_dir() -> Option<std::path::PathBuf> {
    match std::env::var_os("JCODE_HOME") {
        Some(path) => Some(std::path::PathBuf::from(path)),
        None => std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .map(|home| home.join(".jcode")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_falls_back_to_one_line() {
        assert_eq!(resolve(None), 1.0);
        assert_eq!(
            resolve(Some("[keybindings]\nscroll_up = \"ctrl+k\"\n")),
            1.0
        );
    }

    #[test]
    fn configured_value_is_honored() {
        assert_eq!(resolve(Some("[keybindings]\nscroll_lines = 5\n")), 5.0);
        // Quoted and comment-trailed forms are common in hand-edited TOML.
        assert_eq!(resolve(Some("scroll_lines = \"3\" # coarse\n")), 3.0);
    }

    #[test]
    fn commented_out_value_is_ignored() {
        assert_eq!(resolve(Some("# scroll_lines = 9\n")), 1.0);
    }

    #[test]
    fn out_of_range_values_are_clamped_not_obeyed() {
        // Zero would make the scroll keys do nothing at all.
        assert_eq!(resolve(Some("scroll_lines = 0\n")), 1.0);
        assert_eq!(resolve(Some("scroll_lines = 100000\n")), 200.0);
    }

    #[test]
    fn malformed_value_falls_back_instead_of_panicking() {
        assert_eq!(resolve(Some("scroll_lines = wat\n")), 1.0);
        assert_eq!(resolve(Some("scroll_lines = -3\n")), 1.0);
    }
}
