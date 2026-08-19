use std::collections::HashSet;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Duration;

const MAGIC: u32 = 0x5653_544e;
const VERSION: u16 = 1;
const MAX_PACKET: usize = 64;
const HEADER_LEN: usize = 16;
const KEY_MAX: u32 = 0x2ff;
const BTN_MISC: u32 = 0x100;
const MAX_AXIS_STEP: i32 = 12_000;

const READY: u16 = 1;
const ERROR: u16 = 2;
const POINTER_ABSOLUTE: u16 = 3;
const POINTER_BUTTON: u16 = 4;
const POINTER_AXIS: u16 = 5;
const KEY: u16 = 6;
const RELEASE_ALL: u16 = 7;
const SHUTDOWN: u16 = 8;

pub struct InputChannel {
    fd: OwnedFd,
    width: u32,
    height: u32,
    sequence: u32,
    keys: HashSet<u32>,
    buttons: HashSet<u32>,
    closed: bool,
}

impl InputChannel {
    pub fn new(fd: OwnedFd, width: u32, height: u32) -> Self {
        Self {
            fd,
            width,
            height,
            sequence: 0,
            keys: HashSet::new(),
            buttons: HashSet::new(),
            closed: false,
        }
    }

    pub fn wait_ready(&self, timeout: Duration) -> io::Result<()> {
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut descriptor = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd for the duration of the call.
        let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Weston input module did not become ready",
            ));
        }
        let mut packet = [0_u8; MAX_PACKET];
        // SAFETY: packet is writable and the descriptor is a connected seqpacket socket.
        let count = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                packet.as_mut_ptr().cast(),
                packet.len(),
                0,
            )
        };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Weston input module disconnected before readiness",
            ));
        }
        let count = usize::try_from(count).unwrap_or(0);
        let (kind, payload) = decode_packet(&packet[..count])?;
        match kind {
            READY if payload.is_empty() => Ok(()),
            ERROR if payload.len() == 4 => Err(io::Error::from_raw_os_error(i32::from_be_bytes(
                payload.try_into().expect("checked four-byte payload"),
            ))),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected Weston input-module readiness reply",
            )),
        }
    }

    pub fn check_status(&self) -> io::Result<()> {
        let mut packet = [0_u8; MAX_PACKET];
        // SAFETY: packet is writable and MSG_DONTWAIT prevents the terminal loop from blocking.
        let count = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                packet.as_mut_ptr().cast(),
                packet.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            return Err(error);
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Weston input module disconnected",
            ));
        }
        let count = usize::try_from(count).unwrap_or(0);
        let (kind, payload) = decode_packet(&packet[..count])?;
        match kind {
            ERROR if payload.len() == 4 => Err(io::Error::from_raw_os_error(i32::from_be_bytes(
                payload.try_into().expect("checked four-byte payload"),
            ))),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected asynchronous Weston input-module reply",
            )),
        }
    }

    pub fn pointer_absolute(&mut self, x: u32, y: u32) -> io::Result<()> {
        super::check_pointer_bounds(x, y, self.width, self.height, "Weston")?;
        self.send_pair(POINTER_ABSOLUTE, x, y)
    }

    pub fn pointer_button(&mut self, code: u32, pressed: bool) -> io::Result<()> {
        if !(BTN_MISC..=KEY_MAX).contains(&code) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pointer button is outside the evdev button range",
            ));
        }
        let changes_state = if pressed {
            !self.buttons.contains(&code)
        } else {
            self.buttons.contains(&code)
        };
        if !changes_state {
            return Ok(());
        }
        self.send_pair(POINTER_BUTTON, code, u32::from(pressed))?;
        if pressed {
            self.buttons.insert(code);
        } else {
            self.buttons.remove(&code);
        }
        Ok(())
    }

    pub fn pointer_axis(&mut self, axis: u32, value_120: i32) -> io::Result<()> {
        if axis > 1 || !(-MAX_AXIS_STEP..=MAX_AXIS_STEP).contains(&value_120) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pointer axis or step is outside the input protocol range",
            ));
        }
        self.send_pair(
            POINTER_AXIS,
            axis,
            u32::from_ne_bytes(value_120.to_ne_bytes()),
        )
    }

    pub fn key(&mut self, code: u32, pressed: bool) -> io::Result<()> {
        if code == 0 || code > KEY_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "key is outside the evdev key range",
            ));
        }
        let changes_state = if pressed {
            !self.keys.contains(&code)
        } else {
            self.keys.contains(&code)
        };
        if !changes_state {
            return Ok(());
        }
        self.send_pair(KEY, code, u32::from(pressed))?;
        if pressed {
            self.keys.insert(code);
        } else {
            self.keys.remove(&code);
        }
        Ok(())
    }

    pub fn release_all(&mut self) -> io::Result<()> {
        self.send(RELEASE_ALL, &[])?;
        self.keys.clear();
        self.buttons.clear();
        Ok(())
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let _ = self.release_all();
        self.closed = true;
        self.send(SHUTDOWN, &[])
    }

    fn send_pair(&mut self, kind: u16, first: u32, second: u32) -> io::Result<()> {
        let mut payload = [0_u8; 8];
        payload[..4].copy_from_slice(&first.to_be_bytes());
        payload[4..].copy_from_slice(&second.to_be_bytes());
        self.send(kind, &payload)
    }

    fn send(&mut self, kind: u16, payload: &[u8]) -> io::Result<()> {
        if payload.len() > MAX_PACKET - HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Weston input IPC payload is too large",
            ));
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Weston input sequence exhausted"))?;
        let mut packet = Vec::with_capacity(HEADER_LEN + payload.len());
        packet.extend_from_slice(&MAGIC.to_be_bytes());
        packet.extend_from_slice(&VERSION.to_be_bytes());
        packet.extend_from_slice(&kind.to_be_bytes());
        packet.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("bounded input payload")
                .to_be_bytes(),
        );
        packet.extend_from_slice(&self.sequence.to_be_bytes());
        packet.extend_from_slice(payload);
        // SAFETY: packet is readable for its length and the fd is a connected Unix socket.
        let count = unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if usize::try_from(count).ok() != Some(packet.len()) {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short Weston input seqpacket write",
            ));
        }
        Ok(())
    }
}

