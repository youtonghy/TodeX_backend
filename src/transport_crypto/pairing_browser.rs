//! Private, short-lived browser presentation for pairing QRs. Retain the
//! returned handle until the TUI exits; Drop removes the page from disk.
use crate::error::AppError;
use std::{fs, io::Write, path::PathBuf, process::Stdio, time::Duration};

pub(crate) struct PairingQrBrowserPage {
    directory: PathBuf,
    path: PathBuf,
}

impl Drop for PairingQrBrowserPage {
    fn drop(&mut self) {
        // Do not recursively remove anything: only the file we created and its
        // now-empty private directory belong to this handle.
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

impl PairingQrBrowserPage {
    fn create(html: &str) -> Result<Self, AppError> {
        let parent = fs::canonicalize(std::env::temp_dir())?;
        let directory = parent.join(format!("todex-pairing-{}", uuid::Uuid::new_v4()));
        let builder = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                builder
            }
            #[cfg(not(unix))]
            {
                fs::DirBuilder::new()
            }
        };
        builder.create(&directory)?;
        let page = Self {
            path: directory.join("pairing.html"),
            directory,
        };
        #[cfg(windows)]
        let owner = current_windows_sid()?;
        #[cfg(windows)]
        secure_windows_path(&page.directory, &owner, true)?;
        #[cfg(not(any(unix, windows)))]
        return Err(AppError::Unsupported(
            "Private pairing pages are unsupported on this platform".to_owned(),
        ));

        #[cfg(any(unix, windows))]
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&page.path)?;
            #[cfg(windows)]
            secure_windows_path(&page.path, &owner, false)?;
            // Write sensitive content only after the file and containing
            // directory have their owner-only permissions in place.
            file.write_all(html.as_bytes())?;
            file.sync_all()?;
            Ok(page)
        }
    }
}

pub(crate) async fn open_pairing_qr_browser(
    payloads: &[String],
) -> Result<PairingQrBrowserPage, AppError> {
    let html = super::render_pairing_qr_browser_html(payloads)?;
    let page = tokio::task::spawn_blocking(move || PairingQrBrowserPage::create(&html))
        .await
        .map_err(|_| {
            AppError::InvalidRequest("Unable to prepare private pairing page".to_owned())
        })??;
    let mut command = browser_command(&page.path)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| {
        AppError::InvalidRequest("Unable to open the default browser on this machine".to_owned())
    })?;
    // Linux launchers can remain attached until the browser exits. A launcher
    // that stays alive is a successful dispatch; never kill the user's browser.
    match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(page),
        Err(_) => Ok(page),
        _ => Err(AppError::InvalidRequest(
            "The default browser could not open the pairing page".to_owned(),
        )),
    }
}

fn browser_command(path: &std::path::Path) -> Result<tokio::process::Command, AppError> {
    // Pass a file URL as one argument. No command shell or payload text is used.
    let url = reqwest::Url::from_file_path(path).map_err(|_| {
        AppError::InvalidRequest("Unable to locate private pairing page".to_owned())
    })?;
    #[cfg(target_os = "macos")]
    let mut command = tokio::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = tokio::process::Command::new("xdg-open");
    #[cfg(windows)]
    let mut command = {
        let mut command = tokio::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(any(unix, windows))]
    {
        command.arg(url.as_str());
        Ok(command)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = url;
        Err(AppError::Unsupported(
            "No browser launcher for this platform".to_owned(),
        ))
    }
}

#[cfg(windows)]
fn current_windows_sid() -> Result<String, AppError> {
    let output = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| {
            AppError::InvalidRequest("Unable to determine private file ownership".to_owned())
        })?;
    if !output.status.success() {
        return Err(AppError::InvalidRequest(
            "Unable to determine private file ownership".to_owned(),
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .split(',')
        .map(|part| part.trim().trim_matches('"'))
        .find(|part| {
            part.starts_with("S-1-")
                && part
                    .bytes()
                    .all(|byte| byte == b'S' || byte == b'-' || byte.is_ascii_digit())
        })
        .map(str::to_owned)
        .ok_or_else(|| {
            AppError::InvalidRequest("Unable to determine private file ownership".to_owned())
        })
}

#[cfg(windows)]
fn secure_windows_path(
    path: &std::path::Path,
    owner: &str,
    directory: bool,
) -> Result<(), AppError> {
    let permission = if directory {
        format!("*{owner}:(OI)(CI)F")
    } else {
        format!("*{owner}:F")
    };
    let status = std::process::Command::new("icacls.exe")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(permission)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| {
            AppError::InvalidRequest("Unable to protect the private pairing page".to_owned())
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(
            "Unable to protect the private pairing page".to_owned(),
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn browser_page_is_private_and_deleted_when_handle_drops() {
        let page = PairingQrBrowserPage::create("synthetic pairing page").unwrap();
        let directory = page.directory.clone();
        let path = page.path.clone();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "synthetic pairing page");
        drop(page);
        assert!(!path.exists());
        assert!(!directory.exists());
    }
}
