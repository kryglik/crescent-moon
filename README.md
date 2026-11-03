# rmc

A small Midnight Commander-style two-panel file manager for the terminal, written in Rust.
It runs natively on Windows (Windows Terminal, PowerShell) and should build on Linux and macOS too.

What it does differently from the Windows port of mc:

- F8 and Delete move files to the **Recycle Bin**; only Shift+F8 / Shift+Delete delete permanently.
- Long paths (over 260 characters) work.
- Czech and other non-ASCII file names work everywhere, including opening a file in its app.
- F3/F4 open files in **Neovim** (or any editor you set) instead of a built-in viewer and editor.
- `..` at a drive root opens the drive picker.

## Build and install

```powershell
cargo test
cargo install --path .     # puts rmc.exe into ~/.cargo/bin
rmc                        # or: rmc LEFT_DIR RIGHT_DIR
```

The release build links the C runtime statically (`.cargo/config.toml`), so `rmc.exe` can be
copied to another PC without installing anything.

## Keys

Press **F1** inside rmc for the full list. The essentials:

| Key | Action |
| --- | --- |
| Tab | switch panel |
| Enter / Backspace | open directory or file / parent directory |
| Alt+F1, Alt+F2 | drive picker for the left / right panel |
| Ctrl+\ or Ctrl+D | favourite folders: Ins adds the current one, Enter or 1–9 jumps, Del removes |
| Ctrl+Space | size of the directory under the cursor (on `..`: all of them, plus a folder total) |
| Insert, Shift+Up/Down, `+` `-` `*` | mark files |
| F2 | rename |
| F3 / F4 / Shift+F4 | view / edit / new file (in the editor) |
| F5 / F6 / F7 | copy / move / make directory |
| F8, Delete | to the Recycle Bin (Shift: permanently) |
| F9 | sort by name, extension, size, time |
| Ctrl+S | quick search |
| Ctrl+O | shell (type `exit` to come back) |
| F10 | quit |

Typing anywhere goes to the command line; Enter runs it in the active panel's directory.
`cd DIR` changes the panel's directory.

Each panel remembers where it was on every drive: picking E: in the drive picker (or `cd e:`)
returns to that folder, and picking the drive the panel is already on goes to its root.
Network drives show the share they are mapped to and their free space, which is fetched in
the background so an unreachable share can't freeze the screen.

### Mouse

Click selects, double-click opens, right-click marks, the wheel scrolls the panel under the
pointer. Click a column heading (Name, Size, Modify time) to sort by it, and the F-key bar
labels to run them; dialog buttons and the drive and hotlist rows are clickable too.

While rmc has the mouse, Windows Terminal selects text with **Shift+drag**. Start it with
`rmc --no-mouse` to leave the mouse to the terminal.

## Settings

| Variable | Default |
| --- | --- |
| `RMC_EDITOR` (then `VISUAL`, `EDITOR`) | `nvim` |
| `RMC_VIEWER` | the editor, plus `-R` for vim/nvim |
| `RMC_SHELL` | `pwsh`, then `powershell`, then `cmd` |

Favourite folders are saved in `%APPDATA%\rmc\hotlist.txt` (one path per line; you can edit it).

Ctrl+\ only works while a US-style layout is active (on the Czech layout that key is a dead
key), hence Ctrl+D. Directory sizes don't follow links or junctions, so nothing is counted twice;
with sizes measured, F9's size sort orders directories by them.

### Staying in the last directory

`rmc --cwd-file FILE` writes the active panel's directory to `FILE` on exit. With this function
in your PowerShell profile, `rmc` leaves the shell in that directory:

```powershell
function rmc {
    $f = New-TemporaryFile
    rmc.exe --cwd-file $f @args
    $dir = Get-Content $f -ErrorAction SilentlyContinue
    Remove-Item $f
    if ($dir) { Set-Location -LiteralPath $dir }
}
```

## Layout

| File | Contents |
| --- | --- |
| `src/main.rs` | arguments, terminal setup, event loop, running nvim/shell/commands |
| `src/app.rs` | application state and all key handling |
| `src/panel.rs` | a panel: listing, sorting, cursor, marks |
| `src/ops.rs` | copy/move/delete jobs on a worker thread (progress, questions, cancel) |
| `src/sizes.rs` | directory size measuring on a worker thread |
| `src/mouse.rs` | mouse handling, using the screen areas `ui.rs` records each frame |
| `src/hotlist.rs` | favourite folders and their file |
| `src/dialog.rs` | dialogs, buttons, line editor, help text |
| `src/ui.rs` | drawing with ratatui |
| `src/platform.rs` | Windows bits: drives, free space, hidden files, ShellExecute, shells |
| `src/util.rs` | formatting, path resolution, wildcard matching, column fitting |

## Not there yet

Archives and SFTP, a persistent shell like mc's subshell, a user menu, themes.
