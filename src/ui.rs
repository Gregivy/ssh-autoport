//! TUI state and rendering (ratatui).

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::types::{AppView, Cmd, Ev, FwdView, HostView, MasterView};

const TOAST_TTL: Duration = Duration::from_secs(6);

pub enum Action {
    None,
    Quit,
}

struct Edit {
    host: String,
    rport: u16,
    label: String,
    buf: String,
}

pub struct Ui {
    snap: Vec<HostView>,
    auto: bool,
    show_system: bool,
    /// Selected app row, tracked by identity so it survives refreshes.
    sel: Option<(String, u16)>,
    editing: Option<Edit>,
    toast: Option<(String, Instant)>,
    scroll: u16,
}

impl Ui {
    pub fn new(show_system: bool) -> Ui {
        Ui {
            snap: Vec::new(),
            auto: true,
            show_system,
            sel: None,
            editing: None,
            toast: None,
            scroll: 0,
        }
    }

    pub fn apply(&mut self, ev: Ev) {
        match ev {
            Ev::Snapshot(s) => {
                self.snap = s;
                self.fix_selection();
            }
            Ev::Toast(msg) => self.toast = Some((msg, Instant::now())),
            Ev::AutoMode(a) => self.auto = a,
            Ev::CleanedUp => {}
        }
    }

    fn selectable(&self) -> Vec<(String, u16)> {
        let mut v = Vec::new();
        for h in &self.snap {
            for a in &h.apps {
                if self.show_system || !a.system {
                    v.push((h.key.clone(), a.rport));
                }
            }
        }
        v
    }

    fn fix_selection(&mut self) {
        let rows = self.selectable();
        match &self.sel {
            Some(s) if rows.contains(s) => {}
            _ => self.sel = rows.first().cloned(),
        }
    }

    fn selected_app(&self) -> Option<(&HostView, &AppView)> {
        let (hk, rp) = self.sel.as_ref()?;
        let h = self.snap.iter().find(|h| &h.key == hk)?;
        let a = h.apps.iter().find(|a| a.rport == *rp)?;
        Some((h, a))
    }

