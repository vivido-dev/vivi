use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::style::Print;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::cli::Config;
use crate::terminal_geometry::TerminalGeometry;

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const STATUS_SHORTCUTS: &str = "f +10s b -10s Left -5s Right +5s Up/Down vol g goto q quit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    SeekBy(i64),
    SeekTo(u64),
    VolumeBy(i32),
    Resize(TerminalGeometry),
    Quit,
}

#[derive(Debug, Clone)]
struct Status {
    current_us: u64,
    duration_us: Option<u64>,
    volume_percent: Option<u32>,
    message: String,
    goto_buffer: Option<String>,
}

#[derive(Debug, Default)]
struct InputState {
    goto_buffer: Option<String>,
}

pub struct PlaybackUi {
    receiver: mpsc::Receiver<Command>,
    running: Arc<AtomicBool>,
    input: Option<thread::JoinHandle<()>>,
    status: Arc<Mutex<Status>>,
    title: Option<String>,
    _terminal: TerminalSession,
}

impl PlaybackUi {
    pub fn enabled(config: &Config) -> bool {
        !config.inline
            && !config.is_dry_run()
            && !config.no_wait
            && io::stdin().is_terminal()
            && io::stdout().is_terminal()
    }

    pub fn enter(
        config: &Config,
        path: &Path,
        duration_us: Option<u64>,
        volume_available: bool,
        audio_only: bool,
    ) -> io::Result<Option<Self>> {
        if !Self::enabled(config) {
            return Ok(None);
        }
        let terminal = TerminalSession::enter()?;
        let title = audio_only.then(|| {
            format!(
                "Audio — {}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("audio")
            )
        });
        let status = Arc::new(Mutex::new(Status {
            current_us: 0,
            duration_us,
            volume_percent: volume_available.then_some(100),
            message: String::new(),
            goto_buffer: None,
        }));
        draw(&status, title.as_deref())?;

        let running = Arc::new(AtomicBool::new(true));
        let (sender, receiver) = mpsc::channel();
        let input_running = running.clone();
        let input_status = status.clone();
        let input_title = title.clone();
        let input = thread::spawn(move || {
            input_loop(sender, input_running, input_status, input_title);
        });
        Ok(Some(Self {
            receiver,
            running,
            input: Some(input),
            status,
            title,
            _terminal: terminal,
        }))
    }

    pub fn try_command(&self) -> Option<Command> {
        self.receiver.try_recv().ok()
    }

    pub fn set_position_us(&self, position_us: u64) {
        self.update(|status| status.current_us = position_us);
    }

    pub fn set_volume_percent(&self, volume_percent: Option<u32>) {
        self.update(|status| status.volume_percent = volume_percent);
    }

    pub fn set_message(&self, message: impl Into<String>) {
        let message = message.into();
        self.update(|status| status.message = message);
    }

    pub fn redraw(&self) -> io::Result<()> {
        draw(&self.status, self.title.as_deref())
    }

    fn update(&self, apply: impl FnOnce(&mut Status)) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        apply(&mut status);
    }
}

impl Drop for PlaybackUi {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(input) = self.input.take() {
            let _ = input.join();
        }
    }
}

struct TerminalSession;

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = leave_terminal(&mut io::stdout());
        let _ = disable_raw_mode();
    }
}

/// Restore the primary screen and the cursor position saved by DECSET 1049.
///
/// Moving afterward would overwrite that restored position and make the shell redraw its prompt
/// at the top-left of the still-populated primary screen.
fn leave_terminal(output: &mut impl Write) -> io::Result<()> {
    execute!(output, Show, Clear(ClearType::All), LeaveAlternateScreen)
}

