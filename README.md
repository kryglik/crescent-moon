# rmc

A small Midnight Commander-style two-panel file manager for the terminal, written in Rust.
It runs natively on Windows (Windows Terminal, PowerShell) and on Linux, including WSL
(bash or zsh). It should build on macOS too, but that hasn't been tried.

What it does differently from the Windows port of mc:

- F8 and Delete move files to the **Recycle Bin**; only Shift+F8 / Shift+Delete delete permanently.
  The question counts what would go while you decide, and for files only an administrator may
  delete (Windows' own, owned by TrustedInstaller) it offers **As admin**, like Total Commander.
- It works **with Explorer**: Ctrl+C / Ctrl+X put files on the clipboard the way Explorer does,
  Alt+V pastes what Explorer copied; panels show what other programs change at once; Enter on
  a shortcut to a folder opens it; Shift+Enter asks Windows which app to open a file in.
- Total Commander habits: Ctrl+Q quick view, Shift+F2 compare folders, Alt+F5 / Alt+F9 pack and
  unpack zips (and zips open as folders), folder tabs (Ctrl+T), back and forward (Alt+Left /
  Alt+Right), Ctrl+D hotlist, Alt+F7 find.
- **Git** at a glance: each panel's bottom line shows the branch and what changed, a mark beside
  every changed, new or conflicting file (ignored ones dimmed), and Alt+W jumps between the
  repository's worktrees, into the same subfolder.
- Copies and moves never put a file at risk: an overwrite only replaces the old file once the
  new one is complete, a move to another drive deletes each file only after its copy, and the
  same file under another name is recognised.
- Long paths (over 260 characters) work, as do Czech and other non-ASCII names everywhere.
- F3/F4 open files in **Neovim** (or any editor you set) instead of a built-in viewer and editor.
- F12 lists every command, found by typing part of its name.

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

Press **F1** inside rmc for the full list, or **F12** to find a command by typing part of its
name ("zip", "clip", "hist"; Enter runs it). The essentials:

#### Moving around

| Key | Action |
| --- | --- |
| Tab | switch panel |
| Enter / Backspace | open directory or file / parent directory; a Windows shortcut (.lnk) to a folder opens the folder in the panel, as in Total Commander |
| Left / Right | with an empty command line: up to the parent folder / into the folder under the cursor, as mc's "Lynx-like motion" (an option, on by default) |
| Ctrl+PgUp / Ctrl+PgDn | up / into a folder, as in Total Commander and Far |
| Alt+Left / Alt+Right | back / forward through the folders the panel was in, as in a browser (mc's Alt+Y / Alt+U too); `cd -` goes back |
| Alt+H or Alt+Down | folder history: the folders visited lately, newest first, kept between sessions; type part of a path to narrow it, Enter goes there, Del forgets one |
| Alt+F1, Alt+F2 | drive picker for the left / right panel |
| Ctrl+\ or Ctrl+D | favourite folders: Ins adds the current one, Enter or 1–9 jumps, Del removes |
| Alt+W | the git repository's worktrees: Enter goes to one, into the same subfolder (see Git) |
| Ctrl+T / Ctrl+W / Alt+1…9 | folder tabs, as in Total Commander: a new tab on the same folder, close the tab (with an empty command line; otherwise Ctrl+W deletes a word), go to tab N. Each tab keeps its folder, order and filter; a row of tabs shows above the panels once a side has more than one, and they are clickable |
| Ctrl+S or Alt+S | quick search by name; the same key again jumps to the next match |
| Alt+F7 or Alt+? | find text in files below the panel's folder (see Find) |

#### Looking at things

| Key | Action |
| --- | --- |
| F3 / F4 / Shift+F4 | view / edit / new file (in the editor); a file over 10 MB is shown in `less` instead, and F4 asks before loading it into the editor |
| Ctrl+Q | quick view (as in Total Commander and Far): the other panel shows what is under the cursor, read in the background: the start of a text file (UTF-8 or UTF-16), a hex dump of a binary one, a picture's format and size (PNG, JPEG, GIF, BMP, WebP), a folder's contents. Tab goes into it to scroll (arrows, pages), the wheel scrolls it too |
| Shift+Enter | open with… : Windows asks which app to open the file in |
| Ctrl+F | filter the panel: only files matching `*.jpg *.cr2`, or not `!*.tmp`, or containing a word; folders stay. The panel's title shows it until Ctrl+F empties it |
| F9 | sort menu: name, extension, size, time; choosing the current order again (or F9 F9) flips it A-Z / Z-A |
| Ctrl+Space | size of the directory under the cursor (on `..`: all of them, plus a folder total) |
| Shift+F2 | compare the two folders (as in Total Commander): marks on each side the files and folders the other side lacks or has an older copy of (same time but another size counts too), so F5 brings the other side up to date |

#### Marking and changing files

| Key | Action |
| --- | --- |
| Insert, Shift+Up/Down, `+` `-` `*` | mark files |
| Ctrl+A | mark everything (with an empty command line) |
| F2 | rename |
| F5 / F6 / F7 | copy / move / make directory; the progress shows the speed and the time left |
| F8, Delete | to the Recycle Bin (Shift: permanently); see Deleting as administrator |
| Alt+A | attributes of the marked files (or the one under the cursor): read-only, hidden, system, archive on Windows, the permission bits (rwx) on Linux. Space toggles, Enter sets them; a row where the files differ shows [?] and is left as each has it |
| Alt+F5 / Alt+F9 | pack into a zip / unpack the zip under the cursor (see Zip archives) |
| Ctrl+C / Ctrl+X | the marked files (or the one under the cursor) to the clipboard, copied or cut, the way Explorer puts them there: paste them in Explorer, a mail or a chat |
| Ctrl+Shift+C | their full paths to the clipboard as text |
| Alt+V | paste files copied or cut in Explorer or in rmc into the panel's folder (cut ones are moved, and the clipboard is emptied afterwards, as Explorer does). Windows Terminal keeps Ctrl+V for its own paste; where a terminal lets Ctrl+V through, it works too. On Linux rmc keeps its own clipboard for files, and the paths go to the terminal's clipboard (OSC 52, over ssh too) |

#### Everything else

| Key | Action |
| --- | --- |
| Esc | close any dialog; stop a running copy, move or delete (no extra box to dismiss) |
| Ctrl+O | switch between the panels and the shell (one pwsh for the whole session) |
| Ctrl+P | options (see Settings) |
| F10 | quit (right away; it asks only if the option says so, or while the shell is busy) |
| F12 | every command with its key, found by typing |

Typing anywhere goes to the command line; Enter runs it in the shell (see below), in the
active panel's folder. `cd DIR` is handled by rmc itself and changes the panel's folder; it
reads quotes and environment variables as the shell would, and `cd -` goes back.

Panels show what other programs change in their folders (a download finishing, a build) by
themselves, as Explorer does, keeping the cursor and the marks; removable drives aren't
watched, so they can still be ejected. The window's title names the active folder.

Each panel remembers where it was on every drive: picking E: in the drive picker (or `cd e:`)
returns to that folder, and picking the drive the panel is already on goes to its root.
Network drives show the share they are mapped to and their free space, which is fetched in
the background so an unreachable share can't freeze the screen.

### Deleting as administrator

The delete question appears at once and fills in what would go (files, folders, size) while
it counts in the background; Yes doesn't have to wait for it, No or Esc stops it. It also
checks whether you may delete everything. Where you may not, it says which item and why
(who owns it), and on Windows offers **As admin**, as does the error dialog if a delete runs
into such a file halfway.

As admin starts a second rmc with administrator rights, after Windows asks you (UAC). It
turns on the backup and restore privileges an administrator has, so even files belonging to
TrustedInstaller go, with no `takeown` or `icacls` needed, and deletes permanently: the
Recycle Bin refuses protected files even then. Its progress shows as for any delete, and Esc
stops it. On Linux there is no As admin.

### Zip archives

Enter on a `.zip` opens it in the panel as a folder, as Total Commander and mc do; the title
shows the path through it (`E:\trips\2024.zip\day 1`), `..` goes up inside it and out at its
top. Archives open read-only: copying into one, renaming, deleting and making folders in one
say so.

- **F5** copies the marked files and folders out of it, into the other panel's folder by
  default, with the usual progress, overwrite questions and Esc.
- **F3** and **Enter** take a file out into rmc's temp folder and view it or open it in its
  app (a zip in the zip opens as a folder too); quick view (Ctrl+Q) reads files straight from
  the zip. Ctrl+Space sizes its folders at once.
- **Alt+F5** packs the marked files (folders with what they hold) into a new zip, in the
  other panel's folder by default; **Alt+F9** unpacks the zip under the cursor into the other
  panel's folder, and one with several things at its top into a folder of its own name, as
  7-Zip's "Extract to name\" does. Both are Total Commander's keys.
- Names are UTF-8 when the zip says so or when they read as UTF-8; older zips made by Windows
  use its OEM code page (852 on a Czech Windows), which rmc reads too. Names that would climb
  out of the folder they are unpacked into (`../x`) are left out. Packing writes UTF-8 names,
  deflated, ZIP64 for files over 4 GB.

### Git

In a git repository the panel's bottom line shows the branch and how the working tree stands,
in as many words as fit: `main ↑1 · 3 changed · 1 untracked`, or in a narrow panel
`main ↑1 ~3 ?1`. ↑ and ↓ count the commits ahead of and behind the upstream branch. A rebase,
merge, cherry-pick or bisect under way shows the way git's prompt shows it (`REBASE 2/5`,
`MERGING`), a detached HEAD shows its commit (`(1a2b3c4)`), and a linked worktree says
`worktree`.

Beside each name is a mark, as in VS Code: **M** changed (staged or not), **A** added, **R**
renamed, **?** untracked, **!** in conflict, and **•** on a folder with changes inside, in
the colour of the weightiest one. What git ignores (`target`, `.venv`, `node_modules`) is
dimmed.

- The branch, the operation under way and the worktrees are read from the repository's own
  files, so they show the moment a panel enters a repository, with or without git installed.
- What changed comes from `git status`, run in the background: when the panels reload (after
  a command, F4, Ctrl+R, a copy), when files in the working tree change, and now and then
  anyway. On Windows one watch covers the whole working tree; on Linux the git folder is
  watched and the rest looked at every few seconds. The status never takes locks or writes
  the index (`--no-optional-locks`), so it can't get in the way of your own git commands.
  Ignored files come from `--ignored=matching`, which doesn't look inside ignored folders
  (git's default mode took a minute in a repository with a Python `.venv`). A repository
  whose status takes over 20 seconds is left alone. The option "Git: mark changed and new
  files" (Ctrl+P) turns all this off; the branch still shows.
- **Alt+W** lists the repository's worktrees (`git worktree add`): the main one first, then
  the others, the most recently used first, with `*` on the one the panel is in. Typing
  narrows the list by branch or folder, and Enter goes there: into the same subfolder when the
  other worktree has it (`src\ui` in one, `src\ui` in the other), else as near it as it has.
  Handy with tools that work in worktrees of their own, such as Claude Code's sessions.

### Find

Alt+F7 (or mc's Alt+?) searches below the active panel's folder the way
[ag](https://github.com/ggreer/the_silver_searcher) and ripgrep do, using ripgrep's own
crates: in parallel, skipping files that `.gitignore` (or `.ignore`) ignores, hidden files
and binary files. Results appear while you type, grouped by file, with the matches marked.

- **Text** is a regular expression with smart case: all lowercase ignores case, a capital
  letter makes it count. Something that isn't a valid regex (`main(`) is searched as typed.
- **Files** (Tab) narrows the search by name: `*.rs *.toml`, `!*test*` to leave names out,
  or a plain word for names containing it. With Text empty, it finds files and folders by
  name alone.
- Up/Down and PgUp/PgDn move through the results; **Enter** takes the panel there; **F3/F4**
  open the file at the matching line (`nvim +42`, `less +42`) and come back to the results.
- **Alt+U** searches everything, hidden and ignored files too (like `ag -u`).
- Esc closes it; Alt+F7 brings it back as it was, or runs the same search again if the
  panel has moved. It stops at 10,000 results.

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
to new). Click the F-key bar labels to run them; dialog buttons and the rows of the drive
picker, hotlist, sort menu, folder history, worktree list, F12 list and attributes are
clickable too, as are the tabs. The wheel scrolls the quick view.

While rmc has the mouse, Windows Terminal selects text with **Shift+drag**. Start it with
`rmc --no-mouse` to leave the mouse to the terminal.

## Settings

Ctrl+P opens the options editor; arrows move, Space toggles a checkbox, and you type into a
text row (it counts when you move off it). Everything takes effect at once and is saved in
`settings.ini` next to the hotlist (`%APPDATA%\rmc`, or `~/.config/rmc` on Linux), a plain
`name = value` file you can also edit by hand:

| Option | Default |
| --- | --- |
| Ask before quitting (F10) | off: F10 quits at once, unless the shell is still running a command |
| Show hidden files | on (Ctrl+H or Alt+. toggles it for the session) |
| Mouse | on (`--no-mouse` turns it off for one run) |
| Left/Right go up and into folders | on: with an empty command line, Left is the parent folder and Right the folder under the cursor |
| Numbers in names by value | on: "day 2" sorts before "day 10", as in Explorer |
| Git: mark changed and new files | on: in a repository `git status` runs in the background (see Git); off, only the branch shows |
| Big file limit, MB | 10: bigger files go to the pager on F3, and F4 asks first |
| Editor, Viewer, Pager, Shell | empty: automatic, from the variables below, then what is installed |

Environment variables, used when the matching option is empty:

| Variable | Default |
| --- | --- |
| `RMC_EDITOR` (then `VISUAL`, `EDITOR`) | `nvim` (if missing: vim, vi, nano) |
| `RMC_VIEWER` | the editor, plus `-R` for vim/nvim or `-v` for nano |
| `RMC_PAGER` (then `PAGER`) | `less`, for files over 10 MB; on Windows Git for Windows' `less.exe` is found too. Without a pager, F3 asks like F4 |
| `RMC_SHELL` | `pwsh`, then `powershell`, then `cmd`; on Linux `$SHELL` |
| `RMC_LOG` | file to log the shell's pseudo-console traffic to, for debugging (typed input is logged as a byte count only) |
| `RMC_TRACE` | file to log a line per frame to: what woke rmc, how long until the frame was sent, drawing time and bytes; tells a slow rmc from a slow terminal |

Favourite folders are saved in `%APPDATA%\rmc\hotlist.txt` (one path per line; you can edit it).
The folders visited (Alt+H) and the commands typed (Alt+P / Alt+N) are kept in `history.txt`
beside it: the last 300 folders and 500 commands.

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
| `src/panel.rs` | a panel: listing, sorting, cursor, marks, back/forward, filter, a zip shown as a folder |
| `src/ops.rs` | copy/move/delete/extract/pack jobs on a worker thread (progress, questions, cancel) |
| `src/archive.rs` | zip archives: the index, names, times |
| `src/git.rs` | git: the branch, operation under way and worktrees from the repository's files; `git status` in the background |
| `src/watch.rs` | watching the panels' folders for changes (Windows change notifications, inotify) |
| `src/history.rs` | folders visited and commands typed, kept in `history.txt` |
| `src/compare.rs` | Shift+F2's comparison of two folders |
| `src/preview.rs` | quick view: reading what to show on a background thread |
| `src/clipboard.rs` | files on the Windows clipboard (as Explorer puts them), OSC 52 elsewhere |
| `src/lnk.rs` | reading where a Windows shortcut leads |
| `src/sizes.rs` | directory size measuring on a worker thread |
| `src/mouse.rs` | mouse handling, using the screen areas `ui.rs` records each frame |
| `src/subshell.rs` | the persistent shell: ConPTY via portable-pty, screen copy via vt100, key encoding |
| `src/hotlist.rs` | favourite folders and their file |
| `src/find.rs` | the Find search: ignore + grep crates on a background thread, cancellable |
| `src/finder.rs` | the Find window's state: fields, results sorted by path, cursor |
| `src/settings.rs` | `settings.ini`: loading, saving |
| `src/survey.rs` | counting what a delete would remove, and checking permissions, in the background |
| `src/admin.rs` | "As admin": starting the elevated helper (`rmc --admin-job`) and the helper itself |
| `src/options.rs` | the options editor's rows |
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
console. That works for rmc inside WSL too: give them the pid of its `wsl.exe`. A `NO_COLOR`
variable in the environment the window inherits (some tools set it) turns rmc's colours off,
as crossterm honours it.

```powershell
wt -w new cmd /c rmc E:\some\dir          # then find its pid: Get-Process rmc
tools\sendkey.ps1 -ProcessId <pid> -Keys 'ctrl+o', 'text:dir', 'enter'   # also esc, f1-f12, alt+f1, shift+f4, ctrl+d…
tools\readscreen.ps1 -ProcessId <pid>      # prints what the window shows
```

## Not there yet

Writing into archives, other archive formats (7z, rar, tar), SFTP, a user menu, themes.