    fn move_sel(&mut self, delta: i32) {
        let rows = self.selectable();
        if rows.is_empty() {
            self.sel = None;
            return;
        }
        let cur = self
            .sel
            .as_ref()
            .and_then(|s| rows.iter().position(|r| r == s))
            .unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, rows.len() as i32 - 1) as usize;
        self.sel = Some(rows[next].clone());
    }

    pub fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        cmds: &Sender<Cmd>,
    ) -> Action {
        use crossterm::event::{KeyCode, KeyModifiers};

        if let Some(edit) = &mut self.editing {
            match key.code {
                KeyCode::Esc => self.editing = None,
                KeyCode::Backspace => {
                    edit.buf.pop();
                }
                KeyCode::Char(c) if c.is_ascii_digit() && edit.buf.len() < 5 => {
                    edit.buf.push(c);
                }
                KeyCode::Enter => {
                    let edit = self.editing.take().unwrap();
                    match edit.buf.parse::<u16>() {
                        Ok(p) if p >= 1 => {
                            let _ = cmds.send(Cmd::Assign {
                                host: edit.host,
                                rport: edit.rport,
                                lport: p,
                            });
                        }
                        _ => {
                            self.toast = Some((
                                format!("\"{}\" is not a valid port (1-65535)", edit.buf),
                                Instant::now(),
                            ));
                        }
                    }
                }
                _ => {}
            }
            return Action::None;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                return Action::Quit
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => self.move_sel(-1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => self.move_sel(1),
            (KeyCode::Enter, _) | (KeyCode::Char('e'), _) => {
                if let Some((h, a)) = self.selected_app() {
                    self.editing = Some(Edit {
                        host: h.key.clone(),
                        rport: a.rport,
                        label: format!(
                            "{} (remote :{}) on {}",
                            a.process.as_deref().unwrap_or("?"),
                            a.rport,
                            h.title
                        ),
                        buf: a.lport.map(|p| p.to_string()).unwrap_or_default(),
                    });
                }
            }
            (KeyCode::Char('f'), _) => {
                if let Some((h, a)) = self.selected_app() {
                    let _ = cmds.send(Cmd::Toggle { host: h.key.clone(), rport: a.rport });
                }
            }
            (KeyCode::Char('a'), _) => {
                self.show_system = !self.show_system;
                self.fix_selection();
            }
            (KeyCode::Char('p'), _) => {
                let _ = cmds.send(Cmd::ToggleAuto);
            }
            (KeyCode::Char('r'), _) => {
                let _ = cmds.send(Cmd::Refresh);
            }
            (KeyCode::Char('o'), _) => {
                if let Some((_, a)) = self.selected_app() {
                    if let (FwdView::Active | FwdView::External, Some(lp)) = (&a.status, a.lport) {
                        open_browser(lp);
                    } else {
                        self.toast =
                            Some(("not forwarded yet — nothing to open".into(), Instant::now()));
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let [head, body, note, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());

        // Header
        let auto = if self.auto { "auto-forward ON".green() } else { "auto-forward OFF".red() };
        let header = Line::from(vec![
            " ssh-autoport ".bold().reversed(),
            "  ".into(),
            auto,
            format!(
                "  ·  {} connection{}",
                self.snap.len(),
                if self.snap.len() == 1 { "" } else { "s" }
            )
            .dim(),
        ]);
        f.render_widget(Paragraph::new(header), head);

        // Body
        let (lines, sel_line) = self.body_lines();
        let h = body.height.max(1);
        if let Some(sl) = sel_line {
            let sl = sl as u16;
            if sl < self.scroll {
                self.scroll = sl;
            } else if sl >= self.scroll + h {
                self.scroll = sl - h + 1;
            }
        }
        self.scroll = self.scroll.min(lines.len().saturating_sub(1) as u16);
        f.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), body);

        // Toast / edit prompt
        if let Some(edit) = &self.editing {
            let l = Line::from(vec![
                " local port for ".into(),
                edit.label.clone().bold(),
                ": ".into(),
                edit.buf.clone().yellow().bold(),
                "▏".yellow(),
                "   Enter apply · Esc cancel".dim(),
            ]);
            f.render_widget(Paragraph::new(l), note);
        } else if let Some((msg, at)) = &self.toast {
            if at.elapsed() < TOAST_TTL {
                f.render_widget(
                    Paragraph::new(Line::from(format!(" {msg}").yellow())),
                    note,
                );
            } else {
                self.toast = None;
            }
        }

        // Help
        let help_text = " ↑↓ select · ⏎/e set port · f forward on/off · o open · a system ports · p auto · r refresh · q quit";
        f.render_widget(Paragraph::new(Line::from(help_text.dim())), help);
    }

    /// Render all host blocks; returns lines plus the selected row's index.
    fn body_lines(&self) -> (Vec<Line<'static>>, Option<usize>) {
        let mut lines: Vec<Line> = Vec::new();
        let mut sel_line = None;

        if self.snap.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(
                "  No active SSH connections. Open one (e.g. `ssh myserver`) and it will appear here."
                    .dim(),
            ));
            return (lines, None);
        }

        for host in &self.snap {
            lines.push(Line::from(""));
            lines.push(host_line(host));

            let apps: Vec<&AppView> = host
                .apps
                .iter()
                .filter(|a| self.show_system || !a.system)
                .collect();
            let hidden = host.apps.len() - apps.len();

            match &host.master {
                MasterView::Ready { .. } => {
                    if apps.is_empty() {
                        let msg = if !host.scanned_once {
                            "   scanning…".to_string()
                        } else if hidden > 0 {
                            format!("   no apps detected ({hidden} system port{} hidden — press a)",
                                if hidden == 1 { "" } else { "s" })
                        } else {
                            "   no listening apps detected".to_string()
                        };
                        lines.push(Line::from(msg.dim()));
                    } else {
                        lines.push(Line::from(
                            format!(
                                "   {:<18} {:>12} {:>17}  {}",
                                "PROCESS", "REMOTE", "LOCAL", "STATUS"
                            )
                            .dim(),
                        ));
                        for a in apps {
                            let selected =
                                self.sel == Some((host.key.clone(), a.rport));
                            if selected {
                                sel_line = Some(lines.len());
                            }
                            lines.push(app_line(a, selected));
                        }
                        if hidden > 0 && !self.show_system {
                            lines.push(Line::from(
                                format!("   + {hidden} system port{} hidden (press a)",
                                    if hidden == 1 { "" } else { "s" })
                                .dim(),
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        (lines, sel_line)
    }
}

fn host_line(host: &HostView) -> Line<'static> {
    let mut spans: Vec<Span> = vec![" ".into()];
    match &host.master {
        MasterView::Connecting => {
            spans.push("⟳ ".yellow());
            spans.push(host.title.clone().bold());
            spans.push("  connecting…".yellow());
        }
        MasterView::Ready { shared } => {
            spans.push("● ".green());
            spans.push(host.title.clone().bold());
            if *shared {
                spans.push("  (sharing your ssh session)".dim());
            }
        }
        MasterView::Failed(e) => {
            spans.push("✖ ".red());
            spans.push(host.title.clone().bold());
            spans.push(format!("  {e}").red());
        }
    }
    if let Some(alias) = &host.alias {
        spans.push(format!("  [{alias}]").dim());
    }
    if let Some(e) = &host.scan_err {
        spans.push(format!("  scan failed: {e}").red());
    }
    Line::from(spans)
}

fn app_line(a: &AppView, selected: bool) -> Line<'static> {
    let proc = a.process.clone().unwrap_or_else(|| "?".into());
    let remote = format!("{}:{}", a.addr, a.rport);
    let local = match a.lport {
        Some(p) => format!("127.0.0.1:{p}"),
        None => "—".into(),
    };
    let (mark, status, color) = match &a.status {
        FwdView::Active => ("●", "forwarded".to_string(), Color::Green),
        FwdView::External => ("⇄", "via your ssh".to_string(), Color::Cyan),
        FwdView::Pending => ("◌", "connecting…".to_string(), Color::Yellow),
        FwdView::Off if a.muted => ("○", "off".to_string(), Color::DarkGray),
        FwdView::Off => ("○", "—".to_string(), Color::DarkGray),
        FwdView::Error(e) => ("✖", e.clone(), Color::Red),
    };
    let mut tail = status;
    if a.pinned {
        tail.push_str("  ⊙ pinned");
    }
    if a.system {
        tail.push_str("  [system]");
    }

    let cursor = if selected { " ▸ " } else { "   " };
    let base = format!("{cursor}{proc:<18} {remote:>12} {local:>17}  ");
    let mut spans: Vec<Span> = vec![
        Span::raw(base),
        Span::styled(format!("{mark} "), Style::new().fg(color)),
        Span::styled(tail, Style::new().fg(color)),
    ];
    if a.system && !selected {
        spans = spans
            .into_iter()
            .map(|s| s.style(Style::new().add_modifier(Modifier::DIM)))
            .collect();
    }
    let mut line = Line::from(spans);
    if selected {
        line = line.style(Style::new().add_modifier(Modifier::REVERSED));
    }
    line
}

fn open_browser(lport: u16) {
    let url = format!("http://127.0.0.1:{lport}/");
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(not(target_os = "macos"))]
    let prog = "xdg-open";
    let _ = std::process::Command::new(prog)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
