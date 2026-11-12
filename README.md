# Crescent Moon

Crescent Moon (`cm`, mc turned around) is a small Midnight Commander-style two-panel file
manager for the terminal, written in Rust.
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
- **SSH**: a panel can show a folder on another computer, and F5, F6, F7, F8, F2, F3 and F4 work
  on it. Your own `ssh` does the login, with your keys, agent and `~/.ssh/config`.
- **Git** at a glance: a row below each panel's path with the branch, whether it is a
  worktree and what changed, a mark beside every changed, new or conflicting file (ignored
  ones dimmed), and Alt+W jumps between the repository's worktrees, into the same subfolder.
- **Colour schemes**: Alt+C shows each one on the whole screen as you move through the list:
  mc's classic blue (and classic-v2: a darker blue, a yellow cursor bar), dark, the terminal's
  own colours, light, Nord, Gruvbox, Dracula, Tokyo Night, Catppuccin, One Dark, Solarized, mono. F4 there turns one into a file of your own to
  change. Archives, pictures, sound and video, leftovers and hidden files have colours of their own.
- Copies and moves never put a file at risk: an overwrite only replaces the old file once the
  new one is complete and on the disk, a move to another drive deletes each file only after
  that, and the same file under another name is recognised. Two names that land on one (from
  a disk that respects case to one that doesn't) never replace each other.
- Deleting never reaches through a link or junction, nor on Linux into a mounted file system.
- Long paths (over 260 characters) work, as do Czech and other non-ASCII names everywhere. So
  do names Windows itself reads as something else: `foo.` and `foo ` (from WSL, Linux or a Mac)
  are copied, moved and deleted as themselves and never taken for `foo`, and a file called
  `nul` or `con` is a file. (The Recycle Bin and other programs can't reach them: Shift+F8
  deletes them, and Shift+F6 renames them to something they can open.)
- F3/F4 open files in **Neovim** (or any editor you set) instead of a built-in viewer and editor.
- F12 lists every command, found by typing part of its name.

## Build and install

```powershell
cargo test
cargo install --path .     # puts cm.exe into ~/.cargo/bin
cm                        # or: cm LEFT_DIR RIGHT_DIR
```

The release build links the C runtime statically (`.cargo/config.toml`), so `cm.exe` can be
copied to another PC without installing anything.

On Linux the same `cargo install --path .` puts `cm` into `~/.cargo/bin`. Building inside WSL
from the Windows checkout works; keep the build output on the Linux side, where it is much
faster:

```bash
cd /mnt/e/devel/midnight-commander
CARGO_TARGET_DIR=~/cm-target cargo install --path .
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

Press **F1** inside cm for the full list, or **F12** to find a command by typing part of its
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
| Alt+B | the git repository's branches, newest first: type to find one, Enter switches to it after asking (see Git) |
| Ctrl+E | in a git repository: the places you keep there; a key goes to one, Ins adds where the panel is (see Places) |
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
| Insert, Shift+Up/Down | mark files |
| `+` / `-` | mark / unmark what matches a pattern (`*.txt`), files and folders; a slash at the end (`*/`, `src*/`) takes folders only. The option "+ and - mark folders too" (Ctrl+P) off makes it files only, as in mc |
| `*` | invert the marks on files |
| Ctrl+A | mark everything (with an empty command line) |
| F2 | rename: the name is selected, so typing replaces it and the arrows edit it |
| F5 / F6 / F7 | copy / move / make directory; the progress shows the speed and the time left |
| Shift+F5 / Shift+F6 | copy / rename the item under the cursor in this folder, as in mc, whatever is marked. The name is selected, as with F2, and a name that is taken gets the usual question |
| F8, Delete | to the Recycle Bin (Shift: permanently); see Deleting as administrator |
| Alt+A | attributes of the marked files (or the one under the cursor): read-only, hidden, system, archive on Windows, the permission bits (rwx) on Linux. Space toggles, Enter sets them; a row where the files differ shows [?] and is left as each has it |
| Alt+F5 / Alt+F9 | pack into a zip / unpack the zip under the cursor (see Zip archives) |
| Ctrl+C / Ctrl+X | the marked files (or the one under the cursor) to the clipboard, copied or cut, the way Explorer puts them there: paste them in Explorer, a mail or a chat |
| Ctrl+Shift+C | their full paths to the clipboard as text |
| Alt+V | paste files copied or cut in Explorer or in cm into the panel's folder (cut ones are moved, and the clipboard is emptied afterwards, as Explorer does). Windows Terminal keeps Ctrl+V for its own paste; where a terminal lets Ctrl+V through, it works too. On Linux cm keeps its own clipboard for files, and the paths go to the terminal's clipboard (OSC 52, over ssh too) |

#### Everything else

| Key | Action |
| --- | --- |
| Esc | close any dialog; stop a running copy, move or delete (no extra box to dismiss); stop waiting for a slow folder |
| Ctrl+O | switch between the panels and the shell (one pwsh for the whole session) |
| Ctrl+P | options (see Settings) |
| Alt+C | colour scheme: the list shows the one under the cursor on the whole screen; Enter keeps it, Esc goes back, F4 edits it as a file (see Colour schemes) |
| F10 | quit (right away; it asks only if the option says so, or while the shell is busy) |
| F12 | every command with its key, found by typing |

Typing anywhere goes to the command line; Enter runs it in the shell (see below), in the
active panel's folder. `cd DIR` is handled by cm itself and changes the panel's folder; it
reads quotes and environment variables as the shell would, and `cd -` goes back.

On Windows the bar at the bottom follows Shift, Ctrl and Alt: hold one for a moment and it shows
what that key does, and a click there works as with the key held. A terminal on Linux or over ssh
doesn't tell a program about those keys held on their own, so there the bar stays as it is.

Panels show what other programs change in their folders (a download finishing, a build) by
themselves, as Explorer does, keeping the cursor and the marks; removable drives aren't
watched, so they can still be ejected. The window's title names the active folder.

Each panel remembers where it was on every drive: picking E: in the drive picker (or `cd e:`)
returns to that folder, and picking the drive the panel is already on goes to its root.
Network drives show the share they are mapped to and their free space, which is fetched in
the background so an unreachable share can't freeze the screen.

Folders are read in the background too. A slow one keeps out of the way: the panel goes on
showing what it showed until the new folder is in, and after half a second says what it is
waiting for. Esc stops the wait, Tab goes to the other panel, and F10 leaves. Keys typed in that
first half second are for the folder that is coming, as they would be behind a quick one; once
the wait shows, other keys do nothing. The same goes for a zip on a share, and for a server that
has gone quiet over SSH, which gets 30 seconds per request.

### Deleting as administrator

The delete question appears at once and fills in what would go (files, folders, size) while
it counts in the background; Yes doesn't have to wait for it, No or Esc stops it. It also
checks whether you may delete everything. Where you may not, it says which item and why
(who owns it), and on Windows offers **As admin**, as does the error dialog if a delete runs
into such a file halfway.

As admin starts a second cm with administrator rights, after Windows asks you (UAC). It
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
- **F3** and **Enter** take a file out into cm's temp folder and view it or open it in its
  app (a zip in the zip opens as a folder too); quick view (Ctrl+Q) reads files straight from
  the zip. Ctrl+Space sizes its folders at once. What Enter opens is a read-only copy: save
  changes somewhere else, as it goes when cm ends. What F3 shows goes when the viewer closes.
- **Alt+F5** packs the marked files (folders with what they hold) into a new zip, in the
  other panel's folder by default; **Alt+F9** unpacks the zip under the cursor into the other
  panel's folder, and one with several things at its top into a folder of its own name, as
  7-Zip's "Extract to name\" does. Both are Total Commander's keys.
- A zip that is there is only replaced by a complete one: if a file can't be read (and is
  skipped), the new zip goes beside it as `name (incomplete).zip`.
- Names in a zip that would land outside the folder it is unpacked to (`../x`, `C:x`) are left
  out, and cm says so.
- Names are UTF-8 when the zip says so or when they read as UTF-8; older zips made by Windows
  use its OEM code page (852 on a Czech Windows), which cm reads too. Names that would climb
  out of the folder they are unpacked into (`../x`) are left out. Packing writes UTF-8 names,
  deflated, ZIP64 for files over 4 GB.

### SSH

**Ctrl+N** connects the panel to another computer: `host`, `user@host`, `host:/path`, or
`ssh://user@host:port/path`. `cd host:/path` on the command line does the same, and the drive
picker (Alt+F1 / Alt+F2) lists the computers connected to lately and the hosts of
`~/.ssh/config`. Addresses go in the hotlist and the folder history like folders do.

cm runs your `ssh` program and speaks SFTP through it, so whatever `ssh host` does works here
too: keys, the agent, aliases from `~/.ssh/config`, ProxyJump. If `ssh` needs to ask something (a
password, a passphrase, whether to trust a new host), the terminal is handed over for that,
and Ctrl+C gives up. cm itself never sees a password; it refuses one written in an address.
Connections stay open until cm ends. The server has to offer SFTP, as OpenSSH does.

- **F5 / F6** work between a panel here and a panel there, between two computers, and within one.
  The field takes an address too, so a local panel can copy straight to `host:/path/`. In a
  panel on another computer a name counts from the folder there, and a whole path (`C:\x`, `/x`)
  is one on this computer, as the field starts on the other panel. Shift+F5 and Shift+F6 stay on
  the panel's computer, unless a drive is named.
- It is as careful as a local copy: a file is written under a temporary name and replaces its
  target in one step, a move deletes the source only once the copy is complete and its size is
  checked, and nothing is replaced without asking. Times and permissions go along. A move
  within one computer is a rename.
- **F3** brings a copy here for the viewer. **F4** brings one for the editor, and if you saved
  it, it goes back when the editor closes, keeping the permissions the file had. On a link
  both go to what it leads to, and the link stays. If the file changed over there meanwhile,
  cm asks, and keeps yours in `rescued` beside the settings whichever way you answer. An editor
  that goes on by itself (gvim, code) can't be followed: what it saves stays here until cm ends,
  and cm says so. Enter opens a copy in its app (read-only), and a zip as a folder.
- **F8** deletes for good, as there is no trash over SSH; the question says so, and Enter answers
  No. Links are never followed. What is mounted in a folder is left alone as far as a server
  shows it: another disk is seen, a bind mount of the same disk isn't.
- **F2, F7** and Shift+F5 / Shift+F6 work there too, and sorting, filters, marks, quick search
  and Shift+F2's comparison, which goes by what the panels show.
- What needs this computer says so: a command typed on the command line isn't run for a panel
  on another computer (it would run here, in another folder), and neither is Find, Alt+A,
  packing, the file clipboard, Ctrl+Space or quick view. Ctrl+O is this computer's shell.
- `..` at the top of the other computer, or a drive from the picker, comes back to this one. If
  the connection is lost, the panel comes back and says so; a job stops at once and keeps its
  sources. Ctrl+R reads the folder again: what others change there doesn't show by itself.
- Links on another computer are copied as links, except to Windows (where only administrators
  may make them): a link to a file brings the file, and a link to a folder is skipped and
  reported, as it may lead to the top of the disk. A name that can't exist here (`a:b` on
  Windows) is skipped and reported, and so is a name that isn't UTF-8 going over SFTP. In the
  field, a name holding a colon that isn't there yet is read as an address, as in scp: `./a:b`
  is a name.
