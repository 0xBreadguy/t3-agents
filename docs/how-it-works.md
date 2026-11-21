# How it works, and what it touches

T3 Agents reads T3 Code's local database and draws what it finds. It never
writes to it, and it makes no network connections.

## What it reads and touches

- **T3 data:** it opens T3's `userdata/statev2.sqlite` read-only (`query_only`)
  and never creates, migrates or writes it. Query results contain dashboard
  metadata and counts and exclude authentication tables, prompts, message
  bodies, tool output, subagent results and reasoning text. The dashboard shows
  thread titles, tool names, file names and todo steps. For a command, it
  reads the first 240 characters of input and displays the first line locally,
  with terminal controls stripped. You can see it in the row’s current or last
  activity and in the selected agent’s details, including recent calls and
  failures. The status JSON’s `now` holds the activity category, such as
  `shell`. The plain `--once` list omits command excerpts. The developer
  option `--dump-snapshot` prints the dashboard’s data before display
  cleanup, including the bounded excerpt. Trace and Tools return only listed
  program names from a command line. Tools can also return its integer exit code. Recap counts and times
  calls. Ask counts and cached facts exclude command excerpts.
- **Open asks:** T3 records pending formal questions, secret cards and link
  cards. A question that accepts a later message can remain open while a run
  works. A separate estimate counts questions or requests directed to the
  person in assistant prose. A SQLite function inspects eligible messages
  inside the query and returns a number. The dashboard takes the most recent
  positive count since the person last wrote. Automated deliveries and repeated
  reminders do not add to the total. The estimate never changes thread state.
  The version 1 status JSON reports `openAsks` for recorded questions and
  cards and `estimatedAsks` for prose, on each returned agent and across the
  whole snapshot. The widget uses `waiting` for the number of agents that
  need you. Its list shows at most eight agents. Recap counts recorded asks
  that held up an agent during the period and measures the time spent waiting.
- **Storage on Linux:** the dashboard checks the space available on the
  managed worktree folder, each distinct device reached by a local worktree,
  and the effective temporary folder on every refresh. A memory-backed temp
  volume is identified as such. The Storage view measures allocated blocks
  from directory entries and file metadata. It includes nested `target` and
  `node_modules` directories in the worktree size and shows their subtotal.
  It attributes Claude scratch under the current user's temporary folder when
  the working directory identifies an owner. On a dedicated memory-backed temp
  volume, a complete scan also shows the remaining used space in Other. Hard links count once. Scans run
  in the background and results stay in memory. The scanner does not read file
  contents or remove anything. T3 Code owns the `storageCleanup` and
  `worktreeCleanup` settings. The demo uses example folder sizes. An explicit
  `--db` skips filesystem measurements. Disk measurements are Linux-only.
- **Reading everything, on several connections:** `--once`, `--status` and
  the dashboard's first data read every table it uses once. That read runs on
  up to three read-only connections at the same time. Before any of them
  reads, it asks SQLite (`PRAGMA data_version`) whether a write landed while
  they were starting, so all of them see the same moment, and it checks that
  the database files are still the ones it opened. If it can't be sure, or
  anything fails, it reads on one connection instead.
  `T3_AGENTS_FULL_READ_THREADS=1` always reads on one connection.
- **Staying current without rereading everything:** every two seconds it asks
  SQLite whether T3 has written anything by default. If it has, it reads the new entries
  in T3's event log to learn which threads changed, and rereads only those. From
  the log it takes each entry's sequence number, the id of the thread or
  project it names, and its kind and version, plus T3's own note of how far
  its tables have caught up with the log. It also counts T3's migration rows
  to detect changes that can happen outside the event log. It never reads
  what an event contains. Whenever it can't be sure that shortcut gives the
  same answer as reading everything, it reads everything.
- **If you'd rather it didn't read the event log:** start it with
  `T3_AGENTS_SYNC_READ=1` in its environment. It then rereads the tables it
  uses directly, the way the Node.js version did. That
  costs more CPU on a large database and changes nothing you see.
- **Insights** uses the same read-only report data as `--recap --json` and
  `--tools --json`. Opening a report starts a background read. Scrolling and
  resizing reuse the result in memory. Returning to Live ends the visit, and
  reopening reads a fresh report. The persistent report cache described below
  remains opt-in.
- **Recap:** `--recap` reads the same database read-only, with parallel
  item reads when there are enough runs. For runs open during the selected
  week or month it reads when each was asked for, started and finished, how
  it ended, its provider
  and model, and whether it continues an earlier run. For those runs' items it
  reads when each tool call started and ended and whether it failed, who sent
  the message that started the run and from where, a failure's class and code,
  compactions, and each checkpoint's count of files and of lines added and
  removed. It also reads when a person was asked something and when they
  answered, which thread was waiting, and whether an unanswered ask is still
  open; each handoff's model, end state and times; each turn's token
  counts and how full its context was; and each linked pull request's state
  and the times it was linked, merged or closed. A pull request's host,
  repository and number are read only to count one linked on two threads
  once, and are never shown. It never reads a prompt, a reply, a result, a
  failure message, a title, a file path or a command line. The opt-in report
  cache can keep derived facts for finished runs.
- **Tools:** `--tools` normally uses up to four parallel workers with
  read-only connections. A report can combine snapshots taken at different
  moments while T3 is writing. For the runs open
  during the week or month asked for it reads each one's id, its thread and
  its provider. For those runs' tool calls, and for the calls on the threads
  of providers' own subagents, it reads the call's kind, whether it failed,
  when it started and ended, a named tool's name when it looks like a name
  (letters, digits and a few marks, at most 80 characters), a command's exit
  code when it is an integer, and the names of the listed programs a command
  runs. A function inside the query passes over each call record once and
  returns only those fields. It also reads when each thread was created, to
  say how many days have records. The program names are the part to look at.
  For a command line, the function returns only names from a fixed list of
  75 programs, or nothing. Tools never returns, stores, prints or logs the
  command line. Its SQL functions can parse item records and command input
  internally. Their results contain only the fields above.
