//! Input handling: scanning stdin bytes for bound chords and command bindings,
//! and the manager-overlay / name-prompt key state machines.

use crate::Result;
use crate::config::{Binding, BindingAction, Config, Pin};
use crate::model::{NavDir, Rect, Session};
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

/// A mouse/scroll intent extracted from raw client input. `raw` keeps
/// the original SGR bytes so panes that track the mouse themselves get
/// the event untouched.
pub enum MouseEvent {
    Wheel { up: bool, raw: Vec<u8> },
    /// PageUp / PageDown.
    Page { up: bool },
    /// Button press; 1-based screen coordinates.
    Press { left: bool, x: u16, y: u16, raw: Vec<u8> },
    /// Motion with the left button held.
    Drag { x: u16, y: u16, raw: Vec<u8> },
    /// Button release.
    Release { x: u16, y: u16, raw: Vec<u8> },
}

/// Split raw input into (bytes to process normally, mouse events).
/// SGR mouse sequences are always consumed here so they never leak
/// into a shell as garbage bytes. PageUp/PageDown become events too.
pub fn extract_mouse(buf: &[u8]) -> (Vec<u8>, Vec<MouseEvent>) {
    let mut clean = Vec::with_capacity(buf.len());
    let mut events = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if buf[i..].starts_with(b"\x1b[<") {
            // SGR mouse: ESC [ < Cb ; Cx ; Cy (M|m)
            let mut j = i + 3;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j < buf.len() && (buf[j] == b'M' || buf[j] == b'm') {
                let mut params = std::str::from_utf8(&buf[i + 3..j])
                    .unwrap_or("")
                    .split(';')
                    .map(|p| p.parse::<u32>().unwrap_or(0));
                let cb = params.next().unwrap_or(0);
                let x = params.next().unwrap_or(1).clamp(1, u32::from(u16::MAX)) as u16;
                let y = params.next().unwrap_or(1).clamp(1, u32::from(u16::MAX)) as u16;
                let raw = buf[i..=j].to_vec();
                if cb & 64 != 0 {
                    // Wheel: low two bits 0 (up) or 1 (down); modifier
                    // bits may also be set. Wheel left/right dropped.
                    if buf[j] == b'M' && cb & 2 == 0 {
                        events.push(MouseEvent::Wheel {
                            up: cb & 1 == 0,
                            raw,
                        });
                    }
                } else if buf[j] == b'm' {
                    events.push(MouseEvent::Release { x, y, raw });
                } else if cb & 32 != 0 {
                    if cb & 3 == 0 {
                        events.push(MouseEvent::Drag { x, y, raw });
                    }
                } else {
                    events.push(MouseEvent::Press {
                        left: cb & 3 == 0,
                        x,
                        y,
                        raw,
                    });
                }
                i = j + 1;
                continue;
            }
            if j >= buf.len() {
                // Sequence split across reads: drop the fragment rather
                // than leaking it into the shell.
                break;
            }
        }
        if buf[i..].starts_with(b"\x1b[5~") || buf[i..].starts_with(b"\x1b[6~") {
            events.push(MouseEvent::Page {
                up: buf[i + 2] == b'5',
            });
            i += 4;
            continue;
        }
        clean.push(buf[i]);
        i += 1;
    }
    (clean, events)
}

/// Apply scroll events to the focused pane: panes tracking the mouse
/// get the raw wheel bytes, alt-screen apps get arrow/page keys (like
/// most terminal emulators), everything else scrolls our scrollback.
fn apply_scroll(event: &MouseEvent, sessions: &mut [Session], active: usize) {
    use libghostty_vt::screen::Screen;
    use libghostty_vt::terminal::{Mode as TermMode, ScrollViewport};

    let session = &mut sessions[active];
    let tab = &mut session.tabs[session.active_tab];
    let focused = tab.focused;
    let Some(pane) = tab.layout.pane_mut(focused) else {
        return;
    };
    let tracking = pane.term.is_mouse_tracking().unwrap_or(false);
    let alt = matches!(pane.term.active_screen(), Ok(Screen::Alternate));
    match event {
        MouseEvent::Wheel { up, raw } => {
            if tracking {
                pane.pty.write(raw);
            } else if alt {
                let key: &[u8] = match (pane.term.mode(TermMode::DECCKM), up) {
                    (Ok(true), true) => b"\x1bOA",
                    (Ok(true), false) => b"\x1bOB",
                    (_, true) => b"\x1b[A",
                    (_, false) => b"\x1b[B",
                };
                for _ in 0..3 {
                    pane.pty.write(key);
                }
            } else {
                let delta = if *up { -3 } else { 3 };
                pane.term.scroll_viewport(ScrollViewport::Delta(delta));
            }
        }
        MouseEvent::Page { up } => {
            if alt || tracking {
                pane.pty.write(if *up { b"\x1b[5~" } else { b"\x1b[6~" });
            } else {
                let page = pane.term.rows().unwrap_or(24).saturating_sub(2) as isize;
                let delta = if *up { -page } else { page };
                pane.term.scroll_viewport(ScrollViewport::Delta(delta));
            }
        }
        _ => {}
    }
}

