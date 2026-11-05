//! Copying text to the system clipboard through the terminal.

const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let [a, b, c] = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(a) << 16 | u32::from(b) << 8 | u32::from(c);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            let used = match i {
                0 | 1 => true,
                2 => chunk.len() > 1,
                _ => chunk.len() > 2,
            };
            out.push(if used {
                DIGITS[(n >> shift & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// The command that copies to the system clipboard: the one set, else one that suits the session.
pub fn tool(
    setting: Option<&str>,
    wayland: bool,
    x11: bool,
    has: impl Fn(&str) -> bool,
) -> Option<String> {
    if let Some(command) = setting.filter(|c| !c.is_empty()) {
        return Some(command.to_owned());
    }
    let candidates: [(bool, &str, &str); 3] = [
        (wayland, "wl-copy", "wl-copy"),
        (x11, "xclip", "xclip -selection clipboard"),
        (x11, "xsel", "xsel --clipboard --input"),
    ];
    let found = candidates
        .iter()
        .find(|(usable, program, _)| *usable && has(program));
    found.map(|(_, _, command)| (*command).to_owned())
}

/// Whether a program is found in one of the folders of `$PATH`.
pub fn on_path(program: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|folder| folder.join(program).is_file())
}

/// Gives `text` to a clipboard command and tells how it went.
///
/// # Errors
/// Fails when the shell cannot be started.
pub fn copy(
    command: &str,
    text: &str,
    done: impl FnOnce(Option<String>) + Send + 'static,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("sh")
        .args(["-c", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let (command, text) = (command.to_owned(), text.to_owned());
    std::thread::spawn(move || {
        if let Some(mut input) = child.stdin.take() {
            let _ = input.write_all(text.as_bytes());
        }
        done(match child.wait() {
            Ok(status) if status.success() => None,
            Ok(status) => Some(format!("{command} exited: {status}")),
            Err(e) => Some(e.to_string()),
        });
    });
    Ok(())
}

/// The sequence that asks a terminal to copy `text` to the system clipboard.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_follows_the_rfc_vectors() {
        for (text, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(text.as_bytes()), expected, "{text}");
        }
    }

    #[test]
    fn non_ascii_text_and_the_sequence() {
        assert_eq!(base64("héllo 🎉".as_bytes()), "aMOpbGxvIPCfjok=");
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn the_clipboard_command_is_the_setting_or_what_the_session_has() {
        let has = |p: &str| matches!(p, "xclip" | "wl-copy");
        assert_eq!(
            tool(Some("mine -x"), true, true, has).as_deref(),
            Some("mine -x")
        );
        assert_eq!(tool(None, true, true, has).as_deref(), Some("wl-copy"));
        assert_eq!(
            tool(None, false, true, has).as_deref(),
            Some("xclip -selection clipboard")
        );
        assert_eq!(
            tool(Some(""), false, true, |p| p == "xsel").as_deref(),
            Some("xsel --clipboard --input")
        );
        assert_eq!(tool(None, false, false, has), None);
        assert_eq!(tool(None, true, false, |p| p == "xclip"), None);
    }

    #[test]
    fn the_text_reaches_the_command_on_its_input_and_a_failure_is_reported() {
        use std::sync::mpsc::channel;
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let (tx, rx) = channel();
        copy(
            &format!("cat > {}", out.display()),
            "héllo\nworld",
            move |o| {
                let _ = tx.send(o);
            },
        )
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), None);
        assert_eq!(std::fs::read_to_string(out).unwrap(), "héllo\nworld");
        let (tx, rx) = channel();
        copy("exit 3", "x", move |o| {
            let _ = tx.send(o);
        })
        .unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().is_some());
        assert!(on_path("sh") && !on_path("definitely-not-a-program"));
    }
}
