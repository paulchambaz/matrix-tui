//! The commands typed after `:`, each a typed route to something a key does or to a server action.

use std::cmp::Reverse;
use std::path::PathBuf;
use std::sync::Arc;

use super::{App, Loaded};
use crate::api::account::Session;
use crate::api::types::{ChannelId, ChannelKind, Status, UserId};
use crate::api::{Client, Error};
use crate::config;
use crate::finder;
use crate::input::{self, Command, Line};
use crate::markdown;
use crate::model::Server;

const SETTINGS: &str = "editor copy notify open.image open.video open.browser open.text open.other";

/// The names that fit what is typed after a command, best first, with the letters that matched.
pub struct Completion {
    pub command: &'static str,
    pub names: Vec<(String, Vec<usize>)>,
}

/// What `:invite`, `:kick`, `:ban`, `:unban`, `:ignore` and `:unignore` do to a user.
#[derive(Clone, Copy)]
enum Act {
    Invite,
    Kick,
    Ban,
    Unban,
    Ignore,
    Unignore,
}

impl Act {
    fn word(self) -> &'static str {
        match self {
            Act::Invite => "invite",
            Act::Kick => "kick",
            Act::Ban => "ban",
            Act::Unban => "unban",
            Act::Ignore => "ignore",
            Act::Unignore => "unignore",
        }
    }

    fn past(self) -> &'static str {
        match self {
            Act::Invite => "Invited",
            Act::Kick => "Kicked",
            Act::Ban => "Banned",
            Act::Unban => "Unbanned",
            Act::Ignore => "Ignored",
            Act::Unignore => "Unignored",
        }
    }
}

async fn resolve(client: &Client, name: &str, known: Option<UserId>) -> Result<UserId, Error> {
    match known {
        Some(id) => Ok(id),
        None => Ok(client.user_named(name).await?.id),
    }
}

/// One line of the sessions page: when it was last seen, its id, its name and address.
fn session_line(s: &Session) -> String {
    let seen = chrono::DateTime::from_timestamp_millis(s.seen).map_or_else(
        || "never".to_owned(),
        |d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        },
    );
    let mine = if s.current { "  (this one)" } else { "" };
    format!("{seen}  {}  {}  {}{mine}", s.id, s.name, s.ip)
}

/// A path as typed, with a leading `~/` meaning the home folder.
fn expand(typed: &str) -> PathBuf {
    match (typed.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(typed),
    }
}

/// The entries of the folder a typed path points into, folders ending in `/`.
fn paths(typed: &str) -> Vec<String> {
    let (folder, _) = typed.rsplit_once('/').unwrap_or(("", typed));
    let base = if typed.contains('/') {
        format!("{folder}/")
    } else {
        String::new()
    };
    let Ok(entries) = std::fs::read_dir(expand(if base.is_empty() { "." } else { &base })) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| {
            let slash = if e.path().is_dir() { "/" } else { "" };
            format!("{base}{}{slash}", e.file_name().to_string_lossy())
        })
        .collect();
    names.sort();
    names
}

impl App {
    /// What the command line offers to complete, for the commands that take a name.
    pub fn completion(&self) -> Option<Completion> {
        if self.wizard.is_some() || self.changing.is_some() {
            return None;
        }
        let text = self.input.as_ref()?.text();
        let Some((word, rest)) = text.split_once(' ') else {
            let base = self.cycle.as_deref().unwrap_or(&text);
            let hits: Vec<usize> = (0..base.chars().count()).collect();
            let names = input::starting(base)
                .into_iter()
                .map(|name| (name.to_owned(), hits.clone()))
                .collect();
            return Some(Completion { command: "", names });
        };
        let word = match input::matching(word).as_slice() {
            [only] => *only,
            _ => return None,
        };
        let rest = rest.trim().trim_start_matches('@');
        let server = self.servers.get(self.server);
        let (command, pool): (&'static str, Vec<String>) = match word {
            "server" | "remove-server" => (
                if word == "server" {
                    "server"
                } else {
                    "remove-server"
                },
                self.server_names().into_iter().map(|(_, n)| n).collect(),
            ),
            "team" => (
                "team",
                server
                    .iter()
                    .flat_map(|s| s.teams.iter().map(|t| t.info.display_name.clone()))
                    .collect(),
            ),
            "dm" | "invite" | "kick" | "ban" | "unban" | "ignore" | "unignore" | "verify"
            | "trust" => {
                let me = server.and_then(|s| s.me.as_ref()).map(|m| &m.id);
                let mut names: Vec<String> = server
                    .iter()
                    .flat_map(|s| s.users.values())
                    .filter(|u| Some(&u.id) != me)
                    .map(|u| u.username.clone())
                    .collect();
                names.sort();
                (word, names)
            }
            "upload" => ("upload", paths(rest)),
            _ => return None,
        };
        let mut found: Vec<(i32, String, Vec<usize>)> = pool
            .into_iter()
            .filter_map(|name| {
                let (score, hits) = finder::fuzzy(rest, &name)?;
                Some((score, name, hits))
            })
            .collect();
        found.sort_by_key(|(score, ..)| Reverse(*score));
        let names = found.into_iter().map(|(_, n, h)| (n, h)).collect();
        Some(Completion { command, names })
    }

