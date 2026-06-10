mod control;
mod discover;
mod ports;
mod remote;
mod state;
mod types;
mod ui;
mod util;
mod worker;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};

/// Set by SIGTERM/SIGINT/SIGHUP so we can cancel forwards before dying —
/// crucial when they live on the user's own ControlMaster, which outlives us.
static SHOULD_QUIT: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn install_signal_handlers() {
    extern "C" fn on_signal(_: i32) {
        SHOULD_QUIT.store(true, Ordering::SeqCst);
    }
    let handler = on_signal as extern "C" fn(i32);
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(sig, handler as usize);
        }
    }
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

use types::{Cmd, Ev};
use ui::{Action, Ui};

const USAGE: &str = "\
ssh-autoport — auto-discover and port-forward apps on your active SSH connections

USAGE:
    ssh-autoport [OPTIONS]

OPTIONS:
    -i, --interval <SECS>   Remote rescan interval (default: 3)
        --no-auto           Don't auto-forward; assign ports manually in the TUI
        --show-system       Show system services (sshd, dns, ...) from the start
    -V, --version           Print version
    -h, --help              Show this help

KEYS (in the TUI):
    Up/Down    select app          Enter/e   assign a local port (pins it)
    f          forward on/off      o         open http://127.0.0.1:<port>/
    a          show/hide system ports        p   pause/resume auto-forward
    r          rescan now          q         quit (tears down our forwards)

Port assignments are remembered per server+app in
~/.config/ssh-autoport/state.json and reused next time.";

fn main() {
    let mut interval = Duration::from_secs(3);
    let mut auto = true;
    let mut show_system = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-i" | "--interval" => {
                let v = args.next().and_then(|v| v.parse::<u64>().ok());
                match v {
                    Some(s) if s >= 1 => interval = Duration::from_secs(s),
                    _ => die("--interval expects a number of seconds (>= 1)"),
                }
            }
            "--no-auto" => auto = false,
            "--show-system" => show_system = true,
            "-V" | "--version" => {
                println!("ssh-autoport {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => die(&format!("unknown option: {other}\n\n{USAGE}")),
        }
    }

    install_signal_handlers();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (ev_tx, ev_rx) = mpsc::channel::<Ev>();
    std::thread::spawn(move || worker::run(cmd_rx, ev_tx, worker::Options { interval, auto }));

    let mut terminal = ratatui::init();
    let mut ui = Ui::new(show_system);

    loop {
        if SHOULD_QUIT.load(Ordering::SeqCst) {
            break;
        }
        for ev in ev_rx.try_iter() {
            ui.apply(ev);
        }
        if terminal.draw(|f| ui.draw(f)).is_err() {
            break;
        }
        match event::poll(Duration::from_millis(120)) {
            Ok(true) => {
                if let Ok(Event::Key(k)) = event::read() {
                    if k.kind == KeyEventKind::Press || k.kind == KeyEventKind::Repeat {
                        if let Action::Quit = ui.handle_key(k, &cmd_tx) {
                            break;
                        }
                    }
                }
            }
            Ok(false) => {}
            Err(_) => break,
        }
    }

    // Graceful shutdown: let the worker cancel forwards / close masters.
    let _ = cmd_tx.send(Cmd::Quit);
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        match ev_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ev::CleanedUp) => break,
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    ratatui::restore();
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}
