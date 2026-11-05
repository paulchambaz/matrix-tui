//! Prints what a command found, as text for people or JSON for scripts.

use std::io::{self, Write};

use serde::Serialize;
use unicode_width::UnicodeWidthStr;

use crate::api::types::ChannelKind;
use crate::cli::{Error, Format};
use crate::model::glyph;

/// A configured server.
#[derive(Debug, Serialize)]
pub struct ServerOut {
    pub name: String,
    pub url: String,
}

/// A channel with its read state.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelOut {
    pub server: String,
    pub team: String,
    pub channel: String,
    pub channel_id: String,
    pub kind: ChannelKind,
    pub unread: usize,
    pub mentions: usize,
}

/// An emoji and how many reacted with it.
#[derive(Debug, Serialize)]
pub struct ReactionOut {
    pub emoji: String,
    pub count: usize,
}

/// A message with where it was written.
#[derive(Debug, Serialize)]
pub struct MessageOut {
    pub server: String,
    pub team: String,
    pub channel: String,
    pub channel_id: String,
    pub kind: ChannelKind,
    pub id: String,
    pub root_id: String,
    pub time: String,
    pub user: String,
    pub text: String,
    pub unread: bool,
    pub mentions_me: bool,
    pub edited: bool,
    pub files: Vec<String>,
    pub reactions: Vec<ReactionOut>,
    #[serde(skip)]
    pub at: String,
    #[serde(skip)]
    pub reply_to: Option<String>,
}

/// The messages found in one channel, oldest first.
#[derive(Debug)]
pub struct ChannelLog {
    pub channel: ChannelOut,
    pub messages: Vec<MessageOut>,
}

/// What a command found.
#[derive(Debug)]
pub enum Output {
    Servers(Vec<ServerOut>),
    Channels(Vec<ChannelOut>),
    Messages(Vec<ChannelLog>),
}

/// Writes a result in the asked format.
///
/// # Errors
/// Fails when the output cannot be written.
pub fn print(output: &Output, format: Format, out: &mut impl Write) -> io::Result<()> {
    match format {
        Format::Json => {
            match output {
                Output::Servers(servers) => serde_json::to_writer(&mut *out, servers),
                Output::Channels(channels) => serde_json::to_writer(&mut *out, channels),
                Output::Messages(logs) => serde_json::to_writer(
                    &mut *out,
                    &logs
                        .iter()
                        .flat_map(|log| &log.messages)
                        .collect::<Vec<_>>(),
                ),
            }?;
            writeln!(out)
        }
        Format::Text => out.write_all(text(output).as_bytes()),
    }
}

/// The line shown on standard error for a failure.
pub fn failure(error: &Error, format: Format) -> String {
    match format {
        Format::Json => {
            serde_json::json!({"code": error.code(), "message": error.to_string()}).to_string()
        }
        Format::Text => format!("mx: {}: {error}", error.code()),
    }
}

fn text(output: &Output) -> String {
    match output {
        Output::Servers(servers) => table(
            ["NAME", "URL"],
            servers
                .iter()
                .map(|s| vec![s.name.clone(), s.url.clone()])
                .collect(),
        ),
        Output::Channels(channels) => table(
            ["SERVER", "TEAM", "KIND", "CHANNEL", "UNREAD", "MENTIONS"],
            channels
                .iter()
                .map(|c| {
                    vec![
                        c.server.clone(),
                        c.team.clone(),
                        kind_word(c.kind).to_owned(),
                        c.channel.clone(),
                        c.unread.to_string(),
                        c.mentions.to_string(),
                    ]
                })
                .collect(),
        ),
        Output::Messages(logs) => logs.iter().map(log).collect(),
    }
}

fn kind_word(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Public => "public",
        ChannelKind::Private => "private",
        ChannelKind::Direct => "direct",
        ChannelKind::Group => "group",
    }
}

fn table<const N: usize>(head: [&str; N], rows: Vec<Vec<String>>) -> String {
    let rows: Vec<Vec<String>> = std::iter::once(head.map(str::to_owned).to_vec())
        .chain(rows)
        .collect();
    let widths: Vec<usize> = (0..N)
        .map(|i| rows.iter().map(|row| row[i].width()).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|row| {
            let line: Vec<String> = row
                .iter()
                .zip(&widths)
                .map(|(cell, &width)| format!("{cell}{}", " ".repeat(width - cell.width())))
                .collect();
            format!("{}\n", line.join("  ").trim_end())
        })
        .collect()
}

fn header(channel: &ChannelOut) -> String {
    let place = if channel.team.is_empty() {
        channel.server.clone()
    } else {
        format!("{}/{}", channel.server, channel.team)
    };
    let mut counts = Vec::new();
    if channel.unread > 0 {
        counts.push(format!("{} unread", channel.unread));
    }
    match channel.mentions {
        0 => {}
        1 => counts.push("1 mention".to_owned()),
        n => counts.push(format!("{n} mentions")),
    }
    let counts = if counts.is_empty() {
        String::new()
    } else {
        format!("  ({})", counts.join(", "))
    };
    format!(
        "== {place} {}{}{counts} ==\n",
        glyph(channel.kind),
        channel.channel
    )
}