- A replaced file is a new file with the old one's permissions. As in a local copy, it belongs to
  you unless you are root, and hard links to the old one stay with the old contents.
- A folder reached both over SSH and through a share mounted here is two folders to cm. A file
  moved onto itself that way is noticed and left in place; a whole folder moved into itself goes
  down to the depth limit and says so, with nothing lost.

### Git

In a git repository the row below the panel's path is git's: the branch and how the working
tree stands, in as many words as fit: `main ↑1 · 3 changed · 1 untracked`, or in a narrow
panel `main ↑1 ~3 ?1`. ↑ and ↓ count the commits ahead of and behind the upstream branch,
and files in conflict come first (`· 2 conflicts`). A rebase, merge, cherry-pick or bisect
under way shows the way git's prompt names it (`main REBASE 2/5`, `main MERGING`), and a
detached HEAD shows its commit (`(1a2b3c4)`). In a linked worktree the branch is cyan and
the row says so: `login · worktree · 2 changed`; a narrow panel leaves the cyan alone to
tell. A panel too short for the row keeps its rows for files.

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
- **Alt+B** lists the repository's branches, the one with the newest commit first: the
  branches here, and those of a remote that no branch here has or follows (`origin/fix`). Each
  shows when its newest commit was made, how it stands against what it follows (`↑1 ↓2`,
  `gone`) and that commit's first line; typing narrows the list by name or by that line. As in
  `git branch`, `*` marks the branch this worktree is on and `+` one another worktree has.
  Enter on a `+` branch goes to that worktree, like Alt+W. On any other branch cm asks, then
  runs `git switch` (`git switch --track origin/fix` for a remote one, which makes a branch
  `fix` that follows it). Changes not committed go along, and git refuses when they would be
  lost; cm then shows what git said. The switch is never stopped halfway, and keys wait
  until it is done. git lists the branches on a thread of its own, so a slow share doesn't
  hold the screen.

