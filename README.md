<p align="center">
  <img src="logo.svg" alt="ssh-autoport logo" width="200">
</p>

# ssh-autoport

Auto-discover and port-forward apps running on your active SSH connections — with a rich TUI.

Open an SSH session to a server. `ssh-autoport` notices it, finds every app
listening on a TCP port over there (web apps, APIs, dev servers, databases, …),
forwards each one to a local port, and remembers the mapping — so
`localhost:3000` points at the same app tomorrow as it does today.

```
 ssh-autoport   auto-forward ON  ·  2 connections

 ● bob@web01.example.com  [web01]
   PROCESS                  REMOTE             LOCAL  STATUS
 ▸ node                    lo:3000   127.0.0.1:3000   ● forwarded  ⊙ pinned
   gunicorn                 *:8000   127.0.0.1:8000   ● forwarded
   postgres                lo:5432   127.0.0.1:5432   ● forwarded
   + 3 system ports hidden (press a)

 ⟳ deploy@10.1.4.7  connecting…

 ↑↓ select · ⏎/e set port · f forward on/off · o open · a system ports · p auto · r refresh · q quit
```

## Highlights

- **Signal, not noise.** Listeners in the ephemeral port range (≥ 32768) and
  clusters of same-named loopback ports — Jupyter kernels, RPC workers, IDE
  helpers — are classified as *background*: hidden by default and never
  auto-forwarded, so twenty kernels don't become twenty useless tunnels.
  Well-known dev ports (3000, 8000, 8080, 8888, …) are never demoted. Press
  `a` to see everything, `h` to permanently promote/hide any port (the `⚑`
  marker shows your overrides), and forwarding a hidden port manually
  promotes it automatically.
- **Zero server footprint.** Nothing is installed remotely. Scanning runs one
  short-lived `sh` per refresh, using whatever the server already has:
  `ss`, then `netstat`, then raw `/proc/net/tcp` (the same trick VS Code
  Remote uses). Works on any Linux box and most other unix-likes.
- **Watches your connections.** Polls the local process table for `ssh`
  sessions; new connections appear in the table automatically, closed ones are
  torn down (with their forwards).
- **Persistent memory.** Each `server + remote port` pair keeps its local
  port, on/off state, and your notes across restarts
  (`~/.config/ssh-autoport/state.json`); per-server pauses are remembered too.
  If a remembered port happens to be taken, a new one is chosen automatically —
  and remembered.
- **Correct under port collisions.** Two servers both running an app on
  `:8888`? Whoever gets local 8888 first keeps it; the other app is forwarded
  on the next free port. Config `LocalForward`s that silently failed to bind
  (their port was taken when your ssh started) are detected and taken over
  instead of being trusted blindly.
- **Manual control.** Assign any local port yourself in the TUI (this *pins*
  it). If the port can't be used you're told exactly why, e.g.
  `can't use 8123: used by "python3"`.
- **One connection per host.** Forwards ride an SSH ControlMaster multiplexed
  connection. If your ssh config already runs a ControlMaster for that host,
  it's shared — no second login at all.
- **Plays nice with tunnel-style host configs.** `RemoteCommand`,
  `RequestTTY`, `SessionType none` (Jupyter/cloud notebook configs) are
  neutralized on our own connections. Ports you already forward yourself
  (config `LocalForward` or `ssh -L`) are detected and shown as
  `⇄ via your ssh` instead of being forwarded twice.
- **Standalone & portable.** A single static-ish binary; the only runtime
  dependency is the `ssh` client you already use.

## Install

```sh
cargo build --release
install -m755 target/release/ssh-autoport ~/.local/bin/
```

## Use

```sh
ssh myserver        # in one terminal (or many, to many servers)
ssh-autoport        # in another
```

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | select an app |
| `Enter` / `e` | type a local port for the app — checks availability, pins it |
| `f` | forwarding on/off for the selected app (remembered) |
| `F` | forwarding on/off for the whole server (remembered) |
| `p` | forwarding on/off globally |
| `h` | hide a port (stop forwarding, drop from view) or promote a hidden one (remembered) |
| `c` | attach a note to the app — shown in the table, remembered |
| `o` | open `http://127.0.0.1:<port>/` in your browser |
| `a` | show all ports, including background and system ones |
| `r` | rescan now |
| `q` | quit — cancels our forwards, closes our masters |

`f`/`F`/`p` are one switch at three scopes (app / server / global). Turning a
scope **off cancels its forwards immediately** and stops auto-forwarding
there; turning it back on re-establishes them on their remembered ports.
Manual forwards (`f`/`e` on a row) work even while a wider scope is off —
that's also how `--no-auto` mode operates. Cancellations are verified: if a
forward genuinely can't be stopped, its row flips back to `forwarded` with an
error toast rather than pretending it's gone.

The detail panel under the table shows the selected app's pid and full
command line (fetched from `/proc/<pid>/cmdline`, so twenty `python`
processes stop looking identical), plus your note.

Options: `--interval <secs>` rescan cadence (default 3), `--no-auto` manual
mode, `--show-system` show infrastructure ports at start.

## How it connects

1. Active sessions are found via the process table and resolved with
   `ssh -G`, so config aliases, jump hosts, and per-host settings all apply.
   CLI flags that affect identity (`-p`, `-l`, `-i`, `-F`, `-J`) are honored.
2. If your config defines a ControlPath with a live master behind it, forwards
   are added to *your* connection (`ssh -O forward`). On exit (including
   SIGTERM/SIGHUP) only the forwards we added are cancelled — your master and
   your own forwards stay untouched.
3. Otherwise a private master is opened with `BatchMode=yes` (key/agent auth
   only — it will never hang on a password prompt), with your config's
   `RemoteCommand`/`LocalForward`s suppressed so it can't clash with your
   session. If that fails you'll see the reason in the TUI.
4. Scans and forward requests go through the control socket with
   `-F /dev/null`, so no config directive can interfere after the connection
   exists.

**Using password auth?** Add this to `~/.ssh/config` so ssh-autoport can share
your already-authenticated session instead of opening its own:

```
Host *
  ControlMaster auto
  ControlPath ~/.ssh/cm-%C
  ControlPersist 5m
```

## Notes & limitations

- Local machine: Linux/macOS (needs `ps`, `ssh` with ControlMaster — i.e. not
  the Windows OpenSSH port).
- Remote process names are only visible for processes you own (standard
  `/proc` permissions); root sees everything. Unknown ones show as `?` but are
  forwarded all the same.
- Sessions started with a remote command (`ssh host top`) are ignored on
  purpose; plain login shells and `ssh -N` sessions are tracked.
- State file: `~/.config/ssh-autoport/state.json` (override with
  `SSH_AUTOPORT_STATE`). Control sockets live under `$XDG_RUNTIME_DIR`.
