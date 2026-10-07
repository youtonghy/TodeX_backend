//! Key chords such as `cmd+shift+z`, parsed into [`xa11y::input::Key`]s.

use xa11y::input::Key;

/// Modifiers held while one key is pressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Chord {
    pub held: Vec<Key>,
    pub key: Key,
}

/// `cmd` is the platform's primary modifier: ⌘ on macOS, Ctrl elsewhere,
/// so `cmd+c` copies everywhere. `meta`/`super`/`win` always name the
/// logo key.
const PRIMARY: Key = if cfg!(target_os = "macos") {
    Key::Meta
} else {
    Key::Ctrl
};

fn modifier(name: &str) -> Option<Key> {
    Some(match name {
        "cmd" | "command" | "⌘" => PRIMARY,
        "meta" | "super" | "win" | "windows" => Key::Meta,
        "shift" | "⇧" => Key::Shift,
        "alt" | "option" | "opt" | "⌥" => Key::Alt,
        "ctrl" | "control" | "⌃" => Key::Ctrl,
        _ => return None,
    })
}

fn named(name: &str) -> Option<Key> {
    Some(match name {
        "enter" | "return" => Key::Enter,
        "esc" | "escape" => Key::Escape,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        // The Mac's ⌫ key is labelled delete.
        "delete" if cfg!(target_os = "macos") => Key::Backspace,
        "delete" | "del" | "forwarddelete" => Key::Delete,
        "insert" | "ins" => Key::Insert,
        "up" | "arrowup" => Key::ArrowUp,
        "down" | "arrowdown" => Key::ArrowDown,
        "left" | "arrowleft" => Key::ArrowLeft,
        "right" | "arrowright" => Key::ArrowRight,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        _ => {
            let number = name.strip_prefix('f')?.parse::<u8>().ok()?;
            if !(1..=20).contains(&number) {
                return None;
            }
            Key::F(number)
        }
    })
}

/// Parses `cmd+c`, `Cmd-Shift-Z`, `enter`, `f5`. Rejects an empty chord,
/// several non-modifier keys, or only modifiers.
pub(crate) fn parse(raw: &str) -> Result<Chord, String> {
    // `+`/`,` separate keys; `-` only when neither is used
    // (`ctrl-alt-delete`), so `cmd+-` keeps its minus.
    let uses_dash = !raw.contains('+') && !raw.contains(',') && raw.chars().count() > 1;
    let parts: Vec<String> = raw
        .split(|c: char| c == '+' || c == ',' || c == ' ' || (uses_dash && c == '-'))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    if parts.is_empty() {
        return Err("empty key chord".to_owned());
    }
    let mut held: Vec<Key> = Vec::new();
    let mut key = None;
    for part in parts {
        let lower = part.to_lowercase();
        if let Some(modifier) = modifier(&lower) {
            if !held.contains(&modifier) {
                held.push(modifier);
            }
            continue;
        }
        if key.is_some() {
            return Err(format!("a chord has one key besides modifiers: {raw}"));
        }
        let mut chars = part.chars();
        key = Some(match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_uppercase() => {
                if !held.contains(&Key::Shift) {
                    held.push(Key::Shift);
                }
                Key::Char(c.to_lowercase().next().unwrap_or(c))
            }
            (Some(c), None) => Key::Char(c),
            _ => named(&lower).ok_or_else(|| format!("unknown key {part}"))?,
        });
    }
    let key = key.ok_or_else(|| format!("a chord needs a key besides modifiers: {raw}"))?;
    Ok(Chord { held, key })
}

/// System shortcuts agents may never send, as (held, key, what it does).
/// A chord matches when it holds at least these modifiers with this key,
/// so adding Shift or Option does not slip past (⌘⌥⇧⎋ still force
/// quits). The line: shortcuts that end or lock the user's session, kill
/// processes, or open a launcher or command prompt that runs anything
/// outside the per-app approval. App switching (⌘Tab, Alt+Tab) stays
/// allowed: the next action is checked against the app it lands in.
#[cfg(target_os = "macos")]
const SYSTEM_SHORTCUTS: &[(&[Key], Key, &str)] = &[
    (&[Key::Ctrl, Key::Meta], Key::Char('q'), "locks the screen"),
    (&[Key::Meta, Key::Shift], Key::Char('q'), "logs out"),
    (&[Key::Meta, Key::Alt], Key::Escape, "force quits apps"),
    (&[Key::Meta], Key::Space, "opens Spotlight"),
];

