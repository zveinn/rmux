//! Input handling: scanning stdin bytes for bound chords and command bindings,
//! and the manager-overlay / name-prompt key state machines.

use crate::Result;
use crate::config::{Binding, BindingAction, Config, Pin};
use crate::model::{NavDir, Session};
use crate::pty::Pty;

/// Which manager overlay is on screen (or being named for).
#[derive(Clone, Copy)]
pub enum Overlay {
    /// Sessions: named groups of tabs. `agents` selects which list is
    /// shown — normal sessions or agent sessions (toggled with `a`).
    Sessions { agents: bool },
    /// Tabs of the currently viewed session.
    Tabs,
}

/// What a client is currently showing.
#[derive(Default)]
pub enum Mode {
    /// The viewed tab's panes.
    #[default]
    Running,
    /// A manager overlay, with the highlighted entry.
    Manager { overlay: Overlay, selected: usize },
    /// The name prompt for a session/tab being created or renamed.
    Naming {
        overlay: Overlay,
        name: String,
        /// `None` = creating something new; `Some` = renaming this.
        rename: Option<RenameTarget>,
    },
}

/// What a rename prompt applies to.
#[derive(Clone, Copy)]
pub enum RenameTarget {
    /// A session, by id (stable across list changes).
    Session(u64),
    /// A tab of the viewed session, by index.
    Tab(usize),
}

/// A control chord found in the input stream.
#[derive(Clone, Copy)]
pub enum InputAction {
    /// Open a manager overlay.
    Manager(Overlay),
    /// Split the focused pane horizontally (stacked).
    SplitH,
    /// Split the focused pane vertically (side by side).
    SplitV,
    /// Move focus to the next pane.
    FocusNext,
    /// Move focus directionally.
    FocusDir(NavDir),
    /// Detach the client from the server.
    Detach,
    /// Toggle fullscreen for the focused pane.
    Fullscreen,
    /// Switch to the pinned session at this index in `Config::pins`,
    /// starting it if it isn't running.
    OpenSession(usize),
}

/// One row of the session-manager list: every pinned session (running or
/// not, in slot order) followed by running unpinned sessions.
pub struct SessionEntry {
    pub name: String,
    /// Index into the sessions vec when the session is running.
    pub running: Option<usize>,
}

pub fn session_entries(pins: &[Pin], sessions: &[Session]) -> Vec<SessionEntry> {
    let mut entries: Vec<SessionEntry> = pins
        .iter()
        .map(|pin| SessionEntry {
            name: pin.name.clone(),
            running: sessions.iter().position(|s| s.name == pin.name),
        })
        .collect();
    for (si, session) in sessions.iter().enumerate() {
        if !session.agent && !pins.iter().any(|p| p.name == session.name) {
            entries.push(SessionEntry {
                name: session.name.clone(),
                running: Some(si),
            });
        }
    }
    // Agent sessions last, most recently active first.
    entries.extend(crate::agent::manager_entries(&[], sessions, true));
    entries
}

/// Create a session, inserting it at its place in the display order:
/// pinned sessions sit before unpinned ones, in slot order. Returns the
/// new session's index.
pub fn create_session(
    sessions: &mut Vec<Session>,
    config: &Config,
    size: (u16, u16),
    name: String,
) -> Result<usize> {
    let pins: &[Pin] = &config.pins;
    let rank = |n: &str| pins.iter().position(|p| p.name == n);
    let session = Session::new(size, name, config)?;
    let pos = match rank(&session.name) {
        None => sessions.len(),
        Some(new_rank) => sessions
            .iter()
            .position(|s| match rank(&s.name) {
                None => true,
                Some(r) => r > new_rank,
            })
            .unwrap_or(sessions.len()),
    };
    sessions.insert(pos, session);
    Ok(pos)
}