    /// Moves through the completion: a name is chosen, or a command is written in full.
    pub(super) fn tab(&mut self, back: bool) {
        let Some(offered) = self.completion() else {
            return;
        };
        let count = offered.names.len();
        if count == 0 {
            return;
        }
        let step = |from: usize| match back {
            true => (from + count - 1) % count,
            false => (from + 1) % count,
        };
        let from = self.choice.min(count - 1);
        if !offered.command.is_empty() {
            self.choice = step(from);
            return;
        }
        let next = match (&self.cycle, back) {
            (Some(_), _) => step(from),
            (None, false) => 0,
            (None, true) => count - 1,
        };
        let base = self
            .cycle
            .take()
            .or_else(|| self.input.as_ref().map(Line::text));
        self.input = Some(Line::with_text(&offered.names[next].0, false));
        (self.choice, self.cycle) = (next, base);
    }

    /// The line to run: as typed, or with the name chosen in the completion.
    pub(super) fn completed(&self, typed: &str) -> String {
        match self.completion() {
            Some(c) if !c.command.is_empty() && !c.names.is_empty() => {
                let chosen = &c.names[self.choice.min(c.names.len() - 1)].0;
                format!("{} {chosen}", c.command)
            }
            _ => typed.to_owned(),
        }
    }

    /// Runs one parsed command.
    pub(super) fn run_command(&mut self, command: Command) {
        match command {
            Command::NewServer => self.new_server(),
            Command::Help => self.help = Some(0),
            Command::Quit => self.quit = true,
            Command::Server(name) => self.go_server(&name),
            Command::Team(name) => self.go_team(&name),
            Command::Go(words) => self.go(&words),
            Command::RemoveServer(name) => self.remove_server(&name),
            Command::Reload => self.reload(),
            Command::MarkRead => self.mark_read(),
            Command::MarkAllRead => self.mark_all_read(),
            Command::Users => self.toggle_users(),
            Command::Reactions => self.reactions = !self.reactions,
            Command::Open => self.open_selected(),
            Command::Copy(what) => match what.as_str() {
                "" => self.yank(false),
                "full" => self.yank(true),
                _ => self.notice = "Usage: :copy [full]".into(),
            },
            Command::Upload(path) => self.upload(&path),
            Command::React(name) => self.react_named(&name),
            Command::Set(key, value) => self.set(&key, &value),
            Command::Topic(text) => self.topic(&text),
            Command::Create(name) => self.create(&name),
            Command::Space(words) => self.space(&words),
            Command::Invite(name) => self.member(Act::Invite, &name),
            Command::Kick(name) => self.member(Act::Kick, &name),
            Command::Ban(name) => self.member(Act::Ban, &name),
            Command::Unban(name) => self.member(Act::Unban, &name),
            Command::Rename(name) => self.rename(&name),
            Command::Nick(name) => self.nick(&name),
            Command::Ignore(name) => self.member(Act::Ignore, &name),
            Command::Unignore(name) => self.member(Act::Unignore, &name),
            Command::Report(reason) => self.report(&reason),
            Command::Search(text) => self.search(&text),
            Command::Location(place) => self.location(&place),
            Command::Dm(name) => self.dm(&name),
            Command::Join(name) => self.join(&name),
            Command::Leave => self.leave(),
            Command::Status(word) => self.set_status(&word),
            Command::Recover(key) => self.recover(&key),
            Command::Recovery => self.recovery(),
            Command::Sessions => self.sessions(),
            Command::Logout(id) => self.logout(&id),
            Command::Password => self.password(),
            Command::Deactivate(id) => self.deactivate(&id),
            Command::Verify(name) => self.verify(&name),
            Command::Trust(name) => self.trust(&name),
            Command::Pin => self.pin(true),
            Command::Unpin => self.pin(false),
        }
    }

    fn pick(
        &mut self,
        (command, kind): (&str, &str),
        names: &[(usize, String)],
        wanted: &str,
    ) -> Option<usize> {
        if wanted.is_empty() {
            self.notice = format!("Usage: :{command} <name>");
            return None;
        }
        let wanted = wanted.to_lowercase();
        if let Some((i, _)) = names.iter().find(|(_, n)| n.to_lowercase() == wanted) {
            return Some(*i);
        }
        let hits: Vec<&(usize, String)> = names
            .iter()
            .filter(|(_, n)| n.to_lowercase().contains(&wanted))
            .collect();
        match hits.as_slice() {
            [(i, _)] => Some(*i),
            [] => {
                self.notice = format!("No {kind} matches {wanted}");
                None
            }
            many => {
                let all: Vec<&str> = many.iter().map(|(_, n)| n.as_str()).collect();
                self.notice = format!("Several match: {}", all.join(", "));
                None
            }
        }
    }

