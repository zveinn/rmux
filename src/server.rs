//! The server: owns all sessions/tabs/panes and keeps them alive across
//! client connections. Clients attach over a Unix socket; at most one
//! client per session — a new attach kicks the old client.
//!
//! Single-threaded by design: libghostty's types are !Send/!Sync, so
//! everything — pty parsing, input handling, rendering — happens on one
//! poll(2) loop over the listener, client sockets, and pty fds.

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};

use crossterm::{
    queue,
    style::{Attribute, Color, Print, SetAttribute, SetForegroundColor},
};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::Result;
use crate::config::{self, Config};
use crate::input::{Mode, Overlay, handle_input, session_entries};
use crate::model::Session;
use crate::protocol::{
    C2S_ATTACH, C2S_INPUT, C2S_LIST, C2S_RESIZE, FrameReader, S2C_BYE, S2C_LIST, S2C_OUTPUT,
    frame, socket_path,
};
use crate::render::{ListItem, Renderer, content_size, draw_manager, draw_naming, draw_session};

/// A slow client gets this much buffered output before being dropped.
const MAX_OUTBUF: usize = 8 * 1024 * 1024;

/// One connected client.
struct ClientConn {
    stream: UnixStream,
    reader: FrameReader,
    /// Output queued while the socket is full (written when writable).
    outbuf: Vec<u8>,
    /// Id of the viewed session; None until the client attaches.
    attached: Option<u64>,
    size: (u16, u16),
    mode: Mode,
    needs_redraw: bool,
    /// A BYE was queued; drop the client once outbuf drains.
    closing: bool,
    /// The socket died; drop the client this iteration.
    dead: bool,
}

