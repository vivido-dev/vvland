//! Key synthesis: resolve a key *name* or a text string to evdev key events.
//!
//! The terminal translation layer has always owned an XKB keymap so it could turn a typed
//! character into the keycode-plus-modifiers that actually produce it in the session's own layout.
//! That machinery is useful to anything that drives a nested desktop, not just a crossterm event
//! pump, so it lives here — free of crossterm, free of any product name — and [`super::terminal`]
//! is now one caller of it rather than its owner.
//!
//! Everything here speaks the same units as [`crate::producer::TerminalInjector`]: raw Linux evdev key
//! codes, never HID usages and never keysyms.

use std::collections::HashMap;
use std::io;
use std::sync::Mutex;

use xkbcommon::xkb;

use crate::producer::TerminalInjector;

/// Modifier codes, shared with the terminal translation layer so the two cannot drift.
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_CAPSLOCK: u32 = 58;
pub const KEY_NUMLOCK: u32 = 69;
pub const KEY_SCROLLLOCK: u32 = 70;
pub const KEY_RIGHTALT: u32 = 100;
pub const KEY_LEFTMETA: u32 = 125;

/// The highest evdev code both injection transports accept.
pub const MAX_KEY_CODE: u32 = 0x2ff;

/// XKB keycodes are evdev codes offset by 8.
const XKB_KEYCODE_OFFSET: u32 = 8;

/// One key press: the key itself plus the modifiers that must be held for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyStroke {
    pub code: u32,
    pub modifiers: Vec<u32>,
}

impl KeyStroke {
    /// A bare key with no modifiers held.
    pub fn plain(code: u32) -> Self {
        Self {
            code,
            modifiers: Vec::new(),
        }
    }

    /// Add modifiers, keeping the list sorted and free of duplicates so a stroke assembled from
    /// several sources still presses each modifier exactly once.
    pub fn with_modifiers(mut self, modifiers: impl IntoIterator<Item = u32>) -> Self {
        self.modifiers.extend(modifiers);
        self.modifiers.sort_unstable();
        self.modifiers.dedup();
        self
    }
}

/// A compiled keymap plus a memo of the characters already resolved through it.
pub struct KeySynth {
    keymap: xkb::Keymap,
    // `character` scans every keycode × layout × level, so it is O(keymap) per call. Typing a
    // megabyte of text through an uncached scan would be pathological; negative results are cached
    // too, since an unmappable character is just as likely to repeat.
    cache: Mutex<HashMap<char, Option<KeyStroke>>>,
}

