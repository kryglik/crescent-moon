//! The options editor (Ctrl+P): the settings as a list of rows, checkboxes toggled with Space
//! or Enter, text rows typed into directly.

use crate::app::Automatic;
use crate::settings::Settings;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Row {
    ConfirmQuit,
    ShowHidden,
    Mouse,
    Arrows,
    NaturalSort,
    GitStatus,
    MarkFolders,
    PromptHost,
    ReopenLast,
    SearchAnywhere,
    Scheme,
    BigFile,
    Editor,
    Viewer,
    Pager,
    Shell,
}

pub const ROWS: [Row; 16] = [
    Row::ConfirmQuit,
    Row::ShowHidden,
    Row::Mouse,
    Row::Arrows,
    Row::NaturalSort,
    Row::GitStatus,
    Row::MarkFolders,
    Row::PromptHost,
    Row::ReopenLast,
    Row::SearchAnywhere,
    Row::Scheme,
    Row::BigFile,
    Row::Editor,
    Row::Viewer,
    Row::Pager,
    Row::Shell,
];

impl Row {
    pub fn label(self) -> &'static str {
        match self {
            Row::ConfirmQuit => "Ask before quitting (F10)",
            Row::ShowHidden => "Show hidden files",
            Row::Mouse => "Mouse",
            Row::Arrows => "Left/Right go up and into folders",
            Row::NaturalSort => "Numbers in names by value (2 before 10)",
            Row::GitStatus => "Git: mark changed and new files",
            Row::MarkFolders => "+ and - mark folders too",
            Row::PromptHost => "user@host before the command line",
            Row::ReopenLast => "Reopen the last folders and tabs",
            Row::SearchAnywhere => "Quick search finds text anywhere in a name",
            Row::Scheme => "Colour scheme (Alt+C)",
            Row::BigFile => "Big file limit, MB",
            Row::Editor => "Editor (F4)",
            Row::Viewer => "Viewer (F3)",
            Row::Pager => "Pager (big files)",
            Row::Shell => "Shell (Ctrl+O)",
        }
    }

    /// A checkbox rather than a text field.
    pub fn is_flag(self) -> bool {
        matches!(self, Row::ConfirmQuit | Row::ShowHidden | Row::Mouse | Row::Arrows | Row::NaturalSort | Row::GitStatus | Row::MarkFolders | Row::PromptHost | Row::ReopenLast | Row::SearchAnywhere)
    }

    /// Picked from a list (Enter opens it) rather than typed.
    pub fn is_choice(self) -> bool {
        self == Row::Scheme
    }

    pub fn flag(self, settings: &Settings) -> bool {
        match self {
            Row::ConfirmQuit => settings.confirm_quit,
            Row::ShowHidden => settings.show_hidden,
            Row::Mouse => settings.mouse,
            Row::Arrows => settings.arrows_navigate,
            Row::NaturalSort => settings.natural_sort,
            Row::GitStatus => settings.git_status,
            Row::MarkFolders => settings.mark_folders,
            Row::PromptHost => settings.prompt_host,
            Row::ReopenLast => settings.reopen_last,
            Row::SearchAnywhere => settings.search_anywhere,
            _ => false,
        }
    }

    pub fn toggle(self, settings: &mut Settings) {
        match self {
            Row::ConfirmQuit => settings.confirm_quit = !settings.confirm_quit,
            Row::ShowHidden => settings.show_hidden = !settings.show_hidden,
            Row::Mouse => settings.mouse = !settings.mouse,
            Row::Arrows => settings.arrows_navigate = !settings.arrows_navigate,
            Row::NaturalSort => settings.natural_sort = !settings.natural_sort,
            Row::GitStatus => settings.git_status = !settings.git_status,
            Row::MarkFolders => settings.mark_folders = !settings.mark_folders,
            Row::PromptHost => settings.prompt_host = !settings.prompt_host,
            Row::ReopenLast => settings.reopen_last = !settings.reopen_last,
            Row::SearchAnywhere => settings.search_anywhere = !settings.search_anywhere,
            _ => {}
        }
    }

    pub fn text(self, settings: &Settings) -> String {
        match self {
            Row::BigFile => settings.big_file_mb.to_string(),
            Row::Scheme => settings.scheme.clone(),
            Row::Editor => settings.editor.clone(),
            Row::Viewer => settings.viewer.clone(),
            Row::Pager => settings.pager.clone(),
            Row::Shell => settings.shell.clone(),
            _ => String::new(),
        }
    }

    /// Takes what was typed; a limit that isn't a whole number above zero is ignored.
    pub fn set_text(self, settings: &mut Settings, text: &str) {
        let text = text.trim().to_string();
        match self {
            Row::BigFile => {
                if let Ok(mb) = text.parse::<u64>()
                    && mb > 0
                {
                    settings.big_file_mb = mb;
                }
            }
            Row::Editor => settings.editor = text,
            Row::Viewer => settings.viewer = text,
            Row::Pager => settings.pager = text,
            Row::Shell => settings.shell = text,
            _ => {}
        }
    }
}

/// What empty command rows stand for, shown in grey.
#[derive(Clone, Debug, Default)]
pub struct OptionsAutomatic {
    pub editor: String,
    pub viewer: String,
    pub pager: String,
    pub shell: String,
}

impl OptionsAutomatic {
    pub fn detect() -> Self {
        let auto = Automatic::detect();
        let join = |parts: &[String]| {
            parts.iter().map(|p| if p.contains(' ') { format!("\"{p}\"") } else { p.clone() }).collect::<Vec<_>>().join(" ")
        };
        OptionsAutomatic {
            editor: join(&auto.editor),
            viewer: join(&auto.viewer),
            pager: auto.pager.as_deref().map_or_else(|| "none found".into(), join),
            shell: auto.shell.program(),
        }
    }

    pub fn for_row(&self, row: Row) -> Option<&str> {
        match row {
            Row::Editor => Some(&self.editor),
            Row::Viewer => Some(&self.viewer),
            Row::Pager => Some(&self.pager),
            Row::Shell => Some(&self.shell),
            _ => None,
        }
    }
}