- **Report cache:** With `T3_AGENTS_CACHE=1`, Recap and Tools can reuse
  facts for finished runs from
  `insights.sqlite3` in `${XDG_CACHE_HOME:-$HOME/.cache}/t3-agents/` (an
  install made with `--name NAME` uses `NAME` for the folder when started
  through its own command). The cache stores derived facts such as timings,
  counts and program-list indices, plus hashed lookup keys and structural
  data used to verify entries. Raw thread and run IDs, tool names, command
  lines and message bodies stay out of the cache. Cache reuse requires
  Linux with the source on a supported local filesystem (ext, Btrfs, XFS
  or tmpfs). Otherwise the reports read directly from T3.
  The default is off. `T3_AGENTS_NO_CACHE=1` takes precedence, even when
  the cache is enabled. Demo reports use no cache. `T3_AGENTS_CACHE_STATS=1`
  prints hit and write counts or a cache-unused diagnostic on standard error.
  [Uninstalling](install.md#remove) explains when the installer can remove the file.
- **Trace:** `w` and `--trace` read one task the same way, read-only, and store
  nothing. For the thread's runs: when each was asked for, started and ended,
  how it ended, its provider and model, its place in the queue, and whether it
  carries on earlier work. For their items: each tool call's, ask's, reasoning
  item's and reply's start and end and status; a tool's name; a failure's
  class; and for a command, only which programs from a fixed list it runs. The
  command line is handed to a function inside the query that can only return
  names from that list, so no Trace query returns the command itself. For each
  delegate: its model, title, times, how it ended and whether it has a thread
  here. The header uses the thread's title. While Trace is open it checks the
  task on its own connection at the dashboard's refresh interval. It usually
  reuses unchanged items and skips rereading a finished task while the
  database's change counter stays the same.
- **Following T3:** under Hyprland it listens on Hyprland's event socket for
  the T3 Code window's title, which T3 sets to `project / thread`. It queries
  the window list when connecting and keeps only T3 windows' titles.
- **Opening threads:** `Enter` passes a `t3code://threads/…` link to
  `xdg-open` on Linux or `open` on macOS. Local links use the environment ID
  T3 keeps beside the database. Remote links use the recorded destination.
- **Usage limits:** read from Omarchy's agent usage records in
  `~/.local/state/omarchy/agents/usage/`, if they exist.
- **Themes:** it reads the theme it's told to use, once a second so edits
  show: its own files in `~/.config/t3-agents/themes/`, and on Omarchy the live
  theme in `~/.local/state/omarchy/current/theme/` (`colors.toml`,
  `t3-agents.toml` and `theme.name` beside it) and installed theme folders. The
  theme picker saves its choice to the `theme` line in
  `~/.config/t3-agents/config.toml` when you press `c`. With a dark and light pair away from
  Omarchy it asks the terminal for its background color and for color scheme
  reports, and switches those reports off when it quits.
- **Network:** none. The bar widget runs one `t3-agents --status --watch`
  process that checks the database and WAL file sizes and modification times
  every two seconds. It rereads after a change and at least every 30 seconds (so the failed-call
  count ages out) and prints only when the status changes. If that process
  can't start or stops, the widget tries again every 15 seconds.
- **The window:** The Linux installer's launcher handles `--window` and
  opens the dashboard with the app ID `TUI.t3-agents` by default. With
  `hyprctl` and `jq` under Hyprland, it can focus a matching window first.
  Omarchy treats `TUI.*` windows as terminal apps (Super+C
  copies instead of sending Ctrl+C, terminal opacity) and still tiles them; you
  can match the id in your own window rules.

## Limits worth knowing

Only agents in your local T3 database show up;
delegates on linked machines show the status recorded here. Cross-machine
children nest under their actual parents using both the machine and thread
IDs, including chains that return to this machine. When a parent's state is
absent locally, its row says **UNKNOWN** and identifies it as a parent reference.
Nesting does not change settlement values or infer a remote parent's settlement.
Context figures are
whatever the provider reports, sometimes only between turns. Stall thresholds
are judgment calls: a model can think for a while, and builds take minutes, so
read `◌` as "take a look", not "broken".

## Requirements and compatibility

- Linux, on x86-64 or ARM. Each Linux archive lists its required glibc version in
  `BUILD-INFO`. SQLite is compiled in. The macOS preview needs
  macOS 11 or later, on Apple silicon or Intel. Building it yourself needs a
  Rust toolchain (developed with Rust 1.99, released with 1.100).
- T3 Code's desktop app with its v2 local database. This is an unofficial
  adapter to internal storage: a T3 update can change the tables or the window
  title format. Built against T3 Code Nightly, November 2026.
- A terminal with full color or 256 colors. It has been checked against the
  original, pixel for pixel, in foot, kitty, Alacritty, Ghostty, WezTerm and a
  VTE terminal, and inside tmux. Other terminals and real SSH sessions should
  work and haven't been tried yet.
- Optional: Omarchy for the bar widget, its usage records and the live theme
  (the built-in themes work anywhere);
  Hyprland to follow the open T3 thread and to focus an open dashboard; foot,
  `xdg-terminal-exec` or `$TERMINAL` for `--window`.
- Tested on one Omarchy laptop (Hyprland 0.56). Other setups are untested so
  far, and reports are welcome.