impl KeySynth {
    pub fn new(
        model: Option<&str>,
        layout: &str,
        variant: Option<&str>,
        options: Option<String>,
    ) -> io::Result<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            model.unwrap_or("pc105"),
            layout,
            variant.unwrap_or_default(),
            options,
            xkb::COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid XKB keymap"))?;
        Ok(Self {
            keymap,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Resolve a key *name*, trying four spellings in order:
    ///
    /// 1. `code:28` — a literal evdev code, validated against [`MAX_KEY_CODE`].
    /// 2. a single Unicode scalar (`a`, `/`, `£`) — resolved through the keymap, fewest
    ///    modifiers winning, so the answer is correct for the session's own layout.
    /// 3. an XKB keysym name (`Return`, `Escape`, `Page_Up`, `F5`) — also resolved through the
    ///    keymap, so it too follows the layout. Tried case-sensitively first, then
    ///    case-insensitively, so `return` works while an exact name always wins.
    /// 4. an evdev name (`KEY_ENTER`) — a small table for keys that a layout may not expose at
    ///    all. `code:N` is the escape hatch for anything absent from it.
    pub fn resolve(&self, name: &str) -> Option<KeyStroke> {
        if let Some(rest) = name.strip_prefix("code:") {
            let code = rest.parse::<u32>().ok()?;
            return (1..=MAX_KEY_CODE)
                .contains(&code)
                .then(|| KeyStroke::plain(code));
        }
        let mut characters = name.chars();
        if let (Some(character), None) = (characters.next(), characters.next()) {
            return self.character(character);
        }
        if let Some(stroke) = self.named_keysym(name) {
            return Some(stroke);
        }
        evdev_named(name).map(KeyStroke::plain)
    }

    /// Resolve a modifier name to its evdev code. Left-hand variants throughout: a synthesized
    /// chord has no physical side, and picking one keeps the produced events deterministic.
    pub fn modifier(name: &str) -> Option<u32> {
        Some(match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => KEY_LEFTCTRL,
            "shift" => KEY_LEFTSHIFT,
            "alt" | "mod1" => KEY_LEFTALT,
            "super" | "meta" | "mod4" | "win" | "logo" => KEY_LEFTMETA,
            "altgr" | "level3" | "iso_level3_shift" => KEY_RIGHTALT,
            "caps" | "capslock" => KEY_CAPSLOCK,
            "num" | "numlock" => KEY_NUMLOCK,
            "scroll" | "scrolllock" => KEY_SCROLLLOCK,
            _ => return None,
        })
    }

    /// Resolve one character through the keymap, memoized.
    pub fn character(&self, character: char) -> Option<KeyStroke> {
        if let Some(hit) = lock(&self.cache).get(&character) {
            return hit.clone();
        }
        let resolved = self.keysym(xkb::Keysym::from_char(character));
        lock(&self.cache).insert(character, resolved.clone());
        resolved
    }

    /// Press the modifiers in order, then the key.
    pub fn press(&self, input: &mut impl TerminalInjector, stroke: &KeyStroke) -> io::Result<()> {
        for modifier in &stroke.modifiers {
            input.key(*modifier, true)?;
        }
        input.key(stroke.code, true)
    }

    /// Release the key, then the modifiers in reverse — the mirror of [`KeySynth::press`].
    pub fn release(&self, input: &mut impl TerminalInjector, stroke: &KeyStroke) -> io::Result<()> {
        input.key(stroke.code, false)?;
        for modifier in stroke.modifiers.iter().rev() {
            input.key(*modifier, false)?;
        }
        Ok(())
    }

    pub fn tap(&self, input: &mut impl TerminalInjector, stroke: &KeyStroke) -> io::Result<()> {
        self.press(input, stroke)?;
        self.release(input, stroke)
    }

    /// Type a string one character at a time.
    ///
    /// Every character is resolved *before* anything is injected: a string containing one
    /// unmappable character types nothing at all, because a half-typed command line is worse than
    /// a rejection.
    pub fn type_text(&self, input: &mut impl TerminalInjector, text: &str) -> io::Result<()> {
        let strokes = self.plan_text(text)?;
        for stroke in &strokes {
            self.tap(input, stroke)?;
        }
        Ok(())
    }

    /// Resolve every character of `text`, naming the first that the layout cannot produce.
    pub fn plan_text(&self, text: &str) -> io::Result<Vec<KeyStroke>> {
        text.char_indices()
            .map(|(offset, character)| {
                self.character(character).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "character {character:?} at byte {offset} is not in the XKB keymap"
                        ),
                    )
                })
            })
            .collect()
    }

    fn named_keysym(&self, name: &str) -> Option<KeyStroke> {
        for flags in [xkb::KEYSYM_NO_FLAGS, xkb::KEYSYM_CASE_INSENSITIVE] {
            let keysym = xkb::keysym_from_name(name, flags);
            if keysym == xkb::Keysym::NoSymbol {
                continue;
            }
            if let Some(stroke) = self.keysym(keysym) {
                return Some(stroke);
            }
        }
        None
    }

    /// Find the cheapest way to produce `target` in this keymap.
    ///
    /// Scans every keycode, layout, and shift level for one whose symbol is exactly `target`,
    /// then asks the keymap which modifier masks reach that level. The candidate needing the
    /// fewest modifiers wins, so an unshifted key is preferred over a shifted one that happens to
    /// produce the same symbol.
    fn keysym(&self, target: xkb::Keysym) -> Option<KeyStroke> {
        let mut best: Option<KeyStroke> = None;
        for raw in self.keymap.min_keycode().raw()..=self.keymap.max_keycode().raw() {
            let keycode = xkb::Keycode::new(raw);
            for layout in 0..self.keymap.num_layouts_for_key(keycode) {
                for level in 0..self.keymap.num_levels_for_key(keycode, layout) {
                    if self.keymap.key_get_syms_by_level(keycode, layout, level) != [target] {
                        continue;
                    }
                    let mut masks = [0_u32; 32];
                    let count = self
                        .keymap
                        .key_get_mods_for_level(keycode, layout, level, &mut masks);
                    for mask in &masks[..count] {
                        let Some(modifiers) = modifier_codes(&self.keymap, *mask) else {
                            continue;
                        };
                        let code = keycode.raw().checked_sub(XKB_KEYCODE_OFFSET)?;
                        let candidate = KeyStroke { code, modifiers };
                        if best.as_ref().is_none_or(|current| {
                            candidate.modifiers.len() < current.modifiers.len()
                        }) {
                            best = Some(candidate);
                        }
                    }
                }
            }
        }
        best
    }
}

