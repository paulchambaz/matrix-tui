//! Reads the servers and gathers what a command asks for, never changing anything.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use chrono::{SecondsFormat, Utc};

use crate::api::types::{ChannelKind, PostId, PostList, UserId};
use crate::api::{self, Client};
use crate::cli::{Command, Error, Scope};
use crate::config::ServerConfig;
use crate::model::{order_channels, Chan, Message, Server};
use crate::query::{Context, Query};
use crate::render::{ChannelLog, ChannelOut, MessageOut, Output, ReactionOut, ServerOut};
use crate::unread::mentions_me;

const POSTS: u32 = 200;

/// What a command found, with what went wrong on the way.
#[derive(Debug)]
pub struct Report {
    pub output: Output,
    pub failures: Vec<Error>,
    pub notices: Vec<String>,
}

#[derive(Default)]
struct Hits {
    teams: HashSet<String>,
    channels: HashSet<String>,
}

#[derive(Default)]
struct Visit {
    channels: Vec<ChannelOut>,
    logs: Vec<ChannelLog>,
}

/// Runs a command over the configured servers.
///
/// A server that fails is reported and skipped, so the others still answer.
///
/// # Errors
/// Fails when the scope names a server, team or channel that does not exist,
/// or a name matches several channels of a team.
pub async fn execute(
    command: &Command,
    servers: &[ServerConfig],
    browser: &str,
) -> Result<Report, Error> {
    let (scope, query, limit) = match command {
        Command::Servers => {
            return Ok(Report {
                output: Output::Servers(
                    servers
                        .iter()
                        .map(|s| ServerOut {
                            name: s.name.clone(),
                            url: s.url.clone(),
                        })
                        .collect(),
                ),
                failures: Vec::new(),
                notices: Vec::new(),
            })
        }
        Command::Channels(scope, query) => (scope, query, None),
        Command::Messages {
            scope,
            limit,
            query,
        } => (scope, query, Some(*limit)),
    };
    if let Some(unknown) = scope
        .servers
        .iter()
        .find(|wanted| !servers.iter().any(|s| s.name.eq_ignore_ascii_case(wanted)))
    {
        return Err(Error::UnknownServer(unknown.clone()));
    }
    let (mut hits, mut failures, mut notices) = (Hits::default(), Vec::new(), Vec::new());
    let (mut channels, mut logs) = (Vec::new(), Vec::new());
    for config in servers.iter().filter(|s| {
        scope.servers.is_empty()
            || scope
                .servers
                .iter()
                .any(|w| w.eq_ignore_ascii_case(&s.name))
    }) {
        match visit(
            config,
            browser,
            scope,
            query,
            limit,
            &mut hits,
            &mut notices,
        )
        .await
        {
            Ok(found) => {
                channels.extend(found.channels);
                logs.extend(found.logs);
            }
            Err(error) if error.status() == 1 => failures.push(error),
            Err(error) => return Err(error),
        }
    }
    if failures.is_empty() {
        if let Some(team) = scope.teams.iter().find(|t| !hits.teams.contains(*t)) {
            return Err(Error::UnknownTeam(team.clone()));
        }
        if let Some(channel) = scope.channels.iter().find(|c| !hits.channels.contains(*c)) {
            return Err(Error::UnknownChannel(channel.clone()));
        }
    }
    let output = match limit {
        None => Output::Channels(channels),
        Some(_) => Output::Messages(logs),
    };
    Ok(Report {
        output,
        failures,
        notices,
    })
}

