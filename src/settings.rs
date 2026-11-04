//! The user's settings: `settings.ini` in the config folder (`%APPDATA%\rmc`, `~/.config/rmc`),
//! changed with the options editor (Ctrl+P) or by hand. One `name = value` per line; unknown
//! names and unreadable values are ignored, so an older or newer file still loads.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::util;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// F10 asks "Quit rmc?" (it asks anyway while the shell is busy).
    pub confirm_quit: bool,
    pub show_hidden: bool,
    pub mouse: bool,
    /// Files bigger than this many megabytes go to the pager, and the editor asks first.
    pub big_file_mb: u64,
    /// Commands; empty means automatic (environment variables, then what is installed).
    pub editor: String,
    pub viewer: String,
    pub pager: String,
    pub shell: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            confirm_quit: false,
            show_hidden: true,
            mouse: true,
            big_file_mb: 10,
            editor: String::new(),
            viewer: String::new(),
            pager: String::new(),
            shell: String::new(),
        }
    }
}

impl Settings {
    pub fn default_file() -> Option<PathBuf> {
        util::config_dir().map(|dir| dir.join("settings.ini"))
    }

    /// The settings in `file`; defaults for anything missing (or no file at all).
    pub fn load(file: Option<&Path>) -> Self {
        file.and_then(|f| fs::read_to_string(f).ok()).map_or_else(Settings::default, |text| Settings::parse(&text))
    }

    pub fn save(&self, file: &Path) -> io::Result<()> {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(file, self.to_text())
    }

    pub fn big_file_bytes(&self) -> u64 {
        self.big_file_mb.saturating_mul(1024 * 1024)
    }

    fn parse(text: &str) -> Self {
        let mut settings = Settings::default();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with(['#', ';'])) {
            let Some((name, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            let flag = |current: bool| match value.to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => true,
                "false" | "no" | "off" | "0" => false,
                _ => current,
            };
            match name.trim() {
                "confirm_quit" => settings.confirm_quit = flag(settings.confirm_quit),
                "show_hidden" => settings.show_hidden = flag(settings.show_hidden),
                "mouse" => settings.mouse = flag(settings.mouse),
                "big_file_mb" => {
                    if let Ok(mb) = value.parse::<u64>()
                        && mb > 0
                    {
                        settings.big_file_mb = mb;
                    }
                }
                "editor" => settings.editor = value.to_string(),
                "viewer" => settings.viewer = value.to_string(),
                "pager" => settings.pager = value.to_string(),
                "shell" => settings.shell = value.to_string(),
                _ => {}
            }
        }
        settings
    }

    fn to_text(&self) -> String {
        format!(
            "# rmc settings: change them with Ctrl+P in rmc, or here.\n\
             # Empty commands mean automatic (RMC_EDITOR etc., then what is installed).\n\
             confirm_quit = {}\n\
             show_hidden = {}\n\
             mouse = {}\n\
             big_file_mb = {}\n\
             editor = {}\n\
             viewer = {}\n\
             pager = {}\n\
             shell = {}\n",
            self.confirm_quit,
            self.show_hidden,
            self.mouse,
            self.big_file_mb,
            self.editor,
            self.viewer,
            self.pager,
            self.shell
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn saves_and_loads() {
        let tmp = TempDir::new("settings");
        let file = tmp.path().join("rmc").join("settings.ini");
        assert_eq!(Settings::load(Some(&file)), Settings::default(), "no file: defaults");
        let settings = Settings {
            confirm_quit: true,
            mouse: false,
            big_file_mb: 25,
            editor: r#""C:\Program Files\Neovim\bin\nvim.exe" -p"#.into(),
            ..Settings::default()
        };
        settings.save(&file).unwrap();
        assert_eq!(Settings::load(Some(&file)), settings);
    }

    #[test]
    fn odd_lines_are_ignored() {
        let settings = Settings::parse("# comment\nmouse = maybe\nbig_file_mb = 0\nunknown = 1\nconfirm_quit=YES\nno equals sign\n");
        assert!(settings.mouse, "an unreadable value keeps the default");
        assert_eq!(settings.big_file_mb, 10, "zero is not a limit");
        assert!(settings.confirm_quit);
    }
}