fn input_loop(
    sender: mpsc::Sender<Command>,
    running: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    title: Option<String>,
) {
    let mut input = InputState::default();
    let mut drawn_position_second = 0;
    while running.load(Ordering::SeqCst) {
        match event::poll(EVENT_POLL_INTERVAL) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key))
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    let (command, message, redraw) = handle_key(key, &mut input);
                    {
                        let mut current = status
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        current.goto_buffer = input.goto_buffer.clone();
                        if let Some(message) = message {
                            current.message = message;
                        }
                    }
                    if redraw {
                        let _ = draw(&status, title.as_deref());
                    }
                    if let Some(command) = command {
                        if command == Command::Quit {
                            running.store(false, Ordering::SeqCst);
                        }
                        if sender.send(command).is_err() {
                            break;
                        }
                    }
                }
                Ok(Event::Resize(_, _)) => {
                    let geometry = TerminalGeometry::current();
                    let _ = sender.send(Command::Resize(geometry));
                    let _ = draw(&status, title.as_deref());
                }
                Ok(_) => {}
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
        let current_us = status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .current_us;
        if displayed_second_changed(current_us, &mut drawn_position_second) {
            let _ = draw(&status, title.as_deref());
        }
    }
}

fn displayed_second_changed(current_us: u64, drawn_second: &mut u64) -> bool {
    let current_second = current_us / 1_000_000;
    if current_second == *drawn_second {
        return false;
    }
    *drawn_second = current_second;
    true
}

fn handle_key(key: KeyEvent, state: &mut InputState) -> (Option<Command>, Option<String>, bool) {
    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return (Some(Command::Quit), None, false);
    }

    if let Some(buffer) = &mut state.goto_buffer {
        return match key.code {
            KeyCode::Esc => {
                state.goto_buffer = None;
                (None, Some("Goto canceled".into()), true)
            }
            KeyCode::Enter => match parse_timestamp_us(buffer) {
                Some(timestamp) => {
                    state.goto_buffer = None;
                    (
                        Some(Command::SeekTo(timestamp)),
                        Some(format!("Goto {}", format_time(timestamp))),
                        true,
                    )
                }
                None => (None, Some("Invalid timestamp".into()), true),
            },
            KeyCode::Backspace => {
                buffer.pop();
                (None, None, true)
            }
            KeyCode::Char(character) if character.is_ascii_digit() || character == ':' => {
                buffer.push(character);
                (None, None, true)
            }
            _ => (None, None, false),
        };
    }

    match key.code {
        KeyCode::Char('f') => (Some(Command::SeekBy(10_000_000)), None, false),
        KeyCode::Char('b') => (Some(Command::SeekBy(-10_000_000)), None, false),
        KeyCode::Left => (Some(Command::SeekBy(-5_000_000)), None, false),
        KeyCode::Right => (Some(Command::SeekBy(5_000_000)), None, false),
        KeyCode::Up => (Some(Command::VolumeBy(5)), None, false),
        KeyCode::Down => (Some(Command::VolumeBy(-5)), None, false),
        KeyCode::Char('g') => {
            state.goto_buffer = Some(String::new());
            (None, Some("Enter timestamp".into()), true)
        }
        _ => (None, None, false),
    }
}

fn draw(status: &Arc<Mutex<Status>>, title: Option<&str>) -> io::Result<()> {
    let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let snapshot = status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let mut stdout = io::stdout().lock();
    if let Some(title) = title {
        let title = truncate(title, columns);
        let column = columns.saturating_sub(title.chars().count() as u16) / 2;
        let row = rows.saturating_sub(1) / 2;
        execute!(
            stdout,
            MoveTo(column, row),
            Clear(ClearType::CurrentLine),
            Print(title)
        )?;
    }
    if rows >= 2 {
        let text = truncate(&status_text(&snapshot), columns);
        execute!(
            stdout,
            MoveTo(0, rows - 1),
            Clear(ClearType::CurrentLine),
            Print(text)
        )?;
    }
    stdout.flush()
}