/// Returns `Some((action, i))` when a control chord was pressed, where
/// `i` is the index just past it; bytes before it have been forwarded,
/// bytes from `i` on have not.
///
/// When the byte sequence of a `Run` command binding appears, we swallow it and
/// instead type the configured program name plus Enter into the shell.
/// Bindings are matched longest-first (see `config::load`), and a chord
/// split across two reads (e.g. a lone ESC press followed later by a
/// letter) does not match.
pub fn forward_input(pty: &Pty, bindings: &[Binding], buf: &[u8]) -> Option<(InputAction, usize)> {
    let mut start = 0;
    let mut i = 0;
    'scan: while i < buf.len() {
        for binding in bindings {
            if !buf[i..].starts_with(&binding.seq) {
                continue;
            }
            match &binding.action {
                BindingAction::Control(action) => {
                    pty.write(&buf[start..i]);
                    return Some((*action, i + binding.seq.len()));
                }
                BindingAction::Run(cmd) => {
                    pty.write(&buf[start..i]);
                    pty.write(cmd.as_bytes());
                    pty.write(b"\r");
                    i += binding.seq.len();
                    start = i;
                    continue 'scan;
                }
            }
        }
        i += 1;
    }
    pty.write(&buf[start..]);
    None
}

/// A key press inside a manager overlay.
pub enum MgrAction {
    Up,
    Down,
    Select,
    New,
    Rename,
    Kill,
    /// Flip the session manager between normal and agent sessions.
    ToggleAgents,
    Close,
}

/// What happened after applying manager key presses.
enum MgrOutcome {
    /// Keep showing the manager.
    Stay,
    /// Close it and show the viewed tab.
    Close,
    /// Switch to the selected entry, then close.
    Switch(usize),
    /// Open the name prompt for a new session/tab.
    StartNaming,
    /// Open the rename prompt for the selected entry.
    Rename(usize),
    /// Kill the selected entry (all its shells).
    Kill(usize),
    /// Flip between the normal and agent session lists.
    ToggleAgents,
}

/// Parse manager-mode key presses from raw stdin bytes. The chords bound
/// to opening a manager also close it (toggle behavior).
pub fn manager_actions(buf: &[u8], bindings: &[Binding]) -> Vec<MgrAction> {
    let mut actions = Vec::new();
    let mut i = 0;
    'scan: while i < buf.len() {
        for binding in bindings {
            if matches!(
                binding.action,
                BindingAction::Control(InputAction::Manager(_))
            ) && buf[i..].starts_with(&binding.seq)
            {
                actions.push(MgrAction::Close);
                i += binding.seq.len();
                continue 'scan;
            }
        }
        match buf[i] {
            b'\r' | b'\n' => actions.push(MgrAction::Select),
            b'n' => actions.push(MgrAction::New),
            b'r' => actions.push(MgrAction::Rename),
            b'x' => actions.push(MgrAction::Kill),
            b'a' => actions.push(MgrAction::ToggleAgents),
            b'j' => actions.push(MgrAction::Down),
            b'k' => actions.push(MgrAction::Up),
            b'q' => actions.push(MgrAction::Close),
            0x1b => {
                // Arrow keys arrive as ESC [ A/B; a bare ESC closes.
                if buf[i + 1..].starts_with(b"[A") {
                    actions.push(MgrAction::Up);
                    i += 3;
                    continue;
                }
                if buf[i + 1..].starts_with(b"[B") {
                    actions.push(MgrAction::Down);
                    i += 3;
                    continue;
                }
                actions.push(MgrAction::Close);
            }
            _ => {}
        }
        i += 1;
    }
    actions
}

/// Apply manager key presses; any actions after one that leaves the
/// manager are dropped.
fn manager_apply(actions: &[MgrAction], selected: &mut usize, count: usize) -> MgrOutcome {
    for action in actions {
        match action {
            MgrAction::Up => *selected = selected.saturating_sub(1),
            MgrAction::Down => *selected = (*selected + 1).min(count - 1),
            MgrAction::Select => return MgrOutcome::Switch(*selected),
            MgrAction::New => return MgrOutcome::StartNaming,
            MgrAction::Rename => return MgrOutcome::Rename(*selected),
            MgrAction::Kill => return MgrOutcome::Kill(*selected),
            MgrAction::ToggleAgents => return MgrOutcome::ToggleAgents,
            MgrAction::Close => return MgrOutcome::Close,
        }
    }
    MgrOutcome::Stay
}

