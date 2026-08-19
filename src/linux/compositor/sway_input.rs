use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};
use xkbcommon::xkb;

use super::protocols::{zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1};

const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTSHIFT: u32 = 54;
const KEY_LEFTALT: u32 = 56;
const KEY_CAPSLOCK: u32 = 58;
const KEY_NUMLOCK: u32 = 69;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_RIGHTALT: u32 = 100;
const KEY_LEFTMETA: u32 = 125;
const KEY_RIGHTMETA: u32 = 126;
const MAX_INPUT_CODE: u32 = 0x2ff;

struct InputState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for InputState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(InputState: ignore wl_seat::WlSeat);
delegate_noop!(InputState: ignore wl_output::WlOutput);
delegate_noop!(InputState: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(InputState: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);
delegate_noop!(InputState: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(InputState: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);

pub struct InputChannel {
    connection: Connection,
    queue: EventQueue<InputState>,
    state: InputState,
    keyboard: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    pointer: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    _keymap_file: File,
    width: u32,
    height: u32,
    origin: Instant,
    pressed_keys: BTreeSet<u32>,
    pressed_buttons: BTreeSet<u32>,
    modifier_bits: ModifierBits,
    locked_modifiers: u32,
    closed: bool,
}

#[derive(Clone, Copy, Default)]
struct ModifierBits {
    shift: u32,
    control: u32,
    alt: u32,
    logo: u32,
    level3: u32,
    caps: u32,
    num: u32,
}

impl InputChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        socket: &Path,
        width: u32,
        height: u32,
        model: Option<&str>,
        layout: &str,
        variant: Option<&str>,
        options: Option<String>,
    ) -> io::Result<Self> {
        if width == 0 || height == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "virtual input requires a non-empty output",
            ));
        }
        let stream = UnixStream::connect(socket)?;
        let connection = Connection::from_socket(stream).map_err(wayland_error)?;
        let (globals, queue) =
            registry_queue_init::<InputState>(&connection).map_err(wayland_error)?;
        let qh = queue.handle();

        let seat_global = first_global(&globals, wl_seat::WlSeat::interface().name)
            .ok_or_else(|| missing_global("wl_seat"))?;
        let output_global = first_global(&globals, wl_output::WlOutput::interface().name)
            .ok_or_else(|| missing_global("wl_output"))?;
        let seat: wl_seat::WlSeat = globals.registry().bind(
            seat_global.name,
            seat_global
                .version
                .min(wl_seat::WlSeat::interface().version),
            &qh,
            (),
        );
        let output: wl_output::WlOutput = globals.registry().bind(
            output_global.name,
            output_global
                .version
                .min(wl_output::WlOutput::interface().version),
            &qh,
            (),
        );
        let keyboard_manager: zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 =
            globals.bind(&qh, 1..=1, ()).map_err(wayland_error)?;
        let pointer_manager: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 =
            globals.bind(&qh, 1..=2, ()).map_err(wayland_error)?;
        let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());
        let pointer = if pointer_manager.version() >= 2 {
            pointer_manager.create_virtual_pointer_with_output(Some(&seat), Some(&output), &qh, ())
        } else {
            pointer_manager.create_virtual_pointer(Some(&seat), &qh, ())
        };

        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            model.unwrap_or("pc105"),
            layout,
            variant.unwrap_or(""),
            options,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| io::Error::other("could not compile the configured XKB keymap"))?;
        let modifier_bits = modifier_bits(&keymap)?;
        let mut keymap_text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        keymap_text.push('\0');
        let keymap_size = u32::try_from(keymap_text.len())
            .map_err(|_| io::Error::other("XKB keymap is too large"))?;
        let mut keymap_file = anonymous_file()?;
        keymap_file.write_all(keymap_text.as_bytes())?;
        keymap_file.seek(SeekFrom::Start(0))?;
        keyboard.keymap(1, keymap_file.as_fd(), keymap_size);
        keyboard.modifiers(0, 0, 0, 0);
        connection.flush().map_err(wayland_error)?;

        Ok(Self {
            connection,
            queue,
            state: InputState,
            keyboard,
            pointer,
            _keymap_file: keymap_file,
            width,
            height,
            origin: Instant::now(),
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            modifier_bits,
            locked_modifiers: 0,
            closed: false,
        })
    }

    pub fn check_status(&mut self) -> io::Result<()> {
        self.ensure_open()?;
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(wayland_error)?;
        self.connection.flush().map_err(wayland_error)
    }

    pub fn key(&mut self, code: u32, pressed: bool) -> io::Result<()> {
        self.ensure_open()?;
        if code > MAX_INPUT_CODE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "keyboard code exceeds Linux input range",
            ));
        }
        let changed = if pressed {
            self.pressed_keys.insert(code)
        } else {
            self.pressed_keys.remove(&code)
        };
        if !changed {
            return Ok(());
        }
        if pressed {
            if code == KEY_CAPSLOCK {
                self.locked_modifiers ^= self.modifier_bits.caps;
            } else if code == KEY_NUMLOCK {
                self.locked_modifiers ^= self.modifier_bits.num;
            }
        }
        self.keyboard
            .key(self.timestamp(), code, if pressed { 1 } else { 0 });
        let depressed = depressed_modifiers(&self.pressed_keys, self.modifier_bits);
        self.keyboard
            .modifiers(depressed, 0, self.locked_modifiers, 0);
        self.connection.flush().map_err(wayland_error)
    }

    /// Move the pointer, rejecting a position outside the output.
    ///
    /// This used to clamp. Clamping turns "click the button at (2000, 500)" on a 1920-wide output
    /// into a click at (1919, 500), which a caller cannot tell apart from success — so a caller
    /// that computed a coordinate wrongly gets a plausible-looking wrong click instead of an
    /// error. Weston has always rejected (`weston_input.rs`), and one answer on both compositors
    /// is worth more than the clamp ever was. Both in-tree callers already bound their
    /// coordinates before reaching here: `Placement::pointer`/`pointer_clamped` end in a `min`
    /// against the capture size, and `desktop_input::to_pixel` rejects first.
    pub fn pointer_absolute(&mut self, x: u32, y: u32) -> io::Result<()> {
        self.ensure_open()?;
        super::check_pointer_bounds(x, y, self.width, self.height, "Sway")?;
        self.pointer
            .motion_absolute(self.timestamp(), x, y, self.width, self.height);
        self.pointer.frame();
        self.connection.flush().map_err(wayland_error)
    }

    pub fn pointer_button(&mut self, button: u32, pressed: bool) -> io::Result<()> {
        self.ensure_open()?;
        if button > MAX_INPUT_CODE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pointer button exceeds Linux input range",
            ));
        }
        let changed = if pressed {
            self.pressed_buttons.insert(button)
        } else {
            self.pressed_buttons.remove(&button)
        };
        if !changed {
            return Ok(());
        }
        self.pointer.button(
            self.timestamp(),
            button,
            if pressed {
                wl_pointer::ButtonState::Pressed
            } else {
                wl_pointer::ButtonState::Released
            },
        );
        self.pointer.frame();
        self.connection.flush().map_err(wayland_error)
    }

    pub fn pointer_axis(&mut self, axis: u32, delta: i32) -> io::Result<()> {
        self.ensure_open()?;
        let Some((axis, value, discrete)) = scroll_values(axis, delta)? else {
            return Ok(());
        };
        let time = self.timestamp();
        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
        self.pointer.axis_discrete(time, axis, value, discrete);
        self.pointer.frame();
        self.connection.flush().map_err(wayland_error)
    }

    pub fn release_all(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let keys: Vec<_> = self.pressed_keys.iter().copied().collect();
        for code in keys {
            self.keyboard.key(self.timestamp(), code, 0);
        }
        let buttons: Vec<_> = self.pressed_buttons.iter().copied().collect();
        for button in buttons {
            self.pointer
                .button(self.timestamp(), button, wl_pointer::ButtonState::Released);
        }
        if !self.pressed_buttons.is_empty() {
            self.pointer.frame();
        }
        self.pressed_keys.clear();
        self.pressed_buttons.clear();
        self.locked_modifiers = 0;
        self.keyboard.modifiers(0, 0, 0, 0);
        self.connection.flush().map_err(wayland_error)
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.release_all()?;
        self.keyboard.destroy();
        self.pointer.destroy();
        self.closed = true;
        self.connection.flush().map_err(wayland_error)
    }

    fn timestamp(&self) -> u32 {
        self.origin.elapsed().as_millis() as u32
    }

    fn ensure_open(&self) -> io::Result<()> {
        if self.closed {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "virtual input channel is closed",
            ))
        } else {
            Ok(())
        }
    }
}

