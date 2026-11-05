//! The command line: what the user asked for, read with no I/O.

use chrono::{DateTime, Local};
use thiserror::Error;

use crate::api;
use crate::api::types::ChannelKind;
use crate::query::{self, Query};

/// The usage shown by `-h` and after a mistake.
pub const USAGE: &str = "Usage: mx [-hV]
       mx server  list [--json]
       mx channel list [-s server] [-t team] [-k kind] [-c channel] [--json] [query]
       mx message list [-s server] [-t team] [-k kind] [-c channel] [-n number]
                       [--json] [query]

A keyboard-driven terminal client for Matrix. Without a command it starts
the terminal interface; with one it prints to standard output and exits, and
never changes anything on the servers.

  -h    print this help, or the help of a command, and exit
  -V    print the version and exit

Press ? inside mx for the keys, and see man mx for the rest.";

const SERVER_HELP: &str = "Usage: mx server list [--json]

Lists the configured servers without touching the network.";

const CHANNEL_HELP: &str =
    "Usage: mx channel list [-s server] [-t team] [-k kind] [-c channel] [--json] [query]

Lists the channels with their unread and mention counts.

  -s server    only this server
  -t team      only this team
  -k kind      only public, private, direct, group, or dm for direct and group
  -c channel   only this channel, by name or display name, or @user for a direct message
  --json       print JSON instead of text

The query holds `unread` and `mention`, joined by `and`, each possibly after `not`.";

const MESSAGE_HELP: &str =
    "Usage: mx message list [-s server] [-t team] [-k kind] [-c channel] [-n number]
                       [--json] [query]

Lists the messages of the matching channels, oldest first.

  -s server    only this server
  -t team      only this team
  -k kind      only public, private, direct, group, or dm for direct and group
  -c channel   only this channel, by name or display name, or @user for a direct message
  -n number    the last number messages of each channel, 0 for all (default 50,
               and no cut at all when the query holds `unread`)
  --json       print JSON instead of text

The query is clauses joined by `and`, each possibly after `not`, and groups of them
joined by `or`, which binds looser: `dm and unread or mention and unread`.

  from user[,user]   written by one of these users, me being you
  mention            calls you
  dm                 in a direct or group channel
  unread             not read yet
  after time         newer than a time
  before time        older than a time
  body text          contains a text, ignoring case
  thread id          in the thread of a post

A time is an RFC 3339 date or time, or a duration back from now such as 90m, 12h or 7d.";

/// Everything that can make a command fail, each with a stable code.
#[derive(Debug, Error)]
pub enum Error {
    /// The command line is wrong.
    #[error("{0}")]
    Usage(String),
    /// The query is wrong.
    #[error("{0}")]
    BadQuery(String),
    /// No configured server has this name.
    #[error("no server is named {0}")]
    UnknownServer(String),
    /// No team of the servers has this name.
    #[error("no team is named {0}")]
    UnknownTeam(String),
    /// No channel matches this name.
    #[error("no channel matches {0}")]
    UnknownChannel(String),
    /// Several channels match this name.
    #[error("{0} matches several channels: {1}")]
    AmbiguousChannel(String, String),
    /// A server could not be reached or refused us.
    #[error("{server}: {source}")]
    Server {
        server: String,
        #[source]
        source: api::Error,
    },
    /// The secret of a server could not be found.
    #[error("{server}: {message}")]
    Credentials { server: String, message: String },
}

impl Error {
    /// The word that names this kind of failure, which scripts can rely on.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Usage(_) => "usage",
            Self::BadQuery(_) => "bad-query",
            Self::UnknownServer(_) => "unknown-server",
            Self::UnknownTeam(_) => "unknown-team",
            Self::UnknownChannel(_) => "unknown-channel",
            Self::AmbiguousChannel(..) => "ambiguous-channel",
            Self::Credentials { .. } => "no-credentials",
            Self::Server { source, .. } => match source {
                api::Error::Login(_) => "login-failed",
                api::Error::Unreachable(_) => "unreachable",
                api::Error::Server(_) => "server-error",
            },
        }
    }

    /// 2 when the command was asked wrongly, 1 when a server failed.
    pub fn status(&self) -> u8 {
        match self {
            Self::Server { .. } | Self::Credentials { .. } => 1,
            _ => 2,
        }
    }
}

fn usage(text: impl Into<String>) -> Error {
    Error::Usage(text.into())
}

/// How results are printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

/// Where to look; an empty list means everywhere.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scope {
    pub servers: Vec<String>,
    pub teams: Vec<String>,
    pub kinds: Vec<ChannelKind>,
    pub channels: Vec<String>,
}

