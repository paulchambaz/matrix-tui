//! Keyboard-driven terminal client for Matrix.

#![recursion_limit = "256"]

mod api;
mod app;
mod cli;
mod clipboard;
mod config;
mod debug;
mod dump;
mod finder;
mod input;
mod markdown;
mod model;
mod open;
mod query;
mod render;
mod sync;
mod term;
mod ui;
mod unread;

use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use app::{App, Msg};
use cli::Cli;
use ratatui::backend::Backend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::crossterm::cursor::{RestorePosition, SavePosition};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

/// Runs a command that prints and exits, without ever drawing on the terminal.
async fn command(request: cli::Request) -> Result<ExitCode> {
    let servers = config::load(&config::default_path()?)?;
    let settings = config::load_settings(&config::settings_path()?)?;
    let report = match dump::execute(&request.command, &servers, &settings.open.browser).await {
        Ok(report) => report,
        Err(error) => {
            eprintln!("{}", render::failure(&error, request.format));
            return Ok(ExitCode::from(error.status()));
        }
    };
    match render::print(
        &report.output,
        request.format,
        &mut std::io::stdout().lock(),
    ) {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => return Err(error.into()),
        _ => {}
    }
    for notice in &report.notices {
        eprintln!("{notice}");
    }
    for failure in &report.failures {
        eprintln!("{}", render::failure(failure, request.format));
    }
    Ok(ExitCode::from(u8::from(!report.failures.is_empty())))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args, chrono::Local::now()) {
        Ok(Cli::Run) => {}
        Ok(Cli::Help(text)) => {
            println!("{text}");
            return Ok(ExitCode::SUCCESS);
        }
        Ok(Cli::Version) => {
            println!("mx {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        Ok(Cli::Request(request)) => return command(request).await,
        Err(error) => {
            eprintln!("mx: {}: {error}\ntry mx -h", error.code());
            return Ok(ExitCode::from(error.status()));
        }
    }
    let path = config::default_path()?;
    let settings = config::load_settings(&config::settings_path()?)?;
    let boot = config::load(&path)?
        .into_iter()
        .map(|server| {
            let credentials = server
                .credentials(&settings.open.browser)
                .map_err(|e| e.to_string());
            (server, credentials)
        })
        .collect();
    let (tx, rx) = unbounded_channel();

    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    let keys = tx.clone();
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            let msg = match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => Msg::Key(key),
                Event::Resize(..) => Msg::Resize,
                Event::Paste(text) => Msg::Paste(text),
                _ => continue,
            };
            if keys.send(msg).is_err() {
                break;
            }
        }
    });
    let mut app = App::new(boot, path, tx);
    app.settings = settings;
    app.plain = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    let result = run(&mut terminal, &mut app, rx).await;
    drop(app);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result.map(|()| ExitCode::SUCCESS)
}

/// The colored emoji of a frame with where they are, such as the red heart.
fn colored_emoji(buffer: &Buffer) -> Vec<(u16, u16, Cell)> {
    let width = usize::from(buffer.area.width).max(1);
    let at = |i: usize| (u16::try_from(i % width), u16::try_from(i / width));
    buffer
        .content
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.symbol().contains(markdown::COLORED))
        .filter_map(|(i, cell)| match at(i) {
            (Ok(x), Ok(y)) => Some((x, y, cell.clone())),
            _ => None,
        })
        .collect()
}

/// Paints the cell after each colored emoji, then the emoji again, so that a terminal which
/// draws it one cell wide does not leave that cell unpainted while ratatui counts two.
fn settle_colored_emoji(
    terminal: &mut DefaultTerminal,
    colored: &[(u16, u16, Cell)],
) -> Result<()> {
    if colored.is_empty() {
        return Ok(());
    }
    let blanks: Vec<Cell> = colored
        .iter()
        .map(|(_, _, cell)| {
            let mut blank = cell.clone();
            blank.set_symbol(" ");
            blank
        })
        .collect();
    let backend = terminal.backend_mut();
    execute!(backend, SavePosition)?;
    let cells = colored
        .iter()
        .zip(&blanks)
        .flat_map(|((x, y, emoji), blank)| [(x + 1, *y, blank), (*x, *y, emoji)]);
    backend.draw(cells)?;
    execute!(backend, RestorePosition)?;
    Backend::flush(backend)?;
    Ok(())
}

async fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    mut rx: UnboundedReceiver<Msg>,
) -> Result<()> {
    while !app.quit {
        let frame = terminal.draw(|frame| ui::draw(frame, app))?;
        let colored = colored_emoji(frame.buffer);
        settle_colored_emoji(terminal, &colored)?;
        let Some(msg) = rx.recv().await else {
            break;
        };
        app.update(msg);
        while let Ok(msg) = rx.try_recv() {
            app.update(msg);
        }
        for sequence in app.clipboard.drain(..) {
            let mut out = std::io::stdout();
            let _ = out
                .write_all(sequence.as_bytes())
                .and_then(|()| out.flush());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colored_emoji_are_found_with_their_place_and_style() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 6, 2));
        buffer.set_string(
            1,
            1,
            "a\u{2764}\u{fe0f}b",
            ratatui::style::Style::new().bold(),
        );
        let found = colored_emoji(&buffer);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].0, found[0].1), (2, 1));
        assert_eq!(found[0].2.symbol(), "\u{2764}\u{fe0f}");
    }
}