async fn visit(
    config: &ServerConfig,
    browser: &str,
    scope: &Scope,
    query: &Query,
    limit: Option<usize>,
    hits: &mut Hits,
    notices: &mut Vec<String>,
) -> Result<Visit, Error> {
    let name = &config.name;
    let fail = |source| Error::Server {
        server: name.clone(),
        source,
    };
    let credentials = config
        .credentials(browser)
        .map_err(|e| Error::Credentials {
            server: name.clone(),
            message: format!("{e:#}"),
        })?;
    let client = Client::connect(name, &config.url, credentials)
        .await
        .map_err(fail)?;
    let mut server = Server::new(name.clone());
    server.me = Some(client.me().await.map_err(fail)?);
    let found = channels_of(&client, &mut server, scope, hits).await?;
    let Some(limit) = limit else {
        return Ok(Visit {
            channels: found
                .iter()
                .map(|(team, chan)| describe(&server, team, chan))
                .filter(|c| query.selects(c.unread > 0, c.mentions > 0))
                .collect(),
            logs: Vec::new(),
        });
    };
    let logs = match query.thread() {
        Some(id) => threaded(&client, &mut server, &found, query, id, limit, notices).await,
        None => logs_of(&client, &mut server, &found, query, limit, notices).await,
    }
    .map_err(fail)?;
    Ok(Visit {
        channels: Vec::new(),
        logs,
    })
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

async fn channels_of(
    client: &Client,
    server: &mut Server,
    scope: &Scope,
    hits: &mut Hits,
) -> Result<Vec<(String, Chan)>, Error> {
    let name = server.name.clone();
    let fail = |source| Error::Server {
        server: name.clone(),
        source,
    };
    let mut found = Vec::new();
    let mut teamless = false;
    for team in client.teams().await.map_err(fail)? {
        let mut wanted = scope.teams.is_empty();
        for asked in &scope.teams {
            if same(asked, &team.name) || same(asked, &team.display_name) {
                wanted = true;
                hits.teams.insert(asked.clone());
            }
        }
        if !wanted && teamless {
            continue;
        }
        let channels = client.channels(&team.id).await.map_err(fail)?;
        let members = client.members(&team.id).await.map_err(fail)?;
        for chan in order_channels(channels, members) {
            match chan.info.kind {
                ChannelKind::Public | ChannelKind::Private if wanted => {
                    found.push((team.name.clone(), chan));
                }
                ChannelKind::Direct | ChannelKind::Group if !teamless => {
                    found.push((String::new(), chan));
                }
                _ => {}
            }
        }
        teamless = true;
    }
    found.retain(|(_, chan)| scope.kinds.is_empty() || scope.kinds.contains(&chan.info.kind));
    found.sort_by_key(|(_, chan)| Reverse(chan.info.last_post_at));
    let others: Vec<UserId> = found
        .iter()
        .filter(|(_, chan)| chan.info.kind == ChannelKind::Direct)
        .filter_map(|(_, chan)| server.other_of(&chan.info))
        .collect();
    learn(client, server, &others).await.map_err(fail)?;
    select(server, found, &scope.channels, hits)
}

fn called(server: &Server, chan: &Chan, value: &str) -> bool {
    let info = &chan.info;
    match (value.strip_prefix('@'), info.kind) {
        (Some(user), ChannelKind::Direct | ChannelKind::Group) => same(&server.name_of(chan), user),
        (None, ChannelKind::Public | ChannelKind::Private) => {
            same(&info.name, value) || same(&info.display_name, value)
        }
        _ => false,
    }
}

fn select(
    server: &Server,
    found: Vec<(String, Chan)>,
    wanted: &[String],
    hits: &mut Hits,
) -> Result<Vec<(String, Chan)>, Error> {
    if wanted.is_empty() {
        return Ok(found);
    }
    let mut chosen = HashSet::new();
    for value in wanted {
        let hit: Vec<&(String, Chan)> = found
            .iter()
            .filter(|(_, chan)| called(server, chan, value))
            .collect();
        if let Some((team, _)) = hit
            .iter()
            .find(|(team, _)| hit.iter().filter(|(other, _)| other == team).count() > 1)
        {
            let candidates: Vec<String> = hit
                .iter()
                .filter(|(other, _)| other == team)
                .map(|(_, chan)| server.label(chan))
                .collect();
            return Err(Error::AmbiguousChannel(
                value.clone(),
                candidates.join(", "),
            ));
        }
        if !hit.is_empty() {
            hits.channels.insert(value.clone());
        }
        chosen.extend(hit.iter().map(|(_, chan)| chan.info.id.clone()));
    }
    Ok(found
        .into_iter()
        .filter(|(_, chan)| chosen.contains(&chan.info.id))
        .collect())
}

async fn learn(client: &Client, server: &mut Server, ids: &[UserId]) -> Result<(), api::Error> {
    for chunk in server.unknown(ids.iter()).chunks(100) {
        for user in client.users(chunk).await? {
            server.users.insert(user.id.clone(), user);
        }
    }
    Ok(())
}

fn authors(page: &PostList) -> Vec<UserId> {
    page.posts.values().map(|p| p.user_id.clone()).collect()
}

fn describe(server: &Server, team: &str, chan: &Chan) -> ChannelOut {
    ChannelOut {
        server: server.name.clone(),
        team: team.to_owned(),
        channel: server.name_of(chan),
        channel_id: chan.info.id.0.clone(),
        kind: chan.info.kind,
        unread: chan.unread_count(),
        mentions: chan
            .member
            .as_ref()
            .map_or(0, |m| usize::try_from(m.mention_count).unwrap_or(0)),
    }
}

struct Scan<'a> {
    query: &'a Query,
    bound: i64,
    viewed: i64,
    limit: usize,
    found: Vec<Message>,
    seen: HashMap<PostId, UserId>,
}