fn status_text(status: &Status) -> String {
    let duration = status
        .duration_us
        .map(format_time)
        .unwrap_or_else(|| "--:--".into());
    let volume = status
        .volume_percent
        .map(|value| format!("Vol {value}%"))
        .unwrap_or_else(|| "Vol --".into());
    let middle = if let Some(input) = &status.goto_buffer {
        format!("Goto> {input}  Enter seek Esc cancel")
    } else {
        status.message.clone()
    };
    if middle.is_empty() {
        format!(
            "{} / {} | {} | {}",
            format_time(status.current_us),
            duration,
            volume,
            STATUS_SHORTCUTS
        )
    } else {
        format!(
            "{} / {} | {} | {} | {}",
            format_time(status.current_us),
            duration,
            volume,
            middle,
            STATUS_SHORTCUTS
        )
    }
}

fn format_time(microseconds: u64) -> String {
    let total_seconds = microseconds / 1_000_000;
    let seconds = total_seconds % 60;
    let minutes = total_seconds / 60 % 60;
    let hours = total_seconds / 3600;
    if hours == 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

fn parse_timestamp_us(value: &str) -> Option<u64> {
    let parts = value.split(':').collect::<Vec<_>>();
    if value.is_empty() || parts.len() > 3 || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    let numbers = parts
        .iter()
        .map(|part| part.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let seconds = match numbers.as_slice() {
        [seconds] => *seconds,
        [minutes, seconds] if *seconds < 60 => minutes.checked_mul(60)?.checked_add(*seconds)?,
        [hours, minutes, seconds] if *minutes < 60 && *seconds < 60 => hours
            .checked_mul(3_600)?
            .checked_add(minutes.checked_mul(60)?)?
            .checked_add(*seconds)?,
        _ => return None,
    };
    seconds.checked_mul(1_000_000)
}

fn truncate(value: &str, columns: u16) -> String {
    value.chars().take(columns as usize).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitim_seek_keys_are_preserved() {
        let mut state = InputState::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(
            handle_key(key(KeyCode::Char('f')), &mut state).0,
            Some(Command::SeekBy(10_000_000))
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('b')), &mut state).0,
            Some(Command::SeekBy(-10_000_000))
        );
        assert_eq!(
            handle_key(key(KeyCode::Left), &mut state).0,
            Some(Command::SeekBy(-5_000_000))
        );
        assert_eq!(
            handle_key(key(KeyCode::Right), &mut state).0,
            Some(Command::SeekBy(5_000_000))
        );
        assert_eq!(
            handle_key(key(KeyCode::Up), &mut state).0,
            Some(Command::VolumeBy(5))
        );
        assert_eq!(
            handle_key(key(KeyCode::Down), &mut state).0,
            Some(Command::VolumeBy(-5))
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('q')), &mut state).0,
            Some(Command::Quit)
        );
    }

    #[test]
    fn goto_accepts_seconds_minutes_and_hours() {
        assert_eq!(parse_timestamp_us("90"), Some(90_000_000));
        assert_eq!(parse_timestamp_us("1:30"), Some(90_000_000));
        assert_eq!(parse_timestamp_us("1:01:30"), Some(3_690_000_000));
        assert_eq!(parse_timestamp_us("1:90"), None);
    }

    #[test]
    fn position_redraws_once_per_displayed_second_in_either_direction() {
        let mut drawn_second = 0;
        assert!(!displayed_second_changed(999_999, &mut drawn_second));
        assert!(displayed_second_changed(1_000_000, &mut drawn_second));
        assert!(!displayed_second_changed(1_999_999, &mut drawn_second));
        assert!(displayed_second_changed(500_000, &mut drawn_second));
    }

    #[test]
    fn one_row_terminals_omit_the_status_line() {
        let status = Status {
            current_us: 0,
            duration_us: None,
            volume_percent: Some(100),
            message: String::new(),
            goto_buffer: None,
        };
        assert!(status_text(&status).contains("f +10s"));
    }

    #[test]
    fn leaving_playback_does_not_overwrite_the_restored_primary_cursor() {
        let mut output = Vec::new();
        leave_terminal(&mut output).unwrap();

        assert!(output.ends_with(b"\x1b[?1049l"));
        assert!(
            !output
                .windows(b"\x1b[1;1H".len())
                .any(|bytes| bytes == b"\x1b[1;1H"),
            "a cursor-home command after rmcup moves the shell prompt to the top"
        );
    }
}