/// Apply manager key presses to whichever list the overlay shows.
/// Returns the mode to switch to, or `None` to stay in the manager with
/// the (possibly moved) selection. Selecting a pinned session that isn't
/// running starts it.
pub fn run_manager(
    actions: &[MgrAction],
    overlay: Overlay,
    selected: &mut usize,
    sessions: &mut Vec<Session>,
    active: &mut usize,
    size: (u16, u16),
    config: &Config,
) -> Result<Option<Mode>> {
    let pins: &[Pin] = &config.pins;
    let agents = matches!(overlay, Overlay::Sessions { agents: true });
    let count = match overlay {
        Overlay::Sessions { .. } => crate::agent::manager_entries(pins, sessions, agents).len(),
        Overlay::Tabs => sessions[*active].tabs.len(),
    };
    Ok(match manager_apply(actions, selected, count.max(1)) {
        MgrOutcome::Stay => None,
        MgrOutcome::Close => Some(Mode::Running),
        MgrOutcome::ToggleAgents => match overlay {
            Overlay::Sessions { agents } => {
                *selected = 0;
                Some(Mode::Manager {
                    overlay: Overlay::Sessions { agents: !agents },
                    selected: 0,
                })
            }
            Overlay::Tabs => None,
        },
        MgrOutcome::Switch(i) => {
            match overlay {
                Overlay::Sessions { .. } => {
                    let entries = crate::agent::manager_entries(pins, sessions, agents);
                    if let Some(entry) = entries.get(i) {
                        *active = match entry.running {
                            Some(si) => si,
                            None => {
                                create_session(sessions, config, size, entry.name.clone())?
                            }
                        };
                    }
                }
                Overlay::Tabs => sessions[*active].active_tab = i,
            }
            Some(Mode::Running)
        }
        MgrOutcome::StartNaming => Some(Mode::Naming {
            overlay,
            name: String::new(),
            rename: None,
        }),
        MgrOutcome::Rename(i) => match overlay {
            Overlay::Sessions { .. } => {
                let entries = crate::agent::manager_entries(pins, sessions, agents);
                match entries.get(i).and_then(|e| e.running) {
                    Some(si) => Some(Mode::Naming {
                        overlay,
                        name: sessions[si].name.clone(),
                        rename: Some(RenameTarget::Session(sessions[si].id)),
                    }),
                    // A stopped pin's name comes from the config.
                    None => None,
                }
            }
            Overlay::Tabs => sessions[*active].tabs.get(i).map(|tab| Mode::Naming {
                overlay,
                name: tab.name.clone(),
                rename: Some(RenameTarget::Tab(i)),
            }),
        },
        MgrOutcome::Kill(i) => match overlay {
            Overlay::Sessions { .. } => {
                let entries = crate::agent::manager_entries(pins, sessions, agents);
                if let Some(si) = entries.get(i).and_then(|e| e.running) {
                    let viewed = sessions.get(*active).map(|s| s.id);
                    // Dropping the session closes its ptys; the shells
                    // get SIGHUP and the kernel reaps them.
                    let killed = sessions.remove(si);
                    if viewed == Some(killed.id) {
                        *active = 0;
                    } else if let Some(vid) = viewed {
                        *active = sessions.iter().position(|s| s.id == vid).unwrap_or(0);
                    }
                }
                let count = crate::agent::manager_entries(pins, sessions, agents).len();
                *selected = (*selected).min(count.saturating_sub(1));
                None // stay in the manager
            }
            Overlay::Tabs => {
                let session = &mut sessions[*active];
                if session.tabs.len() > 1 && i < session.tabs.len() {
                    session.tabs.remove(i);
                    if i < session.active_tab {
                        session.active_tab -= 1;
                    }
                    session.active_tab = session.active_tab.min(session.tabs.len() - 1);
                    *selected = (*selected).min(session.tabs.len() - 1);
                    None // stay in the manager
                } else if session.tabs.len() == 1 && i == 0 {
                    // Killing the last tab kills the session.
                    sessions.remove(*active);
                    *active = 0;
                    Some(Mode::Running)
                } else {
                    None
                }
            }
        },
    })
}

