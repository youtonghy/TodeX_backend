//! Pure parts of the Windows layer: executable names, idle ticks, session
//! checks. Kept apart from `windows.rs` so they are unit-tested on every
//! host.

/// The app id of an executable path: its file name, lowercase
/// (`C:\Windows\notepad.exe` → `notepad.exe`).
pub(super) fn exe_id(path: &str) -> String {
    path.rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
        .to_lowercase()
}

/// `notepad.exe` → `notepad`.
pub(super) fn exe_stem(id: &str) -> &str {
    let lower = id.to_ascii_lowercase();
    if lower.ends_with(".exe") {
        &id[..id.len() - 4]
    } else {
        id
    }
}

/// The executable file name an agent's identifier refers to, for App Paths
/// and PATH lookups (`notepad` → `notepad.exe`).
pub(super) fn exe_file_name(identifier: &str) -> String {
    let identifier = identifier.trim();
    if identifier.to_ascii_lowercase().ends_with(".exe") {
        identifier.to_owned()
    } else {
        format!("{identifier}.exe")
    }
}

/// Whether `identifier` names the app with executable `id` and display
/// `name`, case-insensitively and with or without `.exe`.
pub(super) fn names_app(identifier: &str, id: &str, name: &str) -> bool {
    let wanted = exe_stem(identifier.trim()).to_lowercase();
    !wanted.is_empty() && (exe_stem(id).to_lowercase() == wanted || name.to_lowercase() == wanted)
}

/// Seconds between two `GetTickCount` values, across its 49.7-day wrap.
pub(super) fn idle_from_ticks(now: u32, last_input: u32) -> f64 {
    f64::from(now.wrapping_sub(last_input)) / 1000.0
}

/// Why this process cannot drive the desktop, from its session.
pub(super) fn session_problem(session_zero: bool, input_desktop: bool) -> Option<String> {
    if session_zero {
        return Some(
            "The TodeX backend runs as a Windows service (session 0), which has no desktop. \
             Run `todex-agentd serve` from the signed-in user's session instead."
                .to_owned(),
        );
    }
    if !input_desktop {
        return Some(
            "This computer's desktop is locked or not interactive; unlock it to use Computer Use."
                .to_owned(),
        );
    }
    None
}

/// A registry or shell path without surrounding quotes.
pub(super) fn unquote(path: &str) -> &str {
    let path = path.trim();
    path.strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
        .unwrap_or(path)
}

/// Last-resort password detection for edit fields UI Automation could not
/// be asked about: their class or accessible name says so.
pub(super) fn looks_like_password(class_name: &str, name: &str) -> bool {
    let class_name = class_name.to_lowercase();
    let name = name.to_lowercase();
    class_name.contains("passwordbox")
        || [
            "password",
            "passcode",
            "passwort",
            "mot de passe",
            "contraseña",
            "密码",
            "密碼",
            "パスワード",
            "비밀번호",
            "pin",
        ]
        .iter()
        .any(|word| {
            if *word == "pin" {
                name.split(|c: char| !c.is_alphanumeric())
                    .any(|token| token == "pin")
            } else {
                name.contains(word)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executables_are_named_by_lowercase_file_name() {
        assert_eq!(exe_id(r"C:\Windows\System32\Notepad.EXE"), "notepad.exe");
        assert_eq!(exe_id("code.exe"), "code.exe");
        assert_eq!(exe_stem("notepad.exe"), "notepad");
        assert_eq!(exe_stem("Notepad.EXE"), "Notepad");
        assert_eq!(exe_file_name("notepad"), "notepad.exe");
        assert_eq!(exe_file_name("Code.EXE"), "Code.EXE");
        assert_eq!(
            unquote(r#" "C:\Program Files\App\app.exe" "#),
            r"C:\Program Files\App\app.exe"
        );
    }

    #[test]
    fn identifiers_match_with_or_without_exe() {
        assert!(names_app("notepad", "notepad.exe", "Notepad"));
        assert!(names_app("NOTEPAD.EXE", "notepad.exe", ""));
        assert!(names_app(
            "Visual Studio Code",
            "code.exe",
            "Visual Studio Code"
        ));
        assert!(!names_app("", "notepad.exe", ""));
        assert!(!names_app("note", "notepad.exe", "Notepad"));
    }

    #[test]
    fn idle_survives_the_tick_wrap() {
        assert_eq!(idle_from_ticks(5_000, 2_000), 3.0);
        assert_eq!(idle_from_ticks(1_000, u32::MAX - 999), 2.0);
    }

    #[test]
    fn services_and_locked_desktops_are_unsupported() {
        assert!(session_problem(true, true).unwrap().contains("session 0"));
        assert!(session_problem(false, false).unwrap().contains("locked"));
        assert_eq!(session_problem(false, true), None);
    }

    #[test]
    fn password_heuristics() {
        assert!(looks_like_password("PasswordBox", ""));
        assert!(looks_like_password("Edit", "Enter your Password"));
        assert!(looks_like_password("", "密码"));
        assert!(looks_like_password("", "PIN"));
        assert!(!looks_like_password("Edit", "Spinner value"));
        assert!(!looks_like_password("Edit", "Search"));
    }
}