/// What to print.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Servers,
    Channels(Scope, Query),
    Messages {
        scope: Scope,
        limit: usize,
        query: Query,
    },
}

/// A command and the way to print its result.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub format: Format,
    pub command: Command,
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Cli {
    Run,
    Help(&'static str),
    Version,
    Request(Request),
}

const SCOPE: [&str; 4] = ["-s", "-t", "-k", "-c"];

#[derive(Default)]
struct Options {
    help: bool,
    json: bool,
    scope: Scope,
    limit: Option<usize>,
}

fn pick<'a>(word: &str, names: &[&'a str]) -> Result<&'a str, Error> {
    let hits: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| name.starts_with(word) || word.strip_suffix('s') == Some(name))
        .collect();
    match hits[..] {
        [one] => Ok(one),
        [] => Err(usage(format!("unknown command {word}"))),
        _ => Err(usage(format!("{word} is ambiguous"))),
    }
}

fn kinds(list: &str) -> Result<Vec<ChannelKind>, Error> {
    let mut kinds = Vec::new();
    for word in list.split(',') {
        match word {
            "public" => kinds.push(ChannelKind::Public),
            "private" => kinds.push(ChannelKind::Private),
            "direct" => kinds.push(ChannelKind::Direct),
            "group" => kinds.push(ChannelKind::Group),
            "dm" => kinds.extend([ChannelKind::Direct, ChannelKind::Group]),
            other => return Err(usage(format!("unknown kind {other}"))),
        }
    }
    Ok(kinds)
}

fn options<'a>(args: &'a [String], allowed: &[&str]) -> Result<(Options, &'a [String]), Error> {
    let mut options = Options::default();
    let mut rest = args;
    while let [flag, tail @ ..] = rest {
        if !flag.starts_with('-') {
            break;
        }
        rest = tail;
        match flag.as_str() {
            "-h" | "--help" => options.help = true,
            "--json" => options.json = true,
            "-n" | "-s" | "-t" | "-k" | "-c" if allowed.contains(&flag.as_str()) => {
                let [value, tail @ ..] = rest else {
                    return Err(usage(format!("{flag} needs a value")));
                };
                rest = tail;
                match flag.as_str() {
                    "-n" => {
                        options.limit = Some(
                            value
                                .parse()
                                .map_err(|_| usage(format!("{value} is not a number")))?,
                        );
                    }
                    "-k" => options.scope.kinds.extend(kinds(value)?),
                    "-s" => options.scope.servers.extend(split(value)),
                    "-t" => options.scope.teams.extend(split(value)),
                    _ => options.scope.channels.extend(split(value)),
                }
            }
            other => return Err(usage(format!("unknown option {other}"))),
        }
    }
    Ok((options, rest))
}

fn split(list: &str) -> impl Iterator<Item = String> + '_ {
    list.split(',').map(str::to_owned)
}