impl<'a> Scan<'a> {
    fn new(query: &'a Query, chan: &Chan, limit: usize) -> Self {
        let viewed = chan.member.as_ref().map_or(i64::MAX, |m| m.last_viewed_at);
        Self {
            query,
            bound: query.since(viewed),
            viewed,
            limit,
            found: Vec::new(),
            seen: HashMap::new(),
        }
    }

    fn take(&mut self, page: &PostList, server: &Server, chan: &Chan) -> bool {
        let Some(me) = server.me.as_ref() else {
            return true;
        };
        for post in page.ordered() {
            let message = Message::from_post(post);
            if message.ms <= self.bound {
                return true;
            }
            self.seen.insert(message.id.clone(), message.user.clone());
            if message.system {
                continue;
            }
            let context = Context {
                me,
                kind: chan.info.kind,
                author: server.username(&message.user),
                viewed: self.viewed,
            };
            if self.query.matches(&message, &context) {
                self.found.push(message);
                if self.limit > 0 && self.found.len() > self.limit {
                    return true;
                }
            }
        }
        false
    }

    fn into_log(
        mut self,
        server: &Server,
        team: &str,
        chan: &Chan,
        notices: &mut Vec<String>,
    ) -> Option<ChannelLog> {
        let channel = describe(server, team, chan);
        if self.limit > 0 && self.found.len() > self.limit {
            self.found.truncate(self.limit);
            notices.push(format!(
                "mx: {} {}{}: only the last {} messages are shown, see -n",
                channel.server,
                crate::model::glyph(channel.kind),
                channel.channel,
                self.limit
            ));
        }
        self.found.reverse();
        let me = server.me.as_ref()?;
        let messages: Vec<MessageOut> = self
            .found
            .iter()
            .map(|m| MessageOut {
                server: channel.server.clone(),
                team: channel.team.clone(),
                channel: channel.channel.clone(),
                channel_id: channel.channel_id.clone(),
                kind: channel.kind,
                id: m.id.0.clone(),
                root_id: m.root.as_ref().map_or_else(String::new, |r| r.0.clone()),
                time: m
                    .at
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                user: server.username(&m.user).to_owned(),
                text: m.text.clone(),
                unread: m.ms > self.viewed && m.user != me.id,
                mentions_me: mentions_me(m, chan.info.kind, me),
                edited: m.edited,
                files: m.files.iter().map(|f| f.name.clone()).collect(),
                reactions: m
                    .reactions
                    .iter()
                    .map(|r| ReactionOut {
                        emoji: r.name.clone(),
                        count: r.users.len(),
                    })
                    .collect(),
                at: m.at.format("%Y-%m-%d %H:%M").to_string(),
                reply_to: m.root.as_ref().map(|root| {
                    self.seen
                        .get(root)
                        .map_or_else(|| "…".to_owned(), |u| server.username(u).to_owned())
                }),
            })
            .collect();
        (!messages.is_empty()).then_some(ChannelLog { channel, messages })
    }
}

