//! Links, attachments and the programs that open them.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The kinds of file that have their own program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Video,
    Text,
    Other,
}

const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "json", "toml", "yaml", "yml", "xml", "csv", "log", "ini", "conf",
    "rs", "py", "c", "h", "cpp", "hpp", "go", "js", "ts", "java", "sh", "lua", "nix", "sql", "tex",
    "diff", "patch",
];

/// What kind of file this is, from its type and its name.
pub fn kind(mime: &str, name: &str) -> Kind {
    let extension = name.rsplit_once('.').map_or("", |(_, e)| e).to_lowercase();
    let primary = mime.split('/').next().unwrap_or("");
    match primary {
        "image" => Kind::Image,
        "video" | "audio" => Kind::Video,
        "text" => Kind::Text,
        _ if TEXT_EXTENSIONS.contains(&extension.as_str()) => Kind::Text,
        _ => Kind::Other,
    }
}

/// The web addresses in a text, in order, without trailing punctuation.
pub fn links(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut found: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let starts = chars[i] == 'h' && (i == 0 || !chars[i - 1].is_alphanumeric());
        let len = if starts {
            crate::markdown::url_len(&chars[i..])
        } else {
            0
        };
        if len > 0 {
            let url: String = chars[i..i + len].iter().collect();
            if !found.contains(&url) {
                found.push(url);
            }
            i += len;
        } else {
            i += 1;
        }
    }
    found
}

/// A file name that cannot leave its folder.
pub fn safe_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = last.chars().filter(|c| !c.is_control()).collect();
    match clean.trim_matches('.') {
        "" => "file".into(),
        _ => clean,
    }
}

/// A size in bytes written for people.
pub fn human_size(bytes: i64) -> String {
    let bytes = bytes.max(0) as f64;
    match bytes {
        b if b < 1024.0 => format!("{b:.0} B"),
        b if b < 1024.0 * 1024.0 => format!("{:.1} KB", b / 1024.0),
        b if b < 1024.0 * 1024.0 * 1024.0 => format!("{:.1} MB", b / 1024.0 / 1024.0),
        b => format!("{:.1} GB", b / 1024.0 / 1024.0 / 1024.0),
    }
}

/// The private folder where downloaded attachments go, removed with the program.
#[derive(Debug)]
pub struct Files {
    base: PathBuf,
}

static FOLDERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Files {
    /// A folder named after this process, created when the first file is saved.
    pub fn new() -> Self {
        Self {
            base: std::env::temp_dir().join(format!(
                "mx-{}-{}",
                std::process::id(),
                FOLDERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )),
        }
    }

    /// Where attachments are saved.
    pub fn base(&self) -> &Path {
        &self.base
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// Saves a file under `base/<id>/<name>`, readable by its owner only.
///
/// # Errors
/// Fails when the folder or the file cannot be made.
pub fn save(base: &Path, id: &str, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let folder = base.join(safe_name(id));
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)?;
    let path = folder.join(safe_name(name));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(bytes)?;
    Ok(path)
}

/// Starts a program on a file or address without waiting for it; `done` hears how it ended.
///
/// # Errors
/// Fails when the shell cannot be started.
pub fn start(
    command: &str,
    target: &str,
    done: impl FnOnce(Option<String>) + Send + 'static,
) -> std::io::Result<()> {
    let mut child = Command::new("sh")
        .args(["-c", "exec $0 \"$1\"", command, target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let command = command.to_owned();
    std::thread::spawn(move || {
        let outcome = match child.wait() {
            Ok(status) if status.success() => None,
            Ok(status) => Some(format!("{command} exited: {status}")),
            Err(e) => Some(e.to_string()),
        };
        done(outcome);
    });
    Ok(())
}

/// Runs the user's notification command, which finds the title in `$1` and the text in `$2`.
///
/// # Errors
/// Fails when the shell cannot be started.
pub fn notify(command: &str, title: &str, text: &str) -> std::io::Result<()> {
    let mut child = Command::new("sh")
        .args(["-c", command, "mx", title, text])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use super::*;

    #[test]
    fn files_are_sorted_by_type_then_by_extension() {
        assert_eq!(kind("image/png", "a.png"), Kind::Image);
        assert_eq!(kind("video/mp4", "a.mp4"), Kind::Video);
        assert_eq!(kind("audio/ogg", "a.ogg"), Kind::Video);
        assert_eq!(kind("text/plain", "a.txt"), Kind::Text);
        assert_eq!(kind("application/octet-stream", "main.RS"), Kind::Text);
        assert_eq!(kind("application/json", "data.json"), Kind::Text);
        assert_eq!(kind("application/pdf", "report.pdf"), Kind::Other);
        assert_eq!(kind("", "noextension"), Kind::Other);
    }

    #[test]
    fn links_are_found_in_order_without_punctuation_or_duplicates() {
        let text = "see https://a.org/x, then (https://b.org/y_(z)) and [t](http://c.org/q?a=1). again https://a.org/x!";
        assert_eq!(
            links(text),
            [
                "https://a.org/x",
                "https://b.org/y_(z)",
                "http://c.org/q?a=1"
            ]
        );
        assert!(links("nothing, not even xhttps://x.org").is_empty());
        assert!(links("http://").is_empty());
    }

    #[test]
    fn names_cannot_escape_their_folder_and_sizes_are_readable() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("a\\b.txt"), "b.txt");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name("ok name.png"), "ok name.png");
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_size(-4), "0 B");
    }

    #[test]
    fn saved_files_are_private_and_the_folder_goes_with_the_session() {
        let base = {
            let files = Files::new();
            let path = save(files.base(), "id1", "../x.png", b"abc").unwrap();
            assert_eq!(path, files.base().join("id1").join("x.png"));
            assert_eq!(fs::read(&path).unwrap(), b"abc");
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let folder = fs::metadata(path.parent().unwrap()).unwrap();
            assert_eq!(folder.permissions().mode() & 0o777, 0o700);
            files.base().to_owned()
        };
        assert!(!base.exists());
    }

    #[test]
    fn a_program_is_started_detached_and_its_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("p.sh");
        fs::write(&script, "#!/bin/sh\nprintf '%s' \"$1\" > \"$1.out\"\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let target = dir.path().join("t").to_string_lossy().into_owned();
        let (tx, rx) = channel();
        start(&format!("sh {}", script.display()), &target, move |o| {
            let _ = tx.send(o);
        })
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), None);
        assert_eq!(fs::read_to_string(format!("{target}.out")).unwrap(), target);
        let (tx, rx) = channel();
        start("definitely-not-a-program", "x", move |o| {
            let _ = tx.send(o);
        })
        .unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().is_some());
    }
}
