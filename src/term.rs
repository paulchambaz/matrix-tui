//! A small terminal inside the screen, where the user's editor runs.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

/// Rows of the conversation and of the editor when the screen is `height` rows tall;
/// a line between them takes one more.
pub fn split(height: u16) -> (u16, u16) {
    let body = height.saturating_sub(2);
    let top = (body / 2).max(3).min(body.saturating_sub(2)).max(1);
    (top, body.saturating_sub(top + 1).max(1))
}

/// Answers the questions an editor asks its terminal, so that it does not wait for a reply.
/// It claims a dark background, which is what editors assume when nobody answers.
#[derive(Default)]
struct Replies(Vec<u8>);

impl vt100::Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first: Option<u8>,
        second: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let number = params.first().and_then(|p| p.first()).copied();
        let (row, column) = screen.cursor_position();
        let reply = match (first, second, c, number) {
            (None, None, 'c', None | Some(0)) => "\x1b[?62;22c".to_owned(),
            (Some(b'>'), None, 'c', None | Some(0)) => "\x1b[>1;0;0c".to_owned(),
            (Some(b'>'), None, 'q', None | Some(0)) => "\x1bP>|mx\x1b\\".to_owned(),
            (None, None, 'n', Some(5)) => "\x1b[0n".to_owned(),
            (None, None, 'n', Some(6)) => format!("\x1b[{};{}R", row + 1, column + 1),
            (Some(b'?'), None, 'n', Some(996)) => "\x1b[?997;1n".to_owned(),
            (Some(b'?'), Some(b'$'), 'p', Some(mode)) => format!("\x1b[?{mode};0$y"),
            _ => return,
        };
        self.0.extend(reply.into_bytes());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let reply = match params {
            [b"10", b"?"] => "\x1b]10;rgb:c0c0/c0c0/c0c0\x1b\\",
            [b"11", b"?"] => "\x1b]11;rgb:0000/0000/0000\x1b\\",
            _ => return,
        };
        self.0.extend(reply.bytes());
    }
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

fn send(writer: &Mutex<Box<dyn Write + Send>>, bytes: &[u8]) {
    let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = writer.write_all(bytes).and_then(|()| writer.flush());
}

/// An editor running on a pseudo-terminal, with the screen it draws.
pub struct Pane {
    parser: Arc<Mutex<vt100::Parser<Replies>>>,
    writer: Writer,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    file: PathBuf,
    owned: bool,
    title: String,
    size: (u16, u16),
}

fn lock(parser: &Mutex<vt100::Parser<Replies>>) -> MutexGuard<'_, vt100::Parser<Replies>> {
    parser.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Pane {
    /// Starts `editor` on a private temporary file holding `text`, in a terminal of the given size.
    /// `wake` is called with false when the editor printed something and with true when it ended.
    ///
    /// # Errors
    /// Fails when the file or the pseudo-terminal cannot be made, or the editor cannot start.
    pub fn spawn(
        editor: &str,
        text: &str,
        rows: u16,
        cols: u16,
        wake: impl Fn(bool) + Send + 'static,
    ) -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let file = std::env::temp_dir().join(format!("mx-{}-{nanos}.md", std::process::id()));
        let mut handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)
            .context("creating the temporary file")?;
        handle.write_all(text.as_bytes())?;
        drop(handle);

        let started = Self::start(editor, &file, rows, cols, wake);
        if started.is_err() {
            let _ = fs::remove_file(&file);
        }
        started
    }

    /// Runs a viewer on a file that already exists and is left alone afterwards.
    ///
    /// # Errors
    /// Fails when the pseudo-terminal cannot be made or the program cannot start.
    pub fn view(
        command: &str,
        file: &std::path::Path,
        title: &str,
        rows: u16,
        cols: u16,
        wake: impl Fn(bool) + Send + 'static,
    ) -> Result<Self> {
        let mut pane = Self::start(command, &file.to_path_buf(), rows, cols, wake)?;
        pane.owned = false;
        pane.title = title.to_owned();
        Ok(pane)
    }

    /// Whether the pane shows a file instead of editing a message.
    pub fn is_viewer(&self) -> bool {
        !self.owned
    }

    /// What the pane shows, for a viewer.
    pub fn title(&self) -> &str {
        &self.title
    }

    fn start(
        editor: &str,
        file: &PathBuf,
        rows: u16,
        cols: u16,
        wake: impl Fn(bool) + Send + 'static,
    ) -> Result<Self> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(anyhow::Error::msg)?;
        let mut command = CommandBuilder::new("sh");
        command.args(["-c", "exec $0 \"$1\"", editor]);
        command.arg(file);
        command.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("starting {editor}"))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(anyhow::Error::msg)?;
        let writer: Writer = Arc::new(Mutex::new(
            pair.master.take_writer().map_err(anyhow::Error::msg)?,
        ));
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            0,
            Replies::default(),
        )));

        let screen = parser.clone();
        let answers = writer.clone();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let replies = {
                    let mut parser = lock(&screen);
                    parser.process(&buffer[..count]);
                    std::mem::take(&mut parser.callbacks_mut().0)
                };
                if !replies.is_empty() {
                    send(&answers, &replies);
                }
                wake(false);
            }
            wake(true);
        });

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            child,
            file: file.clone(),
            owned: true,
            title: String::new(),
            size: (rows, cols),
        })
    }

    /// The size of the terminal the editor sees, as rows and columns.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Tells the editor the terminal changed size.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.size == (rows, cols) || rows == 0 || cols == 0 {
            return;
        }
        self.size = (rows, cols);
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let _ = self.master.resize(size);
        lock(&self.parser).screen_mut().set_size(rows, cols);
    }

    /// Sends a key to the editor as a terminal would.
    pub fn send_key(&mut self, key: KeyEvent) {
        let application = lock(&self.parser).screen().application_cursor();
        send(&self.writer, &encode_key(key, application));
    }

    /// Sends pasted text, marked as a paste when the editor asked for that.
    pub fn send_paste(&mut self, text: &str) {
        let marked = lock(&self.parser).screen().bracketed_paste();
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bytes = match marked {
            true => format!("\x1b[200~{text}\x1b[201~"),
            false => text,
        };
        send(&self.writer, bytes.as_bytes());
    }

    /// Where the editor's cursor is, as a column and a row, unless it hides it.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        let parser = lock(&self.parser);
        let screen = parser.screen();
        let (row, col) = screen.cursor_position();
        (!screen.hide_cursor()).then_some((col, row))
    }

    /// Waits for the editor to end and returns the text it saved.
    ///
    /// # Errors
    /// Fails when the editor exited with an error or the file cannot be read.
    pub fn finish(mut self) -> Result<String> {
        let status = self.child.wait()?;
        let content = fs::read_to_string(&self.file);
        if self.owned {
            let _ = fs::remove_file(&self.file);
        }
        if !status.success() {
            bail!("the editor exited: {status}");
        }
        Ok(content?.trim_end().to_owned())
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.owned {
            let _ = fs::remove_file(&self.file);
        }
    }
}