/// Reads the command line.
///
/// # Errors
/// Fails on an unknown command or option, or a query that cannot be read.
pub fn parse(args: &[String], now: DateTime<Local>) -> Result<Cli, Error> {
    let [first, rest @ ..] = args else {
        return Ok(Cli::Run);
    };
    match first.as_str() {
        "-h" | "--help" => return Ok(Cli::Help(USAGE)),
        "-V" | "--version" => return Ok(Cli::Version),
        word if word.starts_with('-') => return Err(usage(format!("unknown option {word}"))),
        _ => {}
    }
    let noun = pick(first, &["server", "channel", "message"])?;
    let [verb, rest @ ..] = rest else {
        return Err(usage(format!("{noun} needs a command, such as list")));
    };
    pick(verb, &["list"])?;
    let (allowed, help): (&[&str], _) = match noun {
        "server" => (&[], SERVER_HELP),
        "channel" => (&SCOPE, CHANNEL_HELP),
        _ => (&["-s", "-t", "-k", "-c", "-n"], MESSAGE_HELP),
    };
    let (options, words) = options(rest, allowed)?;
    if options.help {
        return Ok(Cli::Help(help));
    }
    let format = if options.json {
        Format::Json
    } else {
        Format::Text
    };
    let command = match noun {
        "server" if words.is_empty() => Command::Servers,
        "server" => return Err(usage("server list takes no query")),
        "channel" => {
            let query = query::parse(words, now)?;
            query.for_channels()?;
            Command::Channels(options.scope, query)
        }
        _ => {
            let query = query::parse(words, now)?;
            let default = if query.unread() { 0 } else { 50 };
            Command::Messages {
                scope: options.scope,
                limit: options.limit.unwrap_or(default),
                query,
            }
        }
    };
    Ok(Cli::Request(Request { format, command }))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn run(line: &str) -> Result<Cli, Error> {
        let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        parse(&args, Local.timestamp_opt(1_800_000_000, 0).unwrap())
    }

    fn request(line: &str) -> Request {
        match run(line).unwrap() {
            Cli::Request(request) => request,
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn no_argument_starts_the_interface() {
        assert_eq!(run("").unwrap(), Cli::Run);
        assert_eq!(run("-h").unwrap(), Cli::Help(USAGE));
        assert_eq!(run("--version").unwrap(), Cli::Version);
        assert_eq!(run("-V").unwrap(), Cli::Version);
        assert!(matches!(run("-x"), Err(Error::Usage(_))));
        assert!(matches!(run("nope"), Err(Error::Usage(_))));
    }

    #[test]
    fn servers_are_listed_as_text_or_json() {
        let text = request("server list");
        assert_eq!(text.command, Command::Servers);
        assert_eq!(text.format, Format::Text);
        assert_eq!(request("server list --json").format, Format::Json);
        assert!(run("server list unread").is_err());
        assert!(run("server list -s work").is_err());
        assert!(run("server").is_err());
    }

    #[test]
    fn commands_can_be_shortened_or_plural() {
        assert_eq!(request("s l").command, Command::Servers);
        assert_eq!(request("servers list").command, Command::Servers);
        assert_eq!(request("mes list"), request("message list"));
        assert_eq!(request("channels li"), request("channel list"));
        assert!(run("x list").is_err());
        assert!(run("server del").is_err());
    }

    #[test]
    fn scope_is_read_from_options_and_lists() {
        let Command::Messages { scope, limit, .. } =
            request("message list -s work,home -t dev -k dm -k public -c @alice -n 200").command
        else {
            panic!("expected messages");
        };
        assert_eq!(scope.servers, ["work", "home"]);
        assert_eq!(scope.teams, ["dev"]);
        assert_eq!(
            scope.kinds,
            [ChannelKind::Direct, ChannelKind::Group, ChannelKind::Public]
        );
        assert_eq!(scope.channels, ["@alice"]);
        assert_eq!(limit, 200);
    }

    #[test]
    fn the_limit_is_50_unless_unread_is_asked() {
        let limit = |line| match request(line).command {
            Command::Messages { limit, .. } => limit,
            other => panic!("expected messages, got {other:?}"),
        };
        assert_eq!(limit("message list"), 50);
        assert_eq!(limit("message list from me"), 50);
        assert_eq!(limit("message list unread"), 0);
        assert_eq!(limit("message list not unread"), 50);
        assert_eq!(limit("message list -n 3 unread"), 3);
        assert_eq!(limit("message list -n 0"), 0);
    }

    #[test]
    fn the_query_starts_at_the_first_word_that_is_not_an_option() {
        let Command::Messages { query, .. } =
            request("message list -c general from me and body -x").command
        else {
            panic!("expected messages");
        };
        assert!(query.thread().is_none());
        assert!(run("message list -c general from me and body").is_err());
    }

    #[test]
    fn options_are_checked_against_the_command() {
        assert!(matches!(run("channel list -n 5"), Err(Error::Usage(_))));
        assert!(matches!(run("message list -k loud"), Err(Error::Usage(_))));
        assert!(matches!(run("message list -n many"), Err(Error::Usage(_))));
        assert!(matches!(run("message list -s"), Err(Error::Usage(_))));
        assert!(matches!(run("message list --nope"), Err(Error::Usage(_))));
    }

    #[test]
    fn channels_take_only_read_state_clauses() {
        assert!(run("channel list -k dm unread and not mention").is_ok());
        assert!(run("message list dm and unread or mention and unread").is_ok());
        assert!(matches!(
            run("channel list from me"),
            Err(Error::BadQuery(_))
        ));
    }

    #[test]
    fn help_is_per_command() {
        assert_eq!(run("server list -h").unwrap(), Cli::Help(SERVER_HELP));
        assert_eq!(run("c l --help").unwrap(), Cli::Help(CHANNEL_HELP));
        assert_eq!(run("m l -s work -h").unwrap(), Cli::Help(MESSAGE_HELP));
    }

    #[test]
    fn errors_carry_stable_codes_and_statuses() {
        assert_eq!(usage("x").code(), "usage");
        assert_eq!(usage("x").status(), 2);
        let refused = Error::Server {
            server: "work".into(),
            source: api::Error::Login("no".into()),
        };
        assert_eq!(refused.code(), "login-failed");
        assert_eq!(refused.status(), 1);
        assert_eq!(Error::UnknownChannel("x".into()).code(), "unknown-channel");
    }
}