async fn logs_of(
    client: &Client,
    server: &mut Server,
    found: &[(String, Chan)],
    query: &Query,
    limit: usize,
    notices: &mut Vec<String>,
) -> Result<Vec<ChannelLog>, api::Error> {
    let mut logs = Vec::new();
    for (team, chan) in found {
        let mut scan = Scan::new(query, chan, limit);
        let mut before: Option<PostId> = None;
        loop {
            let page = client.posts(&chan.info.id, before.as_ref(), POSTS).await?;
            learn(client, server, &authors(&page)).await?;
            let done = scan.take(&page, server, chan);
            before = page.order.last().cloned();
            if done || page.order.len() < POSTS as usize {
                break;
            }
        }
        logs.extend(scan.into_log(server, team, chan, notices));
    }
    Ok(logs)
}

async fn threaded(
    client: &Client,
    server: &mut Server,
    found: &[(String, Chan)],
    query: &Query,
    id: &PostId,
    limit: usize,
    notices: &mut Vec<String>,
) -> Result<Vec<ChannelLog>, api::Error> {
    let mut found_list = None;
    for (_, chan) in found {
        if let Ok(list) = client.thread(&chan.info.id, id).await {
            if list.posts.contains_key(id) {
                found_list = Some(list);
                break;
            }
        }
    }
    let Some(list) = found_list else {
        return Ok(Vec::new());
    };
    let Some(root) = list.posts.get(id) else {
        return Ok(Vec::new());
    };
    let Some((team, chan)) = found
        .iter()
        .find(|(_, chan)| chan.info.id == root.channel_id)
    else {
        return Ok(Vec::new());
    };
    let mut query = query.clone();
    query.root_thread_at(&root.id);
    learn(client, server, &authors(&list)).await?;
    let mut scan = Scan::new(&query, chan, limit);
    scan.take(&list, server, chan);
    Ok(scan
        .into_log(server, team, chan, notices)
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use chrono::Local;

    use super::*;
    use crate::cli::{self, Cli};

    fn config(name: &str, url: &str) -> ServerConfig {
        ServerConfig {
            name: name.into(),
            url: url.into(),
            method: crate::config::Method::Password,
            login: String::new(),
            password: None,
            password_cmd: None,
        }
    }

    async fn run(line: &str, servers: &[ServerConfig]) -> Result<Report, Error> {
        let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        let Ok(Cli::Request(request)) = cli::parse(&args, Local::now()) else {
            panic!("not a request: {line}");
        };
        execute(&request.command, servers, "xdg-open").await
    }

    #[tokio::test]
    async fn servers_are_listed_without_the_network() {
        let servers = [
            config("work", "https://a.example.org"),
            config("club", "https://b.example.org"),
        ];
        let Output::Servers(listed) = run("server list", &servers).await.unwrap().output else {
            panic!("expected servers");
        };
        assert_eq!(listed[0].name, "work");
        assert_eq!(listed[1].url, "https://b.example.org");
    }

    #[tokio::test]
    async fn an_unknown_server_is_refused_before_any_connection() {
        let servers = [config("work", "https://a.example.org")];
        let refused = run("channel list -s nope", &servers).await;
        assert!(matches!(refused, Err(Error::UnknownServer(name)) if name == "nope"));
    }
}
