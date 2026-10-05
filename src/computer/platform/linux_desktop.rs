//! Pure parts of the Linux layer: session classification, `.desktop` entries, the
//! X11 HiDPI scale. Kept apart from `linux.rs` so they are unit-tested on
//! every host.

/// The kind of desktop session this process runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Session {
    X11,
    /// KDE Plasma on Wayland (version checked separately).
    KdeWayland,
    /// Any other Wayland desktop.
    OtherWayland,
    /// Neither a Wayland nor an X11 display.
    NoDisplay,
}

/// Classifies the session from its environment. A Wayland session wins
/// over `DISPLAY`, which there only names XWayland.
pub(super) fn session_kind(
    wayland_display: bool,
    session_type: Option<&str>,
    display: bool,
    current_desktop: Option<&str>,
) -> Session {
    let wayland = wayland_display
        || session_type.is_some_and(|session| session.eq_ignore_ascii_case("wayland"));
    if wayland {
        let kde = current_desktop.is_some_and(|desktops| {
            desktops
                .split(':')
                .any(|desktop| desktop.trim().eq_ignore_ascii_case("kde"))
        });
        return if kde {
            Session::KdeWayland
        } else {
            Session::OtherWayland
        };
    }
    if display {
        Session::X11
    } else {
        Session::NoDisplay
    }
}

/// Why a session cannot run Computer Use, apart from the Plasma version.
pub(super) fn session_problem(session: Session) -> Option<String> {
    match session {
        Session::X11 | Session::KdeWayland => None,
        Session::OtherWayland => Some(
            "Computer Use does not support this Wayland desktop. Use KDE Plasma 6.6 or later, \
             or log into an X11 session, and restart the TodeX backend there."
                .to_owned(),
        ),
        Session::NoDisplay => Some(
            "The TodeX backend has no display (neither WAYLAND_DISPLAY nor DISPLAY is set). \
             Start it from your desktop session, or import the session variables into its \
             service environment (`systemctl --user import-environment WAYLAND_DISPLAY \
             DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE`)."
                .to_owned(),
        ),
    }
}

/// An installed application from a `.desktop` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DesktopEntry {
    /// The desktop id, e.g. `org.kde.dolphin`.
    pub id: String,
    pub name: String,
    /// `Exec` split into arguments, field codes removed.
    pub argv: Vec<String>,
}

impl DesktopEntry {
    /// The executable's file name, lowercase: the app id policy matches.
    pub(super) fn exe(&self) -> Option<String> {
        self.argv.first().map(|program| exe_name(program))
    }
}

/// `applications/org.kde.dolphin.desktop` → `org.kde.dolphin`; files in
/// sub-directories join with `-` as the spec says.
pub(super) fn desktop_id(relative_path: &str) -> Option<String> {
    let stem = relative_path.strip_suffix(".desktop")?;
    Some(stem.trim_start_matches('/').replace('/', "-"))
}

/// Parses the `[Desktop Entry]` group of an application entry; `None` for
/// other types and hidden entries.
pub(super) fn parse_desktop_entry(id: &str, contents: &str) -> Option<DesktopEntry> {
    let mut in_entry = false;
    let (mut name, mut exec, mut kind, mut hidden) = (None, None, None, false);
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Name" => name = Some(value.to_owned()),
            "Exec" => exec = Some(value.to_owned()),
            "Type" => kind = Some(value.to_owned()),
            "Hidden" => hidden = value == "true",
            _ => {}
        }
    }
    if hidden || kind.as_deref() != Some("Application") {
        return None;
    }
    let argv = exec.as_deref().map(exec_argv).unwrap_or_default();
    Some(DesktopEntry {
        id: id.to_owned(),
        name: name.unwrap_or_else(|| id.to_owned()),
        argv,
    })
}

/// Splits an `Exec` value into arguments: double quotes group, `\` escapes
/// inside quotes, field codes (`%f`, `%U`, …) are dropped, `%%` is `%`,
/// and a leading `env VAR=value …` wrapper is skipped.
pub(super) fn exec_argv(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut started) = (false, false);
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }
    let args: Vec<String> = args
        .into_iter()
        .filter_map(|arg| {
            if arg.len() == 2 && arg.starts_with('%') && arg != "%%" {
                return None;
            }
            Some(arg.replace("%%", "%"))
        })
        .collect();
    let skip = if args
        .first()
        .is_some_and(|program| exe_name(program) == "env")
    {
        1 + args[1..]
            .iter()
            .take_while(|arg| arg.contains('=') && !arg.starts_with('-'))
            .count()
    } else {
        0
    };
    args.into_iter().skip(skip).collect()
}

/// The lowercase file name of a program path.
pub(super) fn exe_name(program: &str) -> String {
    program.rsplit('/').next().unwrap_or(program).to_lowercase()
}

