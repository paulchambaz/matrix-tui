//! Drawing: the tab strip, the log, the status line, the input row and the help page.

use std::time::Instant;

use chrono::Local;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::api::types::{ChannelKind, Status};
use crate::app::commands::Completion;
use crate::app::{App, Item, Kind, REACTIONS};
use crate::finder;
use crate::markdown;
use crate::model::{self, group_with_previous, Link, Message, Server};
use crate::open;
use crate::term;

const MARGIN: usize = 1;
const TIME: usize = 5;
const NICK: usize = 16;
const MIN_TEXT: usize = 10;
const USERS_WIDTH: u16 = 24;
const USERS_MIN_WIDTH: u16 = 70;

const BINDINGS: &[(&str, &str)] = &[
    (
        "l h",
        "next / previous message; h at the top loads older ones",
    ),
    (
        "j k",
        "scroll the log down / up by a line; h l then select the nearest message on screen",
    ),
    ("g G", "first / last message; g also loads older ones"),
    ("w b", "next / previous author group"),
    ("J K", "next / previous channel"),
    ("H L", "previous / next team, across servers"),
    (":", "command line"),
    ("?", "this help"),
    ("n", "next unread message, then next unread channel"),
    ("N", "next mention, across all teams and servers"),
    ("c", "mark the open channel as read"),
    ("f F", "find a channel: f in this team, F everywhere"),
    (
        "i",
        "write a message: enter sends, esc leaves (the text is kept)",
    ),
    (
        "ctrl-g",
        "write the message in $EDITOR, in the lower half of the screen",
    ),
    ("r", "reply in the thread of the selected message"),
    ("e", "edit your selected message"),
    ("d", "delete your selected message, y confirms"),
    ("a", "reactions under the messages"),
    (
        "x X",
        "react to the selected message (x: pick 1-9, X: the same again)",
    ),
    ("V", "select a range of messages, h l extend it"),
    ("y Y", "copy the message or range (Y: with author and time)"),
    ("p", "paste what was copied into the input"),
    ("m", "users column: scroll it with j k, m again hides it"),
    ("q", "quit"),
];

const FINDER: &[(&str, &str)] = &[
    (
        "words",
        "all must match the channel name (F: or the server:team)",
    ),
    ("@x", "direct and group messages matching x"),
    (
        "a#b",
        "channel b in the team a; #b any team, a# the team only",
    ),
    ("tab, arrows", "choose"),
    ("enter, esc", "go there, close"),
];

const COMMANDS: &[(&str, &str)] = &[
    ("new-server", "log in to another server"),
    ("remove-server n", "forget a server"),
    ("server n", "go to a server by name"),
    ("team n", "go to a team of this server by name"),
    ("go words", "open a channel, as f does"),
    ("dm user...", "open a direct or group conversation"),
    ("join room", "join a room by its alias, its id or its name"),
    ("leave", "leave the open channel"),
    (
        "topic [text]",
        "show the header and purpose, or set the topic",
    ),
    (
        "create [public] n",
        "create a room, invite-only and encrypted unless public",
    ),
    (
        "space new|add|remove",
        "create a space, or put the open room in one or take it out",
    ),
    ("invite user", "invite a user into the open room"),
    ("kick user", "remove a user from the open room"),
    ("ban user", "bar a user from the open room"),
    ("unban user", "let a banned user join again"),
    ("rename name", "rename the open room"),
    ("nick name", "change your display name"),
    ("ignore user", "hide everything a user says"),
    ("unignore user", "show a user again"),
    (
        "report reason",
        "report the selected message to the moderators",
    ),
    (
        "search text",
        "list the messages of the room that hold some text",
    ),
    ("reload", "fetch the channel and its messages again"),
    ("mark-read", "mark the open channel as read (c)"),
    ("mark-all-read", "mark every channel of the team as read"),
    ("users", "users column (m)"),
    ("reactions", "reactions under the messages (a)"),
    ("react name", "react with any emoji by name (x)"),
    ("pin", "pin the selected message"),
    ("unpin", "unpin the selected message"),
    ("open", "open links and attachments (o)"),
    ("copy [full]", "copy the message (y), full: with author (Y)"),
    ("upload path", "send a file or an image to the channel"),
    ("location lat lon", "send a place to the channel"),
    ("status s", "online, away, dnd or offline"),
    (
        "recover key",
        "unlock old encrypted messages with the recovery key",
    ),
    ("recovery", "create the recovery key, or replace it"),
    ("sessions", "list the devices logged in to the account"),
    ("logout id", "log another device out"),
    ("password", "change the password of the account"),
    (
        "deactivate id",
        "close the account for good, given your full user id",
    ),
    (
        "verify [user]",
        "verify a user, or this session against another one",
    ),
    ("trust user", "accept the new keys of a user"),
    ("set key value", "change a setting until you quit"),
    ("help", "this help"),
    ("quit", "quit"),
];

fn verify_lines(emojis: &[(String, String)]) -> Vec<Line<'static>> {
    let title = Line::from(vec![
        Span::styled(" Verify session", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled("   y they match   n esc they differ", muted()),
    ]);
    let rows = emojis.iter().map(|(symbol, name)| {
        Line::from(vec![
            Span::raw(format!("  {symbol}  ")),
            Span::styled(name.clone(), muted()),
        ])
    });
    std::iter::once(title)
        .chain(std::iter::once(Line::from(
            " The other session shows the same seven emojis?",
        )))
        .chain(rows)
        .collect()
}

fn muted() -> Style {
    Style::new().fg(Color::Indexed(8))
}

/// Cuts `s` to at most `width` cells, ending with an ellipsis when something was cut.
pub fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w >= width {
            break;
        }
        out.push(c);
        used += w;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

/// The first item to show so that `active` fits in `room` cells.
fn first_visible(widths: &[usize], active: usize, room: usize) -> usize {
    (0..active)
        .find(|&first| widths[first..=active].iter().sum::<usize>() <= room)
        .unwrap_or(active)
}

/// The scroll that shows the span of message `index`, moving as little as it can.
fn scrolled(
    scroll: usize,
    span: (usize, usize),
    index: usize,
    count: usize,
    height: usize,
    total: usize,
) -> usize {
    let bottom = total.saturating_sub(height);
    let (start, end) = span;
    if index == 0 {
        0
    } else if index + 1 == count {
        bottom
    } else if start < scroll {
        start
    } else if end >= scroll + height {
        match end - start >= height {
            true => start,
            false => end + 1 - height,
        }
    } else {
        scroll
    }
}

struct Columns {
    time: usize,
    nick: usize,
    text: usize,
}

fn columns(width: usize) -> Columns {
    let text_for = |time: usize, nick: usize| {
        let gap = if time > 0 { 1 } else { 0 };
        width.saturating_sub(2 * MARGIN + time + gap + nick + 2)
    };
    let (time, mut nick) = match text_for(TIME, NICK) >= MIN_TEXT {
        true => (TIME, NICK),
        false => (0, NICK),
    };
    let text = text_for(time, nick);
    if text < MIN_TEXT {
        nick = nick.saturating_sub(MIN_TEXT - text).max(4);
    }
    Columns {
        time,
        nick,
        text: text_for(time, nick).max(MIN_TEXT),
    }
}

