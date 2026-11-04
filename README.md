# rmc

A small Midnight Commander-style two-panel file manager for the terminal, written in Rust.
It runs natively on Windows (Windows Terminal, PowerShell) and on Linux, including WSL
(bash or zsh). It should build on macOS too, but that hasn't been tried.

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

On Linux the same `cargo install --path .` puts `rmc` into `~/.cargo/bin`. Building inside WSL
from the Windows checkout works; keep the build output on the Linux side, where it is much
faster:

```bash
cd /mnt/e/devel/midnight-commander
CARGO_TARGET_DIR=~/rmc-target cargo install --path .
```

### On Linux

- The drive picker (Alt+F1/F2) lists `/`, disks, removable media and network mounts, and in
  WSL the Windows drives (`/mnt/c`…); a letter jumps to the mount whose folder starts with it.
  Free space of network and Windows drives is fetched in the background.
- Enter on a file opens it in the desktop's app (`xdg-open` in a graphical session; in WSL
  the Windows app, via `wslview` or `explorer.exe`). Without one, it opens in the viewer.
- The editor defaults to nvim, or the first of vim, vi and nano that is installed.
- Terminals send Ctrl+\ as Ctrl+4; both open the hotlist.

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
| F9 | sort menu: name, extension, size, time; choosing the current order again (or F9 F9) flips it A-Z / Z-A |
| Esc | close any dialog; stop a running copy, move or delete (no extra box to dismiss) |
| Ctrl+S or Alt+S | quick search by name; the same key again jumps to the next match |
| Ctrl+O | switch between the panels and the shell (one pwsh for the whole session) |
| F10 | quit |

Typing anywhere goes to the command line; Enter runs it in the shell (see below), in the
active panel's folder. `cd DIR` is handled by rmc itself and changes the panel's folder.

Each panel remembers where it was on every drive: picking E: in the drive picker (or `cd e:`)
returns to that folder, and picking the drive the panel is already on goes to its root.
Network drives show the share they are mapped to and their free space, which is fetched in
the background so an unreachable share can't freeze the screen.

### The shell

Like mc's subshell, there is one shell for the whole session. It starts with the first Ctrl+O
(or the first command typed in rmc) and runs in a pseudo-console (ConPTY) that rmc owns, so it
keeps going while the panels are shown: variables stay, and a long command keeps running in the
background. Ctrl+O switches between the panels and the shell's screen, which rmc draws full
screen; Shift+PgUp/PgDn or the mouse wheel scroll back through its output.

- If you `cd` in the shell, the active panel follows when you come back to the panels.
- If you move the panel and the shell is idle at its prompt, the shell follows when you switch
  to it. That's silent: rmc presses a key the shell's start-up script binds (Ctrl+Alt+Shift+F12),
  which changes folder and redraws the prompt in place, so nothing shows up as a command or in
  your history.
- A command typed in rmc while the shell is busy is refused rather than typed into the running
  program.
- `exit` ends the shell and returns to the panels; the next Ctrl+O starts a new one.

This works with pwsh or Windows PowerShell, bash and zsh: rmc starts the shell with a small
start-up script that loads your usual one (your profile, `~/.bashrc`, or `.zshenv` and `.zshrc`
from your ZDOTDIR), then hooks the prompt so each prompt reports its number and folder, and
binds the silent follow key. bash redraws its prompt by drawing a new one in the old one's
place (bash 4.4 or later; older ones get a visible ` cd`). With cmd, sh, fish or other shells
the shell still works, but the folders don't follow each other, and rmc can't tell whether a
command is still running, so it never refuses one.

### Mouse

Click selects, double-click opens, right-click marks, the wheel scrolls the panel under the
pointer. Click a column heading (Name, Size, Modify time) to sort by it, and again to flip
the order; an arrow on the heading shows which way round it is (↑ A to Z, small to big, old
to new). Click the F-key bar labels to run them; dialog buttons and the drive, hotlist and
sort menu rows are clickable too.

While rmc has the mouse, Windows Terminal selects text with **Shift+drag**. Start it with
`rmc --no-mouse` to leave the mouse to the terminal.

## Settings

| Variable | Default |
| --- | --- |
| `RMC_EDITOR` (then `VISUAL`, `EDITOR`) | `nvim` (if missing: vim, vi, nano) |
| `RMC_VIEWER` | the editor, plus `-R` for vim/nvim or `-v` for nano |
| `RMC_SHELL` | `pwsh`, then `powershell`, then `cmd`; on Linux `$SHELL` |
| `RMC_LOG` | file to log the shell's pseudo-console traffic to, for debugging (typed input is logged as a byte count only) |

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
| `src/subshell.rs` | the persistent shell: ConPTY via portable-pty, screen copy via vt100, key encoding |
| `src/hotlist.rs` | favourite folders and their file |
| `src/dialog.rs` | dialogs, buttons, line editor, help text |
| `src/ui.rs` | drawing with ratatui |
| `src/platform.rs` | OS bits: drives and mounts, free space, hidden files, opening files, shells and their start-up scripts |
| `src/util.rs` | formatting, path resolution, wildcard matching, column fitting |

## Testing in a real terminal

`cargo test` covers most things, including a real shell in the embedded terminal: pwsh on
Windows, bash and zsh on Linux (`RMC_TEST_ZSH` can point at a zsh that isn't on the PATH).
Run the Linux tests from Windows with WSL:

```bash
wsl -- bash -lc 'cd /mnt/e/devel/midnight-commander && CARGO_TARGET_DIR=~/rmc-target cargo test'
```

To check rmc itself in a Windows Terminal window, the scripts in `tools/` attach to its
console. That works for rmc inside WSL too: give them the pid of its `wsl.exe`.

```powershell
wt -w new cmd /c rmc E:\some\dir          # then find its pid: Get-Process rmc
tools\sendkey.ps1 -ProcessId <pid> -Keys 'ctrl+o', 'text:dir', 'enter'   # also esc, f1-f12, alt+f1, shift+f4, ctrl+d…
tools\readscreen.ps1 -ProcessId <pid>      # prints what the window shows
```

## Not there yet

Archives and SFTP, a user menu, themes.
