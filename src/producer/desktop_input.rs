use std::io;

use vivid_protocol::input::{InputEvent, InputTuple};
use vivid_protocol::revision::SurfaceGeneration;
use vivid_protocol::time::Monotonic;
use vivid_sdk::{DesktopPreconditions, InputBindingGuard, InputLane, InputLaneEvent};

use crate::producer::TerminalInjector;

/// The owner check `apply` performs as the final step before the OS injection API.
///
/// The complete 1.5 final injection gate — surface generation, producer epoch, presenter grant
/// generation, watchdog, and class — is enforced by [`InputRuntime::drain`] immediately before
/// this translation runs; the tuple check here is the last owner validation inside it.
fn validate_binding(binding: InputTuple, context_id: u64, surface_id: u64) -> io::Result<()> {
    if binding.context_id != context_id || binding.surface_id != surface_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "desktop input targets a different surface",
        ));
    }
    Ok(())
}

/// Convert a canonical 32.32 surface unit to a pixel, strictly inside the streamed output.
fn to_pixel(unit: u64, limit: u32) -> io::Result<u32> {
    let pixel = unit >> 32;
    let pixel = u32::try_from(pixel).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "desktop pointer coordinate is outside the streamed output",
        )
    })?;
    if pixel >= limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "desktop pointer coordinate is outside the streamed output",
        ));
    }
    Ok(pixel)
}

pub fn apply(
    event: InputEvent,
    input: &mut impl TerminalInjector,
    context_id: u64,
    surface_id: u64,
    dimensions: (u32, u32),
) -> io::Result<()> {
    match event {
        InputEvent::Key {
            binding,
            usage,
            pressed,
        } => {
            validate_binding(binding, context_id, surface_id)?;
            if let Some(code) = hid_to_evdev(usage) {
                input.key(code, pressed)?;
            }
        }
        InputEvent::PointerMotion { binding, x, y } => {
            validate_binding(binding, context_id, surface_id)?;
            let x = to_pixel(x, dimensions.0)?;
            let y = to_pixel(y, dimensions.1)?;
            input.pointer_absolute(x, y)?;
        }
        InputEvent::PointerButton {
            binding,
            button,
            pressed,
            ..
        } => {
            validate_binding(binding, context_id, surface_id)?;
            input.pointer_button(button_to_evdev(button), pressed)?;
        }
        InputEvent::PointerAxis {
            binding,
            horizontal,
            vertical,
            ..
        } => {
            validate_binding(binding, context_id, surface_id)?;
            if vertical != 0 {
                input.pointer_axis(
                    0,
                    i32::try_from(vertical).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "pointer axis delta overflow")
                    })?,
                )?;
            }
            if horizontal != 0 {
                input.pointer_axis(
                    1,
                    i32::try_from(horizontal).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "pointer axis delta overflow")
                    })?,
                )?;
            }
        }
    }
    Ok(())
}

fn button_to_evdev(button: u8) -> u32 {
    [272, 274, 273, 275, 276][usize::from(button)]
}