fn pad_left(s: &str, width: usize) -> String {
    format!("{}{s}", " ".repeat(width.saturating_sub(s.width())))
}

fn pad_right(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(s.width())))
}

fn mention_marks(text: &markdown::Text, me: &str) -> Vec<u8> {
    let c = &text.chars;
    let mut marks = vec![0u8; c.len()];
    let mut i = 0;
    while i < c.len() {
        let free = text.flags[i] & markdown::CODE == 0 && (i == 0 || !c[i - 1].is_alphanumeric());
        if c[i] == '@' && free {
            let mut j = i + 1;
            while j < c.len() && (c[j].is_alphanumeric() || matches!(c[j], '.' | '_' | '-')) {
                j += 1;
            }
            while j > i + 1 && matches!(c[j - 1], '.' | '-') {
                j -= 1;
            }
            if j > i + 1 {
                let name: String = c[i + 1..j].iter().collect();
                let mine = name == me || matches!(name.as_str(), "all" | "channel" | "here");
                marks[i..j].fill(if mine { 2 } else { 1 });
                i = j;
                continue;
            }
        }
        i += 1;
    }
    marks
}

fn style_of(flags: markdown::Flags, mention: u8, base: Style) -> Style {
    let mut style = base;
    for (flag, modifier) in [
        (markdown::BOLD, Modifier::BOLD),
        (markdown::ITALIC, Modifier::ITALIC),
        (markdown::STRIKE, Modifier::CROSSED_OUT),
        (markdown::LINK, Modifier::UNDERLINED),
    ] {
        if flags & flag != 0 {
            style = style.add_modifier(modifier);
        }
    }
    if flags & markdown::CODE != 0 {
        style = style.fg(Color::Indexed(6));
    }
    if flags & (markdown::QUOTE | markdown::MUTED) != 0 {
        style = style.fg(Color::Indexed(8));
    }
    match mention {
        1 => style.fg(Color::Indexed(12)).add_modifier(Modifier::BOLD),
        2 => style.fg(Color::Indexed(11)).add_modifier(Modifier::BOLD),
        _ => style,
    }
}

fn text_spans(
    text: &markdown::Text,
    marks: &[u8],
    range: std::ops::Range<usize>,
    base: Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut start = range.start;
    while start < range.end {
        let key = (text.flags[start], marks[start]);
        let end = (start..range.end)
            .find(|&k| (text.flags[k], marks[k]) != key)
            .unwrap_or(range.end);
        let run: String = text.chars[start..end].iter().collect();
        spans.push(Span::styled(run, style_of(key.0, key.1, base)));
        start = end;
    }
    spans
}

struct Log {
    lines: Vec<Line<'static>>,
    spans: Vec<(usize, usize)>,
}

fn separator(text: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::raw(" ".repeat(MARGIN)),
        Span::styled(format!("─── {text}"), style),
    ])
}

fn notice_log(text: &str) -> Log {
    Log {
        lines: vec![Line::styled(format!(" {text}"), muted())],
        spans: Vec::new(),
    }
}

fn render_log(app: &App, width: usize) -> Log {
    let Some((server, chan)) = app.channel() else {
        return notice_log(match app.servers.is_empty() {
            true => "No server yet. Type :new-server to log in.",
            false => "Loading…",
        });
    };
    if let Some(inviter) = &chan.info.invited {
        let who = server.username(inviter);
        let first = match chan.info.kind {
            ChannelKind::Direct | ChannelKind::Group => {
                format!("{who} wants to start a direct conversation with you.")
            }
            _ => format!("{who} invited you to {}.", server.label(chan)),
        };
        return Log {
            lines: vec![
                Line::styled(format!(" {first}"), muted()),
                Line::styled(
                    " Type :join to accept or :leave to decline.".to_owned(),
                    muted(),
                ),
            ],
            spans: Vec::new(),
        };
    }
    let Some(messages) = app.messages() else {
        return notice_log("Loading…");
    };
    if messages.is_empty() {
        return notice_log(&format!("No messages in {}", server.label(chan)));
    }
    let start = server
        .complete
        .contains(&chan.info.id)
        .then(|| format!("Start of {}", server.label(chan)));
    messages_log(app, server, messages, start, width)
}

fn messages_log(
    app: &App,
    server: &Server,
    messages: &[Message],
    start: Option<String>,
    width: usize,
) -> Log {
    let cols = columns(width);
    let now = Local::now();
    let me = server.me.as_ref().map_or("", |u| u.username.as_str());
    let unread_at = app
        .unread_boundary()
        .and_then(|id| messages.iter().position(|m| &m.id == id));
    let indent = " ".repeat(MARGIN + cols.time + usize::from(cols.time > 0) + cols.nick + 2);
    let mut log = Log {
        lines: Vec::new(),
        spans: Vec::new(),
    };

    if let Some(start) = start {
        log.lines.push(separator(&start, muted()));
    }
    for (i, message) in messages.iter().enumerate() {
        let previous = i.checked_sub(1).map(|p| &messages[p]);
        let start = log.lines.len();
        if previous.is_none_or(|p| p.at.date_naive() != message.at.date_naive()) {
            log.lines
                .push(separator(&model::day_label(message.at, now), muted()));
        }
        if unread_at == Some(i) {
            log.lines.push(separator(
                "new messages",
                Style::new().fg(Color::Indexed(1)),
            ));
        }
        let continuing = previous.is_some_and(|p| group_with_previous(p, message));

        let color = Color::Indexed(model::user_color(&message.user));
        let name = server.username(&message.user);
        let selected = app.is_selected(i);
        let nick = match (message.system, continuing, selected) {
            (true, _, _) => Span::raw(" ".repeat(cols.nick)),
            (_, _, true) => {
                let block = format!(" {} ", truncate(name, cols.nick.saturating_sub(2).max(1)));
                Span::styled(
                    pad_left(&block, cols.nick),
                    Style::new()
                        .bg(color)
                        .fg(Color::Indexed(0))
                        .add_modifier(Modifier::BOLD),
                )
            }
            (_, true, _) => Span::raw(" ".repeat(cols.nick)),
            _ => Span::styled(
                pad_left(&truncate(name, cols.nick), cols.nick),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ),
        };
        let clock = match (cols.time, continuing || message.system) {
            (0, _) | (_, true) => " ".repeat(cols.time),
            _ => message.at.format("%H:%M").to_string(),
        };

        let base = if message.system {
            muted()
        } else {
            Style::new()
        };
        let mut text = markdown::Text::default();
        if let Some(root) = &message.root {
            let author = messages
                .iter()
                .find(|m| &m.id == root)
                .map_or("?", |m| server.username(&m.user));
            text.push_str(&format!("↪ {} ", truncate(author, 12)), markdown::MUTED);
        }
        text.append(markdown::render(&message.text));
        let marks = mention_marks(&text, me);
        let ranges = markdown::wrap(&text.chars, cols.text);
        let last = ranges.len() - 1;
        for (n, range) in ranges.into_iter().enumerate() {
            let mut spans = Vec::new();
            if n == 0 {
                spans.push(Span::raw(" ".repeat(MARGIN)));
                if cols.time > 0 {
                    spans.push(Span::styled(pad_right(&clock, cols.time), muted()));
                    spans.push(Span::raw(" "));
                }
                spans.push(nick.clone());
                spans.push(Span::raw("  "));
            } else {
                spans.push(Span::raw(indent.clone()));
            }
            spans.extend(text_spans(&text, &marks, range, base));
            if message.edited && n == last {
                spans.push(Span::styled(" *", muted()));
            }
            log.lines.push(Line::from(spans));
        }
        if app.reactions && !message.reactions.is_empty() {
            let mut spans = vec![Span::raw(indent.clone())];
            for (n, group) in message.reactions.iter().enumerate() {
                let mine = server
                    .me
                    .as_ref()
                    .is_some_and(|m| group.users.contains(&m.id));
                let style = if mine {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    muted()
                };
                let sep = if n > 0 { "  " } else { "" };
                spans.push(Span::styled(
                    format!(
                        "{sep}{} {}",
                        markdown::emoji(&format!(":{}:", group.name)),
                        group.users.len()
                    ),
                    style,
                ));
            }
            log.lines.push(Line::from(spans));
        }
        for file in &message.files {
            log.lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled(
                    format!("[file: {}, {}]", file.name, open::human_size(file.size)),
                    muted(),
                ),
            ]));
        }
        log.spans.push((start, log.lines.len() - 1));
    }
    log
}

