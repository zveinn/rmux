//! The attach client: a thin pump between the user's terminal and the
//! server. Raw stdin bytes go up the socket; rendered output frames come
//! back down and go straight to stdout. All key handling, state, and
//! rendering live in the server.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

use crossterm::{
    cursor::{Hide, Show},
    execute, terminal,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::Result;
use crate::protocol::{
    C2S_ATTACH, C2S_INPUT, C2S_LIST, C2S_RESIZE, FrameReader, S2C_BYE, S2C_LIST, S2C_OUTPUT,
    frame, socket_path,
};

/// Print the server's session listing and exit.
pub fn list() -> Result<()> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        format!(
            "cannot connect to server at {}: {e}\n\
             start it with: rmux server  (or: sudo systemctl start rmux)",
            path.display()
        )
    })?;
    stream.write_all(&frame(C2S_LIST, &[]))?;

    let mut reader = FrameReader::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Err("server closed the connection without a listing".into());
        }
        reader.extend(&buf[..n]);
        while let Some((kind, payload)) = reader.next_frame().map_err(io::Error::other)? {
            if kind == S2C_LIST {
                let text = String::from_utf8_lossy(&payload);
                // Colors for humans; plain text when piped.
                if std::io::IsTerminal::is_terminal(&io::stdout()) {
                    print!("{text}");
                } else {
                    print!("{}", strip_ansi(&text));
                }
                return Ok(());
            }
        }
    }
}

/// Drop CSI escape sequences (colors, attributes) from server output.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

pub fn run(name: &str) -> Result<()> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        format!(
            "cannot connect to server at {}: {e}\n\
             start it with: rmux server  (or: sudo systemctl start rmux)",
            path.display()
        )
    })?;

    let mut size = match terminal::size() {
        Ok((0, _)) | Ok((_, 0)) | Err(_) => (80, 24),
        Ok(s) => s,
    };

    // Attach first so the server's initial frame is already in flight
    // when we enter the alternate screen.
    let mut payload = Vec::with_capacity(4 + name.len());
    payload.extend_from_slice(&size.0.to_le_bytes());
    payload.extend_from_slice(&size.1.to_le_bytes());
    payload.extend_from_slice(name.as_bytes());
    stream.write_all(&frame(C2S_ATTACH, &payload))?;

    let guard = ScreenGuard::enter()?;

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut stdin_buf = [0u8; 1024];
    let mut sock_buf = [0u8; 65536];
    let mut reader = FrameReader::new();
    let mut reason = String::from("connection closed by server");

    'outer: loop {
        let (stdin_ready, sock_ready) = {
            let mut fds = [
                PollFd::new(stdin.as_fd(), PollFlags::POLLIN),
                PollFd::new(stream.as_fd(), PollFlags::POLLIN),
            ];
            poll(&mut fds, PollTimeout::from(100u16))?;
            (
                fds[0].any().unwrap_or(false),
                fds[1].any().unwrap_or(false),
            )
        };

        if stdin_ready {
            match nix::unistd::read(stdin.as_fd(), &mut stdin_buf) {
                Ok(0) => {
                    reason = "detached (stdin closed)".to_string();
                    break;
                }
                Ok(len) => stream.write_all(&frame(C2S_INPUT, &stdin_buf[..len]))?,
                Err(nix::errno::Errno::EINTR) => {}
                Err(e) => return Err(e.into()),
            }
        }

        if sock_ready {
            match stream.read(&mut sock_buf) {
                Ok(0) => break,
                Ok(n) => reader.extend(&sock_buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
            while let Some((kind, payload)) = reader.next_frame().map_err(io::Error::other)? {
                match kind {
                    S2C_OUTPUT => {
                        stdout.write_all(&payload)?;
                        stdout.flush()?;
                    }
                    S2C_BYE => {
                        reason = String::from_utf8_lossy(&payload).into_owned();
                        break 'outer;
                    }
                    _ => {}
                }
            }
        }

        // Propagate terminal resizes (checked per tick; SIGWINCH-free).
        if let Ok(now) = terminal::size()
            && now != size
            && now.0 > 0
            && now.1 > 0
        {
            size = now;
            let mut payload = Vec::with_capacity(4);
            payload.extend_from_slice(&size.0.to_le_bytes());
            payload.extend_from_slice(&size.1.to_le_bytes());
            stream.write_all(&frame(C2S_RESIZE, &payload))?;
        }
    }

    drop(guard);
    println!("[rmux] {reason}");
    Ok(())
}

/// Puts the terminal into raw mode + alternate screen, and restores it
/// on drop so a panic doesn't leave the user's terminal broken.
struct ScreenGuard;

impl ScreenGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}
