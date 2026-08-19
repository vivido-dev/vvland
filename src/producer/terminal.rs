use std::io::{self, Write};
use std::time::Duration;

use vivid_sdk::{Session, Track, TrackWaitCondition};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{execute, queue};

use crate::producer::audio::AudioGain;
use crate::producer::keysynth::{
    KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT, KeyStroke, KeySynth,
};
use crate::producer::scene::Placement;

const KEY_ESC: u32 = 1;
const KEY_BACKSPACE: u32 = 14;
const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;
const KEY_F1: u32 = 59;
const KEY_F11: u32 = 87;
const KEY_F12: u32 = 88;
const KEY_HOME: u32 = 102;
const KEY_UP: u32 = 103;
const KEY_PAGEUP: u32 = 104;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_END: u32 = 107;
const KEY_DOWN: u32 = 108;
const KEY_PAGEDOWN: u32 = 109;
const KEY_INSERT: u32 = 110;
const KEY_DELETE: u32 = 111;
const BTN_LEFT: u32 = 272;
const BTN_RIGHT: u32 = 273;
const BTN_MIDDLE: u32 = 274;

fn status_help(compositor_name: &str, leader_only: bool, detach_enabled: bool) -> String {
    let detach = if detach_enabled { " d=detach" } else { "" };
    if leader_only {
        format!(
            "{compositor_name} desktop: keys forwarded | C-b:{detach} r=run m=mute +/-=volume q=quit"
        )
    } else {
        format!(
            "keys/mouse -> {compositor_name} | C-b:{detach} r=run i=type m=mute +/-=volume q=quit"
        )
    }
}