fn tabs_line(app: &App, width: usize) -> Line<'static> {
    let Some(server) = app.servers.get(app.server) else {
        return Line::default();
    };
    let places: usize = app.servers.iter().map(|s| s.teams.len()).sum();
    let place = app.servers[..app.server]
        .iter()
        .map(|s| s.teams.len())
        .sum::<usize>()
        + app.team
        + 1;
    let Some(team) = server.teams.get(app.team) else {
        let state = if server.client.is_some() {
            "loading…"
        } else {
            "connecting…"
        };
        return Line::from(vec![
            chip(&format!(" {} ", server.name)),
            Span::styled(format!(" {state}"), muted()),
        ]);
    };
    let chip = chip(&truncate(
        &format!(
            " {}:{} {place}/{places} ",
            server.name, team.info.display_name
        ),
        (width / 2).max(8),
    ));
    let Some(chans) = team.chans.as_ref().filter(|c| !c.is_empty()) else {
        return Line::from(vec![chip, Span::styled(" loading…", muted())]);
    };

    let active = team.cur.min(chans.len() - 1);
    let flagged = |i: usize| i != active && chans[i].unread();
    let labels: Vec<String> = chans
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                " {}{}{} ",
                if flagged(i) { "•" } else { "" },
                server.label(c),
                if i != active && c.mentions() { "!" } else { "" }
            )
        })
        .collect();
    let widths: Vec<usize> = labels.iter().map(|l| l.width()).collect();
    let room = width.saturating_sub(chip.width() + 2).max(1);
    let first = first_visible(&widths, active, room);

    let mut spans = vec![chip, Span::raw(if first > 0 { "‹" } else { " " })];
    let mut used = 0;
    let mut last = first;
    for i in first..chans.len() {
        if used + widths[i] > room {
            break;
        }
        let style = match (i == active, flagged(i)) {
            (true, _) => Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            (_, true) => Style::new().add_modifier(Modifier::BOLD),
            _ => Style::new(),
        };
        spans.push(Span::styled(labels[i].clone(), style));
        used += widths[i];
        last = i;
    }
    spans.push(Span::raw(if last + 1 < chans.len() { "›" } else { " " }));
    Line::from(spans)
}

fn chip(text: &str) -> Span<'static> {
    Span::styled(
        text.to_owned(),
        Style::new()
            .bg(Color::Indexed(1))
            .fg(Color::Indexed(15))
            .add_modifier(Modifier::BOLD),
    )
}

fn status_line(app: &App, width: usize, percent: usize) -> Line<'static> {
    let mode = app.mode();
    let (left, right) = if app.notice.is_empty() {
        let place = match (app.servers.get(app.server), app.channel()) {
            (Some(server), Some((_, chan))) => {
                let team = server
                    .teams
                    .get(app.team)
                    .map_or("", |t| t.info.display_name.as_str());
                format!("{}:{team} > {}", server.name, server.label(chan))
            }
            (Some(server), None) => server.name.clone(),
            _ => String::new(),
        };
        let count = app.messages().map_or(0, <[Message]>::len);
        let offline = if server_down(app) { "offline  " } else { "" };
        let offline = format!("{}{offline}", typing_names(app));
        let right = match count {
            0 => format!("{offline}? help "),
            _ => format!(
                "{offline}{}/{count}  {percent:>3}%  ? help ",
                app.cursor + 1
            ),
        };
        (format!(" {mode}  {place}"), right)
    } else {
        (format!(" {mode}  {}", app.notice), String::new())
    };
    let right = match width.saturating_sub(right.width()) >= mode.width() + 4 {
        true => right,
        false => String::new(),
    };
    let left = truncate(&left, width.saturating_sub(right.width()));
    let gap = width.saturating_sub(left.width() + right.width());
    Line::styled(
        format!("{left}{}{right}", " ".repeat(gap)),
        Style::new().add_modifier(Modifier::REVERSED),
    )
}

fn server_down(app: &App) -> bool {
    app.servers
        .get(app.server)
        .is_some_and(|s| s.link == Link::Down)
}

/// The reactions of the picker, written on the status line.
fn picker_line(width: usize) -> Line<'static> {
    let line = Style::new().add_modifier(Modifier::REVERSED);
    let mut spans = vec![Span::styled(" REACT ", line.add_modifier(Modifier::BOLD))];
    for (i, name) in REACTIONS.iter().enumerate() {
        let glyph = markdown::emoji(&format!(":{name}:"));
        spans.push(Span::styled(" ", line));
        spans.push(Span::styled(
            (i + 1).to_string(),
            line.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
        spans.push(Span::styled(" ", line));
        spans.push(Span::styled(glyph, line));
        spans.push(Span::styled(" ", line));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), line));
    Line::from(spans)
}

/// The things `o` can open, written on the status line.
fn open_line(items: &[Item], width: usize) -> Line<'static> {
    let line = Style::new().add_modifier(Modifier::REVERSED);
    let mut spans = vec![Span::styled(" OPEN ", line.add_modifier(Modifier::BOLD))];
    for (i, item) in items.iter().take(9).enumerate() {
        let label = match item {
            Item::Link(url) => truncate(url, 30),
            Item::File(file) => truncate(&file.name, 30),
        };
        spans.push(Span::styled(" ", line));
        spans.push(Span::styled(
            (i + 1).to_string(),
            line.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
        spans.push(Span::styled(format!(" {label} "), line));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), line));
    Line::from(spans)
}