### Places

In a git repository **Ctrl+E** opens a menu of the places you keep going back to there: `r`
the repository's top, then the ones you add. A key goes there at once, as do Enter and a
click; a place that is a file puts the cursor on it, so F4 edits it. The menu opens on the place
the panel is at, and Alt+Left comes back.

- **Ins** adds where the panel is: its folder, or the file under the cursor, under the first
  key no place has.
- **Del** forgets the place under the cursor.
- **F4** opens all of them in the editor, as `places.ini` beside the settings.

The places are kept per repository, as paths from its top, so they are the same in every
worktree of it. A repository goes by the address of its remote (origin), so they are the same in
every clone of it too; one with no remote goes by where it is. A place that isn't there (a
worktree on an older branch) is left out. A `*` in a path stands for any one folder, and makes a
place of each folder that has the rest:

```ini
[git@gitlab.example.com:team/workspace.git]
1 = * package | packages/*/package.json
s = Scripts | scripts
d = Docs | docs/guide.md
```

gives `1 app package`, `2 blog package` and so on for each folder in `packages` with a
`package.json`, keys counting on from the digit, and the menu opens on the app package
anywhere in `packages/app`. Lines that are wrong are said when the menu opens, and the rest
works. With text on the command line Ctrl+E goes to its end, as in bash; outside a repository it
says so.

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
(or the first command typed in cm) and runs in a pseudo-console (ConPTY) that cm owns, so it
keeps going while the panels are shown: variables stay, and a long command keeps running in the
background. Ctrl+O switches between the panels and the shell's screen, which cm draws full
screen; Shift+PgUp/PgDn or the mouse wheel scroll back through its output.

