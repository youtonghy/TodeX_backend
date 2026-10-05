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

#[cfg(test)]
mod tests {
    use super::*;

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