/// The members of the open channel: who is around with a dot for their status, then who is not.
fn users_column(app: &App, inner: usize) -> Vec<Line<'static>> {
    let Some((server, chan)) = app.channel() else {
        return Vec::new();
    };
    if !server.members.contains_key(&chan.info.id) {
        return vec![Line::styled(" loading…", muted())];
    }
    let (present, absent) = server.presence(&chan.info.id);
    let heading = |title: &str, count: usize| {
        Line::styled(
            format!(" {title} — {count}"),
            muted().add_modifier(Modifier::BOLD),
        )
    };
    let mut rows = vec![heading("present", present.len())];
    for member in &present {
        let dot = match member.status {
            Status::Online => Color::Indexed(2),
            Status::Away => Color::Indexed(3),
            Status::DoNotDisturb => Color::Indexed(1),
            Status::Offline => Color::Indexed(8),
        };
        let color = Color::Indexed(model::user_color(member.user));
        rows.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("●", Style::new().fg(dot)),
            Span::raw(" "),
            Span::styled(
                truncate(member.name, inner.saturating_sub(3).max(1)),
                Style::new().fg(color),
            ),
        ]));
    }
    rows.push(Line::default());
    rows.push(heading("absent", absent.len()));
    for member in &absent {
        let name = truncate(&format!(" ○ {}", member.name), inner);
        rows.push(Line::styled(name, muted()));
    }
    if server
        .members
        .get(&chan.info.id)
        .is_some_and(|m| m.len() >= 200)
    {
        rows.push(Line::styled(" + more not listed", muted()));
    }
    rows
}

/// Text in `base`, with the characters at `hits` in `hl`.
fn highlighted(text: &str, hits: &[usize], base: Style, hl: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut marked = false;
    for (i, c) in text.chars().enumerate() {
        if hits.contains(&i) != marked && !run.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                if marked { hl } else { base },
            ));
        }
        marked = hits.contains(&i);
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if marked { hl } else { base }));
    }
    spans
}

/// The matches of the finder on one line: the count, the channels side by side, and the scope.
fn wildmenu(app: &App, width: usize) -> Line<'static> {
    let Some(finder) = &app.finder else {
        return Line::default();
    };
    let line = Style::new().add_modifier(Modifier::REVERSED);
    let mode = Span::styled(" SEARCH ", line);
    let text = finder.line.text();
    let words: Vec<&str> = text.split_whitespace().collect();
    let candidates = finder::candidates(&app.servers, (app.server, app.team), finder.all);
    let found = finder::entries(&words, &candidates, finder.all);
    if found.is_empty() {
        let rest = width.saturating_sub(mode.width());
        return Line::from(vec![
            mode,
            Span::styled(pad_right(" no result", rest), line),
        ]);
    }

    let selected = finder.selected.min(found.len() - 1);
    let chip = Span::styled(format!(" {}/{} ", selected + 1, found.len()), line);
    let chosen = found[selected].candidate;
    let scope = match (finder.all, chosen.direct()) {
        (false, _) => " this team ".to_owned(),
        (true, true) => " ".to_owned(),
        (true, false) => format!(" {} ", truncate(&chosen.scope, (width / 3).max(8))),
    };
    let scope = Span::styled(scope, line);

    let items: Vec<Vec<Span<'static>>> = found
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let candidate = entry.candidate;
            let mut base = line;
            if candidate.unread {
                base = base.add_modifier(Modifier::BOLD);
            }
            let mut hl = base.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
            if i == selected {
                base = Style::new()
                    .bg(Color::Indexed(3))
                    .fg(Color::Indexed(0))
                    .add_modifier(Modifier::BOLD);
                hl = base.add_modifier(Modifier::UNDERLINED);
            }
            let mut spans = vec![Span::styled(" ", base)];
            if finder.all && !candidate.direct() {
                let scope = truncate(&candidate.scope, 14);
                spans.extend(highlighted(&scope, &entry.scope_hits, base, hl));
            }
            spans.push(Span::styled(model::glyph(candidate.kind).to_string(), base));
            spans.extend(highlighted(&candidate.name, &entry.name_hits, base, hl));
            spans.push(Span::styled(" ", base));
            spans
        })
        .collect();
    menu_line(mode, chip, scope, items, selected, width)
}

/// The names the command line offers, on the status line like the finder's matches.
fn completion_line(choice: usize, completion: &Completion, width: usize) -> Line<'static> {
    let line = Style::new().add_modifier(Modifier::REVERSED);
    let mode = Span::styled(" COMMAND ", line);
    if completion.names.is_empty() {
        let rest = width.saturating_sub(mode.width());
        return Line::from(vec![
            mode,
            Span::styled(pad_right(" no result", rest), line),
        ]);
    }
    let selected = choice.min(completion.names.len() - 1);
    let chip = Span::styled(
        format!(" {}/{} ", selected + 1, completion.names.len()),
        line,
    );
    let label = if completion.command.is_empty() {
        "command"
    } else {
        completion.command
    };
    let scope = Span::styled(format!(" {label} "), line);
    let at = match completion.command {
        "dm" | "invite" | "kick" | "ban" | "unban" | "ignore" | "unignore" | "verify" | "trust" => {
            "@"
        }
        _ => "",
    };
    let items = completion
        .names
        .iter()
        .enumerate()
        .map(|(i, (name, hits))| {
            let mut base = line;
            let mut hl = base.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
            if i == selected {
                base = Style::new()
                    .bg(Color::Indexed(3))
                    .fg(Color::Indexed(0))
                    .add_modifier(Modifier::BOLD);
                hl = base.add_modifier(Modifier::UNDERLINED);
            }
            let mut spans = vec![Span::styled(format!(" {at}"), base)];
            spans.extend(highlighted(name, hits, base, hl));
            spans.push(Span::styled(" ", base));
            spans
        })
        .collect();
    menu_line(mode, chip, scope, items, selected, width)
}

/// The mode, the count, the items side by side scrolled to show the chosen one, and the scope.
fn menu_line(
    mode: Span<'static>,
    chip: Span<'static>,
    scope: Span<'static>,
    items: Vec<Vec<Span<'static>>>,
    selected: usize,
    width: usize,
) -> Line<'static> {
    let line = Style::new().add_modifier(Modifier::REVERSED);
    let total = items.len();
    let widths: Vec<usize> = items
        .iter()
        .map(|spans| spans.iter().map(Span::width).sum())
        .collect();

    let room = width
        .saturating_sub(chip.width() + scope.width() + 2)
        .max(1);
    let first = first_visible(&widths, selected, room);
    let (mut used, mut last) = (0, first);
    let mut menu = Vec::new();
    for (i, spans) in items.into_iter().enumerate().skip(first) {
        if used + widths[i] > room {
            break;
        }
        used += widths[i];
        last = i;
        menu.extend(spans);
    }

    let left = Span::styled(if first > 0 { "‹" } else { " " }, line);
    let right = Span::styled(if last + 1 < total { "›" } else { " " }, line);
    let gap = width.saturating_sub(mode.width() + chip.width() + 2 + used + scope.width());
    let mut spans = vec![mode, chip, left];
    spans.extend(menu);
    spans.push(right);
    spans.push(Span::styled(" ".repeat(gap), line));
    spans.push(scope);
    Line::from(spans)
}

fn compose_title(kind: &Kind) -> String {
    match kind {
        Kind::Message => "New message".into(),
        Kind::Reply { to, .. } => format!("Reply to {}", truncate(to, 20)),
        Kind::Edit(_) => "Editing your message".into(),
    }
}