/// What happened after applying name-prompt key presses.
pub enum NamingOutcome {
    /// Still typing.
    Pending,
    /// Enter pressed — create with the typed name.
    Create,
    /// Esc pressed — back to the manager.
    Cancel,
}

/// Apply key presses to the name being typed in the name prompt.
pub fn naming_apply(buf: &[u8], name: &mut String) -> NamingOutcome {
    for ch in String::from_utf8_lossy(buf).chars() {
        match ch {
            '\r' | '\n' => return NamingOutcome::Create,
            '\u{1b}' => return NamingOutcome::Cancel,
            '\u{7f}' | '\u{8}' => {
                name.pop();
            }
            c if !c.is_control() && name.chars().count() < 40 => name.push(c),
            _ => {}
        }
    }
    NamingOutcome::Pending
}

/// Handle one chunk of client input against the client's viewed session.
///
/// `active` is the index of the viewed session and may change (manager
/// switches); `mode` is the client's overlay state. Returns true when
/// the client asked to detach.
pub fn handle_input(
    mut buf: &[u8],
    mode: &mut Mode,
    sessions: &mut Vec<Session>,
    active: &mut usize,
    size: (u16, u16),
    config: &Config,
) -> Result<bool> {
    use crate::model::SplitDir;
    let bindings: &[Binding] = &config.bindings;

    loop {
        let mut next_mode = None;
        match mode {
            Mode::Running => {
                // Forward keyboard input untouched to the focused pane;
                // `forward_input` stops at bound control chords.
                let session = &sessions[*active];
                let tab = &session.tabs[session.active_tab];
                let pty = std::rc::Rc::clone(
                    &tab.layout
                        .pane(tab.focused)
                        .expect("focused pane exists")
                        .pty,
                );
                let Some((action, rest)) = forward_input(&pty, bindings, buf) else {
                    break;
                };
                buf = &buf[rest..];
                match action {
                    InputAction::Detach => return Ok(true),
                    InputAction::OpenSession(pi) => {
                        if let Some(pin) = config.pins.get(pi) {
                            *active = match sessions.iter().position(|s| s.name == pin.name) {
                                Some(si) => si,
                                None => {
                                    create_session(sessions, config, size, pin.name.clone())?
                                }
                            };
                        }
                    }
                    InputAction::Fullscreen => {
                        let session = &mut sessions[*active];
                        let tab = &mut session.tabs[session.active_tab];
                        tab.set_zoom(!tab.zoomed, size)?;
                    }
                    InputAction::SplitH | InputAction::SplitV => {
                        let dir = match action {
                            InputAction::SplitH => SplitDir::Horizontal,
                            _ => SplitDir::Vertical,
                        };
                        let session = &mut sessions[*active];
                        let tab = &mut session.tabs[session.active_tab];
                        tab.set_zoom(false, size)?;
                        tab.split(dir, size, config)?;
                    }
                    InputAction::FocusNext => {
                        let session = &mut sessions[*active];
                        let tab = &mut session.tabs[session.active_tab];
                        let _ = tab.set_zoom(false, size);
                        tab.focus_next();
                    }
                    InputAction::FocusDir(dir) => {
                        let session = &mut sessions[*active];
                        session.tabs[session.active_tab].set_zoom(false, size)?;
                        let moved =
                            session.tabs[session.active_tab].focus_dir(dir, size)?;
                        // At the tab's edge, left/right jump to the
                        // neighboring tab (wrapping) and land on the pane
                        // nearest the edge we came in over.
                        if !moved {
                            let count = session.tabs.len();
                            match dir {
                                NavDir::Right => {
                                    session.active_tab = (session.active_tab + 1) % count;
                                    session.tabs[session.active_tab]
                                        .focus_edge(NavDir::Left, size)?;
                                }
                                NavDir::Left => {
                                    session.active_tab =
                                        (session.active_tab + count - 1) % count;
                                    session.tabs[session.active_tab]
                                        .focus_edge(NavDir::Right, size)?;
                                }
                                NavDir::Up | NavDir::Down => {}
                            }
                        }
                    }
                    InputAction::Manager(overlay) => {
                        // Bytes typed right after the chord are already
                        // manager input.
                        let mut selected = match overlay {
                            Overlay::Sessions { .. } => *active,
                            Overlay::Tabs => sessions[*active].active_tab,
                        };
                        next_mode = Some(
                            run_manager(
                                &manager_actions(buf, bindings),
                                overlay,
                                &mut selected,
                                sessions,
                                active,
                                size,
                                config,
                            )?
                            .unwrap_or(Mode::Manager { overlay, selected }),
                        );
                        buf = &[];
                    }
                }
            }
            Mode::Manager { overlay, selected } => {
                next_mode = run_manager(
                    &manager_actions(buf, bindings),
                    *overlay,
                    selected,
                    sessions,
                    active,
                    size,
                    config,
                )?;
                buf = &[];
            }
            Mode::Naming {
                overlay,
                name,
                rename,
            } => {
                match naming_apply(buf, name) {
                    NamingOutcome::Pending => {}
                    NamingOutcome::Cancel => {
                        let selected = match overlay {
                            Overlay::Sessions { .. } => *active,
                            Overlay::Tabs => sessions[*active].active_tab,
                        };
                        next_mode = Some(Mode::Manager {
                            overlay: *overlay,
                            selected,
                        });
                    }
                    NamingOutcome::Create => {
                        let typed = std::mem::take(name);
                        let typed = typed.trim().to_string();
                        match rename {
                            // Rename: apply and return to the manager;
                            // an empty name changes nothing.
                            Some(RenameTarget::Session(id)) => {
                                if !typed.is_empty()
                                    && let Some(s) = sessions.iter_mut().find(|s| s.id == *id)
                                {
                                    s.name = typed;
                                }
                                next_mode = Some(Mode::Manager {
                                    overlay: *overlay,
                                    selected: *active,
                                });
                            }
                            Some(RenameTarget::Tab(ti)) => {
                                if !typed.is_empty()
                                    && let Some(t) = sessions[*active].tabs.get_mut(*ti)
                                {
                                    t.name = typed;
                                }
                                next_mode = Some(Mode::Manager {
                                    overlay: *overlay,
                                    selected: *ti,
                                });
                            }
                            None => {
                                match overlay {
                                    Overlay::Sessions { agents } => {
                                        let name = if typed.is_empty() {
                                            format!("session {}", sessions.len() + 1)
                                        } else {
                                            typed
                                        };
                                        *active =
                                            create_session(sessions, config, size, name)?;
                                        // Created from the agent view =
                                        // an agent session.
                                        sessions[*active].agent = *agents;
                                    }
                                    Overlay::Tabs => {
                                        let session = &mut sessions[*active];
                                        let name = if typed.is_empty() {
                                            format!("tab {}", session.tabs.len() + 1)
                                        } else {
                                            typed
                                        };
                                        session.tabs.push(crate::model::Tab::new(size, name, config)?);
                                        session.active_tab = session.tabs.len() - 1;
                                    }
                                }
                                next_mode = Some(Mode::Running);
                            }
                        }
                    }
                }
                buf = &[];
            }
        }
        if let Some(m) = next_mode {
            *mode = m;
        }
        if buf.is_empty() {
            break;
        }
    }
    Ok(false)
}