fn log(log: &ChannelLog) -> String {
    let nick = log
        .messages
        .iter()
        .map(|m| m.user.width())
        .max()
        .unwrap_or(0);
    let mut out = header(&log.channel);
    for message in &log.messages {
        let flag = if message.mentions_me {
            '!'
        } else if message.unread {
            '*'
        } else {
            ' '
        };
        let pad = " ".repeat(nick - message.user.width());
        let head = format!("{flag}{}  {}{pad}  ", message.at, message.user);
        let indent = " ".repeat(head.width());
        let mut body = message.text.clone();
        if let Some(name) = &message.reply_to {
            body = format!("(re {name}) {body}");
        }
        for file in &message.files {
            if !body.is_empty() {
                body.push(' ');
            }
            body.push_str(&format!("[file: {file}]"));
        }
        let mut lines = body.lines();
        let first = format!("{head}{}", lines.next().unwrap_or_default());
        out.push_str(first.trim_end());
        out.push('\n');
        for line in lines {
            out.push_str(format!("{indent}{line}").trim_end());
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(kind: ChannelKind, team: &str, name: &str) -> ChannelOut {
        ChannelOut {
            server: "work".into(),
            team: team.into(),
            channel: name.into(),
            channel_id: "c1".into(),
            kind,
            unread: 3,
            mentions: 1,
        }
    }

    fn message(user: &str, text: &str) -> MessageOut {
        MessageOut {
            server: "work".into(),
            team: "dev".into(),
            channel: "general".into(),
            channel_id: "c1".into(),
            kind: ChannelKind::Public,
            id: "p1".into(),
            root_id: String::new(),
            time: "2026-11-04T08:12:00Z".into(),
            user: user.into(),
            text: text.into(),
            unread: false,
            mentions_me: false,
            edited: false,
            files: Vec::new(),
            reactions: vec![ReactionOut {
                emoji: "+1".into(),
                count: 2,
            }],
            at: "2026-11-04 09:12".into(),
            reply_to: None,
        }
    }

    fn written(output: &Output, format: Format) -> String {
        let mut out = Vec::new();
        print(output, format, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_channel_log_reads_like_the_interface() {
        let mut unread = message("bob", "yes, see the thread");
        unread.unread = true;
        let mut call = message("bob", "ping @paul\nsecond line");
        call.mentions_me = true;
        call.reply_to = Some("alice".into());
        let mut upload = message("alice", "");
        upload.files = vec!["a.png".into()];
        let logs = vec![ChannelLog {
            channel: channel(ChannelKind::Public, "dev", "general"),
            messages: vec![message("alice", "did it go?"), unread, call, upload],
        }];
        assert_eq!(
            written(&Output::Messages(logs), Format::Text),
            "== work/dev #general  (3 unread, 1 mention) ==\n \
             2026-11-04 09:12  alice  did it go?\n\
             *2026-11-04 09:12  bob    yes, see the thread\n\
             !2026-11-04 09:12  bob    (re alice) ping @paul\n\
             \x20                         second line\n \
             2026-11-04 09:12  alice  [file: a.png]\n"
        );
    }

    #[test]
    fn a_direct_channel_has_no_team_and_no_counts_when_read() {
        let mut quiet = channel(ChannelKind::Direct, "", "alice");
        quiet.unread = 0;
        quiet.mentions = 0;
        let logs = vec![ChannelLog {
            channel: quiet,
            messages: vec![message("alice", "hi")],
        }];
        assert!(written(&Output::Messages(logs), Format::Text).starts_with("== work @alice ==\n"));
    }

    #[test]
    fn tables_are_aligned_under_their_header() {
        let servers = vec![
            ServerOut {
                name: "work".into(),
                url: "https://chat.example.org".into(),
            },
            ServerOut {
                name: "home".into(),
                url: "https://mx.home".into(),
            },
        ];
        assert_eq!(
            written(&Output::Servers(servers), Format::Text),
            "NAME  URL\nwork  https://chat.example.org\nhome  https://mx.home\n"
        );
        let channels = vec![channel(ChannelKind::Group, "", "alice, bob")];
        assert_eq!(
            written(&Output::Channels(channels), Format::Text),
            "SERVER  TEAM  KIND   CHANNEL     UNREAD  MENTIONS\n\
             work          group  alice, bob  3       1\n"
        );
    }

    #[test]
    fn json_is_one_flat_array_of_standalone_objects() {
        let logs = vec![
            ChannelLog {
                channel: channel(ChannelKind::Public, "dev", "general"),
                messages: vec![message("alice", "one")],
            },
            ChannelLog {
                channel: channel(ChannelKind::Public, "dev", "random"),
                messages: vec![message("bob", "two")],
            },
        ];
        let json: serde_json::Value =
            serde_json::from_str(&written(&Output::Messages(logs), Format::Json)).unwrap();
        let all = json.as_array().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1]["user"], "bob");
        assert_eq!(all[0]["kind"], "public");
        assert_eq!(all[0]["root_id"], "");
        assert_eq!(all[0]["time"], "2026-11-04T08:12:00Z");
        assert_eq!(all[0]["reactions"][0]["count"], 2);
        assert!(all[0].get("at").is_none());
    }

    #[test]
    fn failures_name_their_code() {
        let error = Error::UnknownChannel("x".into());
        assert_eq!(
            failure(&error, Format::Text),
            "mx: unknown-channel: no channel matches x"
        );
        let json: serde_json::Value = serde_json::from_str(&failure(&error, Format::Json)).unwrap();
        assert_eq!(json["code"], "unknown-channel");
    }
}