fn compose_label(kind: &Kind) -> String {
    match kind {
        Kind::Message => String::new(),
        Kind::Reply { to, .. } => format!("↪ {} ", truncate(to, 12)),
        Kind::Edit(_) => "✎ edit ".into(),
    }
}

/// The part of an input that fits in `room` cells with the cursor inside, and the cursor's column in it.
fn visible(shown: &str, cursor: usize, room: usize) -> (String, usize) {
    let chars: Vec<char> = shown.chars().collect();
    let cells = |c: &char| c.width().unwrap_or(0);
    let room = room.max(2);
    let before: usize = chars[..cursor.min(chars.len())].iter().map(cells).sum();
    let (mut start, mut skipped) = (0, 0);
    while before - skipped >= room {
        skipped += cells(&chars[start]);
        start += 1;
    }
    let (mut text, mut used) = (String::new(), 0);
    for c in &chars[start..] {
        if used + cells(c) > room {
            break;
        }
        text.push(*c);
        used += cells(c);
    }
    (text, before - skipped)
}

fn typing_names(app: &App) -> String {
    let Some((server, chan)) = app.channel() else {
        return String::new();
    };
    let names = server.typing_names(&chan.info.id, Instant::now());
    if names.is_empty() {
        return String::new();
    }
    let shown = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
    let more = names.len().saturating_sub(3);
    let more = if more > 0 {
        format!(" +{more}")
    } else {
        String::new()
    };
    format!("✎ {shown}{more}  ")
}

fn help_lines() -> Vec<Line<'static>> {
    let section = |title: &str, rows: &[(&str, &str)]| {
        let heading = Line::styled(
            format!("  {title}"),
            Style::new().add_modifier(Modifier::BOLD),
        );
        let rows = rows
            .iter()
            .map(|(key, what)| Line::from(format!("    {key:<12}{what}")));
        std::iter::once(heading)
            .chain(rows)
            .chain(std::iter::once(Line::default()))
            .collect::<Vec<_>>()
    };
    [
        section("Keys", BINDINGS),
        section("Finder", FINDER),
        section("Commands (type : first)", COMMANDS),
    ]
    .concat()
}