impl ClientConn {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            reader: FrameReader::new(),
            outbuf: Vec::new(),
            attached: None,
            size: (80, 24),
            mode: Mode::Running,
            needs_redraw: false,
            closing: false,
            dead: false,
        }
    }

    /// Queue a frame; kills the client instead if it is hopelessly slow.
    fn send(&mut self, kind: u8, payload: &[u8]) {
        if self.outbuf.len() + payload.len() > MAX_OUTBUF {
            self.dead = true;
            return;
        }
        self.outbuf.extend_from_slice(&frame(kind, payload));
        self.try_flush();
    }

    /// Say goodbye (reason shown by the client) and stop serving.
    fn bye(&mut self, reason: &str) {
        if !self.closing {
            self.send(S2C_BYE, reason.as_bytes());
            self.closing = true;
        }
    }

    fn try_flush(&mut self) {
        while !self.outbuf.is_empty() {
            match self.stream.write(&self.outbuf) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => {
                    self.outbuf.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
    }
}

fn bind_listener() -> Result<UnixListener> {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        // A live server means we must not steal the socket; a stale file
        // from a crash is safe to replace.
        if UnixStream::connect(&path).is_ok() {
            return Err(format!("server already running on {}", path.display()).into());
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Modification time of the config file, if any.
fn config_mtime() -> Option<std::time::SystemTime> {
    let path = config::path()?;
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Pinned sessions first (slot order), unpinned after in their existing
/// order — applied after a reload changes the pins.
fn resort_sessions(sessions: &mut [Session], config: &Config) {
    sessions.sort_by_key(|s| {
        config
            .pins
            .iter()
            .position(|p| p.name == s.name)
            .unwrap_or(usize::MAX)
    });
}

pub fn run() -> Result<()> {
    let mut config: Config = config::load()?;
    // Auto-reap exited shells (kernel discards their status), so killed
    // sessions/tabs don't leave zombies behind.
    unsafe {
        let _ = nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGCHLD,
            nix::sys::signal::SigHandler::SigIgn,
        );
    }
    let listener = bind_listener()?;
    let mut renderer = Renderer::new()?;
    let mut sessions: Vec<Session> = Vec::new();
    let mut clients: Vec<ClientConn> = Vec::new();

    eprintln!("rmux server listening on {}", socket_path().display());

    let mut last_mtime = config_mtime();
    let mut tick: u32 = 0;

    loop {
        // ---- Hot-reload the config when the file changes (checked about
        // once a second). Bindings, accent, and pins apply immediately;
        // shell/envs affect newly spawned shells. A broken config is
        // rejected and the old one stays active.
        tick = tick.wrapping_add(1);
        if tick % 10 == 0 {
            let mtime = config_mtime();
            if mtime != last_mtime {
                last_mtime = mtime;
                match config::load() {
                    Ok(new_config) => {
                        config = new_config;
                        resort_sessions(&mut sessions, &config);
                        for client in clients.iter_mut() {
                            if client.attached.is_some() && !client.closing && !client.dead {
                                client.needs_redraw = true;
                            }
                        }
                        eprintln!("config reloaded");
                    }
                    Err(e) => eprintln!("config reload failed (keeping old config): {e}"),
                }
            }
        }

        // ---- Poll: listener + client sockets + every pane's pty. ----
        let mut fd_map = Vec::new();
        let client_count = clients.len();
        let ready: Vec<(bool, bool)> = {
            let mut fds = vec![PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
            for client in &clients {
                let mut events = PollFlags::POLLIN;
                if !client.outbuf.is_empty() {
                    events |= PollFlags::POLLOUT;
                }
                fds.push(PollFd::new(client.stream.as_fd(), events));
            }
            for (si, session) in sessions.iter().enumerate() {
                for (ti, tab) in session.tabs.iter().enumerate() {
                    for pane in tab.layout.panes() {
                        fds.push(PollFd::new(pane.pty.as_fd(), PollFlags::POLLIN));
                        fd_map.push((si, ti, pane.id));
                    }
                }
            }
            poll(&mut fds, PollTimeout::from(100u16))?;
            fds.iter()
                .map(|f| {
                    let r = f.revents().unwrap_or(PollFlags::empty());
                    (
                        r.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR),
                        r.contains(PollFlags::POLLOUT),
                    )
                })
                .collect()
        };

        // ---- Accept new clients. ----
        if ready[0].0 {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(true)?;
                        clients.push(ClientConn::new(stream));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }

        // ---- Client IO. ----
        for ci in 0..client_count {
            let (readable, writable) = ready[ci + 1];
            if writable {
                clients[ci].try_flush();
            }
            if !readable || clients[ci].closing || clients[ci].dead {
                continue;
            }
            let mut tmp = [0u8; 4096];
            loop {
                match clients[ci].stream.read(&mut tmp) {
                    Ok(0) => {
                        clients[ci].dead = true;
                        break;
                    }
                    Ok(n) => clients[ci].reader.extend(&tmp[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        clients[ci].dead = true;
                        break;
                    }
                }
            }
            loop {
                match clients[ci].reader.next_frame() {
                    Ok(Some((kind, payload))) => {
                        handle_frame(ci, kind, payload, &mut clients, &mut sessions, &config)?
                    }
                    Ok(None) => break,
                    Err(_) => {
                        clients[ci].dead = true;
                        break;
                    }
                }
            }
        }

        // ---- Drain ptys into their pane terminals. ----
        let mut any_removed = false;
        let mut changed_sessions: Vec<u64> = Vec::new();
        for (k, &(si, ti, pane_id)) in fd_map.iter().enumerate() {
            if !ready[k + 1 + client_count].0 {
                continue;
            }
            let session_id = sessions[si].id;
            let tab = &mut sessions[si].tabs[ti];
            let Some(pane) = tab.layout.pane_mut(pane_id) else {
                continue; // already removed earlier this round
            };
            match pane.pty.read(&mut pane.term) {
                Ok(()) => {
                    if ti == sessions[si].active_tab && !changed_sessions.contains(&session_id) {
                        changed_sessions.push(session_id);
                    }
                }
                Err(_) => {
                    tab.remove_pane(pane_id);
                    any_removed = true;
                }
            }
        }

        // Redraw clients viewing sessions whose active tab produced output.
        for client in &mut clients {
            if let Some(id) = client.attached
                && changed_sessions.contains(&id)
                && matches!(client.mode, Mode::Running)
            {
                client.needs_redraw = true;
            }
        }

        // ---- Cleanup: empty tabs, empty sessions, homeless clients. ----
        if any_removed {
            for si in (0..sessions.len()).rev() {
                let session = &mut sessions[si];
                for ti in (0..session.tabs.len()).rev() {
                    if session.tabs[ti].is_empty() {
                        session.tabs.remove(ti);
                        if ti < session.active_tab {
                            session.active_tab -= 1;
                        }
                    }
                }
                if session.tabs.is_empty() {
                    sessions.remove(si);
                } else {
                    session.active_tab = session.active_tab.min(session.tabs.len() - 1);
                }
            }
            for client in &mut clients {
                let Some(id) = client.attached else { continue };
                if sessions.iter().any(|s| s.id == id) {
                    client.needs_redraw = true;
                    continue;
                }
                // Viewed session is gone: fall back to any surviving
                // session, else say goodbye.
                match sessions.first() {
                    Some(first) => {
                        client.attached = Some(first.id);
                        client.mode = Mode::Running;
                        client.needs_redraw = true;
                    }
                    None => client.bye("session closed"),
                }
                // Selections in a manager overlay may now be stale.
                if let Mode::Manager {
                    overlay,
                    ref mut selected,
                } = client.mode
                {
                    let count = match overlay {
                        Overlay::Sessions => sessions.len(),
                        Overlay::Tabs => 1, // clamped properly on redraw input
                    };
                    *selected = (*selected).min(count.saturating_sub(1));
                }
            }
            // Fallback-attached clients need their session resized.
            for ci in 0..clients.len() {
                if let Some(id) = clients[ci].attached
                    && let Some(si) = sessions.iter().position(|s| s.id == id)
                {
                    sessions[si].resize(content_size(clients[ci].size))?;
                }
            }
        }

        // ---- Render for clients that need it. ----
        for ci in 0..clients.len() {
            if !clients[ci].needs_redraw || clients[ci].closing || clients[ci].dead {
                continue;
            }
            clients[ci].needs_redraw = false;
            let Some(id) = clients[ci].attached else {
                continue;
            };
            let Some(si) = sessions.iter().position(|s| s.id == id) else {
                continue;
            };
            let size = clients[ci].size;
            let mut buf: Vec<u8> = Vec::with_capacity(4096);
            match &clients[ci].mode {
                Mode::Running => {
                    draw_session(&mut renderer, &sessions[si], &mut buf, size, config.accent)?;
                }
                Mode::Manager { overlay, selected } => match overlay {
                    Overlay::Sessions => {
                        let entries = session_entries(&config.pins, &sessions);
                        let items: Vec<ListItem> = entries
                            .iter()
                            .map(|e| ListItem {
                                label: e.name.clone(),
                                active: e.running == Some(si),
                                dim: e.running.is_none(),
                            })
                            .collect();
                        draw_manager(&mut buf, "sessions", &items, *selected, size, config.accent)?;
                    }
                    Overlay::Tabs => {
                        let session = &sessions[si];
                        let items: Vec<ListItem> = session
                            .tabs
                            .iter()
                            .enumerate()
                            .map(|(ti, t)| ListItem {
                                label: t.name.clone(),
                                active: ti == session.active_tab,
                                dim: false,
                            })
                            .collect();
                        let title = format!("{} · tabs", session.name);
                        draw_manager(&mut buf, &title, &items, *selected, size, config.accent)?;
                    }
                },
                Mode::Naming {
                    overlay,
                    name,
                    rename,
                } => {
                    let title = match (overlay, rename.is_some()) {
                        (Overlay::Sessions, false) => "new session",
                        (Overlay::Sessions, true) => "rename session",
                        (Overlay::Tabs, false) => "new tab",
                        (Overlay::Tabs, true) => "rename tab",
                    };
                    let footer = if rename.is_some() {
                        "enter rename · esc cancel"
                    } else {
                        "enter create · esc cancel"
                    };
                    draw_naming(&mut buf, title, name, size, config.accent, footer)?;
                }
            }
            clients[ci].send(S2C_OUTPUT, &buf);
        }

        // ---- Drop finished clients (sessions keep running). ----
        clients.retain(|c| !c.dead && !(c.closing && c.outbuf.is_empty()));
    }
}

/// Handle one protocol frame from client `ci`.
fn handle_frame(
    ci: usize,
    kind: u8,
    payload: Vec<u8>,
    clients: &mut Vec<ClientConn>,
    sessions: &mut Vec<Session>,
    config: &Config,
) -> Result<()> {
    match kind {
        C2S_ATTACH => {
            if payload.len() < 4 {
                clients[ci].dead = true;
                return Ok(());
            }
            let cols = u16::from_le_bytes([payload[0], payload[1]]);
            let rows = u16::from_le_bytes([payload[2], payload[3]]);
            let name = String::from_utf8_lossy(&payload[4..]).into_owned();
            let name = if name.trim().is_empty() {
                "main".to_string()
            } else {
                name.trim().to_string()
            };
            if cols > 0 && rows > 0 {
                clients[ci].size = (cols, rows);
            }
            let size = clients[ci].size;

            let si = match sessions.iter().position(|s| s.name == name) {
                Some(si) => si,
                None => crate::input::create_session(
                    sessions,
                    config,
                    crate::render::content_size(size),
                    name,
                )?,
            };
            attach_to(ci, si, clients, sessions)?;
        }
        C2S_INPUT => {
            let Some(id) = clients[ci].attached else {
                return Ok(());
            };
            let Some(mut active) = sessions.iter().position(|s| s.id == id) else {
                return Ok(());
            };
            let size = clients[ci].size;
            // Take the mode out so `clients` stays free for kick handling.
            let mut mode = std::mem::take(&mut clients[ci].mode);
            let overlay_involved = !matches!(mode, Mode::Running);
            let detach = handle_input(&payload, &mut mode, sessions, &mut active, content_size(size), config)?;
            let overlay_involved = overlay_involved || !matches!(mode, Mode::Running);
            clients[ci].mode = mode;
            clients[ci].needs_redraw = true;

            if detach {
                clients[ci].bye("detached");
            } else if sessions.is_empty() {
                // The last session was killed from the manager.
                clients[ci].bye("all sessions closed");
            } else {
                let active = active.min(sessions.len() - 1);
                if sessions.get(active).map(|s| s.id) != Some(id) {
                    // The manager switched sessions (created, killed, ...).
                    attach_to(ci, active, clients, sessions)?;
                }
            }

            // Kills may have orphaned other clients; renames/kills change
            // what everyone's overlays and bars show.
            rehome_homeless_clients(clients, sessions)?;
            if overlay_involved {
                for client in clients.iter_mut() {
                    if client.attached.is_some() && !client.closing && !client.dead {
                        client.needs_redraw = true;
                    }
                }
            }
        }
        C2S_LIST => {
            let listing = format_listing(sessions, clients, config);
            clients[ci].send(S2C_LIST, &listing);
            // One-shot request: close once the reply drains.
            clients[ci].closing = true;
        }
        C2S_RESIZE => {
            if payload.len() < 4 {
                return Ok(());
            }
            let cols = u16::from_le_bytes([payload[0], payload[1]]);
            let rows = u16::from_le_bytes([payload[2], payload[3]]);
            if cols == 0 || rows == 0 {
                return Ok(());
            }
            clients[ci].size = (cols, rows);
            if let Some(id) = clients[ci].attached
                && let Some(si) = sessions.iter().position(|s| s.id == id)
            {
                sessions[si].resize(content_size((cols, rows)))?;
            }
            clients[ci].needs_redraw = true;
        }
        _ => clients[ci].dead = true,
    }
    Ok(())
}

/// One line per session, colored for tty clients (the client strips the
/// colors when its stdout is piped): an accent dot and "attached" tag
/// for attached sessions, dim counts, and pinned-but-stopped sessions
/// listed dim — consistent with the session manager.
fn format_listing(sessions: &[Session], clients: &[ClientConn], config: &Config) -> Vec<u8> {
    let entries = session_entries(&config.pins, sessions);
    let mut out: Vec<u8> = Vec::new();
    if entries.is_empty() {
        out.extend_from_slice(b"no sessions\n");
        return out;
    }
    let name_width = entries
        .iter()
        .map(|e| e.name.chars().count())
        .max()
        .unwrap_or(0);

    for entry in &entries {
        let write = (|| -> crate::Result<()> {
            let padded = format!("{:<name_width$}", entry.name);
            match entry.running {
                Some(si) => {
                    let session = &sessions[si];
                    let panes: usize = session
                        .tabs
                        .iter()
                        .map(|tab| tab.layout.panes().len())
                        .sum();
                    let attached = clients
                        .iter()
                        .any(|c| c.attached == Some(session.id) && !c.closing && !c.dead);
                    if attached {
                        queue!(
                            out,
                            SetForegroundColor(config.accent),
                            Print("● "),
                            SetForegroundColor(Color::Reset),
                            SetAttribute(Attribute::Bold),
                            Print(&padded),
                            SetAttribute(Attribute::Reset),
                        )?;
                    } else {
                        queue!(
                            out,
                            SetAttribute(Attribute::Dim),
                            Print("○ "),
                            SetAttribute(Attribute::Reset),
                            Print(&padded),
                        )?;
                    }
                    queue!(
                        out,
                        SetAttribute(Attribute::Dim),
                        Print(format!(
                            "  {} tab{} · {} pane{}",
                            session.tabs.len(),
                            if session.tabs.len() == 1 { "" } else { "s" },
                            panes,
                            if panes == 1 { "" } else { "s" },
                        )),
                        SetAttribute(Attribute::Reset),
                    )?;
                    if attached {
                        queue!(
                            out,
                            Print("  "),
                            SetForegroundColor(config.accent),
                            Print("attached"),
                            SetForegroundColor(Color::Reset),
                        )?;
                    }
                }
                None => {
                    queue!(
                        out,
                        SetAttribute(Attribute::Dim),
                        Print(format!("○ {padded}  not running")),
                        SetAttribute(Attribute::Reset),
                    )?;
                }
            }
            queue!(out, Print("\n"))?;
            Ok(())
        })();
        // Writing into a Vec cannot fail.
        let _ = write;
    }
    out
}

/// Clients whose session vanished (killed from a manager) fall back to
/// the first surviving session, or get a goodbye when none remain.
fn rehome_homeless_clients(
    clients: &mut [ClientConn],
    sessions: &mut [Session],
) -> Result<()> {
    for ci in 0..clients.len() {
        if clients[ci].closing || clients[ci].dead {
            continue;
        }
        let Some(id) = clients[ci].attached else {
            continue;
        };
        if sessions.iter().any(|s| s.id == id) {
            continue;
        }
        if sessions.is_empty() {
            clients[ci].bye("session closed");
            continue;
        }
        clients[ci].attached = Some(sessions[0].id);
        clients[ci].mode = Mode::Running;
        clients[ci].needs_redraw = true;
        sessions[0].resize(content_size(clients[ci].size))?;
    }
    Ok(())
}

/// Point client `ci` at session index `si`: kick any other client on that
/// session, size it for this client, and schedule a full redraw.
fn attach_to(
    ci: usize,
    si: usize,
    clients: &mut [ClientConn],
    sessions: &mut [Session],
) -> Result<()> {
    let id = sessions[si].id;
    for cj in 0..clients.len() {
        if cj != ci && clients[cj].attached == Some(id) && !clients[cj].closing {
            clients[cj].bye("kicked: another client attached to this session");
        }
    }
    clients[ci].attached = Some(id);
    sessions[si].resize(content_size(clients[ci].size))?;
    clients[ci].needs_redraw = true;
    Ok(())
}