impl Drop for InputChannel {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn first_global(
    globals: &wayland_client::globals::GlobalList,
    interface: &str,
) -> Option<wayland_client::globals::Global> {
    globals
        .contents()
        .clone_list()
        .into_iter()
        .find(|global| global.interface == interface)
}

fn modifier_bits(keymap: &xkb::Keymap) -> io::Result<ModifierBits> {
    fn bit(keymap: &xkb::Keymap, name: &str) -> io::Result<u32> {
        let index = keymap.mod_get_index(name);
        if index == xkb::MOD_INVALID || index >= u32::BITS {
            return Err(io::Error::other(format!(
                "XKB keymap does not define modifier {name}"
            )));
        }
        Ok(1_u32 << index)
    }
    Ok(ModifierBits {
        shift: bit(keymap, xkb::MOD_NAME_SHIFT)?,
        control: bit(keymap, xkb::MOD_NAME_CTRL)?,
        alt: bit(keymap, xkb::MOD_NAME_ALT)?,
        logo: bit(keymap, xkb::MOD_NAME_LOGO)?,
        level3: bit(keymap, "Mod5").unwrap_or(0),
        caps: bit(keymap, xkb::MOD_NAME_CAPS).unwrap_or(0),
        num: bit(keymap, xkb::MOD_NAME_NUM).unwrap_or(0),
    })
}

fn depressed_modifiers(keys: &BTreeSet<u32>, bits: ModifierBits) -> u32 {
    let mut modifiers = 0;
    if keys.contains(&KEY_LEFTSHIFT) || keys.contains(&KEY_RIGHTSHIFT) {
        modifiers |= bits.shift;
    }
    if keys.contains(&KEY_LEFTCTRL) || keys.contains(&KEY_RIGHTCTRL) {
        modifiers |= bits.control;
    }
    if keys.contains(&KEY_LEFTALT) {
        modifiers |= bits.alt;
    }
    if keys.contains(&KEY_RIGHTALT) {
        modifiers |= bits.level3;
    }
    if keys.contains(&KEY_LEFTMETA) || keys.contains(&KEY_RIGHTMETA) {
        modifiers |= bits.logo;
    }
    modifiers
}

fn scroll_values(axis: u32, delta: i32) -> io::Result<Option<(wl_pointer::Axis, f64, i32)>> {
    let axis = match axis {
        0 => wl_pointer::Axis::VerticalScroll,
        1 => wl_pointer::Axis::HorizontalScroll,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pointer axis must be vertical or horizontal",
            ));
        }
    };
    let discrete = delta / 120;
    Ok((discrete != 0).then_some((axis, f64::from(-delta) / 12.0, -discrete)))
}