/// Draws the whole screen, following the cursor when a key asked for it.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let width = usize::from(area.width);
    app.size = (area.width, area.height);
    let editor_rows = match app.pane {
        Some(_) => term::split(area.height).1 + 1,
        None => 1,
    };
    let spare = area.height.saturating_sub(1);
    let [tabs, body, input, status] = Layout::vertical([
        Constraint::Length(u16::from(spare >= 2)),
        Constraint::Min(0),
        Constraint::Length(if spare >= 3 { editor_rows } else { 0 }),
        Constraint::Length(1),
    ])
    .areas(area);
    let height = usize::from(body.height);
    let (body, side) = match app.users.shown && area.width >= USERS_MIN_WIDTH {
        true => {
            let [log, side] =
                Layout::horizontal([Constraint::Min(1), Constraint::Length(USERS_WIDTH)])
                    .areas(body);
            (log, Some(side))
        }
        false => (body, None),
    };
    let log_width = usize::from(body.width);

    frame.render_widget(Paragraph::new(tabs_line(app, width)), tabs);

    let mut percent = 100;
    if let Some(top) = app.help {
        let lines = help_lines();
        let title = Line::from(vec![
            Span::styled(" Help", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled("   j k scroll   esc ? q close", muted()),
        ]);
        let rows = height.saturating_sub(1);
        let top = top.min(lines.len().saturating_sub(rows));
        app.help = Some(top);
        let page: Vec<Line> = std::iter::once(title)
            .chain(lines.into_iter().skip(top).take(rows))
            .collect();
        frame.render_widget(Paragraph::new(page), body);
    } else if let Some(found) = &mut app.found {
        let keys = match found.pick {
            Some(_) => "   j k move   enter open   esc q close",
            None => "   j k scroll   esc q close",
        };
        let title = Line::from(vec![
            Span::styled(
                format!(" {}", found.title),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled(keys, muted()),
        ]);
        let rows = height.saturating_sub(1);
        if let Some(pick) = &found.pick {
            found.top = found
                .top
                .min(pick.at)
                .max((pick.at + 1).saturating_sub(rows));
        }
        found.top = found.top.min(found.lines.len().saturating_sub(rows));
        let chosen = found.pick.as_ref().map(|p| p.at);
        let page: Vec<Line> = std::iter::once(title)
            .chain(
                found
                    .lines
                    .iter()
                    .enumerate()
                    .skip(found.top)
                    .take(rows)
                    .map(|(i, l)| match Some(i) == chosen {
                        true => {
                            Line::styled(l.as_str(), Style::new().add_modifier(Modifier::REVERSED))
                        }
                        false => Line::from(l.as_str()),
                    }),
            )
            .collect();
        frame.render_widget(Paragraph::new(page), body);
    } else if let Some((_, check)) = &app.verifying {
        frame.render_widget(Paragraph::new(verify_lines(&check.emojis)), body);
    } else {
        let log = render_log(app, log_width);
        let bottom = log.lines.len().saturating_sub(height);
        let mut scroll = app.scroll.min(bottom);
        if app.follow {
            if let Some(&span) = log.spans.get(app.cursor) {
                scroll = scrolled(
                    scroll,
                    span,
                    app.cursor,
                    log.spans.len(),
                    height,
                    log.lines.len(),
                )
                .min(bottom);
                app.scroll = scroll;
            }
            app.follow = false;
        }
        if app.shift != 0 {
            scroll = scroll.saturating_add_signed(app.shift).min(bottom);
            app.scroll = scroll;
            app.shift = 0;
        }
        let first = log.spans.iter().position(|&(_, end)| end >= scroll);
        let last = log
            .spans
            .iter()
            .rposition(|&(start, _)| start < scroll + height);
        if let Some(visible) = first.zip(last) {
            app.visible = visible;
        }
        percent = (scroll * 100).checked_div(bottom).unwrap_or(100);
        let page: Vec<Line> = log.lines.into_iter().skip(scroll).take(height).collect();
        frame.render_widget(Paragraph::new(page), body);
    }

    if let Some(side) = side {
        let rows = users_column(app, usize::from(USERS_WIDTH) - 1);
        let top = app.users.top.min(rows.len().saturating_sub(height));
        app.users.top = top;
        let rule = if app.users.focus {
            Color::Indexed(3)
        } else {
            Color::Indexed(8)
        };
        let lines: Vec<Line> = (0..height)
            .map(|i| {
                let mut spans = vec![Span::styled("│", Style::new().fg(rule))];
                spans.extend(rows.get(top + i).cloned().unwrap_or_default().spans);
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), side);
    }

    if let Some(pane) = &mut app.pane {
        let label = match (pane.is_viewer(), &app.compose) {
            (true, _) => format!("Viewing {}", pane.title()),
            (false, Some(compose)) => compose_title(&compose.kind),
            (false, None) => String::new(),
        };
        let rule = format!("─── {label} ");
        let rest = width.saturating_sub(rule.width());
        let line = Line::styled(format!("{rule}{}", "─".repeat(rest)), muted());
        let editor = Rect::new(
            input.x,
            input.y + 1,
            input.width,
            input.height.saturating_sub(1),
        );
        frame.render_widget(Paragraph::new(line), Rect { height: 1, ..input });
        pane.resize(editor.height, editor.width);
        frame.render_widget(&*pane, editor);
        if let Some((column, row)) = pane.cursor() {
            frame.set_cursor_position(Position::new(editor.x + column, editor.y + row));
        }
    }
    let typing = match (&app.compose, &app.finder, &app.input) {
        _ if app.pane.is_some() => None,
        (Some(compose), ..) => Some((compose_label(&compose.kind), "> ", &compose.line)),
        (_, Some(finder), _) => Some((String::new(), "find> ", &finder.line)),
        (.., Some(line)) => Some((String::new(), app.prompt(), line)),
        _ => None,
    };
    if let Some((label, prompt, line)) = typing.filter(|_| input.height > 0) {
        let lead = 1 + label.width() + prompt.width();
        let (text, column) = visible(&line.shown(), line.cursor(), width.saturating_sub(lead));
        let row = Line::from(vec![
            Span::raw(" "),
            Span::styled(label, muted()),
            Span::raw(prompt),
            Span::raw(text),
        ]);
        frame.render_widget(Paragraph::new(row), input);
        let x = input.x + u16::try_from(lead + column).unwrap_or(0);
        frame.set_cursor_position(Position::new(
            x.min(input.right().saturating_sub(1)),
            input.y,
        ));
    }
    let bottom = match (&app.finder, app.picker, &app.opener, app.completion()) {
        (Some(_), ..) => wildmenu(app, width),
        (None, true, ..) => picker_line(width),
        (None, false, Some(items), _) => open_line(items, width),
        (None, false, None, Some(completion)) => completion_line(app.choice, &completion, width),
        (None, false, None, None) => status_line(app, width, percent),
    };
    frame.render_widget(Paragraph::new(bottom), status);
    if app.plain {
        strip_colors(frame.buffer_mut());
    }
}

fn strip_colors(buffer: &mut Buffer) {
    for cell in buffer.content.iter_mut() {
        if cell.bg != Color::Reset {
            cell.modifier.insert(Modifier::REVERSED);
        }
        if cell.fg == Color::Indexed(8) {
            cell.modifier.insert(Modifier::DIM);
        }
        cell.fg = Color::Reset;
        cell.bg = Color::Reset;
    }
}

#[cfg(test)]
pub mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::app::tests::{fixture, invited, press, press_code};

    pub fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| {
                let mut line = String::new();
                let mut skip = 0;
                for cell in row {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    skip = cell.symbol().width().saturating_sub(1);
                    line.push_str(cell.symbol());
                }
                line.trim_end().to_owned()
            })
            .collect()
    }

    fn from_end(lines: &[String], n: usize) -> &str {
        &lines[lines.len() - n]
    }

    #[test]
    fn no_screen_breaks_on_a_tiny_terminal() {
        let scenarios = [
            "",
            "?",
            "f",
            "x",
            "o",
            ":",
            ":new-server\n",
            "m",
            "V",
            "i",
            "ihello",
            "a",
        ];
        for (width, height) in [(1, 1), (2, 2), (5, 3), (10, 4), (20, 4), (30, 8)] {
            for keys in scenarios {
                let mut app = fixture();
                for key in keys.split_inclusive('\n') {
                    match key.strip_suffix('\n') {
                        Some(text) => {
                            press(&mut app, text);
                            press_code(&mut app, ratatui::crossterm::event::KeyCode::Enter);
                        }
                        None => press(&mut app, key),
                    }
                }
                let lines = screen(&mut app, width, height);
                assert_eq!(lines.len(), usize::from(height), "{keys:?}");
                if height > 1 {
                    assert!(!from_end(&lines, 1).is_empty(), "{keys:?} {width}x{height}");
                }
            }
        }
    }

    #[test]
    fn an_invitation_says_who_asked_and_how_to_answer() {
        let mut app = invited("O");
        let text = screen(&mut app, 80, 10).join("\n");
        assert!(text.contains("ana invited you to"), "{text}");
        assert!(text.contains("Type :join to accept or :leave to decline."));
        let mut app = invited("D");
        let text = screen(&mut app, 80, 10).join("\n");
        assert!(text.contains("ana wants to start a direct conversation with you."));
    }

    #[test]
    fn declining_an_invitation_to_a_direct_message_is_allowed() {
        let mut app = invited("D");
        press(&mut app, ":");
        press(&mut app, "leave");
        press_code(&mut app, ratatui::crossterm::event::KeyCode::Enter);
        assert!(!app.notice.contains("cannot be left"), "{}", app.notice);
    }

    #[test]
    fn the_editor_pane_leaves_the_conversation_a_row_on_a_short_screen() {
        for height in [3, 4, 6, 9] {
            let mut app = fixture();
            let _dir = crate::app::tests::editor(&mut app, "printf 'EDITING'; sleep 30");
            app.update(crate::app::tests::ctrl('g'));
            let lines = screen(&mut app, 20, height);
            assert_eq!(lines.len(), usize::from(height));
            assert!(!from_end(&lines, 1).is_empty(), "height {height}");
            let (conversation, editor) = term::split(height);
            assert!(conversation >= 1 && editor >= 1, "height {height}");
        }
    }

    #[test]
    fn plain_drops_every_color_and_keeps_the_emphasis() {
        let mut app = fixture();
        app.plain = true;
        press(&mut app, "a");
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(buffer
            .content()
            .iter()
            .all(|c| c.fg == Color::Reset && c.bg == Color::Reset));
        let status = &buffer.content()[buffer.content().len() - 60];
        assert!(status.modifier.contains(Modifier::REVERSED));
        assert!(buffer
            .content()
            .iter()
            .any(|c| c.modifier.contains(Modifier::DIM)));
    }

    #[test]
    fn the_man_page_describes_every_key_of_the_help() {
        let page = include_str!("../mx.1.scd");
        for (key, _) in BINDINGS {
            assert!(page.contains(&format!("*{key}*")), "{key}");
        }
    }

    #[test]
    fn the_man_page_describes_every_command_of_the_help() {
        let page = include_str!("../mx.1.scd");
        for (command, _) in COMMANDS {
            let name = command.split_whitespace().next().unwrap();
            assert!(page.contains(&format!("*{name}*")), "{name}");
        }
    }

    #[test]
    fn the_help_lists_the_commands_the_command_line_knows() {
        let helped: Vec<&str> = COMMANDS
            .iter()
            .map(|(c, _)| c.split_whitespace().next().unwrap())
            .collect();
        let known: Vec<&str> = crate::input::COMMANDS.iter().map(|(n, _)| *n).collect();
        assert_eq!(helped, known);
    }

    #[test]
    fn typing_a_command_start_lists_the_commands_that_begin_so() {
        let mut app = fixture();
        press(&mut app, ":mark");
        let lines = screen(&mut app, 60, 12);
        assert_eq!(from_end(&lines, 2), " :mark");
        let menu = from_end(&lines, 1);
        assert!(menu.starts_with(" COMMAND  1/2 "), "{menu:?}");
        assert!(menu.contains("mark-read") && menu.contains("mark-all-read"));
        assert!(menu.ends_with(" command"));
    }

    #[test]
    fn truncate_counts_cells() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("日本語です", 5), "日本…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn narrow_screens_drop_the_time_column_first() {
        let wide = columns(80);
        assert_eq!((wide.time, wide.nick), (TIME, NICK));
        let narrow = columns(30);
        assert_eq!(narrow.time, 0);
        assert!(narrow.text >= MIN_TEXT);
        assert!(columns(12).nick >= 4);
    }

    #[test]
    fn the_strip_scrolls_to_the_active_tab() {
        assert_eq!(first_visible(&[5, 5, 5, 5], 3, 10), 2);
        assert_eq!(first_visible(&[5, 5, 5, 5], 0, 10), 0);
        assert_eq!(first_visible(&[20, 5], 1, 10), 1);
    }

    #[test]
    fn scrolling_moves_as_little_as_needed() {
        assert_eq!(scrolled(5, (6, 7), 2, 9, 10, 40), 5);
        assert_eq!(scrolled(5, (2, 3), 2, 9, 10, 40), 2);
        assert_eq!(scrolled(0, (14, 15), 2, 9, 10, 40), 6);
        assert_eq!(scrolled(9, (0, 1), 0, 9, 10, 40), 0);
        assert_eq!(scrolled(0, (38, 39), 8, 9, 10, 40), 30);
    }

    #[test]
    fn the_screen_shows_tabs_log_and_status() {
        let mut app = fixture();
        let lines = screen(&mut app, 80, 12);
        assert!(
            lines[0].starts_with(" work:Engineering 1/1 "),
            "{:?}",
            lines[0]
        );
        assert!(lines[0].contains("+Secret"));
        assert!(lines[0].contains("#Town Square"));
        assert!(lines[0].contains("@ana"));
        assert!(lines[0].contains("ana, bob"));
        assert!(from_end(&lines, 1).starts_with(" NORMAL  work:Engineering > +Secret"));
        assert!(from_end(&lines, 1).ends_with("2/2  100%  ? help"));
    }

    #[test]
    fn replies_emoji_and_files_are_written() {
        let mut app = fixture();
        let lines = screen(&mut app, 80, 12).join("\n");
        assert!(lines.contains("hello 🎉"), "{lines}");
        assert!(
            lines.contains("↪ ana") || lines.contains("↪ paul"),
            "{lines}"
        );
        assert!(lines.contains("[file: a.png, 12 B]"));
        assert!(lines.contains("ana"));
        assert!(lines.contains(" *"));
    }

    fn has_background(app: &mut App, row: usize) -> bool {
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .chunks(80)
            .nth(row)
            .is_some_and(|cells| cells.iter().any(|c| c.bg != Color::Reset))
    }

    #[test]
    fn nothing_is_selected_until_a_key_moves() {
        let mut app = fixture();
        assert!(!has_background(&mut app, 4));
        press(&mut app, "h");
        assert!(has_background(&mut app, 4));
        assert!(!has_background(&mut app, 3));
        press(&mut app, "ll");
        assert!(!has_background(&mut app, 4));
    }

    #[test]
    fn dms_are_tabs_with_an_at_sign_and_follow_the_channel() {
        let mut app = fixture();
        press(&mut app, "JJ");
        let lines = screen(&mut app, 80, 12);
        assert!(from_end(&lines, 1).contains("> @ana"));
        let narrow = screen(&mut app, 34, 12);
        assert!(narrow[0].contains("@ana"), "{:?}", narrow[0]);
    }

    #[test]
    fn j_and_k_scroll_the_log_without_moving_the_selection() {
        let mut app = fixture();
        screen(&mut app, 60, 4);
        let (cursor, browsing) = (app.cursor, app.browsing);
        press(&mut app, "j");
        screen(&mut app, 60, 4);
        let bottom = app.scroll;
        assert_ne!(bottom, usize::MAX);
        press(&mut app, "k");
        screen(&mut app, 60, 4);
        assert_eq!(app.scroll, bottom.saturating_sub(1));
        press(&mut app, "kk");
        screen(&mut app, 60, 4);
        assert_eq!(app.scroll, bottom.saturating_sub(3));
        assert_eq!((app.cursor, app.browsing), (cursor, browsing));
    }

    #[test]
    fn help_opens_scrolls_and_closes() {
        let mut app = fixture();
        press(&mut app, "?");
        let lines = screen(&mut app, 60, 12);
        assert!(lines[1].starts_with(" Help"));
        assert!(lines.iter().any(|l| l.contains("next / previous channel")));
        assert!(from_end(&lines, 1).starts_with(" HELP"));
        press(&mut app, &"j".repeat(80));
        let lines = screen(&mut app, 60, 12);
        assert!(lines
            .iter()
            .any(|l| l.contains("change a setting until you quit")));
        press(&mut app, "?");
        assert!(from_end(&screen(&mut app, 60, 12), 1).starts_with(" NORMAL"));
    }

    #[test]
    fn the_verification_page_lists_the_seven_emojis_and_the_keys() {
        let emojis: Vec<(String, String)> = (1..=7)
            .map(|i| (format!("{i}"), format!("Name{i}")))
            .collect();
        let text: Vec<String> = verify_lines(&emojis)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text[0].starts_with(" Verify session"));
        assert!(text[0].contains("y they match"));
        assert_eq!(text.len(), 9);
        assert!(text.iter().any(|l| l.contains("7  Name7")));
    }

    #[test]
    fn the_command_line_takes_the_input_row() {
        let mut app = fixture();
        press(&mut app, ":qu");
        let lines = screen(&mut app, 60, 12);
        assert_eq!(from_end(&lines, 2), " :qu");
        assert!(from_end(&lines, 1).starts_with(" COMMAND"));
        press_code(&mut app, ratatui::crossterm::event::KeyCode::Esc);
        assert!(from_end(&screen(&mut app, 60, 12), 2).is_empty());
    }

    #[test]
    fn the_wizard_asks_in_order_and_hides_the_password() {
        let mut app = fixture();
        press(&mut app, ":new-server");
        press_code(&mut app, ratatui::crossterm::event::KeyCode::Enter);
        assert!(from_end(&screen(&mut app, 60, 12), 2).starts_with(" Name:"));
        press(&mut app, "home");
        press_code(&mut app, ratatui::crossterm::event::KeyCode::Enter);
        press(&mut app, "chat.example.org");
        press_code(&mut app, ratatui::crossterm::event::KeyCode::Enter);
        press(&mut app, "paul");
        press_code(&mut app, ratatui::crossterm::event::KeyCode::Enter);
        press(&mut app, "secret");
        let lines = screen(&mut app, 60, 12);
        assert_eq!(from_end(&lines, 2), " Password: ••••••");
        assert!(from_end(&lines, 1).starts_with(" LOGIN"));
    }

    #[test]
    fn the_wildmenu_fills_the_last_row_and_the_query_the_one_above() {
        let mut app = fixture();
        press(&mut app, "f");
        let lines = screen(&mut app, 80, 12);
        assert_eq!(from_end(&lines, 2), " find>");
        let menu = from_end(&lines, 1);
        assert!(menu.starts_with(" SEARCH  1/4 "), "{menu:?}");
        assert!(menu.contains("+Secret") && menu.contains("#Town Square") && menu.contains("@ana"));
        assert!(menu.ends_with("this team"));
        assert!(has_background(&mut app, 11));
    }

    #[test]
    fn the_command_line_offers_names_on_the_last_row_like_the_finder() {
        let mut app = fixture();
        app.servers[0].teams.push(crate::model::Team {
            info: serde_json::from_value(
                serde_json::json!({"id": "t2", "name": "d", "display_name": "Design"}),
            )
            .unwrap(),
            chans: None,
            loading: false,
            cur: 0,
        });
        press(&mut app, ":team ");
        let lines = screen(&mut app, 60, 12);
        assert_eq!(from_end(&lines, 2), " :team");
        let menu = from_end(&lines, 1);
        assert!(menu.starts_with(" COMMAND  1/2 "), "{menu:?}");
        assert!(menu.contains("Engineering") && menu.contains("Design"));
        assert!(menu.ends_with(" team"));
        assert!(has_background(&mut app, 11));
        press(&mut app, "zzz");
        assert_eq!(
            from_end(&screen(&mut app, 60, 12), 1),
            " COMMAND  no result"
        );
    }

    #[test]
    fn typing_narrows_the_menu_and_nothing_matching_says_so() {
        let mut app = fixture();
        press(&mut app, "ftown");
        let lines = screen(&mut app, 80, 12);
        assert_eq!(from_end(&lines, 2), " find> town");
        assert!(from_end(&lines, 1).starts_with(" SEARCH  1/1 "));
        assert!(!from_end(&lines, 1).contains("+Secret"));
        press(&mut app, "zzz");
        assert_eq!(from_end(&screen(&mut app, 80, 12), 1), " SEARCH  no result");
    }

    #[test]
    fn a_narrow_menu_scrolls_to_keep_the_choice_visible() {
        let mut app = fixture();
        press(&mut app, "f");
        press_code(&mut app, ratatui::crossterm::event::KeyCode::Tab);
        press_code(&mut app, ratatui::crossterm::event::KeyCode::Tab);
        let menu = from_end(&screen(&mut app, 36, 12), 1).to_owned();
        assert!(menu.contains("@ana") && menu.contains('‹'), "{menu:?}");
        assert!(!menu.contains("+Secret"));
    }

    #[test]
    fn everywhere_the_scope_prefixes_the_items_and_ends_the_row() {
        let mut app = fixture();
        press(&mut app, "Ftown");
        let menu = from_end(&screen(&mut app, 80, 12), 1).to_owned();
        assert!(menu.contains("work:Engineer…#Town Square"), "{menu:?}");
        assert!(menu.ends_with("work:Engineering"), "{menu:?}");
    }

    #[test]
    fn a_long_input_scrolls_so_the_cursor_stays_in_view() {
        assert_eq!(visible("hello", 5, 20), ("hello".to_owned(), 5));
        assert_eq!(visible("0123456789", 10, 5), ("6789".to_owned(), 4));
        assert_eq!(visible("0123456789", 3, 5), ("01234".to_owned(), 3));
        assert_eq!(visible("日本語日本語", 6, 5), ("本語".to_owned(), 4));
    }

    #[test]
    fn insert_mode_shows_in_the_status_line_and_the_text_in_the_input_row() {
        let mut app = fixture();
        press(&mut app, "ihello");
        let lines = screen(&mut app, 40, 12);
        assert_eq!(from_end(&lines, 2), " > hello");
        assert!(
            from_end(&lines, 1).starts_with(" INSERT  work:"),
            "{lines:?}"
        );
        press(&mut app, " and a much longer sentence that cannot fit");
        let row = from_end(&screen(&mut app, 40, 12), 2).to_owned();
        assert!(row.ends_with("cannot fit"), "{row:?}");
    }

    #[test]
    fn the_picker_takes_the_status_line_and_a_range_shows_visual_mode() {
        let mut app = fixture();
        press(&mut app, "x");
        let row = screen(&mut app, 80, 12).pop().unwrap();
        assert!(row.starts_with(" REACT  1 👍"), "{row:?}");
        assert!(row.contains("9 😮"), "{row:?}");
        press(&mut app, "q");
        press(&mut app, "hhVl");
        let row = screen(&mut app, 80, 12).pop().unwrap();
        assert!(row.starts_with(" VISUAL"), "{row:?}");
        assert!(has_background(&mut app, 3) && has_background(&mut app, 4));
    }

    #[test]
    fn mentions_are_marked_outside_code_and_yours_stands_out() {
        let text = markdown::render("hi @ana. mail a@b.c `@code` @paul @here");
        let marks = mention_marks(&text, "paul");
        let at = |word: &str| {
            text.to_plain()
                .find(word)
                .map(|b| text.to_plain()[..b].chars().count())
                .unwrap()
        };
        assert_eq!(marks[at("@ana")], 1);
        assert_eq!(marks[at("@ana") + 4], 0);
        assert_eq!(marks[at("a@b")], 0);
        assert_eq!(marks[at("@code")], 0);
        assert_eq!(marks[at("@paul")], 2);
        assert_eq!(marks[at("@here")], 2);
    }

    fn cell_at(app: &mut App, needle: &str) -> ratatui::buffer::Cell {
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let at = buffer
            .content()
            .iter()
            .position(|c| c.symbol() == needle.chars().next().unwrap().to_string())
            .and_then(|_| {
                buffer
                    .content()
                    .windows(needle.chars().count())
                    .position(|w| w.iter().map(|c| c.symbol()).collect::<String>() == needle)
            })
            .unwrap();
        buffer.content()[at].clone()
    }

    #[test]
    fn markdown_shows_as_styles_and_system_posts_are_muted() {
        let mut app = fixture();
        let text = "**strong** and `code` and > nothing\n> quoted\n[docs](https://x.org/d)";
        let mut post = crate::app::tests::post("p77", "c3", "u2", text, 1_700_000_900_000);
        post.kind = String::new();
        crate::app::tests::posted(&mut app, post, &[]);
        let mut system = crate::app::tests::post(
            "p78",
            "c3",
            "u2",
            "ana joined the channel",
            1_700_001_000_000,
        );
        system.kind = "system_join_channel".into();
        crate::app::tests::posted(&mut app, system, &[]);
        let shown = screen(&mut app, 80, 16).join("\n");
        assert!(shown.contains("strong and code and > nothing"), "{shown}");
        assert!(
            shown.contains("│ quoted") && shown.contains("docs (https://x.org/d)"),
            "{shown}"
        );
        assert!(cell_at(&mut app, "strong")
            .modifier
            .contains(Modifier::BOLD));
        assert_eq!(cell_at(&mut app, "code and").fg, Color::Indexed(6));
        assert!(cell_at(&mut app, "docs")
            .modifier
            .contains(Modifier::UNDERLINED));
        assert_eq!(cell_at(&mut app, "ana joined").fg, Color::Indexed(8));
        let joined = shown.lines().find(|l| l.contains("ana joined")).unwrap();
        assert!(!joined.contains(" ana  ana joined"), "{joined}");
    }
}