    fn server_names(&self) -> Vec<(usize, String)> {
        let live = self
            .servers
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.is_gone());
        live.map(|(i, s)| (i, s.name.clone())).collect()
    }

    fn go_server(&mut self, name: &str) {
        let names = self.server_names();
        if let Some(i) = self.pick(("server", "server"), &names, name) {
            (self.server, self.team) = (i, 0);
            self.reset_view();
            self.ensure();
        }
    }

    fn go_team(&mut self, name: &str) {
        let Some(server) = self.servers.get(self.server) else {
            return;
        };
        let names: Vec<(usize, String)> = server
            .teams
            .iter()
            .enumerate()
            .map(|(t, team)| (t, team.info.display_name.clone()))
            .collect();
        if let Some(t) = self.pick(("team", "team"), &names, name) {
            self.team = t;
            self.reset_view();
            self.ensure();
        }
    }

    fn go(&mut self, text: &str) {
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.is_empty() {
            self.notice = "Usage: :go <words>".into();
            return;
        }
        let here = (self.server, self.team);
        let candidates = finder::candidates(&self.servers, here, false);
        let found = finder::entries(&words, &candidates, false);
        let target = found
            .first()
            .map(|e| (e.candidate.server, e.candidate.team, e.candidate.chan));
        match target {
            Some((server, team, chan)) => {
                (self.server, self.team) = (server, team);
                self.go_channel(chan);
            }
            None => self.notice = format!("No channel matches {text}"),
        }
    }

    pub(super) fn remove_server(&mut self, name: &str) {
        let names = self.server_names();
        let Some(i) = self.pick(("remove-server", "server"), &names, name) else {
            return;
        };
        let old = std::mem::replace(&mut self.servers[i], Server::new(String::new()));
        if let Some(task) = &old.sync {
            task.abort();
        }
        self.configs.retain(|c| c.name != old.name);
        self.notice = match config::save(&self.path, &self.configs) {
            Ok(()) => format!("Removed {}", old.name),
            Err(e) => format!(
                "Removed {}, but the server list was not saved: {e}",
                old.name
            ),
        };
        if self.entry.as_ref().is_some_and(|e| e.server == i) {
            self.entry = None;
        }
        self.drafts.retain(|(server, _), _| *server != i);
        if self.server == i {
            if let Some(&(server, team)) = self.places().first() {
                (self.server, self.team) = (server, team);
                self.reset_view();
                self.ensure();
            }
        }
    }

    fn reload(&mut self) {
        let Some((server, chan)) = self.channel() else {
            return;
        };
        let id = chan.info.id.clone();
        let Some(team) = server.teams.get(self.team).map(|t| t.info.id.clone()) else {
            return;
        };
        let i = self.server;
        if let Some(server) = self.servers.get_mut(i) {
            server.messages.remove(&id);
            server.complete.remove(&id);
            server.refreshed.remove(&id);
        }
        self.fetch_channels(i, team, std::time::Duration::ZERO);
        self.fetch_posts(i, id);
        self.reset_view();
        self.notice = "Reloading…".into();
    }

    fn mark_all_read(&mut self) {
        let i = self.server;
        let Some(team) = self.servers.get(i).and_then(|s| s.teams.get(self.team)) else {
            return;
        };
        let unread: Vec<ChannelId> = team
            .chans
            .iter()
            .flatten()
            .filter(|c| c.unread())
            .map(|c| c.info.id.clone())
            .collect();
        for id in &unread {
            self.servers[i].mark_viewed(id);
            self.send_view(i, id.clone(), None);
        }
        if let Some(entry) = &mut self.entry {
            if entry.server == i && unread.contains(&entry.channel) {
                entry.unread = 0;
                entry.boundary = None;
            }
        }
        self.notice = match unread.len() {
            0 => "Nothing unread".into(),
            1 => "Marked 1 channel as read".into(),
            n => format!("Marked {n} channels as read"),
        };
    }

    fn react_named(&mut self, name: &str) {
        let name = name.trim_matches(':');
        if name.is_empty() {
            self.notice = "Usage: :react <emoji>".into();
            return;
        }
        let shortcode = format!(":{name}:");
        if markdown::emoji(&shortcode) == shortcode {
            self.notice = format!("Unknown emoji {name}");
            return;
        }
        self.react(name);
    }

    fn set(&mut self, key: &str, value: &str) {
        let optional = (!value.is_empty()).then(|| value.to_owned());
        let open = &mut self.settings.open;
        match key {
            "editor" => self.settings.editor = optional,
            "copy" => self.settings.copy = optional,
            "notify" => self.settings.notify = optional,
            "open.image" | "open.video" | "open.browser" | "open.text" | "open.other"
                if value.is_empty() =>
            {
                self.notice = format!("Usage: :set {key} <program>");
                return;
            }
            "open.image" => open.image = value.to_owned(),
            "open.video" => open.video = value.to_owned(),
            "open.browser" => open.browser = value.to_owned(),
            "open.text" => open.text = value.to_owned(),
            "open.other" => open.other = value.to_owned(),
            _ => {
                self.notice = format!("Settings: {SETTINGS}");
                return;
            }
        }
        self.notice = match value.is_empty() {
            true => format!("{key} reset"),
            false => format!("{key} = {value}"),
        };
    }

    fn topic(&mut self, text: &str) {
        let Some((_, chan)) = self.channel() else {
            return;
        };
        if !text.is_empty() {
            let id = chan.info.id.clone();
            let text = text.to_owned();
            let Some((client, _)) = self.session() else {
                return;
            };
            self.spawn(self.server, async move {
                match client.set_topic(&id, &text).await {
                    Ok(()) => Loaded::Said("Topic changed".into()),
                    Err(e) => Loaded::Failed(e.to_string()),
                }
            });
            return;
        }
        let text: Vec<&str> = [&chan.info.header, &chan.info.purpose]
            .into_iter()
            .map(|part| part.trim())
            .filter(|part| !part.is_empty())
            .collect();
        self.notice = match text.is_empty() {
            true => "No topic".into(),
            false => text.join(" · ").replace('\n', " "),
        };
    }

    fn session(&mut self) -> Option<(Arc<Client>, UserId)> {
        let server = self.servers.get(self.server)?;
        let found = server
            .client
            .clone()
            .zip(server.me.as_ref().map(|m| m.id.clone()));
        if found.is_none() {
            self.notice = "Not connected".into();
        }
        found
    }

    fn known_user(&self, name: &str) -> Option<UserId> {
        let server = self.servers.get(self.server)?;
        let same = |u: &&crate::api::types::User| u.username.eq_ignore_ascii_case(name);
        server.users.values().find(same).map(|u| u.id.clone())
    }

    fn dm(&mut self, names: &str) {
        let names: Vec<String> = names
            .split_whitespace()
            .map(|n| n.trim_start_matches('@').to_owned())
            .collect();
        if names.is_empty() {
            self.notice = "Usage: :dm <user>...".into();
            return;
        }
        let Some((client, _)) = self.session() else {
            return;
        };
        let people: Vec<(String, Option<UserId>)> = names
            .into_iter()
            .map(|n| {
                let known = self.known_user(&n);
                (n, known)
            })
            .collect();
        self.spawn(self.server, async move {
            let opened = async {
                let mut ids = Vec::new();
                for (name, known) in &people {
                    ids.push(resolve(&client, name, known.clone()).await?);
                }
                match ids.as_slice() {
                    [only] => client.create_direct(only).await,
                    _ => client.create_group(&ids).await,
                }
            };
            opened
                .await
                .map_or_else(|e| Loaded::Failed(e.to_string()), |c| Loaded::Opened(c.id))
        });
    }

    fn create(&mut self, words: &str) {
        let (public, name) = match words.split_once(' ') {
            Some(("public", name)) => (true, name.trim()),
            _ => (false, words),
        };
        if name.is_empty() {
            self.notice = "Usage: :create [public] <name>".into();
            return;
        }
        let Some((client, _)) = self.session() else {
            return;
        };
        let name = name.to_owned();
        self.spawn(self.server, async move {
            client
                .create_room(&name, public)
                .await
                .map_or_else(|e| Loaded::Failed(e.to_string()), |c| Loaded::Opened(c.id))
        });
    }

    fn space(&mut self, words: &str) {
        let (action, rest) = words.split_once(' ').unwrap_or((words, ""));
        let rest = rest.trim();
        if !(matches!(action, "add" | "remove") || (action == "new" && !rest.is_empty())) {
            self.notice = "Usage: :space new <name> | add <space> | remove".into();
            return;
        }
        let Some((client, _)) = self.session() else {
            return;
        };
        let channel = self.channel().map(|(_, c)| c.info.id.clone());
        let current = self
            .servers
            .get(self.server)
            .and_then(|s| s.teams.get(self.team));
        let current = current.map(|t| t.info.id.clone());
        match (action, channel, current) {
            ("new", _, _) if !rest.is_empty() => {
                let name = rest.to_owned();
                self.spawn(self.server, async move {
                    match client.create_space(&name).await {
                        Ok(()) => Loaded::Refreshed(format!("Space {name} created")),
                        Err(e) => Loaded::Failed(e.to_string()),
                    }
                });
            }
            ("add", Some(channel), _) => {
                let names: Vec<(usize, String)> = self
                    .servers
                    .get(self.server)
                    .into_iter()
                    .flat_map(|s| s.teams.iter().enumerate())
                    .skip(1)
                    .map(|(t, team)| (t, team.info.display_name.clone()))
                    .collect();
                let Some(at) = self.pick(("space add", "space"), &names, rest) else {
                    return;
                };
                let Some(team) = self.servers[self.server].teams.get(at) else {
                    return;
                };
                let (id, name) = (team.info.id.clone(), team.info.display_name.clone());
                self.spawn(self.server, async move {
                    match client.space_add(&id, &channel).await {
                        Ok(()) => Loaded::Refreshed(format!("Added to {name}")),
                        Err(e) => Loaded::Failed(e.to_string()),
                    }
                });
            }
            ("remove", Some(channel), Some(team)) => {
                self.spawn(self.server, async move {
                    match client.space_remove(&team, &channel).await {
                        Ok(()) => Loaded::Refreshed("Removed from the space".into()),
                        Err(e) => Loaded::Failed(e.to_string()),
                    }
                });
            }
            _ => {}
        }
    }

    fn member(&mut self, act: Act, name: &str) {
        let name = name.trim_start_matches('@').to_owned();
        if name.is_empty() {
            self.notice = format!("Usage: :{} <user>", act.word());
            return;
        }
        let Some(id) = self.channel().map(|(_, c)| c.info.id.clone()) else {
            return;
        };
        let Some((client, _)) = self.session() else {
            return;
        };
        let known = self.known_user(&name);
        self.spawn(self.server, async move {
            let done = async {
                let user = resolve(&client, &name, known).await?;
                match act {
                    Act::Invite => client.invite(&id, &user).await,
                    Act::Kick => client.kick(&id, &user).await,
                    Act::Ban => client.ban(&id, &user).await,
                    Act::Unban => client.unban(&id, &user).await,
                    Act::Ignore => client.ignore(&user).await,
                    Act::Unignore => client.unignore(&user).await,
                }
            };
            match done.await {
                Ok(()) => Loaded::Said(format!("{} {name}", act.past())),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn rename(&mut self, name: &str) {
        let Some(id) = self.channel().map(|(_, c)| c.info.id.clone()) else {
            return;
        };
        if name.is_empty() {
            self.notice = "Usage: :rename <name>".into();
            return;
        }
        let Some((client, _)) = self.session() else {
            return;
        };
        let name = name.to_owned();
        self.spawn(self.server, async move {
            match client.rename(&id, &name).await {
                Ok(()) => Loaded::Said(format!("Renamed to {name}")),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn nick(&mut self, name: &str) {
        if name.is_empty() {
            self.notice = "Usage: :nick <name>".into();
            return;
        }
        let Some((client, _)) = self.session() else {
            return;
        };
        let name = name.to_owned();
        self.spawn(self.server, async move {
            match client.set_nick(&name).await {
                Ok(()) => Loaded::Said(format!("You are now {name}")),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn join(&mut self, name: &str) {
        let name = name.to_owned();
        if name.is_empty() {
            let invited = self
                .channel()
                .filter(|(_, c)| c.info.invited.is_some())
                .map(|(_, c)| c.info.id.clone());
            let (Some(id), Some((client, _))) = (invited, self.session()) else {
                self.notice = "Usage: :join <channel>".into();
                return;
            };
            self.spawn(self.server, async move {
                client
                    .join(&id)
                    .await
                    .map_or_else(|e| Loaded::Failed(e.to_string()), |()| Loaded::Opened(id))
            });
            return;
        }
        let team = self
            .servers
            .get(self.server)
            .and_then(|s| s.teams.get(self.team))
            .map(|t| t.info.id.clone());
        let (Some(team), Some((client, _))) = (team, self.session()) else {
            return;
        };
        self.spawn(self.server, async move {
            let joined = async {
                let channel = client.channel_named(&team, &name).await?;
                client.join(&channel.id).await?;
                Ok::<_, Error>(channel.id)
            };
            joined
                .await
                .map_or_else(|e| Loaded::Failed(e.to_string()), Loaded::Opened)
        });
    }

    fn leave(&mut self) {
        let Some((server, chan)) = self.channel() else {
            return;
        };
        let direct = matches!(chan.info.kind, ChannelKind::Direct | ChannelKind::Group);
        if direct && chan.info.invited.is_none() {
            self.notice = "Direct and group conversations cannot be left".into();
            return;
        }
        let label = server.label(chan);
        let id = chan.info.id.clone();
        let Some((client, _)) = self.session() else {
            return;
        };
        self.spawn(self.server, async move {
            match client.leave(&id).await {
                Ok(()) => Loaded::Left(label),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn set_status(&mut self, word: &str) {
        let status = match word {
            "online" | "away" | "dnd" | "offline" => Status::parse(word),
            _ => {
                self.notice = "Usage: :status online|away|dnd|offline".into();
                return;
            }
        };
        let Some((client, _)) = self.session() else {
            return;
        };
        self.spawn(self.server, async move {
            match client.set_status(status).await {
                Ok(()) => Loaded::StatusSet(status),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn upload(&mut self, path: &str) {
        if path.is_empty() {
            self.notice = "Usage: :upload <path>".into();
            return;
        }
        let Some((client, _)) = self.session() else {
            return;
        };
        let Some(channel) = self.channel().map(|(_, c)| c.info.id.clone()) else {
            return;
        };
        let path = expand(path);
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        self.notice = format!("Uploading {name}...");
        self.spawn(self.server, async move {
            match client.upload(&channel, &path).await {
                Ok(()) => Loaded::Said(format!("Sent {name}")),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn recover(&mut self, key: &str) {
        if key.is_empty() {
            self.notice = "Usage: :recover <recovery key>".into();
            return;
        }
        let Some((client, _)) = self.session() else {
            return;
        };
        let key = key.to_owned();
        self.spawn(self.server, async move {
            match client.recover(&key).await {
                Ok(()) => {
                    Loaded::Said("Recovered: older encrypted messages will open on reload".into())
                }
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn verify(&mut self, name: &str) {
        if name.is_empty() {
            let Some((client, _)) = self.session() else {
                return;
            };
            self.notice = "Waiting for the other session to accept...".into();
            return self.spawn(self.server, async move {
                Loaded::Verification(client.verify().await.map_err(|e| e.to_string()))
            });
        }
        let Some((client, name, known)) = self.person("verify", name) else {
            return;
        };
        self.notice = format!("Waiting for {name} to accept...");
        self.spawn(self.server, async move {
            let checked = async {
                let user = resolve(&client, &name, known).await?;
                client.verify_user(&user).await
            };
            Loaded::Verification(checked.await.map_err(|e| e.to_string()))
        });
    }

    fn trust(&mut self, name: &str) {
        let Some((client, name, known)) = self.person("trust", name) else {
            return;
        };
        self.spawn(self.server, async move {
            let done = async {
                let user = resolve(&client, &name, known).await?;
                client.trust(&user).await
            };
            match done.await {
                Ok(()) => Loaded::Said(format!("Accepted the keys of {name}")),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn person(
        &mut self,
        command: &str,
        name: &str,
    ) -> Option<(Arc<Client>, String, Option<UserId>)> {
        let name = name.trim_start_matches('@').to_owned();
        if name.is_empty() {
            self.notice = format!("Usage: :{command} <user>");
            return None;
        }
        let (client, _) = self.session()?;
        let known = self.known_user(&name);
        Some((client, name, known))
    }

    fn recovery(&mut self) {
        let Some((client, _)) = self.session() else {
            return;
        };
        self.notice = "Creating the recovery key...".into();
        self.spawn(self.server, async move {
            match client.recovery_key().await {
                Ok(key) => Loaded::Page {
                    title: "Recovery key: write it down, it is shown only once".into(),
                    lines: vec![key, String::new(), "Any older key no longer works.".into()],
                },
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn sessions(&mut self) {
        let Some((client, _)) = self.session() else {
            return;
        };
        self.spawn(self.server, async move {
            match client.sessions().await {
                Ok(list) => Loaded::Page {
                    title: format!("{} sessions (:logout <id> ends one)", list.len()),
                    lines: list.iter().map(session_line).collect(),
                },
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn logout(&mut self, id: &str) {
        if id.is_empty() {
            self.notice = "Usage: :logout <session id>".into();
            return;
        }
        let Some((client, _)) = self.session() else {
            return;
        };
        let id = id.to_owned();
        self.spawn(self.server, async move {
            match client.end_session(&id).await {
                Ok(()) => Loaded::Said(format!("Logged {id} out")),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn password(&mut self) {
        if self.session().is_some() {
            self.changing = Some(None);
            self.input = Some(Line::new(true));
        }
    }

    pub(super) fn new_password(&mut self, first: Option<String>, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(first) = first else {
            self.changing = Some(Some(text.to_owned()));
            self.input = Some(Line::new(true));
            return;
        };
        if first != text {
            self.notice = "The passwords differ".into();
            return;
        }
        let Some((client, _)) = self.session() else {
            return;
        };
        self.spawn(self.server, async move {
            match client.change_password(&first).await {
                Ok(()) => Loaded::Said(
                    "Password changed: update password or password_cmd in servers.toml".into(),
                ),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn deactivate(&mut self, id: &str) {
        let Some((client, me)) = self.session() else {
            return;
        };
        if id != me.0 {
            self.notice = format!(
                "This closes the account for good: type :deactivate {}",
                me.0
            );
            return;
        }
        self.spawn(self.server, async move {
            match client.deactivate().await {
                Ok(()) => Loaded::Deactivated,
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn report(&mut self, reason: &str) {
        let Some(target) = self.target() else {
            return;
        };
        let Some((client, _)) = self.session() else {
            return;
        };
        let Some(channel) = self.channel().map(|(_, c)| c.info.id.clone()) else {
            return;
        };
        let reason = reason.to_owned();
        self.spawn(self.server, async move {
            match client.report(&channel, &target.id, &reason).await {
                Ok(()) => Loaded::Said("Reported".into()),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn search(&mut self, text: &str) {
        if text.is_empty() {
            self.notice = "Usage: :search <text>".into();
            return;
        }
        let Some(channel) = self.channel().map(|(_, c)| c.info.id.clone()) else {
            return;
        };
        let Some((client, _)) = self.session() else {
            return;
        };
        let text = text.to_owned();
        self.notice = "Searching...".into();
        self.spawn(self.server, async move {
            match client.search(&channel, &text).await {
                Ok((list, run, read)) => Loaded::Found {
                    text,
                    channel,
                    list,
                    run,
                    read,
                },
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn location(&mut self, place: &str) {
        let mut words = place.split([' ', ',']).filter(|w| !w.is_empty());
        let (Some(latitude), Some(longitude), None) = (words.next(), words.next(), words.next())
        else {
            self.notice = "Usage: :location <latitude> <longitude>".into();
            return;
        };
        if latitude.parse::<f64>().is_err() || longitude.parse::<f64>().is_err() {
            self.notice = "Usage: :location <latitude> <longitude>".into();
            return;
        }
        let Some(channel) = self.channel().map(|(_, c)| c.info.id.clone()) else {
            return;
        };
        let Some((client, _)) = self.session() else {
            return;
        };
        let (latitude, longitude) = (latitude.to_owned(), longitude.to_owned());
        self.spawn(self.server, async move {
            match client.send_location(&channel, &latitude, &longitude).await {
                Ok(()) => Loaded::Said("Location sent".into()),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn pin(&mut self, pinned: bool) {
        let Some(target) = self.target() else {
            return;
        };
        let Some((client, _)) = self.session() else {
            return;
        };
        let Some(channel) = self.channel().map(|(_, c)| c.info.id.clone()) else {
            return;
        };
        self.spawn(self.server, async move {
            match client.pin(&channel, &target.id, pinned).await {
                Ok(()) => Loaded::Said(if pinned { "Pinned" } else { "Unpinned" }.into()),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::TeamId;
    use crate::app::tests::{fixture, json, press, press_code};
    use crate::config::ServerConfig;
    use crate::model::Team;
    use ratatui::crossterm::event::KeyCode;

    fn run(app: &mut App, text: &str) {
        press(app, &format!(":{text}"));
        press_code(app, KeyCode::Enter);
    }

    fn here(app: &App) -> String {
        app.channel().unwrap().1.info.id.0.clone()
    }

    fn team(name: &str) -> Team {
        Team {
            info: serde_json::from_value(
                serde_json::json!({"id": name, "name": name, "display_name": name}),
            )
            .unwrap(),
            chans: None,
            loading: false,
            cur: 0,
        }
    }

    #[test]
    fn the_new_password_is_asked_twice_hidden_and_a_mismatch_is_refused() {
        let mut app = fixture();
        app.new_password(None, "first");
        assert_eq!(app.changing, Some(Some("first".into())));
        assert_eq!(app.prompt(), "Again: ");
        press(&mut app, "ab");
        assert_eq!(app.input.as_ref().map(Line::shown).as_deref(), Some("••"));
        app.new_password(Some("first".into()), "second");
        assert_eq!(app.notice, "The passwords differ");
        app.changing = Some(None);
        assert_eq!(app.prompt(), "New password: ");
        assert!(app.completion().is_none());
        press_code(&mut app, KeyCode::Esc);
        assert!(app.changing.is_none());
    }

    #[test]
    fn a_page_from_the_server_opens_the_results_view() {
        let mut app = fixture();
        app.loaded(
            0,
            Loaded::Page {
                title: "2 sessions".into(),
                lines: vec!["a".into(), "b".into()],
            },
        );
        assert_eq!(app.mode(), "RESULTS");
        assert_eq!(app.found.as_ref().map(|f| f.lines.len()), Some(2));
    }

    #[test]
    fn upload_without_a_path_gives_the_usage() {
        let mut app = fixture();
        run(&mut app, "upload");
        assert_eq!(app.notice, "Usage: :upload <path>");
    }

    #[test]
    fn the_room_commands_without_an_argument_give_their_usage() {
        let mut app = fixture();
        for (command, usage) in [
            ("create", "Usage: :create [public] <name>"),
            ("invite", "Usage: :invite <user>"),
            ("kick", "Usage: :kick <user>"),
            ("ban", "Usage: :ban <user>"),
            ("unban", "Usage: :unban <user>"),
            ("rename", "Usage: :rename <name>"),
            ("nick", "Usage: :nick <name>"),
            ("ignore", "Usage: :ignore <user>"),
            ("unignore", "Usage: :unignore <user>"),
            ("search", "Usage: :search <text>"),
            ("location", "Usage: :location <latitude> <longitude>"),
            ("location 48.8", "Usage: :location <latitude> <longitude>"),
            ("location a b", "Usage: :location <latitude> <longitude>"),
            ("dm", "Usage: :dm <user>..."),
            ("trust", "Usage: :trust <user>"),
            ("logout", "Usage: :logout <session id>"),
            ("space", "Usage: :space new <name> | add <space> | remove"),
            (
                "space new",
                "Usage: :space new <name> | add <space> | remove",
            ),
        ] {
            app.notice.clear();
            run(&mut app, command);
            assert_eq!(app.notice, usage);
        }
    }

    #[test]
    fn paths_list_the_folder_typed_with_folders_ending_in_a_slash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("pics")).unwrap();
        std::fs::write(dir.path().join("a.png"), b"").unwrap();
        let base = format!("{}/", dir.path().display());
        assert_eq!(
            paths(&format!("{base}x")),
            vec![format!("{base}a.png"), format!("{base}pics/")]
        );
        assert!(paths("/nonexistent/x").is_empty());
    }

    #[test]
    fn servers_and_teams_are_picked_by_name_and_ambiguity_is_reported() {
        let mut app = fixture();
        app.servers.push(Server::new("club".into()));
        app.servers.push(Server::new("clubby".into()));
        app.run_command(Command::Server("cl".into()));
        assert_eq!(app.notice, "Several match: club, clubby");
        run(&mut app, "server CLUB");
        assert_eq!(app.server, 1);
        run(&mut app, "server wor");
        assert_eq!((app.server, app.team), (0, 0));
        run(&mut app, "server zzz");
        assert_eq!(app.notice, "No server matches zzz");
        run(&mut app, "server");
        assert_eq!(app.notice, "Usage: :server <name>");
        app.servers[0].teams.push(team("Design"));
        app.run_command(Command::Team("e".into()));
        assert_eq!(app.notice, "Several match: Engineering, Design");
        run(&mut app, "team desi");
        assert_eq!(app.team, 1);
    }

    #[test]
    fn typing_after_a_command_offers_names_and_enter_takes_the_chosen_one() {
        let mut app = fixture();
        app.servers.push(Server::new("club".into()));
        app.servers.push(Server::new("clubby".into()));
        press(&mut app, ":server cl");
        let offered = app.completion().unwrap();
        let names: Vec<&str> = offered.names.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["club", "clubby"]);
        press_code(&mut app, KeyCode::Tab);
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.server, 2);
        assert_eq!(app.choice, 0);
        press(&mut app, ":server ");
        press_code(&mut app, KeyCode::BackTab);
        assert_eq!(app.choice, 2);
        press(&mut app, "x");
        assert_eq!(app.choice, 0);
        assert!(app.completion().unwrap().names.is_empty());
        press_code(&mut app, KeyCode::Esc);
        assert!(app.completion().is_none());
    }

    #[test]
    fn teams_and_users_are_offered_too() {
        let mut app = fixture();
        app.servers[0].teams.push(team("Design"));
        press(&mut app, ":team ");
        assert_eq!(app.completion().unwrap().names[0].0, "Design");
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.team, 1);
        app.servers[0].users.insert(
            UserId("u9".into()),
            crate::app::tests::json_user("u9", "zoe"),
        );
        press(&mut app, ":dm @z");
        let offered = app.completion().unwrap();
        assert_eq!(offered.command, "dm");
        assert_eq!(offered.names[0].0, "zoe");
        assert!(offered.names.iter().all(|(n, _)| n != "paul"));
        press_code(&mut app, KeyCode::Esc);
        press(&mut app, ":copy f");
        assert!(app.completion().is_none());
    }

    #[test]
    fn a_command_works_from_its_shortest_start_and_ambiguity_is_said() {
        let mut app = fixture();
        run(&mut app, "to");
        assert_eq!(app.notice, "No topic");
        run(&mut app, "mark");
        assert_eq!(app.notice, "Ambiguous: mark-read, mark-all-read");
        run(&mut app, "zz");
        assert_eq!(app.notice, "Unknown command: zz");
        app.servers.push(Server::new("club".into()));
        run(&mut app, "ser clu");
        assert_eq!(app.server, 1);
        run(&mut app, "q");
        assert!(app.quit);
    }

    #[test]
    fn tab_writes_the_command_in_full_then_cycles_and_names_follow() {
        let mut app = fixture();
        press(&mut app, ":re");
        let offered = app.completion().unwrap();
        assert_eq!(offered.command, "");
        let names: Vec<&str> = offered.names.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "remove-server",
                "rename",
                "report",
                "reload",
                "reactions",
                "react",
                "recover",
                "recovery"
            ]
        );
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(app.input.as_ref().unwrap().text(), "remove-server");
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(app.input.as_ref().unwrap().text(), "rename");
        press(&mut app, " x");
        assert_eq!(app.input.as_ref().unwrap().text(), "rename x");
        press_code(&mut app, KeyCode::Esc);
        press(&mut app, ":server ");
        assert_eq!(app.completion().unwrap().command, "server");
        press_code(&mut app, KeyCode::Esc);
        press_code(&mut app, KeyCode::Esc);
        press(&mut app, ":r");
        press_code(&mut app, KeyCode::Tab);
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(app.input.as_ref().unwrap().text(), "rename");
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(app.input.as_ref().unwrap().text(), "report");
        press_code(&mut app, KeyCode::BackTab);
        assert_eq!(app.input.as_ref().unwrap().text(), "rename");
        press(&mut app, "x");
        assert!(app.cycle.is_none());
        press_code(&mut app, KeyCode::Esc);
        press(&mut app, ":");
        assert_eq!(app.completion().unwrap().names.len(), input::COMMANDS.len());
    }

    #[test]
    fn go_opens_the_best_channel_of_the_team() {
        let mut app = fixture();
        run(&mut app, "go town");
        assert_eq!(here(&app), "c1");
        run(&mut app, "go zzz");
        assert_eq!(app.notice, "No channel matches zzz");
        run(&mut app, "go");
        assert_eq!(app.notice, "Usage: :go <words>");
    }

    #[test]
    fn set_changes_a_setting_for_the_session_only() {
        let mut app = fixture();
        run(&mut app, "set editor nvim -R");
        assert_eq!(app.settings.editor.as_deref(), Some("nvim -R"));
        assert_eq!(app.notice, "editor = nvim -R");
        run(&mut app, "set editor");
        assert_eq!(app.settings.editor, None);
        assert_eq!(app.notice, "editor reset");
        run(&mut app, "set open.image imv");
        assert_eq!(app.settings.open.image, "imv");
        run(&mut app, "set open.video");
        assert_eq!(app.notice, "Usage: :set open.video <program>");
        assert_eq!(app.settings.open.video, "true");
        run(&mut app, "set bogus 1");
        assert!(app.notice.starts_with("Settings: editor copy"));
    }

    #[test]
    fn small_commands_do_what_their_keys_do() {
        let mut app = fixture();
        run(&mut app, "topic");
        assert_eq!(app.notice, "No topic");
        let info = &mut app.servers[0].teams[0].chans.as_mut().unwrap()[0].info;
        info.header = "Be\nnice".into();
        info.purpose = "Chat".into();
        run(&mut app, "topic");
        assert_eq!(app.notice, "Be nice · Chat");
        let before = app.reactions;
        run(&mut app, "reactions");
        assert_eq!(app.reactions, !before);
        run(&mut app, "react not-an-emoji");
        assert_eq!(app.notice, "Unknown emoji not-an-emoji");
        run(&mut app, "copy what");
        assert_eq!(app.notice, "Usage: :copy [full]");
        run(&mut app, "copy full");
        assert!(app.notice.starts_with("Copied"), "{}", app.notice);
        run(&mut app, "nope");
        assert_eq!(app.notice, "Unknown command: nope");
    }

    #[test]
    fn removing_a_server_forgets_it_and_keeps_every_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = fixture();
        app.path = dir.path().join("servers.toml");
        app.servers.push(Server::new("club".into()));
        app.configs.push(ServerConfig {
            name: "club".into(),
            url: "https://club.example.net".into(),
            method: crate::config::Method::Password,
            login: String::new(),
            password: None,
            password_cmd: None,
        });
        run(&mut app, "remove-server club");
        let saved = config::load(&app.path).unwrap();
        assert_eq!(
            saved.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["work"]
        );
        assert_eq!(app.servers.len(), 2);
        assert!(app.servers[1].is_gone());
        app.loaded(1, Loaded::Failed("late".into()));
        assert_ne!(app.notice, ": late");
        run(&mut app, "remove-server work");
        assert!(config::load(&app.path).unwrap().is_empty());
        assert!(app.places().is_empty());
        run(&mut app, "server wor");
        assert_eq!(app.notice, "No server matches wor");
        run(&mut app, "remove-server nobody");
        assert_eq!(app.notice, "No server matches nobody");
    }

    #[test]
    fn a_channel_missing_from_the_list_is_said_so() {
        let mut app = fixture();
        app.go = Some(crate::api::types::ChannelId("c9".into()));
        app.loaded(
            0,
            Loaded::Channels {
                team: TeamId("t1".into()),
                channels: json("channels"),
                members: json("members"),
            },
        );
        assert_eq!(app.notice, "That channel is not in your sidebar");
        assert!(app.go.is_none());
    }
}
