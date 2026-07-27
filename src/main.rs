//! A minimal terminal multiplexer built on `libghostty-vt` that runs
//! inside your existing terminal (no display server needed).
//!
//! It follows the tmux client/server model: a long-lived server (run it
//! under systemd) owns all sessions → tabs → panes, parsing pty output
//! through libghostty's VT engine; thin clients attach to a session by
//! name over a Unix socket, so sessions survive SSH disconnects. At most
//! one client per session — a new attach kicks the old client.
//!
//!   rmux server        run the server (foreground)
//!   rmux a <name>      attach to session <name>, creating it if new
//!
//! Inside a client (defaults; rebindable in the config): Ctrl+O opens
//! the session manager, Ctrl+N the tab manager, Ctrl+K/L split the
//! focused pane, Ctrl+Q/W/E/R move pane focus, Ctrl+T cycles it, and
//! Ctrl+G detaches.

mod agent;
mod client;
mod config;
mod input;
mod model;
mod protocol;
mod pty;
mod render;
mod server;
mod state;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("server") => server::run(),
        Some("a" | "attach") => match args.get(1) {
            Some(name) => client::run(name),
            None => Err("usage: rmux a[ttach] <session-name>".into()),
        },
        Some("list" | "ls") => client::list(),
        Some("agent") => agent::run(&args[1..]),
        _ => {
            eprintln!(
                "usage: rmux server | rmux a[ttach] <session-name> | rmux list\n\
                        rmux agent new|kill|send|read ...  (rmux agent for details)"
            );
            std::process::exit(2);
        }
    };
    // One clean line on stderr (journalctl shows it) instead of Rust's
    // escaped Debug formatting.
    if let Err(e) = result {
        eprintln!("rmux: {e}");
        std::process::exit(1);
    }
}