impl Drop for InputChannel {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn decode_packet(packet: &[u8]) -> io::Result<(u16, &[u8])> {
    if packet.len() < HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short Weston input IPC packet",
        ));
    }
    let magic = u32::from_be_bytes(packet[0..4].try_into().expect("fixed range"));
    let version = u16::from_be_bytes(packet[4..6].try_into().expect("fixed range"));
    let kind = u16::from_be_bytes(packet[6..8].try_into().expect("fixed range"));
    let length = usize::try_from(u32::from_be_bytes(
        packet[8..12].try_into().expect("fixed range"),
    ))
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "IPC length exceeds usize"))?;
    let sequence = u32::from_be_bytes(packet[12..16].try_into().expect("fixed range"));
    if magic != MAGIC
        || version != VERSION
        || sequence != 0
        || length > MAX_PACKET - HEADER_LEN
        || packet.len() != HEADER_LEN + length
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Weston input IPC header",
        ));
    }
    Ok((kind, &packet[HEADER_LEN..]))
}

/// The shared terminal and desktop-input translation drives this backend's libweston
/// input-module IPC through the common injector contract.
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
    use std::os::fd::FromRawFd;

    #[test]
    fn readiness_decoder_rejects_bad_length_and_sequence() {
        let mut ready = Vec::new();
        ready.extend_from_slice(&MAGIC.to_be_bytes());
        ready.extend_from_slice(&VERSION.to_be_bytes());
        ready.extend_from_slice(&READY.to_be_bytes());
        ready.extend_from_slice(&0_u32.to_be_bytes());
        ready.extend_from_slice(&0_u32.to_be_bytes());
        assert_eq!(decode_packet(&ready).unwrap(), (READY, &[][..]));
        ready[15] = 1;
        assert!(decode_packet(&ready).is_err());
    }

    #[test]
    fn readiness_reports_a_disconnected_module() {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors points to exactly two writable integers.
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    descriptors.as_mut_ptr(),
                )
            },
            0
        );
        // SAFETY: socketpair returned two newly owned descriptors.
        let parent = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: socketpair returned two newly owned descriptors.
        let peer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        drop(peer);

        let channel = InputChannel::new(parent, 1, 1);
        let error = channel.wait_ready(Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(error.to_string().contains("disconnected before readiness"));
    }

    #[test]
    fn an_out_of_range_pointer_position_is_rejected_before_anything_is_sent() {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors points to exactly two writable integers.
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    descriptors.as_mut_ptr(),
                )
            },
            0
        );
        // SAFETY: socketpair returned two newly owned descriptors.
        let parent = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: socketpair returned two newly owned descriptors.
        let peer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };

        let mut channel = InputChannel::new(parent, 640, 480);
        for (x, y) in [(640, 0), (0, 480)] {
            let error = channel.pointer_absolute(x, y).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "({x}, {y})");
        }
        // The rejection must happen before the wire write, so the module never sees a bad
        // position and the sequence counter does not advance past it.
        assert_eq!(channel.sequence, 0);
        channel.pointer_absolute(639, 479).unwrap();
        assert_eq!(channel.sequence, 1);
        drop(peer);
    }
}