pub struct TerminalGuard {
    stdout: io::Stdout,
}

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        ) {
            let _ = execute!(stdout, DisableMouseCapture, Show, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self { stdout })
    }

    pub fn status(&mut self, message: &str) -> io::Result<()> {
        let (columns, rows) = terminal::size()?;
        let width = usize::from(columns);
        let mut text = message.chars().take(width).collect::<String>();
        let characters = text.chars().count();
        if characters < width {
            text.push_str(&" ".repeat(width - characters));
        }
        queue!(
            self.stdout,
            MoveTo(0, rows.saturating_sub(1)),
            Clear(ClearType::CurrentLine)
        )?;
        self.stdout.write_all(text.as_bytes())?;
        self.stdout.flush()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, DisableMouseCapture, Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

pub enum LocalCommand {
    Detach,
    Quit,
    Resize,
    Run(String),
}

pub struct TerminalInput {
    mapper: KeyMapper,
    leader_pending: bool,
    leader_only: bool,
    detach_enabled: bool,
    compositor_name: String,
    status: String,
}

impl TerminalInput {
    pub fn new(
        model: Option<&str>,
        layout: &str,
        variant: Option<&str>,
        options: Option<String>,
        compositor_name: &str,
    ) -> io::Result<Self> {
        Ok(Self {
            mapper: KeyMapper::new(model, layout, variant, options)?,
            leader_pending: false,
            leader_only: false,
            detach_enabled: false,
            compositor_name: compositor_name.to_owned(),
            status: status_help(compositor_name, false, false),
        })
    }

    /// Only the leader command sequence is read; ordinary keys are ignored.
    ///
    /// Used when live input is forwarded by the presenter lane (desktop mode), so
    /// locally tapping keys would double-inject with the forwarded events.
    pub fn with_leader_only(mut self) -> Self {
        self.leader_only = true;
        self.status = status_help(&self.compositor_name, true, self.detach_enabled);
        self
    }

    /// Enable the persistent-session detach command (`C-b d`).
    pub fn with_detach(mut self) -> Self {
        self.detach_enabled = true;
        self.status = status_help(&self.compositor_name, self.leader_only, true);
        self
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn poll(
        &mut self,
        timeout: Duration,
        input: &mut impl crate::producer::TerminalInjector,
        placement: Placement,
        gain: Option<&AudioGain>,
    ) -> io::Result<Option<LocalCommand>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        match event::read()? {
            Event::Resize(_, _) => Ok(Some(LocalCommand::Resize)),
            Event::FocusLost => {
                input.release_all()?;
                Ok(None)
            }
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                        if let Some((x, y)) = placement.pointer_clamped(mouse.column, mouse.row) {
                            input.pointer_absolute(x, y)?;
                        }
                    }
                    MouseEventKind::Down(button) => {
                        if let Some((x, y)) = placement.pointer(mouse.column, mouse.row) {
                            input.pointer_absolute(x, y)?;
                            input.pointer_button(mouse_button(button), true)?;
                        }
                    }
                    MouseEventKind::Up(button) => {
                        if let Some((x, y)) = placement.pointer(mouse.column, mouse.row) {
                            input.pointer_absolute(x, y)?;
                        }
                        input.pointer_button(mouse_button(button), false)?;
                    }
                    MouseEventKind::ScrollUp => input.pointer_axis(0, 120)?,
                    MouseEventKind::ScrollDown => input.pointer_axis(0, -120)?,
                    MouseEventKind::ScrollLeft => input.pointer_axis(1, 120)?,
                    MouseEventKind::ScrollRight => input.pointer_axis(1, -120)?,
                }
                Ok(None)
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if self.leader_pending {
                    self.leader_pending = false;
                    return self.leader(key, input, gain);
                }
                if is_leader(key) {
                    self.leader_pending = true;
                    self.status =
                        status_help(&self.compositor_name, self.leader_only, self.detach_enabled);
                    return Ok(None);
                }
                self.mapper.tap(input, key)?;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Poll only the leader command sequence.
    ///
    /// Ordinary keys, mouse, and focus changes are ignored; live input arrives
    /// through the presenter lane instead. The injector is not used here.
    pub fn poll_leader_only(
        &mut self,
        timeout: Duration,
        gain: Option<&AudioGain>,
    ) -> io::Result<Option<LocalCommand>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        match event::read()? {
            Event::Resize(_, _) => Ok(Some(LocalCommand::Resize)),
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if self.leader_pending {
                    self.leader_pending = false;
                    return self.leader_command(key, gain);
                }
                if is_leader(key) {
                    self.leader_pending = true;
                    self.status =
                        status_help(&self.compositor_name, self.leader_only, self.detach_enabled);
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn leader(
        &mut self,
        key: KeyEvent,
        input: &mut impl crate::producer::TerminalInjector,
        gain: Option<&AudioGain>,
    ) -> io::Result<Option<LocalCommand>> {
        self.status = status_help(&self.compositor_name, self.leader_only, self.detach_enabled);
        if is_leader(key) {
            // A literal Ctrl+B: tap it only when this terminal is the input path;
            // in leader-only mode the presenter lane already forwards the key.
            if !self.leader_only {
                let mut literal = key;
                literal.code = KeyCode::Char('b');
                self.mapper.tap(input, literal)?;
            }
            return Ok(None);
        }
        if key.code == KeyCode::Char('i') && !self.leader_only {
            if let Some(text) = prompt_line("Type")? {
                for character in text.chars() {
                    self.mapper.tap_character(input, character)?;
                }
            }
            return Ok(None);
        }
        self.leader_command(key, gain)
    }

    fn leader_command(
        &mut self,
        key: KeyEvent,
        gain: Option<&AudioGain>,
    ) -> io::Result<Option<LocalCommand>> {
        self.status = status_help(&self.compositor_name, self.leader_only, self.detach_enabled);
        match key.code {
            KeyCode::Char('d') if self.detach_enabled => Ok(Some(LocalCommand::Detach)),
            KeyCode::Char('q') => Ok(Some(LocalCommand::Quit)),
            KeyCode::Char('m') => {
                if let Some(gain) = gain {
                    let muted = gain.toggle_mute();
                    self.status = format!("audio {}", if muted { "muted" } else { "unmuted" });
                } else {
                    self.status = "audio unavailable".into();
                }
                Ok(None)
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                if let Some(gain) = gain {
                    self.status = format!("volume {:.0}%", gain.adjust(0.05) * 100.0);
                }
                Ok(None)
            }
            KeyCode::Char('-') => {
                if let Some(gain) = gain {
                    self.status = format!("volume {:.0}%", gain.adjust(-0.05) * 100.0);
                }
                Ok(None)
            }
            KeyCode::Char('r') => Ok(prompt_line("Run")?.map(LocalCommand::Run)),
            _ => Ok(None),
        }
    }
}

/// The crossterm adapter over [`KeySynth`].
///
/// Everything layout-aware lives in [`crate::producer::keysynth`]; this type exists only to turn a
/// `crossterm::KeyEvent` into a [`KeyStroke`], so the shared synthesis code never sees crossterm.
struct KeyMapper {
    synth: KeySynth,
}

impl KeyMapper {
    fn new(
        model: Option<&str>,
        layout: &str,
        variant: Option<&str>,
        options: Option<String>,
    ) -> io::Result<Self> {
        Ok(Self {
            synth: KeySynth::new(model, layout, variant, options)?,
        })
    }

    fn tap(
        &self,
        input: &mut impl crate::producer::TerminalInjector,
        event: KeyEvent,
    ) -> io::Result<()> {
        let stroke = match event.code {
            KeyCode::Char(character) => self.synth.character(character),
            code => special_key(code).map(KeyStroke::plain),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "key is not in the XKB map"))?;
        self.synth.tap(
            input,
            &stroke.with_modifiers(event_modifiers(event.modifiers)),
        )
    }

    fn tap_character(
        &self,
        input: &mut impl crate::producer::TerminalInjector,
        character: char,
    ) -> io::Result<()> {
        self.tap(
            input,
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
        )
    }
}

fn event_modifiers(event: KeyModifiers) -> Vec<u32> {
    let mut modifiers = Vec::new();
    if event.contains(KeyModifiers::SHIFT) {
        modifiers.push(KEY_LEFTSHIFT);
    }
    if event.contains(KeyModifiers::CONTROL) {
        modifiers.push(KEY_LEFTCTRL);
    }
    if event.contains(KeyModifiers::ALT) {
        modifiers.push(KEY_LEFTALT);
    }
    if event.contains(KeyModifiers::SUPER) {
        modifiers.push(KEY_LEFTMETA);
    }
    modifiers
}

fn special_key(code: KeyCode) -> Option<u32> {
    Some(match code {
        KeyCode::Backspace => KEY_BACKSPACE,
        KeyCode::Enter => KEY_ENTER,
        KeyCode::Left => KEY_LEFT,
        KeyCode::Right => KEY_RIGHT,
        KeyCode::Up => KEY_UP,
        KeyCode::Down => KEY_DOWN,
        KeyCode::Home => KEY_HOME,
        KeyCode::End => KEY_END,
        KeyCode::PageUp => KEY_PAGEUP,
        KeyCode::PageDown => KEY_PAGEDOWN,
        KeyCode::Tab => KEY_TAB,
        KeyCode::BackTab => KEY_TAB,
        KeyCode::Delete => KEY_DELETE,
        KeyCode::Insert => KEY_INSERT,
        KeyCode::Esc => KEY_ESC,
        KeyCode::F(number @ 1..=10) => KEY_F1 + u32::from(number - 1),
        KeyCode::F(11) => KEY_F11,
        KeyCode::F(12) => KEY_F12,
        _ => return None,
    })
}

fn mouse_button(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
    }
}

/// How long each `WAIT_TRACK` chunk runs while polling the leader terminal.
const READINESS_CHUNK_US: u64 = 200_000;

/// Wait (chunked and interruptible) for a current-generation milestone, polling
/// the leader terminal so Ctrl+B q aborts the establishment immediately instead
/// of leaving the user staring at a dead screen for the full wait.
///
/// The presenter answers each chunk with `ERROR_TIMEOUT` ("track wait timed
/// out") until the milestone arrives; that reply means keep waiting, not
/// failure. Returns `Ok(true)` when the milestone is reached and `Ok(false)`
/// when the user quit. `status` is rendered between polls so the user sees the
/// session is alive.
pub fn wait_for_milestone(
    session: &mut Session,
    track: &Track,
    milestone: u64,
    status: &str,
    leader: &mut TerminalInput,
    terminal: &mut TerminalGuard,
    gain: Option<&AudioGain>,
) -> io::Result<bool> {
    loop {
        match session.wait_track(
            track,
            TrackWaitCondition::MilestoneSet,
            Some(milestone),
            READINESS_CHUNK_US,
        ) {
            Ok(satisfied) if satisfied.observed_value.is_some() => return Ok(true),
            // A presenter that never reports the observed milestone is broken.
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "WAIT_SATISFIED reported no observed milestone",
                ));
            }
            Err(error) if is_readiness_timeout(&error) => {}
            Err(error) => return Err(error),
        }
        if matches!(
            leader.poll_leader_only(Duration::from_millis(50), gain)?,
            Some(LocalCommand::Quit | LocalCommand::Detach)
        ) {
            return Ok(false);
        }
        terminal.status(status)?;
    }
}

/// The presenter's deadline reply for a not-yet-satisfied `WAIT_TRACK`. The
/// diagnostic is display-only, so matching it for control is safe.
fn is_readiness_timeout(error: &io::Error) -> bool {
    error.to_string().contains("track wait timed out")
}

fn is_leader(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('b') | KeyCode::Char('B'))
}

fn prompt_line(label: &str) -> io::Result<Option<String>> {
    let mut value = String::new();
    let mut stdout = io::stdout();
    loop {
        let (columns, rows) = terminal::size()?;
        queue!(
            stdout,
            MoveTo(0, rows.saturating_sub(1)),
            Clear(ClearType::CurrentLine)
        )?;
        write!(
            stdout,
            "{label}: {}",
            value.chars().take(columns as usize).collect::<String>()
        )?;
        stdout.flush()?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Enter => return Ok(Some(value)),
                KeyCode::Esc => return Ok(None),
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    value.push(character);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_keys_use_linux_evdev_codes() {
        assert_eq!(special_key(KeyCode::Enter), Some(KEY_ENTER));
        assert_eq!(special_key(KeyCode::F(12)), Some(KEY_F12));
        assert_eq!(special_key(KeyCode::Null), None);
    }
}