- If you `cd` in the shell, the active panel follows when you come back to the panels.
- If you move the panel and the shell is idle at its prompt, the shell follows when you switch
  to it. That's silent: cm presses a key the shell's start-up script binds (Ctrl+Alt+Shift+F12),
  which changes folder and redraws the prompt in place, so nothing shows up as a command or in
  your history.
- A command typed in cm while the shell is busy is refused rather than typed into the running
  program. Text typed in the shell while a command ran keeps it busy too, as it waits at the
  next prompt; keys the program read itself don't: htop's q, a password, what goes over ssh
  (a program on the whole screen, or on Linux one that reads key by key or without echo).
- `exit` ends the shell and returns to the panels; the next Ctrl+O starts a new one.
- `cm` typed there (or in cm's command line) doesn't start a second cm inside the first: it
  says cm is already running in this terminal, and Ctrl+O goes back to it. A tab or window
  opened from there is a terminal of its own, so cm starts there as usual. (cm tells by the
  `CM_PID` it passes to everything it runs, and checks which processes share the console, or
  on Linux the session.)

This works with pwsh or Windows PowerShell, bash and zsh: cm starts the shell with a small
start-up script that loads your usual one (your profile, `~/.bashrc`, or `.zshenv` and `.zshrc`
from your ZDOTDIR), then hooks the prompt so each prompt reports its number and folder, and
binds the silent follow key. bash redraws its prompt by drawing a new one in the old one's
place (bash 4.4 or later; older ones get a visible ` cd`). With cmd, sh, fish or other shells
the shell still works, but the folders don't follow each other, and cm can't tell whether a
command is still running, so it never refuses one.

### Mouse

Click selects, double-click opens, right-click marks, the wheel scrolls the panel under the
pointer. Click a column heading (Name, Size, Modify time) to sort by it, and again to flip
the order; an arrow on the heading shows which way round it is (↑ A to Z, small to big, old
to new). Click the F-key bar labels to run them; dialog buttons and the rows of the drive
picker, hotlist, sort menu, folder history, worktree list, F12 list, colour schemes and attributes are
clickable too, as are the tabs. The wheel scrolls the quick view.

While cm has the mouse, Windows Terminal selects text with **Shift+drag**. Start it with
`cm --no-mouse` to leave the mouse to the terminal.

## Settings

Ctrl+P opens the options editor; arrows move, Space toggles a checkbox, and you type into a
text row (it counts when you move off it). Everything takes effect at once and is saved in
`settings.ini` next to the hotlist (`%APPDATA%\crescent-moon`, or `~/.config/crescent-moon` on
Linux), a plain `name = value` file you can also edit by hand. (Crescent Moon used to be called
rmc: the first time it starts, it copies what is in rmc's folder, `%APPDATA%\rmc` or
`~/.config/rmc`, into its own.)

| Option | Default |
| --- | --- |
| Ask before quitting (F10) | off: F10 quits at once, unless the shell is still running a command |
| Show hidden files | on (Ctrl+H or Alt+. toggles it for the session) |
| Mouse | on (`--no-mouse` turns it off for one run) |
| Left/Right go up and into folders | on: with an empty command line, Left is the parent folder and Right the folder under the cursor |
| Numbers in names by value | on: "day 2" sorts before "day 10", as in Explorer |
| Git: mark changed and new files | on: in a repository `git status` runs in the background (see Git); off, only the branch shows |
| + and - mark folders too | on: `+` and `-` take folders as well as files; off, files only, as in mc. A pattern ending in a slash (`*/`) takes folders either way |
| Colour scheme | classic; Enter opens Alt+C's list (see Colour schemes) |
| Big file limit, MB | 10: bigger files go to the pager on F3, and F4 asks first |
| Editor, Viewer, Pager, Shell | empty: automatic, from the variables below, then what is installed |

Environment variables, used when the matching option is empty:

| Variable | Default |
| --- | --- |
| `CM_EDITOR` (then `VISUAL`, `EDITOR`) | `nvim` (if missing: vim, vi, nano) |
| `CM_VIEWER` | the editor, plus `-R` for vim/nvim or `-v` for nano |
| `CM_PAGER` (then `PAGER`) | `less`, for files over 10 MB; on Windows Git for Windows' `less.exe` is found too. Without a pager, F3 asks like F4 |
| `CM_SHELL` | `pwsh`, then `powershell`, then `cmd`; on Linux `$SHELL` |
| `CM_LOG` | file to log the shell's pseudo-console traffic to, for debugging (typed input is logged as a byte count only) |
| `CM_TRACE` | file to log a line per frame to: what woke cm, how long until the frame was sent, drawing time and bytes; tells a slow cm from a slow terminal |

Favourite folders are saved in `%APPDATA%\crescent-moon\hotlist.txt` (one path per line; you
can edit it).
The folders visited (Alt+H) and the commands typed (Alt+P / Alt+N) are kept in `history.txt`
beside it: the last 300 folders and 500 commands.

Ctrl+\ only works while a US-style layout is active (on the Czech layout that key is a dead
key), hence Ctrl+D. Directory sizes don't follow links or junctions, so nothing is counted twice;
with sizes measured, F9's size sort orders directories by them.

### Staying in the last directory

`cm --cwd-file FILE` writes the active panel's directory to `FILE` on exit. With this function
in your PowerShell profile, `cm` leaves the shell in that directory:

```powershell
function cm {
    $f = New-TemporaryFile
    cm.exe --cwd-file $f @args
    $dir = Get-Content $f -ErrorAction SilentlyContinue
    Remove-Item $f
    if ($dir) { Set-Location -LiteralPath $dir }
}
```

## Colour schemes

**Alt+C** (or the Colour scheme row of Ctrl+P, or F12 "colour") lists the schemes. Moving
through the list shows the one under the cursor on the whole screen, panels and dialogs
alike; **Enter** keeps it (saved in `settings.ini`), **Esc** goes back to the one before, a
letter jumps to the next name starting with it. A click on a row keeps that one.

| Scheme | Look |
| --- | --- |
| classic | mc's blue panels, cm's look from the start |
| classic-v2 | classic in exact colours on a darker, calmer blue, with a yellow cursor bar: marked files full yellow, in the panels' blue on the bar |
| dark | classic on black, with dimmer lines and hidden files |
| terminal | the terminal's own background and palette, with `ls`'s colours for folders and files |
| light | black on white, red marks (as Total Commander marks), Windows' blue cursor |
| nord, gruvbox, dracula, tokyo-night, catppuccin, one-dark, solarized-dark, solarized-light | the well-known palettes ([Nord](https://www.nordtheme.com), [Gruvbox](https://github.com/morhetz/gruvbox) dark, [Dracula](https://draculatheme.com), [Tokyo Night](https://github.com/enkia/tokyo-night-vscode-theme), [Catppuccin](https://catppuccin.com) Mocha, Atom's One Dark, [Solarized](https://ethanschoonover.com/solarized)) |
| mono | no colours, only bold, dim and reverse video |

Files have colours by kind: folders, programs, archives (zip, 7z, tar, gz…), pictures (jpg,
png, raw photos such as cr2, cr3, nef, arw, dng; svg…), sound and video, leftovers (`.bak`,
`.tmp`, `.swp`, `name~`), hidden files and those git ignores. Source code and documents have
their own parts too, plain in the built-in schemes.

**F4** in the list opens the scheme in the editor as a file: a built-in one is first written
out as `<name>.ini` in the `schemes` folder beside the settings
(`%APPDATA%\crescent-moon\schemes`, `~/.config/crescent-moon/schemes`), every part spelled out
with what it is, and from then on that file takes the built-in one's place (delete it to get the built-in one back). Back from the editor
the list reads it again and shows it at once; a line it can't read is named under the list.
A file closed without a change is removed again, so a look doesn't pin the scheme: the
built-in one keeps getting cm's updates.
Any other `*.ini` file there is a scheme of its own, named after the file. A line gives a
part of the screen its look:

```ini
base = nord                     ; start from this built-in scheme (classic if none)
description = Nord, my way      ; what the list says about it
panel  = #d8dee9 on #2e3440     ; colour [on colour] [bold] [dim] [italic] [underline] [reverse]
cursor = on #4c566a             ; only a background: a file keeps its own colour on the cursor
marked = lightyellow bold
temp   =                        ; empty: nothing special
```

A colour is `#rrggbb`, a number from 0 to 255 (the terminal's palette), `default` (the
terminal's own), or one of the sixteen names: black, red, green, yellow, blue, magenta,
cyan, gray, darkgray, lightred, lightgreen, lightyellow, lightblue, lightmagenta,
lightcyan, white. A part drawn on a panel, in a dialog or on the command line has their
background unless it names one; a file's look builds up from the panel's, its kind's,
hidden, ignored, marked and the cursor's.

Terminals that show only 256 colours (or 16) get the nearest of theirs. cm takes any
colour in Windows' consoles and Windows Terminal (WSL in it too), and on Linux where
`COLORTERM=truecolor` (or `24bit`) says so. ssh doesn't pass `COLORTERM` on, so a server
reached from Windows Terminal gets 256 colours unless its `~/.bashrc` sets
`export COLORTERM=truecolor`. The list says what the terminal gets. With `NO_COLOR` set,
cm shows the mono scheme.

## Layout

| File | Contents |
| --- | --- |
| `src/main.rs` | arguments, terminal setup, event loop, running nvim/shell/commands |
| `src/app.rs` | application state and all key handling |
| `src/keybar.rs` | the bar at the bottom, and what it shows while Shift, Ctrl or Alt is held |
| `src/panel.rs` | a panel: listing, sorting, cursor, marks, back/forward, filter, a zip shown as a folder |
| `src/reading.rs` | reading a folder for a panel on a thread of its own, so a slow share can't hold the screen |
| `src/ops.rs` | copy/move/delete/extract/pack jobs on a worker thread (progress, questions, cancel) |
| `src/ops/remote.rs` | jobs with an end on another computer: copying, moving, deleting over SSH |
| `src/ssh.rs` | addresses, and connections through the `ssh` program |
| `src/sftp.rs` | an SFTP client over any pair of pipes, with many requests under way |
| `src/lfs.rs` | file calls that give Windows a path literally when it has to be (`foo.`, `nul`), and keeping a copy's flags and mode |
| `src/sandbox_tests.rs` | the jobs at the edges, on real files: links, odd names, long paths, locked files, crafted zips |
| `src/sandbox_tests/random.rs` | random colliding trees through every job and way of answering, the whole disk checked after each (`CM_ROUNDS=2000 cargo test random_` for a long run) |
| `src/archive.rs` | zip archives: the index, names (none that would land outside the folder), times |
| `src/git.rs` | git: the branch, operation under way and worktrees from the repository's files; `git status` in the background |
| `src/places.rs` | Ctrl+E: the places kept per git repository in `places.ini`, and the menu of them |
| `src/watch.rs` | watching the panels' folders for changes (Windows change notifications, inotify) |
| `src/history.rs` | folders visited and commands typed, kept in `history.txt`; what was typed into dialogs, in `inputs.txt` |
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
| `src/scheme.rs` | colour schemes: the parts of the screen, the built-in schemes, scheme files, fewer colours for terminals with fewer, files' kinds |
| `src/survey.rs` | counting what a delete would remove, and checking permissions, in the background |
| `src/admin.rs` | "As admin": starting the elevated helper (`cm --admin-job`) and the helper itself |
| `src/options.rs` | the options editor's rows |
| `src/dialog.rs` | dialogs, buttons, line editor, help text |
| `src/ui.rs` | drawing with ratatui |
| `src/platform.rs` | OS bits: drives and mounts, free space, hidden files, opening files, shells and their start-up scripts |
| `src/util.rs` | formatting, path resolution, wildcard matching, column fitting |
| `vendor/crossterm` | crossterm 0.29.0 with a fix for Konsole's Alt+F1, Shift+F4 and the like (see `vendor/README.md`) |

## Testing in a real terminal

`cargo test` covers most things, including a real shell in the embedded terminal: pwsh on
Windows, bash and zsh on Linux (`CM_TEST_ZSH` can point at a zsh that isn't on the PATH).
Run the Linux tests from Windows with WSL:

```bash
wsl -- bash -lc 'cd /mnt/e/devel/midnight-commander && CARGO_TARGET_DIR=~/cm-target cargo test'
```

To check cm itself in a Windows Terminal window, the scripts in `tools/` attach to its
console. That works for cm inside WSL too: give them the pid of its `wsl.exe`. A `NO_COLOR`
variable in the environment the window inherits (some tools set it) gives the mono scheme.
Trying schemes there saves the one kept in `settings.ini`; point `APPDATA` (on Linux
`XDG_CONFIG_HOME`) at another folder for the window to keep yours as they are.

```powershell
wt -w new cmd /c cm E:\some\dir          # then find its pid: Get-Process cm
tools\sendkey.ps1 -ProcessId <pid> -Keys 'ctrl+o', 'text:dir', 'enter'   # also esc, f1-f12, alt+f1, shift+f4, ctrl+d…
tools\readscreen.ps1 -ProcessId <pid>      # prints what the window shows
```

## The feature page

`site/` is the page at https://crescent-moon.starlume.eu: plain HTML and CSS, deployed by
GitLab CI (`.gitlab-ci.yml`) when it changes on main. Its animations are the real program:

```powershell
tools\demo\record.ps1              # every scene in tools\demo\scenes, into site\gifs
tools\demo\record.ps1 -Only git    # just one
```

The script builds cm and the recorder, `examples/record.rs`, which runs cm in a pseudo-console,
plays a scene's keys and keeps every screen with its time. `tools/demo/make_world.py` makes the
folders the scenes show (a git repository with branches, a worktree and changes, photos,
downloads, a zip), afresh before each scene, and `subst` maps them to M: so no real path shows.
`tools/demo/render.py` draws the screens as Windows Terminal would (Campbell colours, Cascadia
Mono) into a GIF. It needs Python with Pillow, and git.

## Not there yet

Writing into archives, other archive formats (7z, rar, tar), running commands over SSH, a user
menu.
