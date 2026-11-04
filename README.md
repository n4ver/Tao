# Tao

Sorts your Windows Downloads folder into category folders, either once on
demand or continuously in the background. It runs entirely on your machine,
with no network access and no telemetry.

```
Downloads\
  report.pdf          ->  Downloads\Documents\report.pdf
  setup.exe           ->  Downloads\Installers\setup.exe
  holiday.jpg         ->  Downloads\Images\holiday.jpg
  movie.mp4.crdownload    (still downloading: left alone)
```

## Safety guarantees

- **Files are only ever moved, never deleted.** Nothing in the code removes a
  file. (A move to another drive is a copy followed by Windows removing the
  original, done by `MoveFileExW` after the copy has been flushed to disk.)
- **Nothing is overwritten.** If `Documents\report.pdf` exists, the file
  becomes `Documents\report (1).pdf`. This is enforced by Windows itself, not
  by checking first: the rename is made with `MoveFileExW` *without*
  `MOVEFILE_REPLACE_EXISTING`, which fails atomically if the name is taken.
- **Every move is logged to SQLite before it happens,** so `undo` works even
  if the program is killed mid-move.
- **Partial downloads are never touched:** `.crdownload` (Chrome/Edge), `.part`
  (Firefox), `.tmp`, `.opdownload` and so on, plus a file whose `<name>.part`
  sibling still exists (Firefox's empty placeholder).
- **The watcher waits until a file is finished:** its size and last-modified
  time must stay unchanged for `stable_seconds` (default 5).
- **Locked files are skipped and retried** with backoff (5 s, 10 s, 20 s, ...
  up to 5 min). After about an hour the watcher gives up on that file until
  it changes. Hidden/system files such as `desktop.ini` and subfolders are
  ignored.

## Build

Requires Rust (stable, MSVC toolchain). SQLite is compiled in, so nothing
else needs to be installed.

```powershell
cargo build --release
```

This produces two executables in `target\release\`. Keep them in the same
folder; a good permanent home is `%LOCALAPPDATA%\Programs\Tao\`.

| File          | What it is                                                    |
|---------------|---------------------------------------------------------------|
| `tao.exe`     | The command-line tool. Use this for everything.               |
| `taow.exe`    | The background watcher, with no console window. `install` registers it to start at sign-in. |

### Why two executables?

Windows decides whether a program gets a console window from a flag in the
.exe header (the *subsystem*), before the program runs. A console program
started at sign-in opens a terminal window; a GUI-subsystem program has no
window, but `cmd`/PowerShell don't wait for it and its output can't be
reliably redirected. Rather than pick one wart, `taow.exe` is built with
`#![windows_subsystem = "windows"]` and only runs the watcher, logging to a
file, while `tao.exe` stays a normal console program. It's the same split
as `python.exe` / `pythonw.exe`. Both are thin wrappers around the same
library code.

## Usage

```powershell
tao sort --dry-run     # show what would be moved, change nothing
tao sort               # sort everything currently in Downloads
tao undo               # put back everything moved by the last run
tao undo --last 3      # put back the 3 most recent moves, across runs
tao watch              # run the watcher in this terminal (Ctrl+C to stop)
tao install            # start the watcher at every sign-in, and start it now
tao uninstall          # remove the sign-in entry
tao config             # open the rules file in your editor
tao -v <command>       # with debug output
```

`install` writes the value `Tao` under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, which runs a program at
sign-in for the current user, with no admin rights needed. It shows up in Task
Manager > Startup apps, where it can also be disabled. `uninstall` removes the
value; an already-running watcher keeps going until you sign out or end
`taow.exe` in Task Manager.

Only one watcher runs at a time (a named mutex, `Local\tao-watch`),
so running `tao watch` while the background one is active just exits.

On startup the watcher also sorts anything already in Downloads, and it
rescans every 10 minutes in case it missed an event.

## Files

| What                  | Where                                 |
|-----------------------|---------------------------------------|
| Rules / config        | `%APPDATA%\Tao\config.toml`           |
| Move log (SQLite)     | `%LOCALAPPDATA%\Tao\moves.db`         |
| Watcher logs (7 days) | `%LOCALAPPDATA%\Tao\logs\watch.*.log` |

Set `TAO_HOME` to put all three in one folder instead, which is
handy for trying things out. The startup entry does not see this variable.

## Config format

A commented default is created on first run. The watcher reloads it
automatically when you save; an invalid edit is logged and the previous
config stays in effect.

```toml
# downloads_dir = 'D:\Downloads'   # default: your Windows Downloads folder
stable_seconds = 5
skip_extensions = ["crdownload", "part", "partial", "tmp", "opdownload", "download"]
skip_hidden = true
fallback_dest = "Other"            # where unmatched files go

[[rule]]
name = "Screenshots"
glob = "Screenshot*"
extensions = ["png", "jpg"]
dest = "Images/Screenshots"

[[rule]]
name = "Old invoices"
regex = '^invoice[-_ ]?\d+'
older_than = "30d"
dest = 'D:\Paperwork\Invoices'

[[rule]]
name = "Images"
extensions = ["jpg", "jpeg", "png", "gif", "webp"]
dest = "Images"
```

How rules work:

- **Rules are checked top to bottom and the first match wins.** Put specific
  rules (like "Screenshots") above general ones (like "Images").
- **All conditions in a rule must match.** Available conditions:

  | Key          | Matches when                                         | Example             |
  |--------------|------------------------------------------------------|---------------------|
  | `extensions` | the name ends in one of these (no dot needed)         | `["pdf", "tar.gz"]` |
  | `glob`       | the file name matches the wildcard pattern           | `"IMG_*.jpg"`       |
  | `regex`      | the regular expression matches the file name         | `'^scan\d+'`        |
  | `older_than` | last modified more than this long ago                | `"30d"`             |
  | `newer_than` | last modified less than this long ago                | `"2h"`              |

  Durations use `s`, `m`, `h`, `d` or `w`. All name matching is
  case-insensitive, like Windows file names.
- `dest` is relative to the sorted folder unless it's an absolute path.
  Use single quotes (`'D:\Paperwork'`) for Windows paths so backslashes aren't
  treated as escapes.
- A rule with no conditions, an unknown key (e.g. a typo like `extension`) or
  an invalid pattern is rejected with a message naming the rule.
- Only files directly inside Downloads are sorted, never the contents of
  subfolders.

Check your rules any time with `tao sort --dry-run`.

## How undo works

Each `tao sort` is one *run*; the watcher starts a new run for each batch
of files it moves. For every move the log stores the source path, the exact
destination path (including any ` (1)` suffix), the time, the rule, and the
file's size and last-modified time.

- `tao undo` reverses every move of the most recent run that still has
  something to undo. Running it again goes one run further back.
- `tao undo --last N` reverses the N most recent moves regardless of run.

For each file, undo:

1. checks that the file is still at its logged destination with the same
   size and modified time. If it was edited, renamed or replaced since, it is
   **left where it is** and reported, since it might be a different file.
2. moves it back to its original name, or `name (1).ext` if a new file with
   that name has appeared since. Undo never overwrites either.
3. marks the move as undone. Undone moves are kept in the log, not deleted.

Empty category folders are left in place (nothing is ever deleted).

The watcher will not re-sort a file that undo put back, as long as it is
unchanged; otherwise undoing would just trigger another move.

If the program was killed between logging a move and making it, the log
entry is left "pending"; undo checks the disk, sees the file never moved, and
closes the entry.

The log is a plain SQLite database, so you can inspect it with any SQLite tool:

```sql
SELECT datetime(moved_at / 1000, 'unixepoch', 'localtime'), source, dest, status
FROM moves ORDER BY id DESC LIMIT 20;
```

## Development

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

File-moving tests use temporary directories only. Registry tests use a
throwaway key under `HKCU\Software\tao-test-<pid>`, never the real
`Run` key.

### Code layout

| Module          | Role                                                              |
|-----------------|-------------------------------------------------------------------|
| `config.rs`     | TOML schema and the default config                                |
| `facts.rs`      | What is known about a file (name, size, age, attributes)          |
| `rules.rs`      | Compiles rules, first-match evaluation                            |
| `classify.rs`   | Rules, then pluggable `Classifier` fallbacks, then `fallback_dest`|
| `planner.rs`    | Skip logic, turns folder contents into planned moves              |
| `mover.rs`      | No-overwrite move, collision suffixes, locked-file detection     |
| `journal.rs`    | SQLite move log                                                   |
| `undo.rs`       | Reversing logged moves                                            |
| `stable.rs`     | "Has this file stopped changing?" tracker (pure, clock injected)  |
| `watch.rs`      | Watcher loop: events, rescans, retries, config reload             |
| `startup.rs`    | HKCU `Run` entry                                                  |
| `win.rs`        | Win32 helpers: named mutex, ShellExecute                          |

### Extension points

- **Match on download source (Zone.Identifier).** Browsers record where a
  file came from in an NTFS alternate data stream, `file.zip:Zone.Identifier`
  (`HostUrl=...`). To match on it, add a lazily-read field to `FileFacts` and
  a `Condition::SourceUrl(Regex)` variant in `rules.rs`; see the comment on
  `FileFacts`.
- **Content classifier.** Implement `classify::Classifier` and register it
  with `Engine::with_fallback`. It is only asked about files no rule matched,
  and its answer is logged as the move's reason.

## Known limitations

- Paths longer than 260 characters are not supported unless long paths are
  enabled in Windows.
- If you rename or move the executables, run `tao install` again so the
  startup entry points at the new location.