/// Whether `identifier` (an agent's app name) names this app: its
/// executable, desktop id or display name, case-insensitively.
pub(super) fn names_app(identifier: &str, exe: &str, desktop_id: &str, name: &str) -> bool {
    let wanted = identifier
        .trim()
        .trim_end_matches(".desktop")
        .to_lowercase();
    !wanted.is_empty()
        && (exe.to_lowercase() == wanted
            || desktop_id.to_lowercase() == wanted
            || name.to_lowercase() == wanted)
}

/// The `Xft.dpi` resource from an X `RESOURCE_MANAGER` dump.
pub(super) fn parse_xft_dpi(resources: &str) -> Option<f64> {
    resources.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Xft.dpi:")
            .and_then(|dpi| dpi.trim().parse::<f64>().ok())
            .filter(|dpi| *dpi > 0.0)
    })
}

/// xa11y-linux's X11 coordinate scale (`scale.rs`, private there): integer
/// `Xft.dpi / 96` only, else 1. Element bounds and input points are X
/// pixels divided by it, so displays and hit-tests must use it too.
pub(super) fn scale_from_dpi(dpi: f64) -> f64 {
    if !dpi.is_finite() || dpi <= 0.0 {
        return 1.0;
    }
    let raw = dpi / 96.0;
    let nearest = raw.round();
    if (raw - nearest).abs() <= 0.05 && (1.0..=8.0).contains(&nearest) {
        nearest
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_classify_from_the_environment() {
        assert_eq!(
            session_kind(true, None, true, Some("KDE")),
            Session::KdeWayland
        );
        assert_eq!(
            session_kind(false, Some("Wayland"), false, Some("ubuntu:GNOME")),
            Session::OtherWayland
        );
        assert_eq!(
            session_kind(false, Some("x11"), true, Some("KDE")),
            Session::X11
        );
        assert_eq!(session_kind(false, None, false, None), Session::NoDisplay);
        assert!(session_problem(Session::OtherWayland)
            .unwrap()
            .contains("KDE Plasma 6.6"));
        assert!(session_problem(Session::NoDisplay)
            .unwrap()
            .contains("WAYLAND_DISPLAY"));
        assert_eq!(session_problem(Session::X11), None);
        assert_eq!(session_problem(Session::KdeWayland), None);
    }

    #[test]
    fn desktop_entries_parse_name_and_exec() {
        let entry = parse_desktop_entry(
            "org.kde.dolphin",
            "[Desktop Entry]\nType=Application\nName=Dolphin\nName[de]=Dolphin DE\n\
             Exec=dolphin %u\n\n[Desktop Action new]\nName=New Window\nExec=dolphin --new-window\n",
        )
        .unwrap();
        assert_eq!(entry.name, "Dolphin");
        assert_eq!(entry.argv, ["dolphin"]);
        assert_eq!(entry.exe().as_deref(), Some("dolphin"));
        assert_eq!(
            parse_desktop_entry("x", "[Desktop Entry]\nType=Link\nName=X\n"),
            None
        );
        assert_eq!(
            parse_desktop_entry(
                "x",
                "[Desktop Entry]\nType=Application\nName=X\nExec=x\nHidden=true\n"
            ),
            None
        );
        assert_eq!(
            desktop_id("kde/org.kde.konsole.desktop").as_deref(),
            Some("kde-org.kde.konsole")
        );
    }

    #[test]
    fn exec_lines_split_like_the_spec() {
        assert_eq!(
            exec_argv(r#"env GDK_BACKEND=x11 "/opt/My App/app" --flag %F 100%%"#),
            ["/opt/My App/app", "--flag", "100%"]
        );
        assert_eq!(exec_argv(r#""/usr/bin/a \"b\"" %U"#), [r#"/usr/bin/a "b""#]);
        assert_eq!(exe_name("/usr/bin/KeePassXC"), "keepassxc");
        assert!(exec_argv("").is_empty());
    }

    #[test]
    fn apps_match_by_exe_desktop_id_or_name() {
        assert!(names_app(
            "Dolphin",
            "dolphin",
            "org.kde.dolphin",
            "Dolphin"
        ));
        assert!(names_app(
            "org.kde.dolphin.desktop",
            "dolphin",
            "org.kde.dolphin",
            "Dolphin"
        ));
        assert!(!names_app("", "dolphin", "", ""));
        assert!(!names_app("kate", "dolphin", "org.kde.dolphin", "Dolphin"));
    }

    #[test]
    fn x11_scale_follows_integer_xft_dpi() {
        assert_eq!(
            parse_xft_dpi("Xft.antialias:\t1\nXft.dpi:\t192\n"),
            Some(192.0)
        );
        assert_eq!(parse_xft_dpi("Xft.hinting: 1"), None);
        assert_eq!(scale_from_dpi(192.0), 2.0);
        assert_eq!(scale_from_dpi(144.0), 1.0);
        assert_eq!(scale_from_dpi(96.0), 1.0);
        assert_eq!(scale_from_dpi(f64::NAN), 1.0);
    }
}