/// Translate an XKB modifier mask to evdev modifier codes, or `None` when the mask names a
/// modifier with no evdev equivalent — such a candidate is unusable and is skipped.
pub fn modifier_codes(keymap: &xkb::Keymap, mask: u32) -> Option<Vec<u32>> {
    let mut modifiers = Vec::new();
    for index in 0..keymap.num_mods() {
        if mask & (1 << index) == 0 {
            continue;
        }
        modifiers.push(match keymap.mod_get_name(index) {
            "Shift" => KEY_LEFTSHIFT,
            "Control" => KEY_LEFTCTRL,
            "Mod1" => KEY_LEFTALT,
            "Mod4" => KEY_LEFTMETA,
            "LevelThree" | "ISO_Level3_Shift" => KEY_RIGHTALT,
            "Lock" => KEY_CAPSLOCK,
            "NumLock" | "Mod2" => KEY_NUMLOCK,
            "ScrollLock" | "Mod3" => KEY_SCROLLLOCK,
            _ => return None,
        });
    }
    Some(modifiers)
}

/// Keys addressable by their evdev name.
///
/// Deliberately small: rules 2 and 3 of [`KeySynth::resolve`] already reach everything the layout
/// can produce, and `code:N` reaches everything else. This table exists for the keys a caller is
/// most likely to name directly.
fn evdev_named(name: &str) -> Option<u32> {
    let name = name.to_ascii_uppercase();
    let name = name.strip_prefix("KEY_").unwrap_or(&name);
    Some(match name {
        "ESC" | "ESCAPE" => 1,
        "BACKSPACE" => 14,
        "TAB" => 15,
        "ENTER" | "RETURN" => 28,
        "LEFTCTRL" | "CTRL" => KEY_LEFTCTRL,
        "LEFTSHIFT" => KEY_LEFTSHIFT,
        "RIGHTSHIFT" => 54,
        "LEFTALT" | "ALT" => KEY_LEFTALT,
        "CAPSLOCK" => KEY_CAPSLOCK,
        "SPACE" => 57,
        "NUMLOCK" => KEY_NUMLOCK,
        "SCROLLLOCK" => KEY_SCROLLLOCK,
        "RIGHTCTRL" => 97,
        "RIGHTALT" => KEY_RIGHTALT,
        "HOME" => 102,
        "UP" => 103,
        "PAGEUP" => 104,
        "LEFT" => 105,
        "RIGHT" => 106,
        "END" => 107,
        "DOWN" => 108,
        "PAGEDOWN" => 109,
        "INSERT" => 110,
        "DELETE" => 111,
        "LEFTMETA" | "META" | "SUPER" => KEY_LEFTMETA,
        "RIGHTMETA" => 126,
        "COMPOSE" | "MENU" => 127,
        _ => return function_key(name),
    })
}