fn hid_to_evdev(usage: u16) -> Option<u32> {
    Some(match usage {
        0x04..=0x1d => {
            const LETTERS: [u32; 26] = [
                30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22,
                47, 17, 45, 21, 44,
            ];
            LETTERS[usize::from(usage - 0x04)]
        }
        0x1e..=0x27 => u32::from(usage - 0x1e) + 2,
        0x28 => 28,
        0x29 => 1,
        0x2a => 14,
        0x2b => 15,
        0x2c => 57,
        0x2d => 12,
        0x2e => 13,
        0x2f => 26,
        0x30 => 27,
        0x31 => 43,
        0x32 | 0x64 => 86,
        0x33 => 39,
        0x34 => 40,
        0x35 => 41,
        0x36 => 51,
        0x37 => 52,
        0x38 => 53,
        0x39 => 58,
        0x3a..=0x43 => u32::from(usage - 0x3a) + 59,
        0x44 => 87,
        0x45 => 88,
        0x46 => 99,
        0x47 => 70,
        0x48 => 119,
        0x49 => 110,
        0x4a => 102,
        0x4b => 104,
        0x4c => 111,
        0x4d => 107,
        0x4e => 109,
        0x4f => 106,
        0x50 => 105,
        0x51 => 108,
        0x52 => 103,
        0x53 => 69,
        0x54 => 98,
        0x55 => 55,
        0x56 => 74,
        0x57 => 78,
        0x58 => 96,
        0x59 => 79,
        0x5a => 80,
        0x5b => 81,
        0x5c => 75,
        0x5d => 76,
        0x5e => 77,
        0x5f => 71,
        0x60 => 72,
        0x61 => 73,
        0x62 => 82,
        0x63 => 83,
        0x65 => 127,
        0x66 => 116,
        0x67 => 117,
        0x68..=0x73 => u32::from(usage - 0x68) + 183,
        0x74 => 194,
        0x7f => 113,
        0x80 => 115,
        0x81 => 114,
        0xe0 => 29,
        0xe1 => 42,
        0xe2 => 56,
        0xe3 => 125,
        0xe4 => 97,
        0xe5 => 54,
        0xe6 => 100,
        0xe7 => 126,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuple(context_id: u64, surface_id: u64) -> InputTuple {
        use vivid_protocol::revision::{GrantGeneration, InputEpoch, SurfaceGeneration};
        InputTuple {
            producer_epoch: InputEpoch::new(1),
            grant_generation: GrantGeneration::new(1),
            context_id,
            surface_id,
            surface_generation: SurfaceGeneration::ONE,
        }
    }

    #[test]
    fn maps_browser_hid_keys_and_buttons_to_linux_input() {
        assert_eq!(hid_to_evdev(0x04), Some(30));
        assert_eq!(hid_to_evdev(0x1d), Some(44));
        assert_eq!(hid_to_evdev(0xe4), Some(97));
        assert_eq!(hid_to_evdev(0x75), None);
        assert_eq!(button_to_evdev(0), 272);
        assert_eq!(button_to_evdev(2), 273);
    }

    #[test]
    fn canonical_pointer_units_map_to_pixels_and_reject_outside() {
        assert_eq!(to_pixel(1919_u64 << 32, 1920).unwrap(), 1919);
        assert_eq!(to_pixel(0, 1920).unwrap(), 0);
        assert!(to_pixel(1920_u64 << 32, 1920).is_err());
        assert!(to_pixel(0, 1920).is_ok());
        assert!(to_pixel(1919_u64 << 32, 1920).is_ok());
    }

    #[test]
    fn owner_tuple_must_match_the_surface() {
        assert!(validate_binding(tuple(1, 7), 1, 7).is_ok());
        assert!(validate_binding(tuple(2, 7), 1, 7).is_err());
        assert!(validate_binding(tuple(1, 8), 1, 7).is_err());
    }
}

/// The 1.5 input conformance list (desktop §10), producer side, against the Linux injector
/// contract. These run in both producer trees because the module is byte-identical.
#[cfg(test)]
mod input_conformance {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use vivid_protocol::input::{
        INPUT_CLASS_KEYBOARD, INPUT_CLASS_POINTER_AXIS, INPUT_CLASS_POINTER_BUTTON,
        INPUT_CLASS_POINTER_MOTION,
    };
    use vivid_protocol::revision::{GrantGeneration, InputEpoch};
    use vivid_sdk::{InputGrantTermination, InputLeaseRenewal, ProducerConfig, Session};

    const MASK: u64 = INPUT_CLASS_KEYBOARD
        | INPUT_CLASS_POINTER_MOTION
        | INPUT_CLASS_POINTER_BUTTON
        | INPUT_CLASS_POINTER_AXIS;

    /// An offline interactive lane: `SET_INPUT_BINDING` answers synchronously with the
    /// requested grant, so the whole binding lifecycle is testable without a socket.
    /// The session is returned alongside so the lane's lifecycle stays alive.
    fn lane() -> (Session, InputLane) {
        let session = Session::connect(ProducerConfig::offline_desktop()).unwrap();
        let lane = session.open_input_lane(1).unwrap();
        (session, lane)
    }

    fn ready_runtime() -> InputRuntime {
        let mut runtime = InputRuntime::new(1, 7, MASK, DEFAULT_WATCHDOG_US);
        runtime.set_presented(true);
        runtime.set_lane_live(true);
        runtime.set_surface_state(SurfaceGeneration::ONE, MASK, &mut || {});
        runtime
    }

    fn key_event(epoch: u64, grant: u64, context: u64, surface: u64) -> InputEvent {
        InputEvent::Key {
            binding: InputTuple {
                producer_epoch: InputEpoch::new(epoch),
                grant_generation: GrantGeneration::new(grant),
                context_id: context,
                surface_id: surface,
                surface_generation: SurfaceGeneration::ONE,
            },
            usage: 4,
            pressed: true,
        }
    }

    fn motion_event(epoch: u64, grant: u64, context: u64, surface: u64) -> InputEvent {
        InputEvent::PointerMotion {
            binding: InputTuple {
                producer_epoch: InputEpoch::new(epoch),
                grant_generation: GrantGeneration::new(grant),
                context_id: context,
                surface_id: surface,
                surface_generation: SurfaceGeneration::ONE,
            },
            x: 100_u64 << 32,
            y: 100_u64 << 32,
        }
    }

    #[test]
    fn enable_requires_presentation_and_activates_after_input_bound() {
        let (_session, lane) = lane();
        let mut runtime = InputRuntime::new(1, 7, MASK, DEFAULT_WATCHDOG_US);
        runtime.set_lane_live(true);
        runtime.set_surface_state(SurfaceGeneration::ONE, MASK, &mut || {});
        // Not presented yet: enable is refused without advancing the epoch.
        runtime.enable_if_ready(&lane).unwrap();
        assert!(!runtime.is_active(Monotonic::ZERO));
        assert_eq!(runtime.guard().epoch(), 0);
        runtime.set_presented(true);
        runtime.enable_if_ready(&lane).unwrap();
        assert!(runtime.is_active(Monotonic::ZERO));
        assert_eq!(runtime.guard().epoch(), 1);
    }

    #[test]
    fn stale_event_through_the_queue_is_rejected_after_grant_change() {
        let (_session, lane) = lane();
        let mut runtime = ready_runtime();
        runtime.enable_if_ready(&lane).unwrap();
        runtime.disable(REASON_ORDINARY_POLICY, &lane, &mut || {});
        runtime.enable_if_ready(&lane).unwrap();
        assert_eq!(runtime.guard().epoch(), 3);
        // The old grant's tuple must not inject after the grant changed.
        runtime.push(key_event(1, 1, 1, 7));
        let mut injected = 0;
        runtime
            .drain(Monotonic::ZERO, |_| {
                injected += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(injected, 0);
        assert_eq!(runtime.rejections(), 1);
        // The current grant's tuple injects.
        runtime.push(key_event(3, 3, 1, 7));
        runtime
            .drain(Monotonic::ZERO, |_| {
                injected += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(injected, 1);
    }

    #[test]
    fn revocation_releases_held_state_once_and_never_restores() {
        let (_session, lane) = lane();
        let mut runtime = ready_runtime();
        runtime.enable_if_ready(&lane).unwrap();
        let mut releases = 0;
        let termination = InputGrantTermination {
            binding: runtime.guard().current_tag().unwrap(),
            reason: 1,
        };
        runtime
            .observe(
                InputLaneEvent::Revoked(termination),
                Monotonic::ZERO,
                &mut || releases += 1,
            )
            .unwrap();
        assert_eq!(releases, 1);
        // No automatic reinstatement: a later enable attempt is refused.
        runtime.enable_if_ready(&lane).unwrap();
        assert!(!runtime.is_active(Monotonic::ZERO));
        // A second loss does not release again.
        runtime
            .observe(
                InputLaneEvent::Reset(termination),
                Monotonic::ZERO,
                &mut || releases += 1,
            )
            .unwrap();
        assert_eq!(releases, 1);
    }

    #[test]
    fn lane_loss_releases_and_leaves_the_stream_view_only() {
        let (_session, lane) = lane();
        let mut runtime = ready_runtime();
        runtime.enable_if_ready(&lane).unwrap();
        let mut releases = 0;
        let result = runtime.observe(
            InputLaneEvent::LaneClosed {
                diagnostic: "test".into(),
            },
            Monotonic::ZERO,
            &mut || releases += 1,
        );
        assert!(result.is_err());
        assert_eq!(releases, 1);
        runtime.enable_if_ready(&lane).unwrap();
        assert!(!runtime.is_active(Monotonic::ZERO));
    }

    #[test]
    fn watchdog_renews_before_expiry_and_expiry_releases_once() {
        let (_session, lane) = lane();
        let mut runtime = ready_runtime();
        runtime.enable_if_ready(&lane).unwrap();
        let tag = runtime.guard().current_tag().unwrap();
        let mut releases = 0;
        runtime
            .observe(
                InputLaneEvent::Renew(InputLeaseRenewal {
                    binding: tag,
                    renewal_sequence: 1,
                    watchdog_timeout_us: DEFAULT_WATCHDOG_US,
                }),
                Monotonic::ZERO,
                &mut || releases += 1,
            )
            .unwrap();
        assert!(!runtime.watchdog_expired(Monotonic::from_micros(DEFAULT_WATCHDOG_US - 1)));
        assert!(runtime.watchdog_expired(Monotonic::from_micros(DEFAULT_WATCHDOG_US)));
        runtime.on_watchdog_expiry(&mut || releases += 1);
        assert_eq!(releases, 1);
        assert!(!runtime.is_active(Monotonic::from_micros(DEFAULT_WATCHDOG_US)));
        // No automatic reinstatement after expiry.
        runtime.enable_if_ready(&lane).unwrap();
        assert!(!runtime.is_active(Monotonic::from_micros(DEFAULT_WATCHDOG_US)));
    }

    #[test]
    fn late_renewal_under_reordered_delivery_never_re_arms() {
        let (_session, lane) = lane();
        let mut runtime = ready_runtime();
        runtime.enable_if_ready(&lane).unwrap();
        let tag = runtime.guard().current_tag().unwrap();
        let mut releases = 0;
        let mut release = || releases += 1;
        runtime
            .observe(
                InputLaneEvent::Renew(InputLeaseRenewal {
                    binding: tag,
                    renewal_sequence: 1,
                    watchdog_timeout_us: DEFAULT_WATCHDOG_US,
                }),
                Monotonic::ZERO,
                &mut release,
            )
            .unwrap();
        // An older renewal arriving late must not re-arm the watchdog.
        assert!(
            runtime
                .observe(
                    InputLaneEvent::Renew(InputLeaseRenewal {
                        binding: tag,
                        renewal_sequence: 1,
                        watchdog_timeout_us: DEFAULT_WATCHDOG_US,
                    }),
                    Monotonic::from_micros(DEFAULT_WATCHDOG_US / 2),
                    &mut release,
                )
                .is_err()
        );
        // After expiry, even a fresh renewal is rejected and the grant stays dead.
        runtime.on_watchdog_expiry(&mut release);
        assert!(
            runtime
                .observe(
                    InputLaneEvent::Renew(InputLeaseRenewal {
                        binding: tag,
                        renewal_sequence: 2,
                        watchdog_timeout_us: DEFAULT_WATCHDOG_US,
                    }),
                    Monotonic::from_micros(DEFAULT_WATCHDOG_US),
                    &mut release,
                )
                .is_err()
        );
        assert!(!runtime.is_active(Monotonic::from_micros(DEFAULT_WATCHDOG_US)));
    }

    #[test]
    fn queue_overflow_disables_and_releases_held_state_once() {
        let (_session, lane) = lane();
        let mut runtime = ready_runtime();
        runtime.enable_if_ready(&lane).unwrap();
        let mut releases = 0;
        for _ in 0..INPUT_QUEUE_CAPACITY + 1 {
            runtime.push(key_event(1, 1, 1, 7));
        }
        assert!(runtime.overflowed());
        runtime.release_overflow(&mut || releases += 1);
        let mut injected = 0;
        runtime
            .drain(Monotonic::ZERO, |_| {
                injected += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(releases, 1);
        assert_eq!(injected, 0);
        // The grant died with the overflow; nothing else injects.
        assert!(!runtime.is_active(Monotonic::ZERO));
        assert_eq!(runtime.rejections(), INPUT_QUEUE_CAPACITY as u64);
    }

    #[test]
    fn surface_generation_change_releases_and_re_enables_with_greater_epoch() {
        let (_session, lane) = lane();
        let mut runtime = ready_runtime();
        runtime.enable_if_ready(&lane).unwrap();
        assert_eq!(runtime.guard().epoch(), 1);
        let mut releases = 0;
        runtime.set_surface_state(SurfaceGeneration::new(2), MASK, &mut || releases += 1);
        assert_eq!(releases, 1);
        assert!(!runtime.is_active(Monotonic::ZERO));
        // Flow (b): one fresh enable names the new generation with a strictly greater epoch.
        runtime.enable_if_ready(&lane).unwrap();
        assert!(runtime.is_active(Monotonic::ZERO));
        assert_eq!(runtime.guard().epoch(), 2);
        // Events bound to the old generation are stale.
        runtime.push(key_event(1, 1, 1, 7));
        let mut injected = 0;
        runtime
            .drain(Monotonic::ZERO, |_| {
                injected += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(injected, 0);
        assert_eq!(runtime.rejections(), 1);
    }

    #[test]
    fn reused_ids_under_a_second_context_never_cross_inject() {
        let (_session, lane) = lane();
        let mut runtime = ready_runtime();
        runtime.enable_if_ready(&lane).unwrap();
        // A second context reuses the same numeric surface and epochs; its events must be
        // rejected by the final gate, not injected into this surface.
        runtime.push(key_event(1, 1, 2, 7));
        let mut injected = 0;
        runtime
            .drain(Monotonic::ZERO, |_| {
                injected += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(injected, 0);
        assert_eq!(runtime.rejections(), 1);
    }

    #[test]
    fn ungranted_event_class_is_rejected() {
        let (_session, lane) = lane();
        let mut runtime = InputRuntime::new(1, 7, INPUT_CLASS_KEYBOARD, DEFAULT_WATCHDOG_US);
        runtime.set_presented(true);
        runtime.set_lane_live(true);
        runtime.set_surface_state(SurfaceGeneration::ONE, INPUT_CLASS_KEYBOARD, &mut || {});
        runtime.enable_if_ready(&lane).unwrap();
        // The presenter narrowed the grant to keyboard only; motion is not granted.
        runtime.push(motion_event(1, 1, 1, 7));
        let mut injected = 0;
        runtime
            .drain(Monotonic::ZERO, |_| {
                injected += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(injected, 0);
        assert_eq!(runtime.rejections(), 1);
        runtime.push(key_event(1, 1, 1, 7));
        runtime
            .drain(Monotonic::ZERO, |_| {
                injected += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(injected, 1);
    }

    #[test]
    fn blocked_injector_is_never_overtaken_by_release() {
        let (_session, lane) = lane();
        let mut runtime = ready_runtime();
        runtime.enable_if_ready(&lane).unwrap();
        let log = Arc::new(StdMutex::new(Vec::<&str>::new()));
        let record = |log: &Arc<StdMutex<Vec<&str>>>, entry: &'static str| {
            log.lock().unwrap().push(entry);
        };
        runtime.push(key_event(1, 1, 1, 7));
        runtime.push(key_event(1, 1, 1, 7));
        runtime
            .drain(Monotonic::ZERO, |_| {
                record(&log, "inject");
                Ok(())
            })
            .unwrap();
        // Revocation while the injector is mid-stream: the release completes once and no
        // further injection interleaves with it.
        let termination = InputGrantTermination {
            binding: runtime.guard().current_tag().unwrap(),
            reason: 1,
        };
        runtime
            .observe(
                InputLaneEvent::Revoked(termination),
                Monotonic::ZERO,
                &mut || record(&log, "release"),
            )
            .unwrap();
        runtime.push(key_event(1, 1, 1, 7));
        runtime
            .drain(Monotonic::ZERO, |_| {
                record(&log, "inject");
                Ok(())
            })
            .unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.as_slice(), &["inject", "inject", "release"][..]);
        assert_eq!(runtime.rejections(), 1);
    }
}

/// Registered `SET_INPUT_BINDING` transition reasons (desktop §5.1).
#[cfg_attr(not(test), allow(dead_code))]
pub const REASON_ORDINARY_POLICY: u64 = 0;
pub const REASON_OS_SESSION_TRANSITION: u64 = 3;
pub const REASON_SHUTDOWN: u64 = 5;
pub const REASON_INITIAL_ENABLE: u64 = 6;

/// The producer's requested watchdog in microseconds, clamped by the presenter's effective grant.
pub const DEFAULT_WATCHDOG_US: u64 = 2_000_000;

/// The bounded ordinary-input queue capacity, matching the interactive contract's event rate.
const INPUT_QUEUE_CAPACITY: usize = 1000;

/// Why the final injection gate rejected one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRejection {
    NoActiveGrant,
    StaleTuple,
    SurfaceGenerationChanged,
    WatchdogExpired,
    ClassNotGranted,
}

/// The producer-side 1.5 input model (desktop §4–§8).
///
/// The runtime owns the binding lifecycle ([`InputBindingGuard`]), the bounded ordinary-event
/// queue, the enable/disable transitions, the watchdog, and the **final injection gate** that
/// re-verifies the complete tuple, surface generation, watchdog deadline, and event class
/// immediately before the OS injection API. Injection and release run through this type on the
/// session loop thread, so a release can never overtake an in-flight injection.
///
/// No loss path auto-restores a grant: after revocation, reset, lane loss, watchdog expiry, or a
/// surface-generation change, re-enabling requires a fresh producer epoch (flow (b) advances the
/// epoch automatically on a generation change; everything else stays disabled for the session).
pub struct InputRuntime {
    guard: InputBindingGuard,
    events: std::collections::VecDeque<InputEvent>,
    context_id: u64,
    surface_id: u64,
    surface_generation: vivid_protocol::revision::SurfaceGeneration,
    capability_mask: u64,
    watchdog_us: u64,
    presented: bool,
    lane_live: bool,
    enable_attempted: bool,
    enable_reason: u64,
    released_epoch: u64,
    overflowed: bool,
    rejections: u64,
}

impl InputRuntime {
    pub fn new(context_id: u64, surface_id: u64, capability_mask: u64, watchdog_us: u64) -> Self {
        Self {
            guard: InputBindingGuard::new(),
            events: std::collections::VecDeque::new(),
            context_id,
            surface_id,
            surface_generation: vivid_protocol::revision::SurfaceGeneration::ZERO,
            capability_mask,
            watchdog_us,
            presented: false,
            lane_live: false,
            enable_attempted: false,
            enable_reason: REASON_INITIAL_ENABLE,
            released_epoch: 0,
            overflowed: false,
            rejections: 0,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn guard(&self) -> &InputBindingGuard {
        &self.guard
    }

    /// Stale events discarded at the final gate, counted per desktop §8.
    pub fn rejections(&self) -> u64 {
        self.rejections
    }

    /// Whether an unexpired grant is currently effective.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_active(&self, now: Monotonic) -> bool {
        self.guard.is_armed(now)
    }

    pub fn set_presented(&mut self, presented: bool) {
        self.presented = presented;
    }

    pub fn set_lane_live(&mut self, lane_live: bool) {
        self.lane_live = lane_live;
    }

    /// Track the current surface state. A surface-generation change releases held input state,
    /// disables the grant, and permits one fresh enable with a strictly greater producer epoch
    /// (flow (b)).
    pub fn set_surface_state(
        &mut self,
        generation: SurfaceGeneration,
        capability_mask: u64,
        release: &mut dyn FnMut(),
    ) {
        self.capability_mask = capability_mask;
        if generation != self.surface_generation {
            self.surface_generation = generation;
            self.release_once(release);
            self.guard.release();
            // A new surface generation is the one automatic re-enable path (desktop §5.5(b)),
            // named as an OS session transition with a strictly greater producer epoch.
            self.enable_attempted = false;
            self.enable_reason = REASON_OS_SESSION_TRANSITION;
        }
    }

    /// Queue one decoded ordinary input event; the queue is bounded and overflow releases held
    /// state once (desktop §10).
    pub fn push(&mut self, event: InputEvent) {
        if self.events.len() >= INPUT_QUEUE_CAPACITY {
            self.overflowed = true;
            return;
        }
        self.events.push_back(event);
    }

    /// Observe one non-input lane event: renewal, revocation, reset, or lane loss.
    pub fn observe(
        &mut self,
        event: InputLaneEvent,
        now: Monotonic,
        release: &mut dyn FnMut(),
    ) -> io::Result<()> {
        match event {
            InputLaneEvent::Renew(renewal) => {
                self.guard.handle_renewal(&renewal, now)?;
            }
            InputLaneEvent::Revoked(_) | InputLaneEvent::Reset(_) => {
                // Focus, consent, or policy loss: release held state exactly once and never
                // auto-restore. A strictly greater producer epoch is required to re-enable.
                self.release_once(release);
                self.guard.release();
            }
            InputLaneEvent::LaneClosed { diagnostic } => {
                self.lane_live = false;
                self.release_once(release);
                self.guard.release();
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    format!("Vivid interactive lane closed: {diagnostic}"),
                ));
            }
            InputLaneEvent::Error(error) => {
                return Err(io::Error::other(format!(
                    "Vivid interactive lane error: {error}"
                )));
            }
            InputLaneEvent::Input { .. } => {}
        }
        Ok(())
    }

    /// Enable input once the preconditions hold and no enable has been attempted for this
    /// surface generation. The presenter's `INPUT_BOUND` decides the effective grant.
    pub fn enable_if_ready(&mut self, lane: &InputLane) -> io::Result<()> {
        if self.enable_attempted || self.guard.grant().is_some() {
            return Ok(());
        }
        self.guard.set_preconditions(DesktopPreconditions {
            surface_present: true,
            surface_generation: self.surface_generation,
            capability_mask: self.capability_mask,
            presented: self.presented,
            lane_live: self.lane_live,
        });
        let mut binding = match self.guard.enable(
            self.surface_id,
            self.surface_generation,
            self.capability_mask,
            self.watchdog_us,
            self.enable_reason,
        ) {
            Ok(binding) => binding,
            // Preconditions not yet met (for example not presented): retry on a later
            // iteration without advancing the epoch.
            Err(_) => return Ok(()),
        };
        // The guard tracks the owner tuple; the context id is filled in here, where the
        // session's root context is known, before the binding goes on the wire.
        binding.context_id = self.context_id;
        let status = lane.set_binding(&binding)?;
        self.guard.handle_bound(&status)?;
        // Whether granted or denied, one attempt per surface generation; a denial is not
        // retried automatically.
        self.enable_attempted = true;
        Ok(())
    }

    /// Producer-initiated disable (for example shutdown): release, send the disable binding.
    ///
    /// Unlike a loss path, an explicit disable permits one fresh enable later with a strictly
    /// greater producer epoch; loss paths never auto-restore.
    pub fn disable(&mut self, reason: u64, lane: &InputLane, release: &mut dyn FnMut()) {
        self.release_once(release);
        let binding = self.guard.disable(reason);
        self.enable_attempted = false;
        if let Ok(status) = lane.set_binding(&binding) {
            let _ = self.guard.handle_bound(&status);
        }
    }

    pub fn watchdog_expired(&self, now: Monotonic) -> bool {
        self.guard.is_expired(now)
    }

    /// Watchdog expiry: release held state once and disable; no automatic reinstatement.
    pub fn on_watchdog_expiry(&mut self, release: &mut dyn FnMut()) {
        if self.guard.grant().is_some() {
            self.release_once(release);
            self.guard.release();
        }
    }

    /// Whether the bounded queue overflowed since the last drain.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Apply the queue-overflow disable trigger (desktop §5.6): release held state once and
    /// discard every queued event for the dead grant; re-enabling needs a fresh producer epoch.
    pub fn release_overflow(&mut self, release: &mut dyn FnMut()) {
        if self.overflowed {
            self.overflowed = false;
            self.release_once(release);
            self.guard.release();
        }
    }

    /// Drain the ordinary-event queue through the final injection gate.
    ///
    /// The gate re-verifies the complete owner tuple, the current surface generation, the
    /// watchdog deadline, and the granted event class immediately before the OS injection call,
    /// and runs on the same thread as every release, so a blocked injector is never overtaken
    /// by a target switch or release.
    pub fn drain(
        &mut self,
        now: Monotonic,
        mut inject: impl FnMut(InputEvent) -> io::Result<()>,
    ) -> io::Result<()> {
        while let Some(event) = self.events.pop_front() {
            match self.authorize(&event, now) {
                Ok(()) => inject(event)?,
                Err(rejection) => {
                    self.rejections = self.rejections.saturating_add(1);
                    let _ = rejection;
                }
            }
        }
        Ok(())
    }

    fn authorize(&self, event: &InputEvent, now: Monotonic) -> Result<(), GateRejection> {
        let tag = self
            .guard
            .current_tag()
            .ok_or(GateRejection::NoActiveGrant)?;
        let binding = event.binding();
        if binding.producer_epoch != tag.producer_epoch
            || binding.grant_generation != tag.grant_generation
            || binding.context_id != self.context_id
            || binding.surface_id != tag.surface_id
            || binding.surface_generation != tag.surface_generation
        {
            return Err(GateRejection::StaleTuple);
        }
        if binding.surface_generation != self.surface_generation {
            return Err(GateRejection::SurfaceGenerationChanged);
        }
        if self.guard.is_expired(now) {
            return Err(GateRejection::WatchdogExpired);
        }
        let grant = self.guard.grant().ok_or(GateRejection::NoActiveGrant)?;
        if grant.effective_classes & event.class() == 0 {
            return Err(GateRejection::ClassNotGranted);
        }
        Ok(())
    }

    fn release_once(&mut self, release: &mut dyn FnMut()) {
        if self.released_epoch != self.guard.epoch() {
            self.released_epoch = self.guard.epoch();
            release();
        }
    }
}