fn anonymous_file() -> io::Result<File> {
    let mut template = CString::new("/tmp/vvland-keymap-XXXXXX")
        .expect("static keymap temporary-file template has no NUL")
        .into_bytes_with_nul();
    // SAFETY: mkstemp receives a writable NUL-terminated XXXXXX template and returns an owned fd.
    let fd = unsafe { libc::mkstemp(template.as_mut_ptr().cast()) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the generated path stays NUL-terminated; unlink leaves the descriptor valid.
    unsafe {
        libc::unlink(template.as_ptr().cast());
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    // SAFETY: fd was returned as a new owned descriptor above.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn missing_global(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("Sway does not advertise required Wayland global {name}"),
    )
}

fn wayland_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, error.to_string())
}

/// The shared terminal and desktop-input translation drives this backend's Wayland
/// virtual devices through the common injector contract.
impl crate::producer::TerminalInjector for InputChannel {
    fn key(&mut self, code: u32, pressed: bool) -> io::Result<()> {
        self.key(code, pressed)
    }
    fn pointer_absolute(&mut self, x: u32, y: u32) -> io::Result<()> {
        self.pointer_absolute(x, y)
    }
    fn pointer_button(&mut self, button: u32, pressed: bool) -> io::Result<()> {
        self.pointer_button(button, pressed)
    }
    fn pointer_axis(&mut self, axis: u32, delta: i32) -> io::Result<()> {
        self.pointer_axis(axis, delta)
    }
    fn release_all(&mut self) -> io::Result<()> {
        self.release_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_state_is_suppressed_by_sets() {
        let mut keys = BTreeSet::new();
        assert!(keys.insert(KEY_LEFTCTRL));
        assert!(!keys.insert(KEY_LEFTCTRL));
        assert!(keys.remove(&KEY_LEFTCTRL));
        assert!(!keys.remove(&KEY_LEFTCTRL));
    }

    #[test]
    fn explicit_modifier_mask_tracks_both_sides() {
        let bits = ModifierBits {
            shift: 1,
            control: 2,
            alt: 4,
            logo: 8,
            level3: 16,
            ..ModifierBits::default()
        };
        let keys = BTreeSet::from([KEY_RIGHTSHIFT, KEY_RIGHTCTRL, KEY_RIGHTALT, KEY_LEFTMETA]);
        assert_eq!(depressed_modifiers(&keys, bits), 1 | 2 | 8 | 16);
    }

    #[test]
    fn generated_keymap_has_required_modifiers() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("US keymap");
        let bits = modifier_bits(&keymap).expect("modifier bits");
        assert_ne!(bits.shift, 0);
        assert_ne!(bits.control, 0);
        assert_ne!(bits.logo, 0);
    }

    #[test]
    fn scrolling_maps_terminal_steps_to_wayland_direction() {
        let (axis, value, discrete) = scroll_values(0, 120).unwrap().unwrap();
        assert_eq!(axis, wl_pointer::Axis::VerticalScroll);
        assert_eq!(value, -10.0);
        assert_eq!(discrete, -1);
        let (axis, value, discrete) = scroll_values(1, -120).unwrap().unwrap();
        assert_eq!(axis, wl_pointer::Axis::HorizontalScroll);
        assert_eq!(value, 10.0);
        assert_eq!(discrete, 1);
        assert!(scroll_values(0, 1).unwrap().is_none());
        assert!(scroll_values(2, 120).is_err());
    }
}
