# rmux

A minimal terminal multiplexer built on [`libghostty-vt`](https://crates.io/crates/libghostty-vt),
the terminal emulation engine extracted from [Ghostty](https://ghostty.org).
Client/server like tmux: a long-lived server owns **sessions → tabs →
panes**, thin clients attach over a Unix socket — sessions survive SSH
disconnects, reattach and everything is as you left it.

## Built for agents

LLM agents get a first-class, non-interactive control surface — one-shot
commands over the socket, no pty, no keystroke faking:

```sh
rmux agent new    build              # create an agent session (or: new build <tab>)
rmux agent send   build 'cargo test' # type text + Enter into it ([-t tab])
rmux agent read   build              # the rendered screen, as plain text ([-t tab])
rmux agent rename build tests        # short, descriptive names
rmux agent kill   build              # kill a session (or: kill build <tab>)
```

Agent sessions are sandboxed by design: the agent commands **refuse to
touch your sessions**. They live in their own list, sorted by activity
and tagged with a last-activity age — press **`a`** in the session
manager to check on your agents, or attach with `rmux a <name>`; they
are normal sessions underneath. A ready-made Claude Code skill ships in
[`.claude/skills/rmux/`](.claude/skills/rmux/SKILL.md).

![rmux timelapse: splits, focus, fullscreen, tabs, managers, and an agent session](assets/demo.svg)

## Install

Requires Rust 1.90+, plus [Zig](https://ziglang.org) and `git` on PATH
(`libghostty-vt` builds from Ghostty source at compile time).

```sh
cargo install --path .                        # the rmux binary
# server as a systemd system service — survives SSH logouts, no linger
# tricks needed. Edit User= and ExecStart= in rmux.service first:
sudo cp rmux.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now rmux
```

```sh
rmux a work    # attach to session "work", creating it if new
rmux list      # sessions: tabs, panes, attach state, agent ages
```

Detach with **Ctrl+G** (or drop the SSH connection — the session keeps
running). The socket lives at `/tmp/rmux-<uid>.sock` (`RMUX_SOCK`
overrides); server logs land in `journalctl -u rmux`.

## Config

`~/.config/rmux/config.yaml` — created with these defaults on first
run, and **hot-reloaded** within about a second of saving (a broken
config is rejected and the old one stays active):

```yaml
accent: "#7aa2f7"

shell: /usr/bin/bash

terminal_envs:
  TERM: xterm-256color

shortcuts:

keybindings:
  session-manager: ctrl+o
  tab-manager: ctrl+n
  split-horizontal: ctrl+w
  split-vertical: ctrl+q
  focus-next: ctrl+t
  focus-left: ctrl+h
  focus-right: ctrl+l
  focus-up: ctrl+k
  focus-down: ctrl+j
  detach: ctrl+g
  fullscreen: ctrl+f

sessions:
  1: { name: meow, key: F7 }
```

Keys are `[ctrl+][alt+]<char>` or `F1`–`F12`; every binding below is
from this default config and can be remapped. Bound chords are
swallowed by rmux and never reach the inner shell.

## Capabilities

| Capability | Keys / command | Notes |
|---|---|---|
| Sessions | `rmux a <name>` | Created on first attach; survive disconnects; one client per session (a new attach kicks the old) |
| Splits | `ctrl+w` stacked · `ctrl+q` side-by-side | Always 50/50; a pane's sibling takes its space when the shell exits |
| Focus | `ctrl+h/j/k/l` directional · `ctrl+t` cycle | Left/right cross tab boundaries, wrapping — tabs form one strip |
| Fullscreen | `ctrl+f` | Focused pane takes the whole area; tab bar shows `[F]` |
| Session manager | `ctrl+o` | `j/k` move · `enter` switch · `n` new · `r` rename · `x` kill · `esc` close |
| Agent list | `a` inside the session manager | Agent sessions only, most-recently-active first, with ages |
| Tab manager | `ctrl+n` | Same controls as the session manager |
| Pinned sessions | `sessions:` in the config | An F-key opens the session from anywhere, starting it if needed |
| Shortcuts | `shortcuts:` in the config | The chord types `<program><Enter>` into the focused pane |
| Hot reload | edit `config.yaml` | Accent, keys, pins apply live; `shell`/`terminal_envs` to new shells |
| Detach | `ctrl+g` | The session keeps running; reattach with `rmux a` |
| Agent mode | `rmux agent new/send/read/rename/kill` | Sandboxed to agent-created sessions; bumps activity ordering |
| Listing | `rmux list` | Colored on a tty, plain when piped (agents parse this) |

---

rmux does no terminal emulation of its own — that is all
[`libghostty-vt`](https://crates.io/crates/libghostty-vt), the VT
engine from [Ghostty](https://ghostty.org). Credit for every correctly
parsed escape sequence goes there.
