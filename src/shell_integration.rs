//! Shell integration for interactive terminals: small bash/zsh/fish scripts
//! that mark prompts and commands (OSC 133), report the working directory
//! (OSC 7) and set the title, so clients can show prompt marks, follow the
//! directory and tell when a command finished. The scripts ship inside the
//! binary and are written to the data directory on first use.
//!
//! Injection follows the usual terminal approach (as Ghostty and Kitty do):
//! bash gets `--rcfile` (the script loads ~/.bashrc itself), zsh gets a
//! `ZDOTDIR` whose `.zshenv` restores the user's and loads theirs, and fish
//! finds the script as vendor configuration through `XDG_DATA_DIRS`.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use portable_pty::CommandBuilder;

/// Bumped whenever a script changes, so a new daemon never reuses old files.
const VERSION_DIR: &str = "v1";

const FILES: &[(&str, &str)] = &[
    (
        "bash/todex.bash",
        include_str!("shell_integration/bash/todex.bash"),
    ),
    ("zsh/.zshenv", include_str!("shell_integration/zsh/.zshenv")),
    (
        "zsh/todex-integration.zsh",
        include_str!("shell_integration/zsh/todex-integration.zsh"),
    ),
    (
        "fish-xdg/fish/vendor_conf.d/todex-shell-integration.fish",
        include_str!("shell_integration/fish-xdg/fish/vendor_conf.d/todex-shell-integration.fish"),
    ),
];

/// Variables other terminals and multiplexers leave in the daemon's
/// environment; a shell inheriting them would think it runs inside those.
const INHERITED_TERMINAL_VARIABLES: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "TMUX",
    "TMUX_PANE",
    "STY",
    "WINDOWID",
];
const INHERITED_TERMINAL_PREFIXES: &[&str] = &[
    "VSCODE_", "GHOSTTY_", "KITTY_", "WEZTERM_", "ITERM_", "KONSOLE_",
];

/// Writes the scripts under `root` (the daemon data directory) unless they
/// are already there, and returns their directory.
pub async fn install(root: &Path) -> std::io::Result<PathBuf> {
    let dir = root.join("shell-integration").join(VERSION_DIR);
    for (relative, contents) in FILES {
        let path = dir.join(relative);
        if tokio::fs::read(&path).await.ok().as_deref() == Some(contents.as_bytes()) {
            continue;
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Written aside and renamed, so a shell starting meanwhile never
        // reads half a script.
        let partial = path.with_extension("partial");
        tokio::fs::write(&partial, contents).await?;
        tokio::fs::rename(&partial, &path).await?;
    }
    Ok(dir)
}

/// Removes other terminals' variables and names TodeX as the terminal.
pub fn clean_environment(command: &mut CommandBuilder) {
    let inherited = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            key.to_str().is_some_and(|key| {
                INHERITED_TERMINAL_VARIABLES.contains(&key)
                    || INHERITED_TERMINAL_PREFIXES
                        .iter()
                        .any(|prefix| key.starts_with(prefix))
            })
        })
        .collect::<Vec<OsString>>();
    for key in inherited {
        command.env_remove(key);
    }
    command.env("TERM_PROGRAM", "TodeX");
    command.env("TERM_PROGRAM_VERSION", crate::version::APP_VERSION);
}

/// Sets `command` (a plain interactive `shell`) up to load the integration
/// from `dir`. Returns false, leaving it untouched, for other shells.
pub fn apply(command: &mut CommandBuilder, shell: &str, dir: &Path) -> bool {
    let name = Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .trim_start_matches('-');
    match name {
        "bash" => {
            command.arg("--rcfile");
            command.arg(dir.join("bash/todex.bash"));
        }
        "zsh" => {
            if let Some(zdotdir) = std::env::var_os("ZDOTDIR") {
                command.env("TODEX_ZSH_ZDOTDIR", zdotdir);
            }
            command.env("ZDOTDIR", dir.join("zsh"));
        }
        "fish" => {
            // fish reads `<entry>/fish/vendor_conf.d` for each entry.
            let fish_dir = dir.join("fish-xdg");
            let rest = std::env::var_os("XDG_DATA_DIRS")
                .filter(|dirs| !dirs.is_empty())
                .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
            let mut dirs = fish_dir.clone().into_os_string();
            dirs.push(":");
            dirs.push(rest);
            command.env("XDG_DATA_DIRS", dirs);
            command.env("TODEX_FISH_DATA_DIR", fish_dir);
        }
        _ => return false,
    }
    command.env("TODEX_SHELL_INTEGRATION", "1");
    true
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn install_writes_every_script_once() {
        let root = std::env::temp_dir().join(format!(
            "todex-shell-integration-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let dir = install(&root).await.unwrap();
        for (relative, contents) in FILES {
            assert_eq!(
                std::fs::read_to_string(dir.join(relative)).unwrap(),
                *contents
            );
        }
        // A second install leaves matching files alone.
        let modified = std::fs::metadata(dir.join("zsh/.zshenv"))
            .unwrap()
            .modified()
            .unwrap();
        install(&root).await.unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("zsh/.zshenv"))
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn apply_only_handles_known_shells() {
        let dir = Path::new("/data/shell-integration/v1");
        let mut command = CommandBuilder::new("/bin/zsh");
        assert!(apply(&mut command, "/bin/zsh", dir));
        assert_eq!(
            command.get_env("ZDOTDIR"),
            Some(dir.join("zsh").as_os_str())
        );

        let mut command = CommandBuilder::new("/usr/bin/bash");
        assert!(apply(&mut command, "/usr/bin/bash", dir));
        assert_eq!(
            command.get_argv()[1..],
            [
                "--rcfile".into(),
                dir.join("bash/todex.bash").into_os_string()
            ]
        );

        let mut command = CommandBuilder::new("/usr/bin/fish");
        assert!(apply(&mut command, "/usr/bin/fish", dir));
        let dirs = command.get_env("XDG_DATA_DIRS").unwrap().to_str().unwrap();
        assert!(
            dirs.starts_with("/data/shell-integration/v1/fish-xdg:"),
            "{dirs}"
        );

        let mut command = CommandBuilder::new("/bin/sh");
        assert!(!apply(&mut command, "/bin/sh", dir));
        assert!(command.get_env("TODEX_SHELL_INTEGRATION").is_none());
    }
}