#[cfg(target_os = "windows")]
const SYSTEM_SHORTCUTS: &[(&[Key], Key, &str)] = &[
    (&[Key::Meta], Key::Char('l'), "locks the screen"),
    (
        &[Key::Ctrl, Key::Alt],
        Key::Delete,
        "opens the security screen",
    ),
    (&[Key::Ctrl, Key::Shift], Key::Escape, "opens Task Manager"),
    (&[Key::Meta], Key::Char('r'), "opens Run"),
    (&[Key::Meta], Key::Char('x'), "opens the power user menu"),
];

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const SYSTEM_SHORTCUTS: &[(&[Key], Key, &str)] = &[
    (&[Key::Meta], Key::Char('l'), "locks the screen"),
    (&[Key::Ctrl, Key::Alt], Key::Char('l'), "locks the screen"),
    (&[Key::Ctrl, Key::Alt], Key::Delete, "logs out"),
    (&[Key::Ctrl, Key::Alt], Key::Backspace, "kills the X server"),
    (
        &[Key::Ctrl, Key::Alt],
        Key::Escape,
        "kills a window (xkill)",
    ),
    (
        &[Key::Ctrl, Key::Alt, Key::Shift],
        Key::PageDown,
        "shuts down",
    ),
    (&[Key::Ctrl, Key::Alt, Key::Shift], Key::PageUp, "restarts"),
    (&[Key::Alt], Key::F(2), "opens KRunner"),
    (&[Key::Alt], Key::Space, "opens KRunner"),
];

/// What `chord` does if it is a system shortcut agents may not send.
/// Ctrl+Alt+F1…F12 (switching to a text console) is refused off macOS.
pub(crate) fn system_shortcut(chord: &Chord) -> Option<&'static str> {
    let holds = |keys: &[Key]| keys.iter().all(|key| chord.held.contains(key));
    if !cfg!(any(target_os = "macos", target_os = "windows"))
        && matches!(chord.key, Key::F(_))
        && holds(&[Key::Ctrl, Key::Alt])
    {
        return Some("switches to a text console");
    }
    SYSTEM_SHORTCUTS
        .iter()
        .find(|(held, key, _)| *key == chord.key && holds(held))
        .map(|(_, _, what)| *what)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_shortcuts_are_recognised_with_extra_modifiers() {
        let blocked = |raw: &str| system_shortcut(&parse(raw).unwrap());
        #[cfg(target_os = "macos")]
        {
            assert_eq!(blocked("ctrl+cmd+q"), Some("locks the screen"));
            assert_eq!(blocked("cmd+shift+q"), Some("logs out"));
            assert_eq!(blocked("cmd+opt+shift+q"), Some("logs out"));
            assert_eq!(blocked("cmd+opt+esc"), Some("force quits apps"));
            assert_eq!(blocked("cmd+alt+shift+escape"), Some("force quits apps"));
            assert_eq!(blocked("cmd+space"), Some("opens Spotlight"));
            assert_eq!(blocked("cmd+tab"), None);
            assert_eq!(blocked("cmd+q"), None);
        }
        #[cfg(target_os = "windows")]
        {
            assert_eq!(blocked("win+l"), Some("locks the screen"));
            assert_eq!(blocked("ctrl+alt+del"), Some("opens the security screen"));
            assert_eq!(blocked("alt+tab"), None);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            assert_eq!(blocked("super+l"), Some("locks the screen"));
            assert_eq!(blocked("ctrl+alt+delete"), Some("logs out"));
            assert_eq!(blocked("ctrl+alt+backspace"), Some("kills the X server"));
            assert_eq!(blocked("ctrl+alt+f3"), Some("switches to a text console"));
            assert_eq!(blocked("alt+tab"), None);
        }
        assert_eq!(blocked("cmd+c"), None);
        assert_eq!(blocked("enter"), None);
    }

    #[test]
    fn chords_parse_with_aliases_and_separators() {
        assert_eq!(
            parse("cmd+c").unwrap(),
            Chord {
                held: vec![PRIMARY],
                key: Key::Char('c')
            }
        );
        assert_eq!(
            parse("Ctrl-Alt-Del").unwrap(),
            Chord {
                held: vec![Key::Ctrl, Key::Alt],
                key: Key::Delete
            }
        );
        assert_eq!(parse("cmd+-").unwrap().key, Key::Char('-'));
        assert_eq!(parse("enter").unwrap().key, Key::Enter);
        assert_eq!(parse("shift, tab").unwrap().held, vec![Key::Shift]);
        assert_eq!(parse("f12").unwrap().key, Key::F(12));
        assert_eq!(
            parse("cmd+Z").unwrap(),
            Chord {
                held: vec![PRIMARY, Key::Shift],
                key: Key::Char('z')
            }
        );
        assert_eq!(parse("super+l").unwrap().held, vec![Key::Meta]);
    }

    #[test]
    fn malformed_chords_are_rejected() {
        for bad in ["", "cmd+shift", "a+b", "f0", "f21", "hyper+x", "cmd+bogus"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
