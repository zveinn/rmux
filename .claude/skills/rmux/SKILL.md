---
name: rmux
description: Drive the rmux terminal multiplexer as an agent — list/create/kill sessions and tabs, split panes, switch focus, run commands inside panes, and read their output. Use whenever asked to manage rmux sessions or to run/observe work inside rmux.
---

# Driving rmux as an agent

rmux is a client/server terminal multiplexer: a long-lived server owns
**sessions → tabs → panes** (each pane is one shell on a pty); clients
attach to one session over a Unix socket at `/tmp/rmux-<uid>.sock`
(override with the `RMUX_SOCK` env var — both server and client must
agree on it).

**One client per session — attaching to a session kicks whoever is
attached.** Check `rmux list` first and never attach to a session
tagged `attached` unless the user asked you to take it over. Prefer
creating your own session (attaching to a new name creates it).

## Non-interactive commands (safe anywhere)

```sh
rmux list     # plain text when piped; see format below
rmux server   # start a server in the foreground (usually systemd runs it)
```

`rmux list` piped output — `●` running+visible state, `○` otherwise;
`attached` means a client is connected to it right now:

```
○ meow  not running          <- pinned in config but not started
● work  2 tabs · 2 panes  attached
○ scratch  1 tab · 3 panes   <- running, no client attached
```

If `list` errors with "cannot connect to server", the server isn't
running: `sudo systemctl start rmux`, or background `rmux server`.

## Attaching: always through tmux

`rmux a <name>` is a full-screen TUI needing a real pty and raw
keystrokes, so never run it directly — wrap it in tmux and drive it
with `send-keys` / read it with `capture-pane`:

```sh
tmux new-session -d -s agent -x 120 -y 32 "rmux a mywork"   # creates session if new
sleep 1
tmux send-keys -t agent -l 'echo hello from rmux'   # -l = literal text
tmux send-keys -t agent Enter
sleep 0.5
tmux capture-pane -p -t agent    # read the whole screen (add -e for colors)
```

- Sleep ~0.5–1s after every chord or command before capturing.
- The **bottom row is the tab bar**: session name chip, then tabs
  (active one highlighted). Use it to confirm where you are.
- When done: `tmux send-keys -t agent C-g` to detach (session keeps
  running), then `tmux kill-session -t agent`. Killing the tmux
  session without detaching is also fine — rmux survives disconnects.

## Keybindings: read the config first

Controls are user-rebindable. **Read `~/.config/rmux/config.yaml`**
(the server auto-creates it on first run) — its `keybindings:` section
is authoritative. The shipped default config binds:

| action | default key | tmux send-keys |
|---|---|---|
| session-manager | ctrl+o | `C-o` |
| tab-manager | ctrl+n | `C-n` |
| split-horizontal (stack) | ctrl+w | `C-w` |
| split-vertical (side-by-side) | ctrl+q | `C-q` |
| focus left/down/up/right | ctrl+h/j/k/l | `C-h` `C-j` `C-k` `C-l` |
| focus-next (cycle) | ctrl+t | `C-t` |
| fullscreen toggle | ctrl+f | `C-f` |
| detach | ctrl+g | `C-g` |

(Compiled-in fallbacks differ — e.g. split is ctrl+k/ctrl+l — which is
why reading the config matters. `alt+x` chords map to `M-x`, `F1`-`F12`
to `F1`...`F12`. The config may also define `shortcuts:` that type a
program name into the pane, and `sessions:` pins with direct open keys.)

These chords are swallowed by rmux and never reach the inner shell, so
e.g. ctrl+w won't delete a word inside panes.

## Managers: create / rename / kill / switch

`C-o` opens the session manager, `C-n` the tab manager (for the current
session). Inside either, plain keys (no modifier):

- `j` / `k` — move selection; `Enter` — switch to selection
- `n` — new: opens a name prompt; type the name, `Enter` to create
  (empty name gets a default), `Escape` to cancel
- `r` — rename selection (prompt pre-filled)
- `x` — **kill** selection (terminates all its shells; killing the last
  tab kills the session)
- `Escape` — close the manager

Recipes (sleep between steps):

```sh
# new tab named "build" in the current session
tmux send-keys -t agent C-n; sleep 0.5
tmux send-keys -t agent -l 'n'; sleep 0.4
tmux send-keys -t agent -l 'build'; tmux send-keys -t agent Enter

# switch to the first session in the session manager
tmux send-keys -t agent C-o; sleep 0.5; tmux send-keys -t agent Enter

# kill the selected tab (navigate with j/k first)
tmux send-keys -t agent C-n; sleep 0.5
tmux send-keys -t agent -l 'x'
```

A session also dies naturally when all its shells exit (send `exit` to
each pane) — the polite alternative to `x`. A pane's space is taken
over by its sibling when its shell exits.

## Sandbox for experiments

To avoid touching the user's real server, run an isolated stack:

```sh
S=$(mktemp -d)
HOME=$S RMUX_SOCK=$S/rmux.sock nohup rmux server >$S/log 2>&1 &
tmux new-session -d -s sandbox -x 120 -y 32 \
  "env HOME=$S RMUX_SOCK=$S/rmux.sock rmux a test"
```

Cleanup caveat: `pkill -f 'rmux server'` matches any shell whose
command text contains that string — including the one running the
pkill. Run cleanup as its own separate command using
`pkill -f 'rmux serve[r]'`, and never in a script that also spells out
the server command.