fn function_key(name: &str) -> Option<u32> {
    let number = name.strip_prefix('F')?.parse::<u32>().ok()?;
    Some(match number {
        1..=10 => 59 + number - 1,
        11 => 87,
        12 => 88,
        13..=24 => 183 + number - 13,
        _ => return None,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the exact injection sequence so a translation can be asserted event by event.
    #[derive(Default)]
    struct Recorder {
        events: Vec<(u32, bool)>,
    }

    impl TerminalInjector for Recorder {
        fn key(&mut self, code: u32, pressed: bool) -> io::Result<()> {
            self.events.push((code, pressed));
            Ok(())
        }
        fn pointer_absolute(&mut self, _: u32, _: u32) -> io::Result<()> {
            Ok(())
        }
        fn pointer_button(&mut self, _: u32, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn pointer_axis(&mut self, _: u32, _: i32) -> io::Result<()> {
            Ok(())
        }
        fn release_all(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn synth(layout: &str) -> KeySynth {
        KeySynth::new(None, layout, None, None).unwrap()
    }

    #[test]
    fn literal_codes_are_accepted_and_bounded() {
        let synth = synth("us");
        assert_eq!(synth.resolve("code:28"), Some(KeyStroke::plain(28)));
        assert_eq!(
            synth.resolve(&format!("code:{MAX_KEY_CODE}")),
            Some(KeyStroke::plain(MAX_KEY_CODE))
        );
        // Zero and anything past the injectors' ceiling are rejected here rather than at the
        // transport, so a caller gets one consistent answer on both compositors.
        assert_eq!(synth.resolve("code:0"), None);
        assert_eq!(synth.resolve(&format!("code:{}", MAX_KEY_CODE + 1)), None);
        assert_eq!(synth.resolve("code:-1"), None);
        assert_eq!(synth.resolve("code:notanumber"), None);
    }

    #[test]
    fn keysym_names_resolve_through_the_keymap() {
        let synth = synth("us");
        // Named keys resolve to the evdev codes the injectors expect.
        for (name, code) in [
            ("Return", 28),
            ("Escape", 1),
            ("BackSpace", 14),
            ("Tab", 15),
            ("Left", 105),
            ("Right", 106),
            ("Up", 103),
            ("Down", 108),
            ("Home", 102),
            ("End", 107),
            ("Page_Up", 104),
            ("Page_Down", 109),
            ("Insert", 110),
            ("Delete", 111),
            ("F1", 59),
            ("F5", 63),
            ("F12", 88),
        ] {
            let stroke = synth.resolve(name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(stroke.code, code, "{name}");
            assert!(stroke.modifiers.is_empty(), "{name} needed modifiers");
        }
    }

    #[test]
    fn keysym_names_are_forgiving_about_case_but_prefer_the_exact_name() {
        let synth = synth("us");
        assert_eq!(synth.resolve("return"), synth.resolve("Return"));
        assert_eq!(synth.resolve("ESCAPE"), synth.resolve("Escape"));
        // A one-character name is a character, never a keysym name: "a" must be the letter.
        assert_eq!(synth.resolve("a"), synth.character('a'));
    }

    #[test]
    fn characters_resolve_with_the_fewest_modifiers() {
        let synth = synth("us");
        let lower = synth.resolve("a").unwrap();
        assert!(lower.modifiers.is_empty());
        let upper = synth.resolve("A").unwrap();
        assert_eq!(upper.code, lower.code, "same physical key");
        assert_eq!(upper.modifiers, vec![KEY_LEFTSHIFT]);
    }

    #[test]
    fn characters_follow_the_configured_layout() {
        // The whole point of resolving through a keymap: y and z swap between us and de, and a
        // hardcoded table would silently type the wrong letter.
        let us = synth("us");
        let de = synth("de");
        assert_eq!(us.resolve("y").unwrap().code, de.resolve("z").unwrap().code);
        assert_eq!(us.resolve("z").unwrap().code, de.resolve("y").unwrap().code);
        // A character the layout cannot produce at all.
        assert_eq!(us.resolve("\u{1F600}"), None);
    }

    #[test]
    fn evdev_names_cover_keys_a_layout_need_not_expose() {
        let synth = synth("us");
        assert_eq!(synth.resolve("KEY_ENTER"), Some(KeyStroke::plain(28)));
        assert_eq!(
            synth.resolve("key_leftmeta"),
            Some(KeyStroke::plain(KEY_LEFTMETA))
        );
        assert_eq!(synth.resolve("KEY_F24"), Some(KeyStroke::plain(194)));
        assert_eq!(synth.resolve("KEY_NOT_A_KEY"), None);
    }

    #[test]
    fn modifier_names_map_to_left_hand_codes() {
        for (name, code) in [
            ("ctrl", KEY_LEFTCTRL),
            ("Control", KEY_LEFTCTRL),
            ("shift", KEY_LEFTSHIFT),
            ("alt", KEY_LEFTALT),
            ("super", KEY_LEFTMETA),
            ("META", KEY_LEFTMETA),
            ("altgr", KEY_RIGHTALT),
        ] {
            assert_eq!(KeySynth::modifier(name), Some(code), "{name}");
        }
        assert_eq!(KeySynth::modifier("hyper"), None);
    }

    #[test]
    fn a_tap_presses_modifiers_outermost_and_releases_them_in_reverse() {
        let synth = synth("us");
        let stroke = KeyStroke::plain(30).with_modifiers([KEY_LEFTCTRL, KEY_LEFTSHIFT]);
        let mut recorder = Recorder::default();
        synth.tap(&mut recorder, &stroke).unwrap();
        assert_eq!(
            recorder.events,
            vec![
                (KEY_LEFTCTRL, true),
                (KEY_LEFTSHIFT, true),
                (30, true),
                (30, false),
                (KEY_LEFTSHIFT, false),
                (KEY_LEFTCTRL, false),
            ]
        );
    }

    #[test]
    fn press_and_release_compose_into_the_same_sequence_as_tap() {
        let synth = synth("us");
        let stroke = KeyStroke::plain(30).with_modifiers([KEY_LEFTALT]);
        let mut split = Recorder::default();
        synth.press(&mut split, &stroke).unwrap();
        synth.release(&mut split, &stroke).unwrap();
        let mut whole = Recorder::default();
        synth.tap(&mut whole, &stroke).unwrap();
        assert_eq!(split.events, whole.events);
    }

    #[test]
    fn with_modifiers_deduplicates_so_a_modifier_is_pressed_once() {
        let stroke = KeyStroke::plain(30)
            .with_modifiers([KEY_LEFTSHIFT, KEY_LEFTCTRL])
            .with_modifiers([KEY_LEFTSHIFT]);
        assert_eq!(stroke.modifiers, vec![KEY_LEFTCTRL, KEY_LEFTSHIFT]);
    }

    #[test]
    fn typing_a_string_taps_each_character_in_order() {
        let synth = synth("us");
        let mut typed = Recorder::default();
        synth.type_text(&mut typed, "aA").unwrap();
        let mut expected = Recorder::default();
        for character in "aA".chars() {
            synth
                .tap(&mut expected, &synth.character(character).unwrap())
                .unwrap();
        }
        assert_eq!(typed.events, expected.events);
    }

    #[test]
    fn typing_is_all_or_nothing_and_names_the_offending_character() {
        let synth = synth("us");
        let mut recorder = Recorder::default();
        let error = synth.type_text(&mut recorder, "ok\u{1F600}no").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let message = error.to_string();
        assert!(message.contains("byte 2"), "{message}");
        assert!(
            recorder.events.is_empty(),
            "a rejected string must inject nothing, got {:?}",
            recorder.events
        );
    }

    #[test]
    fn the_cache_agrees_with_an_uncached_resolution_across_ascii() {
        let cached_synth = synth("us");
        for character in (0x20_u8..0x7f).map(char::from) {
            let cached = cached_synth.character(character);
            // A fresh synth has an empty cache, so this is always the uncached scan.
            assert_eq!(cached, synth("us").character(character), "{character:?}");
            // And the memoized second call agrees with the first.
            assert_eq!(cached, cached_synth.character(character), "{character:?}");
        }
    }
}