fn color(from: vt100::Color) -> Color {
    match from {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

impl Widget for &Pane {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let parser = lock(&self.parser);
        let screen = parser.screen();
        for row in 0..area.height {
            for col in 0..area.width {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut modifier = Modifier::empty();
                modifier.set(Modifier::BOLD, cell.bold());
                modifier.set(Modifier::ITALIC, cell.italic());
                modifier.set(Modifier::UNDERLINED, cell.underline());
                modifier.set(Modifier::REVERSED, cell.inverse());
                let style = Style::new()
                    .fg(color(cell.fgcolor()))
                    .bg(color(cell.bgcolor()))
                    .add_modifier(modifier);
                let text = cell.contents();
                let target = &mut buf[(area.x + col, area.y + row)];
                target.set_symbol(if text.is_empty() { " " } else { text });
                target.set_style(style);
            }
        }
    }
}

/// The bytes a terminal sends for a key; `application` is the editor's cursor-key mode.
pub fn encode_key(key: KeyEvent, application: bool) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let csi = |letter: char| match (modifier, application) {
        (1, true) => format!("\x1bO{letter}"),
        (1, false) => format!("\x1b[{letter}"),
        _ => format!("\x1b[1;{modifier}{letter}"),
    };
    let tilde = |number: u8| match modifier {
        1 => format!("\x1b[{number}~"),
        _ => format!("\x1b[{number};{modifier}~"),
    };
    let escaped = |bytes: &[u8]| {
        let prefix: &[u8] = if alt { b"\x1b" } else { b"" };
        [prefix, bytes].concat()
    };
    match key.code {
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            letter @ 'a'..='z' => escaped(&[letter as u8 - b'a' + 1]),
            ' ' | '@' | '2' => escaped(&[0]),
            '[' | '3' => escaped(&[27]),
            '\\' | '4' => escaped(&[28]),
            ']' | '5' => escaped(&[29]),
            '^' | '6' => escaped(&[30]),
            '_' | '7' | '/' => escaped(&[31]),
            '?' | '8' => escaped(&[127]),
            other => escaped(other.to_string().as_bytes()),
        },
        KeyCode::Char(c) => escaped(c.to_string().as_bytes()),
        KeyCode::Enter => escaped(b"\r"),
        KeyCode::Esc => escaped(b"\x1b"),
        KeyCode::Backspace => escaped(b"\x7f"),
        KeyCode::Tab => escaped(b"\t"),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Up => csi('A').into_bytes(),
        KeyCode::Down => csi('B').into_bytes(),
        KeyCode::Right => csi('C').into_bytes(),
        KeyCode::Left => csi('D').into_bytes(),
        KeyCode::Home => csi('H').into_bytes(),
        KeyCode::End => csi('F').into_bytes(),
        KeyCode::Insert => tilde(2).into_bytes(),
        KeyCode::Delete => tilde(3).into_bytes(),
        KeyCode::PageUp => tilde(5).into_bytes(),
        KeyCode::PageDown => tilde(6).into_bytes(),
        KeyCode::F(n @ 1..=4) => format!("\x1bO{}", (b'O' + n) as char).into_bytes(),
        KeyCode::F(n @ 5..=12) => {
            let number = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n) - 5];
            tilde(number).into_bytes()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc::{channel, Receiver};
    use std::time::Duration;

    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn bytes(code: KeyCode, modifiers: KeyModifiers, application: bool) -> String {
        String::from_utf8(encode_key(key(code, modifiers), application)).unwrap()
    }

    const NONE: KeyModifiers = KeyModifiers::NONE;

    #[test]
    fn keys_become_the_bytes_a_terminal_sends() {
        assert_eq!(bytes(KeyCode::Char('a'), NONE, false), "a");
        assert_eq!(bytes(KeyCode::Char('é'), NONE, false), "é");
        assert_eq!(
            bytes(KeyCode::Char('g'), KeyModifiers::CONTROL, false),
            "\x07"
        );
        assert_eq!(
            bytes(KeyCode::Char('c'), KeyModifiers::CONTROL, false),
            "\x03"
        );
        assert_eq!(bytes(KeyCode::Char('x'), KeyModifiers::ALT, false), "\x1bx");
        assert_eq!(bytes(KeyCode::Enter, NONE, false), "\r");
        assert_eq!(bytes(KeyCode::Esc, NONE, false), "\x1b");
        assert_eq!(bytes(KeyCode::Backspace, NONE, false), "\x7f");
        assert_eq!(bytes(KeyCode::Tab, NONE, false), "\t");
        assert_eq!(
            bytes(KeyCode::BackTab, KeyModifiers::SHIFT, false),
            "\x1b[Z"
        );
        assert_eq!(bytes(KeyCode::Delete, NONE, false), "\x1b[3~");
        assert_eq!(bytes(KeyCode::PageUp, NONE, false), "\x1b[5~");
        assert_eq!(bytes(KeyCode::F(1), NONE, false), "\x1bOP");
        assert_eq!(bytes(KeyCode::F(5), NONE, false), "\x1b[15~");
        assert_eq!(bytes(KeyCode::F(12), NONE, false), "\x1b[24~");
    }

    #[test]
    fn arrows_follow_the_cursor_mode_and_carry_modifiers() {
        assert_eq!(bytes(KeyCode::Up, NONE, false), "\x1b[A");
        assert_eq!(bytes(KeyCode::Up, NONE, true), "\x1bOA");
        assert_eq!(bytes(KeyCode::Home, NONE, false), "\x1b[H");
        assert_eq!(bytes(KeyCode::End, NONE, true), "\x1bOF");
        assert_eq!(
            bytes(KeyCode::Left, KeyModifiers::CONTROL, false),
            "\x1b[1;5D"
        );
        assert_eq!(
            bytes(
                KeyCode::Right,
                KeyModifiers::SHIFT | KeyModifiers::ALT,
                true
            ),
            "\x1b[1;4C"
        );
        assert_eq!(
            bytes(KeyCode::Delete, KeyModifiers::CONTROL, false),
            "\x1b[3;5~"
        );
    }

    #[test]
    fn the_screen_is_split_in_halves_keeping_room_for_both() {
        assert_eq!(split(24), (11, 10));
        assert_eq!(split(25), (11, 11));
        assert_eq!(split(8), (3, 2));
        let (top, pane) = split(5);
        assert!(top >= 1 && pane >= 1);
        let (top, pane) = split(2);
        assert!(top >= 1 && pane >= 1);
    }

    fn script(dir: &tempfile::TempDir, body: &str) -> String {
        let path = dir.path().join("editor.sh");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        format!("sh {}", path.display())
    }

    fn run(body: &str, text: &str) -> (Pane, Receiver<bool>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let editor = script(&dir, body);
        let (tx, rx) = channel();
        let pane = Pane::spawn(&editor, text, 10, 40, move |done| {
            let _ = tx.send(done);
        })
        .unwrap();
        (pane, rx, dir)
    }

    fn until_done(rx: &Receiver<bool>) {
        while !rx.recv_timeout(Duration::from_secs(5)).unwrap() {}
    }

    fn contents(pane: &Pane) -> String {
        lock(&pane.parser).screen().contents()
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn what_the_editor_prints_lands_on_the_screen_with_its_colors() {
        let (pane, rx, _dir) = run("printf 'hello \\033[1;31mred\\033[0m\\nbye'", "");
        until_done(&rx);
        assert_eq!(contents(&pane), "hello red\nbye");
        let parser = lock(&pane.parser);
        let cell = parser.screen().cell(0, 6).unwrap();
        assert_eq!(cell.contents(), "r");
        assert!(cell.bold());
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn typed_keys_reach_the_editor_and_what_it_saves_comes_back() {
        let (mut pane, rx, _dir) = run(
            "IFS= read -r line\nprintf '%s\\n\\n' \"$line\" > \"$1\"",
            "",
        );
        for code in [KeyCode::Char('h'), KeyCode::Char('i'), KeyCode::Enter] {
            pane.send_key(key(code, NONE));
        }
        until_done(&rx);
        assert_eq!(pane.finish().unwrap(), "hi");
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn the_editor_starts_on_the_text_in_a_private_file_that_is_removed_after() {
        let (pane, rx, _dir) = run("stat -c %a \"$1\"; cat \"$1\" > /dev/null", "start");
        until_done(&rx);
        assert_eq!(contents(&pane), "600");
        let file = pane.file.clone();
        assert!(file.exists());
        pane.finish().unwrap();
        assert!(!file.exists());
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn a_failing_editor_is_an_error_and_a_missing_one_cannot_start() {
        let (pane, rx, _dir) = run("exit 3", "x");
        until_done(&rx);
        assert!(pane.finish().is_err());
        let (tx, rx) = channel();
        let missing = Pane::spawn("definitely-not-an-editor", "", 5, 20, move |d| {
            let _ = tx.send(d);
        })
        .unwrap();
        until_done(&rx);
        assert!(missing.finish().is_err());
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn dropping_a_running_editor_kills_it_and_removes_the_file() {
        let (pane, _rx, _dir) = run("sleep 30", "x");
        let pid = pane.child.process_id().unwrap();
        let file = pane.file.clone();
        drop(pane);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert!(!file.exists());
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn a_resize_reaches_the_editor_and_its_screen() {
        let (mut pane, rx, _dir) = run("sleep 1\nstty size", "");
        pane.resize(6, 30);
        assert_eq!(pane.size(), (6, 30));
        assert_eq!(lock(&pane.parser).screen().size(), (6, 30));
        until_done(&rx);
        assert!(contents(&pane).contains("6 30"), "{}", contents(&pane));
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn a_paste_is_marked_only_when_the_editor_asked_for_it() {
        let (mut pane, rx, _dir) = run(
            "printf '\\033[?2004h'; IFS= read -r line; printf '%s' \"$line\" > \"$1\"",
            "",
        );
        std::thread::sleep(Duration::from_millis(200));
        pane.send_paste("a\nb");
        pane.send_key(key(KeyCode::Enter, NONE));
        until_done(&rx);
        let text = pane.finish().unwrap();
        assert!(text.contains("200~a"), "{text:?}");
    }

    fn reply_to(query: &str, length: usize) -> Vec<u8> {
        let body = format!(
            "stty raw -echo\nprintf '{query}'\ndd bs=1 count={length} 2>/dev/null > \"$1\""
        );
        let (pane, rx, _dir) = run(&body, "");
        until_done(&rx);
        fs::read(&pane.file).unwrap()
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn the_editor_gets_the_answers_to_its_questions_about_the_terminal() {
        assert_eq!(reply_to("\\033[c", 9), b"\x1b[?62;22c");
        assert_eq!(reply_to("ab\\033[6n", 6), b"\x1b[1;3R");
        assert_eq!(reply_to("\\033[5n", 4), b"\x1b[0n");
        assert_eq!(reply_to("\\033[?996n", 9), b"\x1b[?997;1n");
        assert_eq!(reply_to("\\033[?2026$p", 11), b"\x1b[?2026;0$y");
        assert_eq!(
            reply_to("\\033]11;?\\033\\\\", 25),
            b"\x1b]11;rgb:0000/0000/0000\x1b\\"
        );
        assert_eq!(
            reply_to("\\033]10;?\\033\\\\", 25),
            b"\x1b]10;rgb:c0c0/c0c0/c0c0\x1b\\"
        );
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn a_viewer_shows_a_file_and_leaves_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        fs::write(&file, "kept").unwrap();
        let editor = script(&dir, "cat \"$1\"");
        let (tx, rx) = channel();
        let pane = Pane::view(&editor, &file, "note.txt", 6, 30, move |done| {
            let _ = tx.send(done);
        })
        .unwrap();
        assert!(pane.is_viewer() && pane.title() == "note.txt");
        until_done(&rx);
        assert_eq!(contents(&pane), "kept");
        drop(pane);
        assert_eq!(fs::read_to_string(&file).unwrap(), "kept");
    }
}
