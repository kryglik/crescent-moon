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
    /// Left and Right (with an empty command line) go to the parent folder and into the one
    /// under the cursor, as mc's "Lynx-like motion".
    pub arrows_navigate: bool,
    /// Numbers in names sort by value ("day 2" before "day 10"), as in Explorer.
    pub natural_sort: bool,
    /// In a git repository, `git status` runs now and then so the panels mark changed, new and
    /// ignored files. (The branch shows either way: it is read from the repository's files.)
    pub git_status: bool,
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
            arrows_navigate: true,
            natural_sort: true,
            git_status: true,
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

    /// The settings in `file` (in whatever encoding an editor saved it); defaults for anything
    /// missing, or no file at all. Err when the file is there but can't be read: it mustn't be
    /// saved over then.
    pub fn load(file: Option<&Path>) -> io::Result<Self> {
        match file.map(util::read_text) {
            Some(Ok(text)) => Ok(Settings::parse(&text)),
            Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(Settings::default()),
        }
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
                "arrows_navigate" => settings.arrows_navigate = flag(settings.arrows_navigate),
                "natural_sort" => settings.natural_sort = flag(settings.natural_sort),
                "git_status" => settings.git_status = flag(settings.git_status),
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
             arrows_navigate = {}\n\
             natural_sort = {}\n\
             git_status = {}\n\
             big_file_mb = {}\n\
             editor = {}\n\
             viewer = {}\n\
             pager = {}\n\
             shell = {}\n",
            self.confirm_quit,
            self.show_hidden,
            self.mouse,
            self.arrows_navigate,
            self.natural_sort,
            self.git_status,
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
        assert_eq!(Settings::load(Some(&file)).unwrap(), Settings::default(), "no file: defaults");
        let settings = Settings {
            confirm_quit: true,
            mouse: false,
            git_status: false,
            big_file_mb: 25,
            editor: r#""C:\Program Files\Neovim\bin\nvim.exe" -p"#.into(),
            ..Settings::default()
        };
        settings.save(&file).unwrap();
        assert_eq!(Settings::load(Some(&file)).unwrap(), settings);
        // Saved again by an editor that adds a BOM.
        fs::write(&file, format!("\u{feff}{}", fs::read_to_string(&file).unwrap())).unwrap();
        assert_eq!(Settings::load(Some(&file)).unwrap(), settings);
    }

    #[test]
    fn odd_lines_are_ignored() {
        let settings = Settings::parse("# comment\nmouse = maybe\nbig_file_mb = 0\nunknown = 1\nconfirm_quit=YES\nno equals sign\n");
        assert!(settings.mouse, "an unreadable value keeps the default");
        assert_eq!(settings.big_file_mb, 10, "zero is not a limit");
        assert!(settings.confirm_quit);
    }
}