/// Per-client mouse-selection state (`select_copy`).
pub struct SelectState {
    /// The gesture and the pane it is anchored in.
    gesture: Option<(u64, libghostty_vt::selection::gesture::Gesture<'static>)>,
    /// Pane holding an installed (visible) selection.
    selected_pane: Option<u64>,
    /// Pane-local cell the gesture was anchored at.
    anchor: Option<(u16, u16)>,
    /// Whether the current drag runs backward (before the anchor).
    backward: bool,
    /// Monotonic base for click timestamps (double-click detection).
    epoch: std::time::Instant,
}

impl Default for SelectState {
    fn default() -> Self {
        Self {
            gesture: None,
            selected_pane: None,
            anchor: None,
            backward: false,
            epoch: std::time::Instant::now(),
        }
    }
}

/// Clear the visible selection (if any) and forget the gesture.
fn clear_selection(select: &mut SelectState, sessions: &mut [Session]) {
    select.gesture = None;
    let Some(pid) = select.selected_pane.take() else {
        return;
    };
    for session in sessions.iter_mut() {
        for tab in &mut session.tabs {
            if let Some(pane) = tab.layout.pane(pid) {
                let _ = pane.term.set_selection(None);
                return;
            }
        }
    }
}

/// Handle press/drag/release for mouse select-to-copy. Returns text to
/// copy to the client's clipboard when a selection completes.
fn apply_select(
    event: &MouseEvent,
    select: &mut SelectState,
    sessions: &mut [Session],
    active: usize,
    size: (u16, u16),
    enabled: bool,
) -> Option<String> {
    use libghostty_vt::selection::gesture::{DragEvent, Gesture, PressEvent, ReleaseEvent};
    use libghostty_vt::selection::FormatOptions;
    use libghostty_vt::terminal::{Point, PointCoordinate};
    use std::time::Duration;

    let (x, y, raw, kind) = match event {
        MouseEvent::Press { left, x, y, raw } => (*x, *y, raw, if *left { 0 } else { 3 }),
        MouseEvent::Drag { x, y, raw } => (*x, *y, raw, 1),
        MouseEvent::Release { x, y, raw } => (*x, *y, raw, 2),
        _ => return None,
    };
    // 1-based screen coords -> 0-based content coords.
    let (px, py) = (x.saturating_sub(1), y.saturating_sub(1));

    // Pane rectangles of the visible tab.
    let session = &mut sessions[active];
    let tab = &mut session.tabs[session.active_tab];
    let full = Rect {
        x: 0,
        y: 0,
        w: size.0,
        h: size.1,
    };
    let mut rects: Vec<(u64, Rect)> = Vec::new();
    if tab.zoomed {
        rects.push((tab.focused, full));
    } else {
        let _ = tab.layout.for_each(full, &mut |pane, rect| {
            rects.push((pane.id, rect));
            Ok(())
        });
    }

    // Drags/releases stick to the gesture's pane; presses hit-test.
    let target = match kind {
        1 | 2 => select.gesture.as_ref().map(|(pid, _)| *pid).or_else(|| {
            rects
                .iter()
                .find(|(_, r)| px >= r.x && px < r.x + r.w && py >= r.y && py < r.y + r.h)
                .map(|(id, _)| *id)
        }),
        _ => rects
            .iter()
            .find(|(_, r)| px >= r.x && px < r.x + r.w && py >= r.y && py < r.y + r.h)
            .map(|(id, _)| *id),
    };
    let (pane_id, rect) = match target.and_then(|id| {
        rects
            .iter()
            .find(|(pid, _)| *pid == id)
            .map(|(pid, r)| (*pid, *r))
    }) {
        Some(v) => v,
        None => return None, // divider or tab bar
    };

    // Panes that track the mouse get the raw event; no rmux selection.
    let tracking = tab
        .layout
        .pane(pane_id)
        .is_some_and(|p| p.term.is_mouse_tracking().unwrap_or(false));
    if tracking {
        if let Some(pane) = tab.layout.pane(pane_id) {
            pane.pty.write(raw);
        }
        return None;
    }

    // Pane-local cell coordinates, clamped into the pane.
    let lx = px.clamp(rect.x, rect.x + rect.w.saturating_sub(1)) - rect.x;
    let ly = py.clamp(rect.y, rect.y + rect.h.saturating_sub(1)) - rect.y;

    match kind {
        // Press: click-to-focus, then anchor a selection gesture.
        0 | 3 => {
            clear_only_selection(select, sessions);
            let session = &mut sessions[active];
            let tab = &mut session.tabs[session.active_tab];
            tab.focused = pane_id;
            if kind == 3 || !enabled {
                return None; // middle/right click: focus only
            }
            // Reuse the gesture on the same pane so double/triple
            // clicks (word/line select) chain up.
            let keep = matches!(&select.gesture, Some((pid, _)) if *pid == pane_id);
            if !keep {
                select.gesture = Gesture::new().ok().map(|g| (pane_id, g));
            }
            let Some((_, gesture)) = &mut select.gesture else {
                return None;
            };
            let Some(pane) = tab.layout.pane(pane_id) else {
                return None;
            };
            let result = (|| -> crate::Result<Option<()>> {
                let grid_ref = pane.term.grid_ref(Point::Viewport(PointCoordinate {
                    x: lx,
                    y: u32::from(ly),
                }))?;
                let mut press = PressEvent::new()?;
                press
                    .set_time(select.epoch.elapsed())?
                    .set_repeat_interval(Duration::from_millis(400))?
                    .set_repeat_distance(8.0)?
                    .set_position(
                        // Left-of-cell boundary: the anchor cell is
                        // included when dragging forward (re-anchored
                        // right-of-cell when a drag turns backward).
                        (f64::from(lx) + 0.25) * f64::from(crate::model::CELL_PX.0),
                        (f64::from(ly) + 0.5) * f64::from(crate::model::CELL_PX.1),
                    )?;
                if let Some(selection) = press.apply(gesture, &pane.term, grid_ref)? {
                    pane.term.set_selection(Some(&selection))?;
                    return Ok(Some(()));
                }
                pane.term.set_selection(None)?;
                Ok(None)
            })();
            if matches!(result, Ok(Some(()))) {
                select.selected_pane = Some(pane_id);
            }
            select.anchor = Some((lx, ly));
            select.backward = false;
            None
        }
        // Drag: extend the selection.
        1 => {
            if !enabled {
                return None;
            }
            let Some((gpid, _)) = &select.gesture else {
                return None;
            };
            if *gpid != pane_id {
                return None;
            }
            let (ax, ay) = select.anchor?;
            // Endpoint boundaries sit on the biased side of the cell:
            // the leftmost end needs a left bias, the rightmost a right
            // bias. The anchor's side is fixed at press time, so when a
            // drag crosses to the other side of the anchor, re-anchor
            // with a fresh (untimed) press biased the other way.
            let backward = (ly, lx) < (ay, ax);
            if backward != select.backward {
                select.backward = backward;
                let rebuilt = (|| -> crate::Result<()> {
                    let session = &sessions[active];
                    let tab = &session.tabs[session.active_tab];
                    let Some(pane) = tab.layout.pane(pane_id) else {
                        return Err("pane gone".into());
                    };
                    let mut gesture = Gesture::new()?;
                    let grid_ref = pane.term.grid_ref(Point::Viewport(PointCoordinate {
                        x: ax,
                        y: u32::from(ay),
                    }))?;
                    let bias = if backward { 0.75 } else { 0.25 };
                    let mut press = PressEvent::new()?;
                    press.set_position(
                        (f64::from(ax) + bias) * f64::from(crate::model::CELL_PX.0),
                        (f64::from(ay) + 0.5) * f64::from(crate::model::CELL_PX.1),
                    )?;
                    let _ = press.apply(&mut gesture, &pane.term, grid_ref)?;
                    select.gesture = Some((pane_id, gesture));
                    Ok(())
                })();
                if rebuilt.is_err() {
                    return None;
                }
            }
            let Some((_, gesture)) = &mut select.gesture else {
                return None;
            };
            let session = &sessions[active];
            let tab = &session.tabs[session.active_tab];
            let Some(pane) = tab.layout.pane(pane_id) else {
                return None;
            };
            let pointer_bias = if backward { 0.25 } else { 0.75 };
            let result = (|| -> crate::Result<Option<()>> {
                let grid_ref = pane.term.grid_ref(Point::Viewport(PointCoordinate {
                    x: lx,
                    y: u32::from(ly),
                }))?;
                let mut drag = DragEvent::new()?;
                drag.set_position(
                    (f64::from(lx) + pointer_bias) * f64::from(crate::model::CELL_PX.0),
                    (f64::from(ly) + 0.5) * f64::from(crate::model::CELL_PX.1),
                )?;
                let geometry = libghostty_vt::selection::gesture::Geometry {
                    columns: u32::from(rect.w.max(1)),
                    cell_width: crate::model::CELL_PX.0,
                    padding_left: 0,
                    screen_height: u32::from(rect.h.max(1)) * crate::model::CELL_PX.1,
                };
                if let Some(selection) = drag.apply(gesture, &pane.term, grid_ref, geometry)? {
                    pane.term.set_selection(Some(&selection))?;
                    return Ok(Some(()));
                }
                Ok(None)
            })();
            if matches!(result, Ok(Some(()))) {
                select.selected_pane = Some(pane_id);
            }
            None
        }
        // Release: finish the gesture; copy when something is selected.
        _ => {
            if !enabled {
                return None;
            }
            let Some((gpid, gesture)) = &mut select.gesture else {
                return None;
            };
            if *gpid != pane_id {
                return None;
            }
            let session = &sessions[active];
            let tab = &session.tabs[session.active_tab];
            let Some(pane) = tab.layout.pane(pane_id) else {
                return None;
            };
            (|| -> crate::Result<Option<String>> {
                let grid_ref = pane.term.grid_ref(Point::Viewport(PointCoordinate {
                    x: lx,
                    y: u32::from(ly),
                }))?;
                let mut release = ReleaseEvent::new()?;
                release.apply(gesture, &pane.term, Some(grid_ref))?;
                // Copy on drag or multi-click (word/line); a bare click
                // just moves focus.
                let meaningful = gesture.dragged(&pane.term).unwrap_or(false)
                    || gesture.click_count(&pane.term).unwrap_or(0) >= 2;
                if !meaningful {
                    return Ok(None);
                }
                let options = FormatOptions::new().with_unwrap(true).with_trim(true);
                let text = pane
                    .term
                    .format_selection_alloc(None, options)?
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .filter(|t| !t.is_empty());
                Ok(text)
            })()
            .ok()
            .flatten()
        }
    }
}

/// Clear the visible selection but keep the gesture (used on a fresh
/// press, where the gesture must survive for double-click chaining).
fn clear_only_selection(select: &mut SelectState, sessions: &mut [Session]) {
    let Some(pid) = select.selected_pane.take() else {
        return;
    };
    for session in sessions.iter_mut() {
        for tab in &mut session.tabs {
            if let Some(pane) = tab.layout.pane(pid) {
                let _ = pane.term.set_selection(None);
                return;
            }
        }
    }
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
    buf: &[u8],
    mode: &mut Mode,
    sessions: &mut Vec<Session>,
    active: &mut usize,
    size: (u16, u16),
    config: &Config,
    select: &mut SelectState,
) -> Result<(bool, Option<String>)> {
    use crate::model::SplitDir;
    let bindings: &[Binding] = &config.bindings;

    // Pull mouse and PageUp/PageDown events out of the stream first.
    let (clean, mouse) = extract_mouse(buf);
    let mut synthetic: Vec<u8> = Vec::new();
    let mut copied: Option<String> = None;
    match mode {
        Mode::Running => {
            for event in &mouse {
                match event {
                    MouseEvent::Wheel { .. } | MouseEvent::Page { .. } => {
                        apply_scroll(event, sessions, *active);
                    }
                    _ => {
                        if let Some(text) = apply_select(
                            event,
                            select,
                            sessions,
                            *active,
                            size,
                            config.select_copy,
                        ) {
                            copied = Some(text);
                        }
                    }
                }
            }
            if !clean.is_empty() {
                // Typing snaps the view back to the live bottom and
                // drops any visible selection.
                use libghostty_vt::terminal::ScrollViewport;
                clear_selection(select, sessions);
                let session = &mut sessions[*active];
                let tab = &mut session.tabs[session.active_tab];
                let focused = tab.focused;
                if let Some(pane) = tab.layout.pane_mut(focused) {
                    pane.term.scroll_viewport(ScrollViewport::Bottom);
                }
            }
        }
        // In a manager, wheel/page move the selection.
        Mode::Manager { .. } => {
            for event in &mouse {
                match event {
                    MouseEvent::Wheel { up: true, .. } | MouseEvent::Page { up: true } => {
                        synthetic.push(b'k');
                    }
                    MouseEvent::Wheel { up: false, .. } | MouseEvent::Page { up: false } => {
                        synthetic.push(b'j');
                    }
                    _ => {}
                }
            }
        }
        Mode::Naming { .. } => {}
    }
    synthetic.extend_from_slice(&clean);
    let mut buf: &[u8] = &synthetic;

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
                    InputAction::Detach => return Ok((true, copied)),
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
    Ok((false, copied))
}
