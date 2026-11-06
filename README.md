# Tao

[![CI](https://github.com/n4ver/Tao/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/n4ver/Tao/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/n4ver/Tao)](https://github.com/n4ver/Tao/releases/latest)

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
- **None of your files is overwritten.** If `Documents\report.pdf` exists,
  the file becomes `Documents\report (1).pdf`. This is enforced by Windows
  itself, not by checking first: the rename is made with `MoveFileExW`
  *without* `MOVEFILE_REPLACE_EXISTING`, which fails atomically if the name
  is taken. (The one thing Tao does replace is its own program files:
  `tao install` copies `tao.exe` and `taow.exe` over the previous copies in
  `%LOCALAPPDATA%\Programs\Tao\`, and refuses if one of them is running.)
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
folder. `tao install` copies both to `%LOCALAPPDATA%\Programs\Tao\` (see
below), so the build folder can be rebuilt or deleted afterwards.

Both carry an icon, version information (Properties > Details) and an
application manifest. These are compiled in by `build.rs` with `rc.exe` from
the Windows SDK, which the MSVC build tools include.

| File          | What it is                                                    |
|---------------|---------------------------------------------------------------|
| `tao.exe`     | The command-line tool. Use this for everything.               |
| `taow.exe`    | The background watcher with a tray icon and no console window. `install` registers it to start at sign-in. |

## Usage

```powershell
tao sort --dry-run     # show what would be moved, change nothing
tao sort               # sort everything currently in Downloads
tao undo               # put back everything moved by the last run
tao undo --last 3      # put back the 3 most recent moves, across runs
tao log                # show the last 20 entries of the move log (--limit N for more)
tao log --prune --older-than 90d --dry-run   # how many old entries could go
tao log --prune --older-than 90d --vacuum    # remove them and compact the file
tao watch              # run the watcher from this terminal, with a tray icon
tao watch --headless   # ...without the tray icon (Ctrl+C to stop)
tao install            # install, start the watcher at every sign-in, and start it now
tao install --in-place # ...registering taow.exe where it is, without copying
tao uninstall          # remove the sign-in entry
tao config             # open the rules file in your editor
tao -v <command>       # with debug output
```

`install` copies `tao.exe` and `taow.exe` to `%LOCALAPPDATA%\Programs\Tao\`
(the per-user equivalent of Program Files, no admin rights needed), replacing
older copies there. If the installed watcher is running, nothing is copied and
you are asked to quit it first (tray icon > Quit). It then writes the value
`Tao` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, pointing at
the installed `taow.exe`, so rebuilding or moving the files you installed from
no longer affects startup. Add that folder to your `PATH` to run `tao` from
any terminal; `install` tells you if it isn't there yet, but doesn't change
`PATH` itself.

The entry shows up in Task Manager > Startup apps, where it can also be
disabled. Windows records that separately, so writing the Run value again
doesn't re-enable it; `install` checks and tells you if the entry is disabled
there, without changing it. (Task Manager stores this as a binary value named
`Tao` under `HKCU\...\Explorer\StartupApproved\Run`: its first byte is `02`
when enabled and `03` when disabled, followed by the time it was disabled.
Tao only reads it.)

`uninstall` removes the Run value and leaves the installed files in place;
an already-running watcher keeps going until you sign out or end `taow.exe` in
Task Manager.

Only one watcher runs at a time (a named mutex, `Local\tao-watch`),
so running `tao watch` while the background one is active just exits.

On startup the watcher also sorts anything already in Downloads, and it
rescans every 10 minutes in case it missed an event.

### Tray icon

| Item               | What it does                                                                 |
|--------------------|------------------------------------------------------------------------------|
| **Recent moves**   | The last 10 moves still in effect, live. Click one to open its folder in Explorer with the file selected. |
| **Sort now**       | Rescans Downloads right away, including files it had given up on (e.g. ones that were locked). Files still wait the usual `stable_seconds`. |
| **Undo last move** | Same as `tao undo --last 1`.                                                 |
| **Pause / Resume** | Stops moving files (the icon turns grey). Resuming catches up on whatever arrived meanwhile. |
| **Open config**    | Same as `tao config`.                                                        |
| **Quit**           | Lets a move in progress finish, then exits.                                  |

If the tray icon can't be created (or on non-Windows systems), the watcher
logs why and carries on headless. `taow.exe --headless` skips the tray
deliberately. If the watcher itself stops, for example because the config
became invalid at startup, the icon stays, with the reason in its tooltip, so
you can open the config, fix it, and quit.

## Files

| What                  | Where                                 |
|-----------------------|---------------------------------------|
| Rules / config        | `%APPDATA%\Tao\config.toml`           |
| Move log (SQLite)     | `%LOCALAPPDATA%\Tao\moves.db`         |
| Watcher logs (7 days) | `%LOCALAPPDATA%\Tao\logs\watch.*.log` |
| Installed programs    | `%LOCALAPPDATA%\Programs\Tao\`        |

Set `TAO_HOME` to put the config, move log and logs in one folder instead,
which is handy for trying things out. The startup entry does not see this
variable, and it doesn't change where `install` puts the programs.

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
name = "Big videos"
extensions = ["mp4", "mkv"]
min_size = "1.5GB"
dest = 'D:\Videos'

[[rule]]
name = "GitHub downloads"
source_host = ["github.com", "githubusercontent.com"]
dest = "GitHub"

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
  | `min_size`   | the file is at least this big                        | `"500MB"`           |
  | `max_size`   | the file is at most this big                         | `"20KB"`            |
  | `source_url` | the regex matches the URL it was downloaded from, or the page that linked to it | `'github\.com/.*/releases'` |
  | `source_host`| it was downloaded from one of these sites or a subdomain of them | `["github.com"]` |

  Durations use `s`, `m`, `h`, `d` or `w`. All name and URL matching is
  case-insensitive, like Windows file names.

  **Sizes** use `B`, `KB`, `MB`, `GB` or `TB` (any case; `KiB`, `MiB`, ...
  also work). They are *binary* units, 1 KB = 1024 bytes, because that is
  what Explorer's Size column shows, so `"500MB"` means what Explorer
  calls 500 MB. Fractions are fine (`"1.5GB"`), a unit is required, and
  both limits are inclusive. A rule with `min_size` larger than `max_size`
  is rejected.

  **Download source** comes from the `Zone.Identifier` that browsers attach to
  downloaded files on NTFS (the same data behind "this file came from another
  computer"). It holds the URL the file was fetched from (`HostUrl`) and
  usually the page that linked to it (`ReferrerUrl`); both are checked.
  `source_host = ["github.com"]` matches `github.com` and any subdomain such
  as `objects.github.com`, but not `notgithub.com`. Give host names only (no
  `https://`, path or port). Files without that information (copied from a
  USB stick, saved by non-browser programs, private-mode downloads that
  record only `about:internet`, non-NTFS drives) never match a source
  condition. The source is only read for rules that use these conditions,
  and only after the rule's other conditions have matched. `tao sort
  --dry-run` and the move log show which URL matched:

  ```
  tool.zip  ->  GitHub\   [rule "GitHub downloads" (source https://objects.githubusercontent.com/...)]
  ```

  GitHub release downloads come from `objects.githubusercontent.com` with
  `github.com` as the referring page, so `source_host = ["github.com"]` alone
  usually matches them through the referrer; listing `githubusercontent.com`
  too also covers downloads where the browser recorded no referrer.
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

### Pruning the log

The log only grows on its own. `tao log --prune --older-than <duration>`
removes entries that finished longer ago than that and that nothing needs
any more:

- **failed** moves (the file was never moved);
- **undone** moves whose file is no longer where undo put it, unchanged.

It never removes a move that can still be undone (`done`, or `pending`
after a crash), whatever its age. It also keeps an undone move for as long as
its file is still sitting where undo put it with the same size and modified
time, because that entry is what stops the watcher from sorting the file
again. Runs left with no entries are removed too. `--dry-run` reports the
exact counts without changing anything (the prune runs and is rolled back),
and `--vacuum` compacts the database file afterwards.

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
cargo deny check      # advisories, licenses, sources (cargo install cargo-deny)
```

The minimum supported Rust version is in `Cargo.toml` (`rust-version`). CI
checks it, runs the tests on Windows and on Ubuntu (which exercises the
non-Windows fallbacks), and runs `cargo deny`. A release tag only publishes
if all of that passes.

File-moving tests use temporary directories only. Registry tests use a
throwaway key under `HKCU\Software\tao-test-<pid>`, never the real
`Run` key.

### Code layout

| Module          | Role                                                              |
|-----------------|-------------------------------------------------------------------|
| `config.rs`     | TOML schema and the default config                                |
| `facts.rs`      | What is known about a file (name, size, age, attributes, lazily its source) |
| `zone.rs`       | Parsing the `Zone.Identifier` stream, host matching               |
| `rules.rs`      | Compiles rules, first-match evaluation                            |
| `classify.rs`   | Rules, then pluggable `Classifier` fallbacks, then `fallback_dest`|
| `planner.rs`    | Skip logic, turns folder contents into planned moves              |
| `mover.rs`      | No-overwrite move, collision suffixes, locked-file detection     |
| `journal.rs`    | SQLite move log                                                   |
| `undo.rs`       | Reversing logged moves                                            |
| `stable.rs`     | "Has this file stopped changing?" tracker (pure, clock injected)  |
| `watch.rs`      | Watcher loop: events, rescans, retries, config reload, commands   |
| `tray.rs`       | Tray icon and menu (`tao` event loop on the main thread)          |
| `logging.rs`    | Console and daily log-file output                                 |
| `startup.rs`    | HKCU `Run` entry, Task Manager state, copying the executables     |
| `win.rs`        | Win32 helpers: named mutex, ShellExecute, long-path conversion    |
| `build.rs`      | Embeds icon, version info and manifest into both executables      |

The tray runs the `tao` event loop on the main thread, which Windows requires
because tray clicks arrive as window messages to the thread that created the
icon. The watcher runs on a second thread, posts `WatchEvent`s to the loop
through an `EventLoopProxy`, and takes commands (sort now, undo, pause,
quit) over a channel. The icon is 32×32 RGBA bytes embedded with
`include_bytes!` from `assets/tray-icon.rgba`. `assets/make-icon.ps1`
regenerates it from a pixel grid, together with `assets/tao.ico`, the
executables' icon (16 to 256 px).

### Extension points

- **New rule conditions.** Add a variant to `Condition` in `rules.rs` and
  the fact it needs to `FileFacts`. Facts that cost a disk read should be
  lazy (a `OnceCell`, like the Zone.Identifier source), and their conditions
  compiled after the cheap ones.
- **Content classifier.** Implement `classify::Classifier` and register it
  with `Engine::with_fallback`. It is only asked about files no rule matched,
  and its answer is logged as the move's reason.

## Known limitations

- **Long paths.** Moving files works with paths of any length (up to
  Windows' ~32,767 characters), whether or not long paths are enabled in
  Windows: the mover passes paths over 260 characters to Windows with the
  `\\?\` prefix, and both executables are marked `longPathAware`. Still
  limited: opening such a folder from the tray's Recent moves (Explorer
  itself handles long paths poorly), and any single file or folder *name*
  longer than 255 characters (an NTFS limit).
- **Download source** (`source_url`, `source_host`) is only known for files
  that keep their `Zone.Identifier`: browser downloads on NTFS drives. It is
  lost when a file is copied to a FAT/exFAT drive or unblocked in its
  Properties, and some browsers record only `about:internet` in private
  mode.
- With `tao install --in-place`, the startup entry points at wherever
  `taow.exe` was. If you rename, move or rebuild over it, run `tao install`
  again. (The default `tao install` copies the programs and doesn't have this
  problem.)
