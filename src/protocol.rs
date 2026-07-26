//! The client ↔ server wire protocol: length-prefixed frames over a
//! Unix domain socket.
//!
//! Frame layout: `[kind: u8][len: u32 LE][payload: len bytes]`.

use std::path::PathBuf;

/// Client → server: attach to a session by name (creating it if needed).
/// Payload: `[cols: u16 LE][rows: u16 LE][name: utf-8]`.
pub const C2S_ATTACH: u8 = 1;
/// Client → server: raw keyboard bytes. Payload: the bytes.
pub const C2S_INPUT: u8 = 2;
/// Client → server: the client terminal was resized.
/// Payload: `[cols: u16 LE][rows: u16 LE]`.
pub const C2S_RESIZE: u8 = 3;
/// Client → server: list sessions; the server replies with `S2C_LIST`
/// and closes the connection.
pub const C2S_LIST: u8 = 4;
/// Client → server, one-shot agent commands (see `agent.rs` for the
/// payload encodings); the server replies with `S2C_AGENT_OK` or
/// `S2C_AGENT_ERR` and closes.
pub const C2S_AGENT_NEW: u8 = 5;
pub const C2S_AGENT_KILL: u8 = 6;
pub const C2S_AGENT_SEND: u8 = 7;
pub const C2S_AGENT_READ: u8 = 8;

/// Server → client: bytes to write to the client's terminal.
pub const S2C_OUTPUT: u8 = 1;
/// Server → client: the server is done with this client (detached,
/// kicked, session closed). Payload: a utf-8 reason to show the user.
pub const S2C_BYE: u8 = 2;
/// Server → client: the session listing, ready to print.
pub const S2C_LIST: u8 = 3;
/// Server → client: an agent command succeeded; payload is its output.
pub const S2C_AGENT_OK: u8 = 4;
/// Server → client: an agent command failed; payload is the error text.
pub const S2C_AGENT_ERR: u8 = 5;

/// Ceiling on a single frame; anything larger is a protocol error.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Where the server listens. Overridable for tests via `RMUX_SOCK`.
///
/// Deliberately not under `$XDG_RUNTIME_DIR`: that dir is torn down on
/// logout and unset for system services, so server (systemd) and client
/// (login session) would disagree. `/tmp/rmux-<uid>.sock` is stable and
/// visible to both.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("RMUX_SOCK") {
        return PathBuf::from(path);
    }
    PathBuf::from(format!("/tmp/rmux-{}.sock", nix::unistd::getuid()))
}

/// Encode one frame.
pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Accumulates stream bytes and yields complete frames.
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn extend(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pop the next complete frame, `Ok(None)` if more bytes are needed,
    /// or an error when the stream is corrupt (caller should disconnect).
    pub fn next_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>, String> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[1..5].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            return Err(format!("oversized frame ({len} bytes)"));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let kind = self.buf[0];
        let payload = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Ok(Some((kind, payload)))
    }
}
