//! Key name and keysym resolution against the seat's XKB keymap.
//!
//! All lookups are pure functions over `xkbcommon` primitives (re-exported via
//! `smithay::input::keyboard::xkb`) so they can be exercised without a running
//! compositor.

use smithay::input::keyboard::{Keycode, Keysym, keysyms, xkb};

/// The XKB levels a keysym may live at, and the modifiers they require.
///
/// Level 0 is unmodified, level 1 is Shift, level 2 is ISO_Level3_Shift
/// (AltGr) and level 3 is AltGr+Shift — matching the classic 4-level pc105
/// group layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedKey {
    /// The evdev-style XKB keycode that produces the keysym.
    pub code: Keycode,
    /// Modifier keycodes that must be held for the keysym to be produced.
    pub modifiers: Vec<Keycode>,
}

/// Friendly aliases on top of the official XKB keysym names.
fn alias_keysym(name: &str) -> Option<Keysym> {
    Some(Keysym::new(match name {
        "enter" | "return" => keysyms::KEY_Return,
        "esc" => keysyms::KEY_Escape,
        "del" => keysyms::KEY_Delete,
        "ins" => keysyms::KEY_Insert,
        "pgup" | "pageup" => keysyms::KEY_Prior,
        "pgdn" | "pagedown" => keysyms::KEY_Next,
        "backspace" => keysyms::KEY_BackSpace,
        "space" => keysyms::KEY_space,
        "win" | "cmd" | "command" => keysyms::KEY_Super_L,
        _ => return None,
    }))
}

/// The keysym produced by a named modifier.
///
/// Recognised names (case-insensitive): `ctrl`/`control`, `shift`, `alt`,
/// `super`/`logo`/`meta`/`win`/`cmd`, `altgr`, `caps`/`capslock`, `numlock`.
pub(crate) fn modifier_keysym(name: &str) -> Option<Keysym> {
    Some(Keysym::new(match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => keysyms::KEY_Control_L,
        "shift" => keysyms::KEY_Shift_L,
        "alt" => keysyms::KEY_Alt_L,
        "super" | "logo" | "meta" | "win" | "cmd" | "command" => keysyms::KEY_Super_L,
        "altgr" => keysyms::KEY_ISO_Level3_Shift,
        "caps" | "capslock" => keysyms::KEY_Caps_Lock,
        "numlock" => keysyms::KEY_Num_Lock,
        _ => return None,
    }))
}

/// Resolves a key name to a keysym.
///
/// Accepts the modifier aliases of [`modifier_keysym`], a small set of
/// convenience aliases (`enter`, `esc`, `pgup`, …) and any name
/// `xkb_keysym_from_name` understands (case-insensitively): `a`, `F5`,
/// `Return`, `space`, `Left`, `BackSpace`, `dead_grave`, …
pub(crate) fn key_name_to_keysym(name: &str) -> Option<Keysym> {
    let lower = name.to_ascii_lowercase();
    if let Some(sym) = alias_keysym(&lower).or_else(|| modifier_keysym(&lower)) {
        return Some(sym);
    }
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
    (sym.raw() != keysyms::KEY_NoSymbol).then_some(sym)
}

/// The keysyms that must be held for a key at `level` to yield its keysym.
fn level_modifier_keysyms(level: u32) -> &'static [u32] {
    match level {
        0 => &[],
        1 => &[keysyms::KEY_Shift_L],
        2 => &[keysyms::KEY_ISO_Level3_Shift],
        _ => &[keysyms::KEY_ISO_Level3_Shift, keysyms::KEY_Shift_L],
    }
}

/// Finds the keycode producing `sym` in `keymap`, preferring layout 0 and the
/// lowest level, along with the modifiers needed to reach that level.
///
/// Returns `None` when no key on the keymap produces the keysym at all.
pub(crate) fn resolve_keycode(keymap: &xkb::Keymap, sym: Keysym) -> Option<ResolvedKey> {
    let layouts = keymap.num_layouts().max(1);
    let mut best: Option<(u32, u32, Keycode)> = None; // (layout, level, code)
    'codes: for code in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        let code = Keycode::new(code);
        for layout in 0..layouts {
            for level in 0..4u32 {
                if keymap
                    .key_get_syms_by_level(code, layout, level)
                    .contains(&sym)
                {
                    let entry = (layout, level, code);
                    if best.is_none_or(|(l, lv, _)| entry.0 < l || (entry.0 == l && entry.1 < lv)) {
                        best = Some(entry);
                    }
                    if layout == 0 && level == 0 {
                        return Some(ResolvedKey {
                            code,
                            modifiers: Vec::new(),
                        });
                    }
                    continue 'codes;
                }
            }
        }
    }
    let (_, level, code) = best?;
    let modifiers = level_modifier_keysyms(level)
        .iter()
        .filter_map(|&sym| level_zero_keycode(keymap, Keysym::new(sym)))
        .collect();
    Some(ResolvedKey { code, modifiers })
}

/// Finds the (level-0) keycode producing a modifier keysym such as Shift_L.
fn level_zero_keycode(keymap: &xkb::Keymap, sym: Keysym) -> Option<Keycode> {
    for code in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        let code = Keycode::new(code);
        if keymap.key_get_syms_by_level(code, 0, 0).contains(&sym) {
            return Some(code);
        }
    }
    None
}

/// The keysym to type for a single character of text.
///
/// Control characters get their usual semantic mapping (`\n` → Return,
/// `\t` → Tab); everything else goes through `xkb_utf32_to_keysym` which
/// covers the whole Unicode range of mapped keysyms.
pub(crate) fn char_keysym(ch: char) -> Option<Keysym> {
    Some(match ch {
        '\n' | '\r' => Keysym::new(keysyms::KEY_Return),
        '\t' => Keysym::new(keysyms::KEY_Tab),
        _ => {
            let sym = xkb::utf32_to_keysym(ch as u32);
            (sym.raw() != keysyms::KEY_NoSymbol).then_some(sym)?
        }
    })
}
