//! Application state, the events that change it, and the requests it starts.

pub mod commands;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::api::types::{
    Channel, ChannelId, ChannelKind, ChannelMember, Post, PostId, PostList, Reaction, Status,
    Team as WireTeam, TeamId, User, UserId, UserStatus,
};
use crate::api::verify::Check;
use crate::api::{Client, Error};
use crate::clipboard;
use crate::config::{self, Credentials, ServerConfig, Settings};
use crate::debug;
use crate::finder;
use crate::input::{self, Line, Step, Wizard};
use crate::model::{
    self, group_start, group_with_previous, Attachment, Link, Message, Server, Team,
};
use crate::open::{self, Files, Kind as FileKind};
use crate::sync::{self, Action, Event, Sync};
use crate::term::{self, Pane};
use crate::unread;

/// Everything that can wake the application.
#[derive(Debug)]
pub enum Msg {
    Key(KeyEvent),
    Resize,
    Loaded(usize, Loaded),
    LoggedIn(Result<Box<Session>, Error>),
    Sync(usize, Sync),
    Paste(String),
    Notice(String),
    Tick,
    Pty,
    EditorDone,
}

/// The answer to one request, tagged by the server it came from.
#[derive(Debug)]
pub enum Loaded {
    Connected(Result<Box<Session>, String>),
    Teams(Vec<WireTeam>),
    Channels {
        team: TeamId,
        channels: Vec<Channel>,
        members: Vec<ChannelMember>,
    },
    Posts {
        channel: ChannelId,
        list: PostList,
    },
    Older {
        channel: ChannelId,
        before: PostId,
        list: Result<PostList, String>,
        asked: usize,
    },
    Users(Vec<User>),
    Downloaded {
        path: std::path::PathBuf,
        name: String,
        kind: FileKind,
    },
    Members {
        channel: ChannelId,
        users: Vec<User>,
        statuses: Vec<UserStatus>,
    },
    Found {
        text: String,
        channel: ChannelId,
        list: PostList,
        run: PostList,
        read: usize,
    },
    Reacted(Reaction),
    Unreacted(Reaction),
    Opened(ChannelId),
    Left(String),
    StatusSet(Status),
    Said(String),
    Refreshed(String),
    Page {
        title: String,
        lines: Vec<String>,
    },
    Deactivated,
    Verification(Result<Check, String>),
    Created(Box<Post>),
    Edited(Box<Post>),
    Deleted {
        channel: ChannelId,
        id: PostId,
    },
    NotSent {
        channel: ChannelId,
        text: String,
        error: String,
    },
    Viewed,
    Failed(String),
}

/// Something `o` can open: a link or an attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Link(String),
    File(Attachment),
}

/// The reactions of the picker, chosen with the keys 1 to 9.
pub const REACTIONS: [&str; 9] = [
    "+1",
    "heart",
    "joy",
    "tada",
    "eyes",
    "fire",
    "skull",
    "ok_hand",
    "open_mouth",
];

/// The narrowest terminal that has room for the users column.
const MIN_WIDTH_FOR_USERS: u16 = 70;

/// Pause before the background load of the next team that was never shown.
const HIDDEN_TEAM_EVERY: Duration = Duration::from_secs(1);

/// How old a message may be and still ring the notification command.
const RECENT_MS: i64 = 120_000;

/// The messages a search found, as lines to scroll through.
pub struct Found {
    pub title: String,
    pub lines: Vec<String>,
    pub top: usize,
    pub pick: Option<Pick>,
}

/// The hit chosen in a search page, and what opens it in place.
pub struct Pick {
    channel: ChannelId,
    ids: Vec<PostId>,
    pub at: usize,
    run: Vec<Message>,
}

/// The users column: whether it is shown, has the focus, and how far it is scrolled.
#[derive(Debug, Default)]
pub struct Users {
    pub shown: bool,
    pub focus: bool,
    pub top: usize,
}

/// The channel finder: what is being typed and which match is chosen.
#[derive(Debug)]
pub struct Finder {
    pub all: bool,
    pub line: Line,
    pub selected: usize,
}

/// What the input line is writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Message,
    Reply { root: PostId, to: String },
    Edit(PostId),
}

/// The message being written in INSERT mode.
#[derive(Debug)]
pub struct Compose {
    pub line: Line,
    pub kind: Kind,
}

/// A question the next key answers.
#[derive(Debug)]
enum Awaiting {
    Delete { channel: ChannelId, id: PostId },
}

/// The selected message, as the writing keys need it.
struct Target {
    id: PostId,
    root: Option<PostId>,
    text: String,
    author: String,
    own: bool,
    system: bool,
    mine: Vec<String>,
}

/// What entering a channel should put under the cursor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Goal {
    #[default]
    Unread,
    Mention,
}

/// The channel being read, and how much of it was already read when it was entered.
#[derive(Debug)]
struct Entry {
    server: usize,
    channel: ChannelId,
    unread: usize,
    boundary: Option<PostId>,
    goal: Goal,
    placed: bool,
}

/// A logged-in connection and the account behind it.
#[derive(Debug)]
pub struct Session {
    pub client: Arc<Client>,
    pub me: User,
}

/// The whole state of the program.
pub struct App {
    pub servers: Vec<Server>,
    configs: Vec<ServerConfig>,
    path: PathBuf,
    tx: UnboundedSender<Msg>,
    pub server: usize,
    pub team: usize,
    pub cursor: usize,
    pub browsing: bool,
    pub follow: bool,
    pub reactions: bool,
    pub scroll: usize,
    /// Lines the log should scroll at the next draw, which alone knows where the bottom is.
    pub shift: isize,
    /// The first and last messages with a line on screen, as of the last draw.
    pub visible: (usize, usize),
    pub notice: String,
    pub input: Option<Line>,
    pub wizard: Option<Wizard>,
    /// The new password being asked: nothing typed yet, or the first entry awaiting its repeat.
    pub changing: Option<Option<String>>,
    pub help: Option<usize>,
    pub found: Option<Found>,
    pub finder: Option<Finder>,
    /// The name chosen among those the command line offers.
    pub choice: usize,
    cycle: Option<String>,
    pub picker: bool,
    pub selecting: bool,
    anchor: usize,
    register: String,
    last_reaction: Option<String>,
    go: Option<ChannelId>,
    pub clipboard: Vec<String>,
    pub users: Users,
    pub compose: Option<Compose>,
    pub pane: Option<Pane>,
    pub size: (u16, u16),
    pub settings: Settings,
    /// Whether colors are dropped, as `NO_COLOR` asks.
    pub plain: bool,
    files: Files,
    pub opener: Option<Vec<Item>>,
    editor_fresh: bool,
    awaiting: Option<Awaiting>,
    /// The verification waiting for the reader to compare emojis, with its server.
    pub verifying: Option<(usize, Check)>,
    drafts: HashMap<(usize, ChannelId), String>,
    typed: Option<Instant>,
    pub quit: bool,
    entry: Option<Entry>,
    goal: Goal,
}

impl App {
    /// Starts with the configured servers, connecting to each; asks for one when there is none.
    pub fn new(
        boot: Vec<(ServerConfig, Result<Credentials, String>)>,
        path: PathBuf,
        tx: UnboundedSender<Msg>,
    ) -> Self {
        let mut app = Self {
            servers: Vec::new(),
            configs: Vec::new(),
            path,
            tx,
            server: 0,
            team: 0,
            cursor: 0,
            browsing: false,
            follow: false,
            reactions: false,
            scroll: usize::MAX,
            shift: 0,
            visible: (0, usize::MAX),
            notice: String::new(),
            input: None,
            wizard: None,
            changing: None,
            help: None,
            found: None,
            finder: None,
            choice: 0,
            cycle: None,
            picker: false,
            selecting: false,
            anchor: 0,
            register: String::new(),
            last_reaction: None,
            go: None,
            clipboard: Vec::new(),
            users: Users::default(),
            compose: None,
            pane: None,
            size: (80, 24),
            settings: Settings::default(),
            files: Files::new(),
            opener: None,
            plain: false,
            editor_fresh: false,
            awaiting: None,
            verifying: None,
            drafts: HashMap::new(),
            typed: None,
            quit: false,
            entry: None,
            goal: Goal::Unread,
        };
        for (i, (config, credentials)) in boot.into_iter().enumerate() {
            app.servers.push(Server::new(config.name.clone()));
            match credentials {
                Ok(credentials) => app.connect(i, &config.name, &config.url, credentials),
                Err(e) => app.notice = format!("{}: {e}", config.name),
            }
            app.configs.push(config);
        }
        if app.servers.is_empty() {
            app.new_server();
        }
        app
    }

    /// Applies one event.
    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Key(key) => self.key(key),
            Msg::Resize => {}
            Msg::Loaded(i, loaded) => self.loaded(i, loaded),
            Msg::LoggedIn(result) => self.logged_in(result),
            Msg::Sync(i, sync) => self.sync(i, sync),
            Msg::Paste(text) => self.paste(&text),
            Msg::Notice(text) => self.notice = text,
            Msg::Tick | Msg::Pty => {}
            Msg::EditorDone => self.editor_done(),
        }
    }

    /// Name of the current mode, written on the status line.
    pub fn mode(&self) -> &'static str {
        if let Some(pane) = &self.pane {
            return if pane.is_viewer() { "VIEWER" } else { "EDITOR" };
        }
        if self.compose.is_some() {
            return "INSERT";
        }
        match (&self.input, &self.wizard, &self.help, &self.finder) {
            (_, Some(_), ..) => "LOGIN",
            _ if self.changing.is_some() => "LOGIN",
            (Some(_), ..) => "COMMAND",
            (.., Some(_)) => "SEARCH",
            (_, _, Some(_), _) => "HELP",
            _ if self.found.is_some() => "RESULTS",
            _ if self.picker => "REACT",
            _ if self.opener.is_some() => "OPEN",
            _ if self.users.focus => "USERS",
            _ if self.selecting => "VISUAL",
            _ => "NORMAL",
        }
    }

    /// What the input row asks for.
    pub fn prompt(&self) -> &'static str {
        match (&self.wizard, &self.changing) {
            (Some(w), _) => w.step.prompt(),
            (_, Some(None)) => "New password: ",
            (_, Some(Some(_))) => "Again: ",
            _ => ":",
        }
    }

    /// The channel the reader is looking at, with its server.
    pub fn channel(&self) -> Option<(&Server, &model::Chan)> {
        let server = self.servers.get(self.server)?;
        let team = server.teams.get(self.team)?;
        Some((server, team.chans.as_ref()?.get(team.cur)?))
    }

    /// The messages of the current channel, none until they are loaded.
    pub fn messages(&self) -> Option<&[Message]> {
        let (server, chan) = self.channel()?;
        server.messages.get(&chan.info.id).map(Vec::as_slice)
    }

    fn count(&self) -> usize {
        self.messages().map_or(0, <[Message]>::len)
    }

    fn places(&self) -> Vec<(usize, usize)> {
        let teams = |(s, server): (usize, &Server)| (0..server.teams.len()).map(move |t| (s, t));
        self.servers.iter().enumerate().flat_map(teams).collect()
    }

    fn key(&mut self, key: KeyEvent) {
        self.notice.clear();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let plain = self.input.is_none()
            && self.finder.is_none()
            && self.help.is_none()
            && self.found.is_none()
            && self.wizard.is_none();
        if let Some(pane) = &mut self.pane {
            pane.send_key(key);
        } else if key.code == KeyCode::Char('c') && ctrl {
            self.quit = true;
        } else if let Some((i, check)) = self.verifying.take() {
            self.verification_key(i, check, key);
        } else if let Some(Awaiting::Delete { channel, id }) = self.awaiting.take() {
            if key.code == KeyCode::Char('y') {
                self.delete(channel, id);
            }
        } else if key.code == KeyCode::Char('g') && ctrl && (plain || self.compose.is_some()) {
            self.open_editor();
        } else if self.found.is_some() {
            self.found_key(key);
        } else if self.picker {
            self.picker_key(key);
        } else if self.opener.is_some() {
            self.opener_key(key);
        } else if self.compose.is_some() {
            self.compose_key(key);
        } else if self.finder.is_some() {
            self.finder_key(key);
        } else if self.input.is_some() {
            self.input_key(key);
        } else if self.wizard.is_none() {
            match self.help {
                Some(_) => self.help_key(key),
                None => self.normal_key(key),
            }
        }
    }

    fn verification_key(&mut self, i: usize, check: Check, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') => self.spawn(i, async move {
                match check.confirm().await {
                    Ok(()) => Loaded::Said("Session verified".into()),
                    Err(e) => Loaded::Failed(e.to_string()),
                }
            }),
            KeyCode::Char('n') | KeyCode::Esc => self.spawn(i, async move {
                match check.refuse().await {
                    Ok(()) => Loaded::Said("Verification refused".into()),
                    Err(e) => Loaded::Failed(e.to_string()),
                }
            }),
            _ => self.verifying = Some((i, check)),
        }
    }

    fn normal_key(&mut self, key: KeyEvent) {
        if self.users.focus {
            return self.users_key(key);
        }
        if key.code == KeyCode::Esc {
            self.selecting = false;
            return;
        }
        let KeyCode::Char(c) = key.code else {
            return;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        match c {
            'q' => self.quit = true,
            ':' => self.input = Some(Line::new(false)),
            '?' => self.help = Some(0),
            'h' => self.step_up(),
            'l' => self.step_down(),
            'j' => self.shift += 1,
            'k' => self.shift -= 1,
            'g' => {
                self.move_cursor(0);
                self.load_older();
            }
            'G' => self.move_cursor(usize::MAX),
            'a' => self.reactions = !self.reactions,
            'n' => self.next_unread(),
            'N' => self.next_mention(),
            'c' => self.mark_read(),
            'o' => self.open_selected(),
            'x' => self.open_picker(),
            'X' => self.repeat_reaction(),
            'y' => self.yank(false),
            'Y' => self.yank(true),
            'p' => self.paste_register(),
            'V' => self.toggle_selection(),
            'm' => self.toggle_users(),
            'i' => self.start_compose(Kind::Message, None),
            'r' => self.reply(),
            'e' => self.edit(),
            'd' => self.ask_delete(),
            'f' | 'F' => {
                self.finder = Some(Finder {
                    all: c == 'F',
                    line: Line::new(false),
                    selected: 0,
                });
            }
            'w' => self.next_group(),
            'b' => self.previous_group(),
            'J' => self.move_channel(1),
            'K' => self.move_channel(-1),
            'H' => self.move_team(-1),
            'L' => self.move_team(1),
            _ => {}
        }
    }

    fn target(&self) -> Option<Target> {
        let (server, _) = self.channel()?;
        let message = self.messages()?.get(self.cursor)?;
        Some(Target {
            id: message.id.clone(),
            root: message.root.clone(),
            text: message.text.clone(),
            author: server.username(&message.user).to_owned(),
            own: server.me.as_ref().is_some_and(|m| m.id == message.user),
            system: message.system,
            mine: message
                .reactions
                .iter()
                .filter(|g| server.me.as_ref().is_some_and(|m| g.users.contains(&m.id)))
                .map(|g| g.name.clone())
                .collect(),
        })
    }

    fn draft_key(&self) -> Option<(usize, ChannelId)> {
        self.channel()
            .map(|(_, chan)| (self.server, chan.info.id.clone()))
    }

    fn start_compose(&mut self, kind: Kind, text: Option<String>) {
        let Some(key) = self.draft_key() else {
            return;
        };
        let draft = match kind {
            Kind::Edit(_) => None,
            _ => self.drafts.remove(&key),
        };
        let text = text.or(draft).unwrap_or_default();
        self.compose = Some(Compose {
            line: Line::with_text(&text, false),
            kind,
        });
        self.typed = None;
    }

    fn reply(&mut self) {
        let Some(target) = self.target().filter(|t| !t.system) else {
            return;
        };
        self.browsing = true;
        let root = target.root.unwrap_or(target.id);
        self.start_compose(
            Kind::Reply {
                root,
                to: target.author,
            },
            None,
        );
    }

    fn edit(&mut self) {
        let Some(target) = self.target() else {
            return;
        };
        self.browsing = true;
        if !target.own || target.system {
            self.notice = "That is not your message".into();
            return;
        }
        self.start_compose(Kind::Edit(target.id), Some(target.text));
    }

    fn ask_delete(&mut self) {
        let (Some(target), Some((_, chan))) = (self.target(), self.channel()) else {
            return;
        };
        let channel = chan.info.id.clone();
        self.browsing = true;
        if !target.own || target.system {
            self.notice = "That is not your message".into();
            return;
        }
        self.notice = "Delete this message?  y = yes, any other key = no".into();
        self.awaiting = Some(Awaiting::Delete {
            channel,
            id: target.id,
        });
    }

    fn delete(&mut self, channel: ChannelId, id: PostId) {
        let Some(client) = self.servers.get(self.server).and_then(|s| s.client.clone()) else {
            return;
        };
        self.spawn(self.server, async move {
            match client.delete_post(&channel, &id).await {
                Ok(()) => Loaded::Deleted { channel, id },
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn compose_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.leave_compose(),
            KeyCode::Enter => self.send(),
            _ => {
                let Some(compose) = &mut self.compose else {
                    return;
                };
                let before = compose.line.text();
                if compose.line.edit(key) && compose.line.text() != before {
                    self.ping_typing();
                }
            }
        }
    }

    fn open_editor(&mut self) {
        self.editor_fresh = self.compose.is_none();
        if self.editor_fresh {
            self.start_compose(Kind::Message, None);
        }
        let Some(compose) = &self.compose else {
            return;
        };
        let (_, rows) = term::split(self.size.1);
        let tx = self.tx.clone();
        let wake = move |done| {
            let _ = tx.send(if done { Msg::EditorDone } else { Msg::Pty });
        };
        let editor = self
            .settings
            .editor(std::env::var("EDITOR").ok().as_deref());
        match Pane::spawn(&editor, &compose.line.text(), rows, self.size.0, wake) {
            Ok(pane) => self.pane = Some(pane),
            Err(e) => {
                self.notice = format!("Editor: {e}");
                if self.editor_fresh {
                    self.leave_compose();
                }
            }
        }
    }

    fn editor_done(&mut self) {
        let Some(pane) = self.pane.take() else {
            return;
        };
        if !pane.is_viewer() {
            self.finished(pane.finish().map_err(|e| e.to_string()));
        }
    }

    /// Takes back what the editor saved, or says why it failed.
    fn finished(&mut self, result: Result<String, String>) {
        let fresh = std::mem::take(&mut self.editor_fresh);
        match result {
            Ok(text) if text.is_empty() && fresh => self.leave_compose(),
            Ok(text) => {
                if let Some(compose) = &mut self.compose {
                    compose.line = Line::with_text(&text, false);
                    self.ping_typing();
                }
            }
            Err(e) => {
                self.notice = format!("Editor: {e}");
                if fresh {
                    self.leave_compose();
                }
            }
        }
    }

    fn leave_compose(&mut self) {
        let (Some(compose), Some(key)) = (self.compose.take(), self.draft_key()) else {
            return;
        };
        self.typed = None;
        let text = compose.line.text();
        if compose.kind != Kind::Edit(PostId::default())
            && !matches!(compose.kind, Kind::Edit(_))
            && !text.is_empty()
        {
            self.drafts.insert(key, text);
        }
    }

    fn send(&mut self) {
        let Some(compose) = &self.compose else {
            return;
        };
        let text = compose.line.text();
        let message = text.trim().to_owned();
        if message.is_empty() {
            return;
        }
        if text.starts_with('/') {
            self.notice = "Slash commands are not supported".into();
            return;
        }
        let kind = compose.kind.clone();
        let (Some((server, chan)), i) = (self.channel(), self.server) else {
            return;
        };
        let Some(client) = server.client.clone() else {
            self.notice = format!("{}: not connected", server.name);
            return;
        };
        let channel = chan.info.id.clone();
        let edited = matches!(kind, Kind::Edit(_));
        if !edited {
            if let Some(server) = self.servers.get_mut(i) {
                server.mark_viewed(&channel);
            }
        }
        self.spawn(i, async move {
            let sent = match &kind {
                Kind::Edit(id) => client
                    .edit_post(&channel, id, &message)
                    .await
                    .map(|post| Loaded::Edited(Box::new(post))),
                Kind::Message => client
                    .create_post(&channel, &message, None)
                    .await
                    .map(|post| Loaded::Created(Box::new(post))),
                Kind::Reply { root, .. } => client
                    .create_post(&channel, &message, Some(root))
                    .await
                    .map(|post| Loaded::Created(Box::new(post))),
            };
            if sent.is_ok() && !matches!(kind, Kind::Edit(_)) {
                let _ = client.view(&channel, None).await;
            }
            sent.unwrap_or_else(|e| match kind {
                Kind::Edit(_) => Loaded::Failed(e.to_string()),
                _ => Loaded::NotSent {
                    channel,
                    text: message,
                    error: e.to_string(),
                },
            })
        });
        self.compose = match edited {
            true => None,
            false => Some(Compose {
                line: Line::new(false),
                kind: Kind::Message,
            }),
        };
    }

    fn ping_typing(&mut self) {
        if self
            .typed
            .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        if !matches!(
            self.compose.as_ref().map(|c| &c.kind),
            Some(Kind::Message | Kind::Reply { .. })
        ) {
            return;
        }
        let Some((server, chan)) = self.channel() else {
            return;
        };
        let Some(actions) = &server.actions else {
            return;
        };
        let channel = chan.info.id.clone();
        if actions.send(Action::Typing { channel }).is_ok() {
            self.typed = Some(Instant::now());
        }
    }

    fn paste(&mut self, text: &str) {
        if let Some(pane) = &mut self.pane {
            return pane.send_paste(text);
        }
        let flat = text.replace(['\r', '\n'], " ");
        if let Some(compose) = &mut self.compose {
            compose.line.insert_str(&flat);
            self.ping_typing();
        } else if let Some(finder) = &mut self.finder {
            finder.line.insert_str(&flat);
            finder.selected = 0;
        } else if let Some(line) = &mut self.input {
            line.insert_str(&flat);
        }
    }

    fn tick_later(&self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let tx = self.tx.clone();
            runtime.spawn(async move {
                tokio::time::sleep(Duration::from_secs(6)).await;
                let _ = tx.send(Msg::Tick);
            });
        }
    }

    /// Whether message `i` is shown as selected: the cursor, or the range being selected.
    pub fn is_selected(&self, i: usize) -> bool {
        let (low, high) = match self.selecting {
            true => (self.anchor.min(self.cursor), self.anchor.max(self.cursor)),
            false => (self.cursor, self.cursor),
        };
        self.browsing && (low..=high).contains(&i)
    }

    fn toggle_selection(&mut self) {
        if self.count() == 0 {
            return;
        }
        self.selecting = !self.selecting;
        self.anchor = self.cursor;
        self.browsing = true;
    }

    fn yank(&mut self, quote: bool) {
        let (Some((server, _)), Some(messages)) = (self.channel(), self.messages()) else {
            return;
        };
        if messages.is_empty() {
            return;
        }
        let last = messages.len() - 1;
        let (low, high) = match self.selecting {
            true => (self.anchor.min(self.cursor), self.anchor.max(self.cursor)),
            false => (self.cursor, self.cursor),
        };
        let chosen = &messages[low.min(last)..=high.min(last)];
        let lines: Vec<String> = chosen
            .iter()
            .map(|m| match quote {
                true => format!(
                    "[{}] {}: {}",
                    m.at.format("%H:%M"),
                    server.username(&m.user),
                    m.text
                ),
                false => m.text.clone(),
            })
            .collect();
        let count = lines.len();
        self.register = lines.join("\n");
        self.clipboard.push(clipboard::osc52(&self.register));
        self.copy_to_system();
        self.selecting = false;
        self.browsing = true;
        self.notice = match count {
            1 => "Copied 1 message".into(),
            n => format!("Copied {n} messages"),
        };
    }

    fn copy_to_system(&mut self) {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let x11 = std::env::var_os("DISPLAY").is_some();
        let setting = self.settings.copy.as_deref();
        let Some(command) = clipboard::tool(setting, wayland, x11, clipboard::on_path) else {
            return;
        };
        let tx = self.tx.clone();
        let done = move |outcome: Option<String>| {
            if let Some(why) = outcome {
                let _ = tx.send(Msg::Notice(format!("Copy failed: {why}")));
            }
        };
        if let Err(e) = clipboard::copy(&command, &self.register, done) {
            self.notice = format!("Copy failed: {e}");
        }
    }

    fn paste_register(&mut self) {
        if self.register.is_empty() {
            self.notice = "Nothing to paste: copy a message with y first".into();
            return;
        }
        if self.compose.is_none() {
            self.start_compose(Kind::Message, None);
        }
        if let Some(compose) = &mut self.compose {
            compose.line.insert_str(&self.register);
            self.ping_typing();
        }
    }

    fn open_selected(&mut self) {
        let Some(message) = self.messages().and_then(|m| m.get(self.cursor)) else {
            return;
        };
        let mut items: Vec<Item> = open::links(&message.text)
            .into_iter()
            .map(Item::Link)
            .collect();
        items.extend(message.files.iter().cloned().map(Item::File));
        self.browsing = true;
        match items.len() {
            0 => self.notice = "Nothing to open".into(),
            1 => self.open_item(items.remove(0)),
            _ => self.opener = Some(items),
        }
    }

    fn opener_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c @ '1'..='9') => {
                let index = usize::from(c as u8 - b'1');
                if let Some(item) = self
                    .opener
                    .as_ref()
                    .and_then(|items| items.get(index))
                    .cloned()
                {
                    self.opener = None;
                    self.open_item(item);
                }
            }
            KeyCode::Esc | KeyCode::Char('o' | 'q') => self.opener = None,
            _ => {}
        }
    }

    fn open_item(&mut self, item: Item) {
        match item {
            Item::Link(url) => self.start_program(&self.settings.open.browser.clone(), &url, &url),
            Item::File(file) => {
                let Some(client) = self.servers.get(self.server).and_then(|s| s.client.clone())
                else {
                    self.notice = "Not connected".into();
                    return;
                };
                self.notice = format!(
                    "Downloading {} ({})…",
                    file.name,
                    open::human_size(file.size)
                );
                let base = self.files.base().to_owned();
                let kind = open::kind(&file.mime, &file.name);
                self.spawn(self.server, async move {
                    let bytes = client.download(&file.id).await.map_err(|e| e.to_string());
                    let saved = bytes.and_then(|b| {
                        open::save(&base, &file.id, &file.name, &b).map_err(|e| e.to_string())
                    });
                    match saved {
                        Ok(path) => Loaded::Downloaded {
                            path,
                            name: file.name,
                            kind,
                        },
                        Err(e) => Loaded::Failed(e),
                    }
                });
            }
        }
    }

    fn start_program(&mut self, command: &str, target: &str, name: &str) {
        let tx = self.tx.clone();
        let done = move |outcome: Option<String>| {
            if let Some(why) = outcome {
                let _ = tx.send(Msg::Notice(why));
            }
        };
        match open::start(command, target, done) {
            Ok(()) => self.notice = format!("Opened {name}"),
            Err(e) => self.notice = format!("{command}: {e}"),
        }
    }

    fn downloaded(&mut self, path: &std::path::Path, name: &str, kind: FileKind) {
        let open = &self.settings.open;
        let command = match kind {
            FileKind::Image => open.image.clone(),
            FileKind::Video => open.video.clone(),
            FileKind::Text => open.text.clone(),
            FileKind::Other => open.other.clone(),
        };
        let target = path.to_string_lossy().into_owned();
        if kind != FileKind::Text {
            return self.start_program(&command, &target, name);
        }
        let (_, rows) = term::split(self.size.1);
        let tx = self.tx.clone();
        let wake = move |done| {
            let _ = tx.send(if done { Msg::EditorDone } else { Msg::Pty });
        };
        match Pane::view(&command, path, name, rows, self.size.0, wake) {
            Ok(pane) => self.pane = Some(pane),
            Err(e) => self.notice = format!("{command}: {e}"),
        }
    }

    fn open_picker(&mut self) {
        if self.target().is_some() {
            self.browsing = true;
            self.picker = true;
        }
    }

    fn picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c @ '1'..='9') => {
                self.picker = false;
                self.react(REACTIONS[usize::from(c as u8 - b'1')]);
            }
            KeyCode::Esc | KeyCode::Char('x' | 'q') => self.picker = false,
            _ => {}
        }
    }

    fn repeat_reaction(&mut self) {
        match self.last_reaction.clone() {
            Some(name) => self.react(&name),
            None => self.notice = "No recent reaction: pick one with x".into(),
        }
    }

    fn react(&mut self, name: &str) {
        let Some(target) = self.target() else {
            return;
        };
        let Some((server, chan)) = self.channel() else {
            return;
        };
        let channel = chan.info.id.clone();
        let (Some(client), Some(me)) = (
            server.client.clone(),
            server.me.as_ref().map(|m| m.id.clone()),
        ) else {
            self.notice = "Not connected".into();
            return;
        };
        self.browsing = true;
        let name = name.to_owned();
        self.last_reaction = Some(name.clone());
        let post = target.id;
        let had = target.mine.contains(&name);
        self.spawn(self.server, async move {
            let done = match had {
                true => client.unreact(&channel, &post, &name).await.map(|()| {
                    Loaded::Unreacted(Reaction {
                        user_id: me.clone(),
                        post_id: post.clone(),
                        emoji_name: name.clone(),
                    })
                }),
                false => client
                    .react(&channel, &post, &name)
                    .await
                    .map(Loaded::Reacted),
            };
            done.unwrap_or_else(|e| Loaded::Failed(e.to_string()))
        });
    }

    fn toggle_users(&mut self) {
        match (self.users.shown, self.users.focus) {
            (false, _) if self.size.0 < MIN_WIDTH_FOR_USERS => {
                self.notice = "Screen too narrow for the users list".into();
            }
            (false, _) => {
                self.users = Users {
                    shown: true,
                    focus: true,
                    top: 0,
                };
                if let Some(id) = self.channel().map(|(_, c)| c.info.id.clone()) {
                    if let Some(server) = self.servers.get_mut(self.server) {
                        server.members.remove(&id);
                        server.members_loading.remove(&id);
                    }
                }
                self.ensure_members();
            }
            (true, false) => self.users.focus = true,
            (true, true) => self.users = Users::default(),
        }
    }

    fn users_key(&mut self, key: KeyEvent) {
        let users = &mut self.users;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => users.focus = false,
            KeyCode::Char('m') => *users = Users::default(),
            KeyCode::Char('j') | KeyCode::Down => users.top += 1,
            KeyCode::Char('k') | KeyCode::Up => users.top = users.top.saturating_sub(1),
            KeyCode::PageDown => users.top += 10,
            KeyCode::PageUp => users.top = users.top.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => users.top = 0,
            KeyCode::Char('G') | KeyCode::End => users.top = usize::MAX / 2,
            _ => {}
        }
    }

    fn ensure_members(&mut self) {
        if !self.users.shown {
            return;
        }
        let Some((server, chan)) = self.channel() else {
            return;
        };
        let id = chan.info.id.clone();
        let Some(client) = server
            .client
            .clone()
            .filter(|_| !server.members.contains_key(&id))
        else {
            return;
        };
        let i = self.server;
        let Some(server) = self.servers.get_mut(i) else {
            return;
        };
        if !server.members_loading.insert(id.clone()) {
            return;
        }
        self.spawn(i, async move {
            match client.channel_users(&id).await {
                Ok(users) => {
                    let ids: Vec<UserId> = users.iter().map(|u| u.id.clone()).collect();
                    let statuses = client.statuses(&ids).await.unwrap_or_default();
                    Loaded::Members {
                        channel: id,
                        users,
                        statuses,
                    }
                }
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    /// The (server, team, channel) of every match of the finder, best first.
    fn finder_targets(&self) -> Vec<(usize, usize, usize)> {
        let Some(finder) = &self.finder else {
            return Vec::new();
        };
        let text = finder.line.text();
        let words: Vec<&str> = text.split_whitespace().collect();
        let here = (self.server, self.team);
        let candidates = finder::candidates(&self.servers, here, finder.all);
        let found = finder::entries(&words, &candidates, finder.all);
        found
            .iter()
            .map(|e| (e.candidate.server, e.candidate.team, e.candidate.chan))
            .collect()
    }

    fn finder_key(&mut self, key: KeyEvent) {
        let targets = self.finder_targets();
        let count = targets.len();
        let Some(finder) = &mut self.finder else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.finder = None,
            KeyCode::Enter => {
                let target = targets.get(finder.selected.min(count.saturating_sub(1)));
                let target = target.copied();
                self.finder = None;
                if let Some((server, team, chan)) = target {
                    (self.server, self.team) = (server, team);
                    self.go_channel(chan);
                }
            }
            KeyCode::Up | KeyCode::BackTab if count > 0 => {
                finder.selected = (finder.selected + count - 1) % count;
            }
            KeyCode::Down | KeyCode::Tab if count > 0 => {
                finder.selected = (finder.selected + 1) % count;
            }
            _ => {
                if finder.line.edit(key) {
                    finder.selected = 0;
                }
            }
        }
    }

    fn found_key(&mut self, key: KeyEvent) {
        let Some(found) = &mut self.found else {
            return;
        };
        if let Some(pick) = &mut found.pick {
            let last = pick.ids.len().saturating_sub(1);
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => pick.at = (pick.at + 1).min(last),
                KeyCode::Char('k') | KeyCode::Up => pick.at = pick.at.saturating_sub(1),
                KeyCode::PageDown => pick.at = (pick.at + 10).min(last),
                KeyCode::PageUp => pick.at = pick.at.saturating_sub(10),
                KeyCode::Char('g') | KeyCode::Home => pick.at = 0,
                KeyCode::Char('G') | KeyCode::End => pick.at = last,
                KeyCode::Enter => self.open_hit(),
                KeyCode::Esc | KeyCode::Char('q') => self.found = None,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => found.top += 1,
            KeyCode::Char('k') | KeyCode::Up => found.top = found.top.saturating_sub(1),
            KeyCode::PageDown => found.top += 10,
            KeyCode::PageUp => found.top = found.top.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => found.top = 0,
            KeyCode::Char('G') | KeyCode::End => found.top = usize::MAX / 2,
            KeyCode::Esc | KeyCode::Char('q') => self.found = None,
            _ => {}
        }
    }

    fn open_hit(&mut self) {
        let Some(pick) = self.found.as_ref().and_then(|f| f.pick.as_ref()) else {
            return;
        };
        let Some(id) = pick.ids.get(pick.at) else {
            return;
        };
        if !pick.run.iter().any(|m| m.id == *id) {
            self.notice = "Too old to open here: scroll up instead".into();
            return;
        }
        let here = self.channel().map(|(_, c)| &c.info.id) == Some(&pick.channel);
        let Some(Pick {
            channel,
            ids,
            at,
            run,
            ..
        }) = self.found.take().and_then(|f| f.pick)
        else {
            return;
        };
        let Some(server) = self.servers.get_mut(self.server).filter(|_| here) else {
            return;
        };
        let users = server.unknown(run.iter().map(|m| &m.user));
        let cached = server.messages.entry(channel).or_default();
        model::merge_page(cached, run);
        if let Some(to) = cached.iter().position(|m| m.id == ids[at]) {
            self.move_cursor(to);
        }
        self.fetch_users(self.server, users);
    }

    fn help_key(&mut self, key: KeyEvent) {
        let Some(top) = self.help else {
            return;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.help = Some(top + 1),
            KeyCode::Char('k') | KeyCode::Up => self.help = Some(top.saturating_sub(1)),
            KeyCode::Esc | KeyCode::Char('q' | '?') => self.help = None,
            _ => {}
        }
    }

    fn input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input = None;
                self.wizard = None;
                self.changing = None;
                self.choice = 0;
                self.cycle = None;
            }
            KeyCode::Enter => {
                if let Some(line) = &self.input {
                    let text = self.completed(&line.text());
                    self.input = None;
                    (self.choice, self.cycle) = (0, None);
                    self.submit(&text);
                }
            }
            KeyCode::Tab | KeyCode::Down => self.tab(false),
            KeyCode::BackTab | KeyCode::Up => self.tab(true),
            _ => {
                if let Some(line) = &mut self.input {
                    if line.edit(key) {
                        (self.choice, self.cycle) = (0, None);
                    }
                }
            }
        }
    }

    fn submit(&mut self, text: &str) {
        if self.wizard.is_some() {
            return self.answer(text);
        }
        if let Some(first) = self.changing.take() {
            return self.new_password(first, text);
        }
        match (input::parse(text), text.trim()) {
            (_, "") => {}
            (Some(command), _) => self.run_command(command),
            (None, other) => {
                let word = other.split_whitespace().next().unwrap_or(other);
                let hits = input::matching(word);
                self.notice = match hits.len() > 1 {
                    true => format!("Ambiguous: {}", hits.join(", ")),
                    false => format!("Unknown command: {other}"),
                };
            }
        }
    }

    fn new_server(&mut self) {
        self.wizard = Some(Wizard::new());
        self.input = Some(Line::new(false));
    }

    fn answer(&mut self, text: &str) {
        let Some(wizard) = &mut self.wizard else {
            return;
        };
        let taken = wizard.step == Step::Name && self.servers.iter().any(|s| s.name == text.trim());
        if taken {
            self.notice = format!("A server named {} already exists", text.trim());
        } else if wizard.answer(text) {
            return self.login();
        }
        self.input = Some(Line::new(wizard.step.masked()));
    }

    fn login(&mut self) {
        let Some(wizard) = &self.wizard else {
            return;
        };
        let credentials = Credentials::Password {
            login: wizard.login().to_owned(),
            password: wizard.password().to_owned(),
        };
        let (name, url) = (wizard.config().name, wizard.url().to_owned());
        let tx = self.tx.clone();
        self.notice = "Logging in…".into();
        tokio::spawn(async move {
            let result = open(&name, &url, credentials).await;
            let _ = tx.send(Msg::LoggedIn(result.map(Box::new)));
        });
    }

    fn logged_in(&mut self, result: Result<Box<Session>, Error>) {
        let Some(wizard) = &mut self.wizard else {
            return;
        };
        match result {
            Ok(session) => {
                let config = wizard.config();
                self.wizard = None;
                self.notice = format!("Logged in to {}", config.name);
                self.configs.push(config.clone());
                if let Err(e) = config::save(&self.path, &self.configs) {
                    self.notice = format!("Logged in, but the server list was not saved: {e}");
                }
                let mut server = Server::new(config.name);
                server.me = Some(session.me.clone());
                server.users.insert(session.me.id.clone(), session.me);
                server.client = Some(session.client);
                self.servers.push(server);
                self.server = self.servers.len() - 1;
                self.team = 0;
                self.reset_view();
                self.load_teams(self.server);
                let added = self.server;
                self.start_sync(added);
            }
            Err(e) => {
                self.notice = e.to_string();
                self.input = Some(Line::new(wizard.step.masked()));
            }
        }
    }

    fn reset_view(&mut self) {
        self.selecting = false;
        self.cursor = self.count().saturating_sub(1);
        self.browsing = false;
        self.follow = false;
        self.scroll = usize::MAX;
    }

    fn move_cursor(&mut self, to: usize) {
        let count = self.count();
        if count > 0 {
            self.cursor = to.min(count - 1);
            self.browsing = true;
            self.follow = true;
        }
    }

    fn snap(&mut self) -> bool {
        let (first, last) = self.visible;
        let snapped = self.cursor.clamp(first, last);
        let moved = snapped != self.cursor;
        if moved && self.count() > 0 {
            self.cursor = snapped.min(self.count() - 1);
            self.browsing = true;
        }
        moved
    }

    fn step_up(&mut self) {
        if self.snap() {
            return;
        }
        match (self.browsing, self.cursor) {
            (true, 0) => self.load_older(),
            (true, c) => self.move_cursor(c - 1),
            (false, _) => self.browsing = self.count() > 0,
        }
    }

    fn load_older(&mut self) {
        if self.cursor != 0 {
            return;
        }
        let Some((server, chan)) = self.channel() else {
            return;
        };
        let id = chan.info.id.clone();
        let Some(messages) = server.messages.get(&id) else {
            return;
        };
        let (Some(before), len) = (messages.first().map(|m| m.id.clone()), messages.len()) else {
            return;
        };
        if server.complete.contains(&id) {
            self.notice = "Start of the channel".into();
            return;
        }
        if len >= model::MAX_MESSAGES {
            self.notice = format!("Showing the last {} messages", model::MAX_MESSAGES);
            return;
        }
        let asked = model::PAGE.min(model::MAX_MESSAGES - len);
        let Some(server) = self.servers.get_mut(self.server) else {
            return;
        };
        let Some(client) = server.client.clone() else {
            return;
        };
        if !server.paging.insert(id.clone()) {
            return;
        }
        self.notice = "Loading older messages…".into();
        self.spawn(self.server, async move {
            let list = client.posts(&id, Some(&before), asked as u32).await;
            Loaded::Older {
                channel: id,
                before,
                list: list.map_err(|e| e.to_string()),
                asked,
            }
        });
    }

    fn step_down(&mut self) {
        if self.snap() {
            return;
        }
        match (self.browsing, self.cursor + 1 >= self.count()) {
            (false, _) => {}
            (true, true) => self.browsing = self.selecting,
            (true, false) => self.move_cursor(self.cursor + 1),
        }
    }

    fn next_group(&mut self) {
        let Some(messages) = self.messages() else {
            return;
        };
        let mut i = self.cursor + 1;
        while i < messages.len() && group_with_previous(&messages[i - 1], &messages[i]) {
            i += 1;
        }
        self.move_cursor(i);
    }

    fn previous_group(&mut self) {
        let Some(messages) = self.messages().filter(|m| !m.is_empty()) else {
            return;
        };
        let here = self.cursor.min(messages.len() - 1);
        let start = group_start(messages, here);
        let target = match (start < self.cursor, self.cursor) {
            (true, _) => start,
            (false, 0) => return,
            (false, c) => group_start(messages, c - 1),
        };
        self.move_cursor(target);
    }

    fn move_channel(&mut self, step: isize) {
        let Some(team) = self
            .servers
            .get(self.server)
            .and_then(|s| s.teams.get(self.team))
        else {
            return;
        };
        let Some(count) = team.chans.as_ref().map(Vec::len).filter(|&n| n > 0) else {
            return;
        };
        self.go_channel(team.cur.saturating_add_signed(step).min(count - 1));
    }

    fn go_channel(&mut self, index: usize) {
        if let Some(team) = self
            .servers
            .get_mut(self.server)
            .and_then(|s| s.teams.get_mut(self.team))
        {
            team.cur = index;
        }
        self.reset_view();
        self.ensure();
    }

    fn move_team(&mut self, step: isize) {
        let places = self.places();
        let Some(here) = places.iter().position(|&p| p == (self.server, self.team)) else {
            return;
        };
        let next = here.saturating_add_signed(step).min(places.len() - 1);
        (self.server, self.team) = places[next];
        self.reset_view();
        self.ensure();
    }

    fn current_id(&self, i: usize) -> Option<ChannelId> {
        if i != self.server {
            return None;
        }
        self.channel().map(|(_, chan)| chan.info.id.clone())
    }

    fn loaded(&mut self, i: usize, loaded: Loaded) {
        let loaded = match loaded {
            Loaded::Created(post) => {
                if self.current_id(i).as_ref() == Some(&post.channel_id) {
                    self.reset_view();
                }
                let mentions = Vec::new();
                return self.apply_event(
                    i,
                    Event::Posted {
                        post: *post,
                        mentions,
                    },
                );
            }
            Loaded::Edited(post) => return self.apply_event(i, Event::Edited(*post)),
            Loaded::Reacted(reaction) => {
                let glyph = crate::markdown::emoji(&format!(":{}:", reaction.emoji_name));
                self.notice = format!("Reacted {glyph}");
                self.reactions = true;
                return self.apply_event(i, Event::ReactionAdded(reaction));
            }
            Loaded::Unreacted(reaction) => {
                let glyph = crate::markdown::emoji(&format!(":{}:", reaction.emoji_name));
                self.notice = format!("Removed {glyph}");
                self.reactions = true;
                return self.apply_event(i, Event::ReactionRemoved(reaction));
            }
            Loaded::Deleted { channel, id } => {
                let post = Post {
                    id,
                    channel_id: channel,
                    ..Post::default()
                };
                return self.apply_event(i, Event::Deleted(post));
            }
            Loaded::NotSent {
                channel,
                text,
                error,
            } => {
                let kept = self
                    .drafts
                    .remove(&(i, channel.clone()))
                    .unwrap_or_default();
                let draft = format!("{text} {kept}").trim_end().to_owned();
                self.drafts.insert((i, channel), draft);
                let name = self.servers.get(i).map_or("", |s| s.name.as_str());
                self.notice = format!("{name}: {error} (kept as a draft)");
                return;
            }
            other => other,
        };
        let current = self.current_id(i);
        let Some(server) = self.servers.get_mut(i).filter(|s| !s.is_gone()) else {
            return;
        };
        match loaded {
            Loaded::Connected(Ok(session)) => {
                server.me = Some(session.me.clone());
                server.users.insert(session.me.id.clone(), session.me);
                server.client = Some(session.client);
                self.load_teams(i);
                self.start_sync(i);
            }
            Loaded::Connected(Err(e)) | Loaded::Failed(e) => {
                self.notice = format!("{}: {e}", server.name);
            }
            Loaded::Teams(teams) => {
                let mut old = std::mem::take(&mut server.teams);
                server.teams = teams
                    .into_iter()
                    .map(|info| {
                        let kept = old.iter().position(|t| t.info.id == info.id);
                        let kept = kept.map(|at| old.swap_remove(at));
                        Team {
                            info,
                            cur: kept.as_ref().map_or(0, |t| t.cur),
                            chans: kept.and_then(|t| t.chans),
                            loading: false,
                        }
                    })
                    .collect();
                if i == self.server {
                    self.team = self.team.min(server.teams.len().saturating_sub(1));
                }
                let shown = (i == self.server).then_some(self.team);
                let ids: Vec<TeamId> = server
                    .teams
                    .iter_mut()
                    .enumerate()
                    .filter(|(at, t)| Some(*at) == shown || t.chans.is_some())
                    .map(|(_, t)| {
                        t.loading = true;
                        t.info.id.clone()
                    })
                    .collect();
                for id in ids {
                    self.fetch_channels(i, id, Duration::ZERO);
                }
            }
            Loaded::Opened(id) => {
                if i == self.server {
                    self.go = Some(id);
                    self.load_teams(i);
                }
            }
            Loaded::Left(name) => {
                self.notice = format!("Left {name}");
                self.load_teams(i);
            }
            Loaded::StatusSet(status) => {
                if let Some(me) = server.me.as_ref().map(|m| m.id.clone()) {
                    server.status.insert(me, status);
                }
                self.notice = format!("Status: {}", status.word());
            }
            Loaded::Said(text) => self.notice = text,
            Loaded::Refreshed(text) => {
                self.notice = text;
                self.load_teams(i);
            }
            Loaded::Page { title, lines } => {
                self.found = Some(Found {
                    title,
                    lines,
                    top: 0,
                    pick: None,
                });
            }
            Loaded::Deactivated => {
                let name = server.name.clone();
                self.remove_server(&name);
                self.notice = format!("Account of {name} closed");
            }
            Loaded::Found {
                text,
                channel,
                list,
                run,
                read,
            } => {
                let lines: Vec<String> = list
                    .ordered()
                    .map(Message::from_post)
                    .map(|m| {
                        let name = server.username(&m.user);
                        let said = m.text.replace('\n', " ");
                        format!("{}  {name}: {said}", m.at.format("%Y-%m-%d %H:%M"))
                    })
                    .collect();
                if lines.is_empty() {
                    self.notice = format!("No message holds \"{text}\" in the last {read}");
                } else {
                    let title = format!(
                        "{} found for \"{text}\" in the last {read} messages",
                        lines.len()
                    );
                    let mut run = model::messages_of(&run);
                    run.drain(..run.len().saturating_sub(model::MAX_MESSAGES));
                    let ids = list.ordered().map(|p| p.id.clone()).collect();
                    let pick = Pick {
                        channel,
                        ids,
                        at: 0,
                        run,
                    };
                    self.found = Some(Found {
                        title,
                        lines,
                        top: 0,
                        pick: Some(pick),
                    });
                }
            }
            Loaded::Verification(Ok(check)) => self.verifying = Some((i, check)),
            Loaded::Verification(Err(e)) => self.notice = e,
            Loaded::Channels {
                team,
                channels,
                members,
            } => {
                let chans = model::order_channels(channels, members);
                if debug::on() {
                    for chan in &chans {
                        let (total, read, mentions) = (
                            chan.info.total_msg_count,
                            chan.member.as_ref().map(|m| m.msg_count),
                            chan.member.as_ref().map(|m| m.mention_count),
                        );
                        debug::log(&format!(
                            "loaded {} {} last_post_at={} total={total} read={read:?} mentions={mentions:?} unread={}",
                            chan.info.id.0,
                            chan.info.display_name,
                            chan.info.last_post_at,
                            chan.unread()
                        ));
                    }
                }
                let others: Vec<UserId> = chans
                    .iter()
                    .flat_map(|c| [server.other_of(&c.info), c.info.invited.clone()])
                    .flatten()
                    .collect();
                let ids = server.unknown(others.iter());
                let wanted = self.go.clone();
                let alone = server.teams.len() == 1;
                if let Some(at) = server.teams.iter().position(|t| t.info.id == team) {
                    let t = &mut server.teams[at];
                    let kept = t.chans.as_ref().and_then(|c| c.get(t.cur));
                    let kept = kept.map(|c| c.info.id.clone());
                    let find = |id: ChannelId| chans.iter().position(|c| c.info.id == id);
                    let target = wanted.clone().and_then(find);
                    let everywhere = target.is_some_and(|c| {
                        matches!(chans[c].info.kind, ChannelKind::Direct | ChannelKind::Group)
                    });
                    let target =
                        target.filter(|_| !(everywhere && i == self.server && at != self.team));
                    if target.is_some() || (wanted.is_some() && alone) {
                        self.go = None;
                    }
                    if target.is_some() && i == self.server {
                        self.team = at;
                    }
                    if wanted.is_some() && target.is_none() && alone {
                        self.notice = "That channel is not in your sidebar".into();
                    }
                    t.cur = target.or_else(|| kept.and_then(find)).unwrap_or(0);
                    t.chans = Some(chans);
                    t.loading = false;
                }
                self.fetch_users(i, ids);
                self.fetch_hidden();
                if self.current_id(i) != current {
                    self.reset_view();
                    self.ensure();
                }
            }
            Loaded::Posts { channel, list } => {
                let messages = model::messages_of(&list);
                let ids = server.unknown(messages.iter().map(|m| &m.user));
                server.loading.remove(&channel);
                let selected = (current.as_ref() == Some(&channel) && self.browsing)
                    .then(|| server.messages.get(&channel)?.get(self.cursor))
                    .flatten()
                    .map(|m| m.id.clone());
                let cached = server.messages.entry(channel.clone()).or_default();
                let kept = model::replace_window(cached, messages);
                if let Some(at) = selected.and_then(|id| cached.iter().position(|m| m.id == id)) {
                    self.cursor = at;
                }
                if !kept {
                    server.complete.remove(&channel);
                }
                if list.order.len() < model::PAGE {
                    server.complete.insert(channel);
                }
                self.fetch_users(i, ids);
            }
            Loaded::Older {
                channel,
                before,
                list,
                asked,
            } => {
                server.paging.remove(&channel);
                match list {
                    Err(e) => self.notice = format!("{}: {e}", server.name),
                    Ok(list) => {
                        let page = model::messages_of(&list);
                        let ids = server.unknown(page.iter().map(|m| &m.user));
                        let intact = server
                            .messages
                            .get(&channel)
                            .and_then(|m| m.first())
                            .is_some_and(|m| m.id == before);
                        if intact {
                            if list.order.len() < asked {
                                server.complete.insert(channel.clone());
                            }
                            let here = current.as_ref() == Some(&channel);
                            let cached = server.messages.entry(channel).or_default();
                            let selected = here
                                .then(|| cached.get(self.cursor))
                                .flatten()
                                .map(|m| m.id.clone());
                            model::merge_page(cached, page);
                            if let Some(id) = selected {
                                let at = cached.iter().position(|m| m.id == id).unwrap_or(0);
                                self.cursor = if id == before {
                                    at.saturating_sub(1)
                                } else {
                                    at
                                };
                                self.follow = true;
                            }
                        }
                        self.fetch_users(i, ids);
                    }
                }
            }
            Loaded::Created(_)
            | Loaded::Edited(_)
            | Loaded::Reacted(_)
            | Loaded::Unreacted(_)
            | Loaded::Deleted { .. }
            | Loaded::NotSent { .. }
            | Loaded::Viewed => {}
            Loaded::Downloaded { path, name, kind } => {
                self.downloaded(&path, &name, kind);
            }
            Loaded::Members {
                channel,
                users,
                statuses,
            } => {
                server.members_loading.remove(&channel);
                let ids = users.iter().map(|u| u.id.clone()).collect();
                server
                    .users
                    .extend(users.into_iter().map(|u| (u.id.clone(), u)));
                for entry in statuses {
                    server
                        .status
                        .insert(entry.user_id, Status::parse(&entry.status));
                }
                server.members.insert(channel, ids);
            }
            Loaded::Users(users) => {
                server
                    .users
                    .extend(users.into_iter().map(|u| (u.id.clone(), u)));
            }
        }
        if i == self.server {
            self.cursor = self.cursor.min(self.count().saturating_sub(1));
            if self.scroll == usize::MAX && !self.browsing {
                self.reset_view();
            }
            self.ensure();
        }
    }

    fn sync(&mut self, i: usize, sync: Sync) {
        let Some(server) = self.servers.get_mut(i).filter(|s| !s.is_gone()) else {
            return;
        };
        match sync {
            Sync::Up { resync } => {
                server.link = Link::Up;
                if resync {
                    self.resync(i);
                }
            }
            Sync::Down(reason) => {
                server.link = Link::Down;
                if self.notice.is_empty() {
                    self.notice = format!("{}: {reason}", server.name);
                }
            }
            Sync::Event(event) => self.apply_event(i, *event),
        }
    }

    fn resync(&mut self, i: usize) {
        let current = self.current_id(i);
        let Some(server) = self.servers.get_mut(i) else {
            return;
        };
        server.messages.retain(|id, _| Some(id) == current.as_ref());
        server.complete.retain(|id| Some(id) == current.as_ref());
        server.members.clear();
        server.members_loading.clear();
        self.refresh_channels(i);
        if let Some(id) = current {
            self.fetch_posts(i, id);
        }
        self.ensure_members();
    }

    fn refresh_channels(&self, i: usize) {
        let Some(server) = self.servers.get(i) else {
            return;
        };
        if !server.teams.is_empty() {
            self.load_teams(i);
        }
    }

    fn apply_event(&mut self, i: usize, event: Event) {
        let current = self.current_id(i);
        let before = self.count();
        let at_end = !self.browsing || self.cursor + 1 >= before;
        let Some(server) = self.servers.get_mut(i) else {
            return;
        };
        let me = server.me.as_ref().map(|m| m.id.clone());
        let (mut touched, mut removed, mut refresh, mut view) = (None, None, false, None);
        let mut typed = false;
        let (mut added, mut dropped) = (false, 0);
        let mut ids = Vec::new();
        match event {
            Event::Posted { post, mentions } => {
                let channel = post.channel_id.clone();
                let mine = me.as_ref() == Some(&post.user_id);
                let mentioned = !mine && me.as_ref().is_some_and(|m| mentions.contains(m));
                let mut known = false;
                let fresh = server
                    .messages
                    .get(&channel)
                    .is_none_or(|l| l.iter().all(|m| m.id != post.id));
                let chans = server
                    .teams
                    .iter_mut()
                    .flat_map(|t| t.chans.iter_mut().flatten());
                for chan in chans.filter(|c| c.info.id == channel) {
                    known = true;
                    chan.info.last_post_at = chan.info.last_post_at.max(post.create_at);
                    if fresh {
                        chan.info.total_msg_count += 1;
                    }
                    if let (true, Some(member)) = (mentioned, chan.member.as_mut()) {
                        member.mention_count += 1;
                    }
                }
                if known {
                    server.teams.iter_mut().for_each(Team::resort);
                }
                if fresh && (mine || current.as_ref() == Some(&channel)) {
                    server.mark_viewed(&channel);
                }
                if fresh && !mine && current.as_ref() == Some(&channel) {
                    view = Some(channel.clone());
                }
                if let (true, Some(command)) = (
                    fresh
                        && !mine
                        && post.kind.is_empty()
                        && current.as_ref() != Some(&channel)
                        && chrono::Utc::now().timestamp_millis() - post.create_at < RECENT_MS,
                    &self.settings.notify,
                ) {
                    let chans = server.teams.iter().flat_map(|t| t.chans.iter().flatten());
                    let chan = chans.into_iter().find(|c| c.info.id == channel);
                    let loud = |c: &crate::model::Chan| {
                        matches!(c.info.kind, ChannelKind::Direct | ChannelKind::Group)
                            && c.member
                                .as_ref()
                                .is_some_and(|m| m.notify_props.mark_unread == "all")
                    };
                    if let Some(chan) = chan.filter(|c| mentioned || loud(c)) {
                        let text = format!("{}: {}", server.username(&post.user_id), post.message);
                        let _ = open::notify(command, &server.name_of(chan), &text);
                    }
                }
                server.stopped(&channel, &post.user_id);
                refresh = !known && server.refreshed.insert(channel.clone());
                ids = server.unknown(std::iter::once(&post.user_id));
                if let Some(list) = server.messages.get_mut(&channel) {
                    added = list.iter().all(|m| m.id != post.id);
                    model::insert_message(list, Message::from_post(&post));
                    dropped = model::cap(list, model::MAX_MESSAGES);
                }
                if dropped > 0 {
                    server.complete.remove(&channel);
                }
                touched = Some(channel);
            }
            Event::Edited(post) => {
                if let Some(list) = server.messages.get_mut(&post.channel_id) {
                    model::edit_message(list, &post);
                }
            }
            Event::Deleted(post) => {
                if let Some(list) = server.messages.get_mut(&post.channel_id) {
                    removed = model::remove_message(list, &post.id);
                }
                touched = Some(post.channel_id);
            }
            Event::ReactionAdded(reaction) => {
                server
                    .messages
                    .values_mut()
                    .for_each(|l| model::react(l, &reaction));
            }
            Event::ReactionRemoved(reaction) => {
                server
                    .messages
                    .values_mut()
                    .for_each(|l| model::unreact(l, &reaction));
            }
            Event::Viewed(channel) => server.mark_viewed(&channel),
            Event::Invited(text) => {
                self.notice = text;
                refresh = true;
            }
            Event::Verification(check) => {
                if self.verifying.is_none() {
                    self.verifying = Some((i, check));
                }
            }
            Event::Status { user, status } => {
                server.status.insert(user, status);
            }
            Event::Typing { channel, user } => {
                if me.as_ref() != Some(&user) {
                    server.typed(&channel, &user, Instant::now());
                    typed = true;
                }
            }
        }
        if refresh {
            self.refresh_channels(i);
        }
        if typed {
            self.tick_later();
        }
        if let Some(channel) = view.filter(|c| current.as_ref() == Some(c)) {
            self.send_view(i, channel, None);
        }
        self.fetch_users(i, ids);
        if touched.is_some() && touched == current {
            let after = self.count();
            match removed {
                Some(at) => {
                    self.cursor = self.cursor.saturating_sub(usize::from(at < self.cursor));
                    self.cursor = self.cursor.min(after.saturating_sub(1));
                }
                None if added && at_end => {
                    self.cursor = after - 1;
                    match self.browsing {
                        true => self.follow = true,
                        false => self.scroll = usize::MAX,
                    }
                }
                None => self.cursor = self.cursor.saturating_sub(dropped),
            }
        }
        if i == self.server {
            self.ensure();
        }
    }

    fn ensure(&mut self) {
        let (i, t) = (self.server, self.team);
        let Some(server) = self.servers.get_mut(i) else {
            return;
        };
        let has_client = server.client.is_some();
        let Some(team) = server.teams.get_mut(t) else {
            return;
        };
        if team.chans.is_none() {
            if has_client && !team.loading {
                team.loading = true;
                let id = team.info.id.clone();
                self.fetch_channels(i, id, Duration::ZERO);
            }
            return;
        }
        let chan = team.chans.as_ref().and_then(|c| c.get(team.cur));
        let Some(id) = chan.map(|c| c.info.id.clone()) else {
            return;
        };
        let missing = !server.messages.contains_key(&id);
        self.enter();
        if missing {
            self.fetch_posts(i, id);
        }
        self.place();
    }

    fn enter(&mut self) {
        let Some((_, chan)) = self.channel() else {
            return;
        };
        let (id, unread) = (chan.info.id.clone(), chan.unread_count());
        let same = |e: &Entry| e.server == self.server && e.channel == id;
        if self.entry.as_ref().is_some_and(same) {
            return;
        }
        if debug::on() {
            debug::log(&format!("enter {} unread={unread}", id.0));
        }
        let old = self.entry.take();
        if let Some(e) = &old {
            if let Some(server) = self.servers.get_mut(e.server) {
                server.trim(&e.channel);
            }
        }
        let prev = old.filter(|e| e.server == self.server).map(|e| e.channel);
        let goal = std::mem::take(&mut self.goal);
        self.entry = Some(Entry {
            server: self.server,
            channel: id.clone(),
            unread,
            boundary: None,
            goal,
            placed: false,
        });
        if let Some(server) = self.servers.get_mut(self.server) {
            server.mark_viewed(&id);
        }
        self.send_view(self.server, id, prev);
    }

    fn place(&mut self) {
        let Some(entry) = self.entry.as_ref().filter(|e| !e.placed) else {
            return;
        };
        let (unread, goal) = (entry.unread, entry.goal);
        let Some((server, chan)) = self.channel() else {
            return;
        };
        let (Some(messages), Some(me)) = (server.messages.get(&chan.info.id), server.me.as_ref())
        else {
            return;
        };
        let boundary = unread::first_unread(messages, unread);
        let start = messages.len().saturating_sub(unread);
        let mention = || {
            let found = unread::first_mention(messages, start, None, chan.info.kind, me);
            found.filter(|_| unread > 0)
        };
        let target = match goal {
            Goal::Mention => mention().or(boundary),
            Goal::Unread => boundary,
        };
        let boundary = boundary.map(|i| messages[i].id.clone());
        if let Some(entry) = &mut self.entry {
            entry.placed = true;
            entry.boundary = boundary;
        }
        match target {
            Some(index) => self.move_cursor(index),
            None => self.reset_view(),
        }
    }

    fn send_view(&self, i: usize, channel: ChannelId, prev: Option<ChannelId>) {
        let Some(client) = self.servers.get(i).and_then(|s| s.client.clone()) else {
            return;
        };
        self.spawn(i, async move {
            let sent = client.view(&channel, prev.as_ref()).await;
            if debug::on() {
                let outcome = sent
                    .as_ref()
                    .map_or_else(ToString::to_string, |()| "ok".into());
                let before = prev.as_ref().map(|p| p.0.as_str());
                debug::log(&format!("view {} prev={before:?}: {outcome}", channel.0));
            }
            match sent {
                Ok(()) => Loaded::Viewed,
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    /// The first post that was unread when the open channel was entered, unless it is the first shown.
    pub fn unread_boundary(&self) -> Option<&PostId> {
        self.entry.as_ref()?.boundary.as_ref()
    }

    fn unread_start(&self) -> Option<usize> {
        let entry = self.entry.as_ref().filter(|e| e.unread > 0)?;
        let messages = self.messages()?;
        let boundary = entry.boundary.as_ref();
        Some(
            boundary
                .and_then(|id| messages.iter().position(|m| &m.id == id))
                .unwrap_or(0),
        )
    }

    fn next_unread(&mut self) {
        let first = self.unread_start().filter(|&f| f > 0);
        if let Some(first) = first.filter(|&f| !self.browsing || self.cursor < f) {
            return self.move_cursor(first);
        }
        let Some(team) = self
            .servers
            .get(self.server)
            .and_then(|s| s.teams.get(self.team))
        else {
            return;
        };
        let Some(chans) = &team.chans else {
            return;
        };
        match unread::next_marked(chans.len(), team.cur, |i| {
            i != team.cur && chans[i].unread()
        }) {
            Some(index) => self.go_channel(index),
            None => self.notice = "No other unread channel in this team".into(),
        }
    }

    fn next_mention(&mut self) {
        let here = self.channel().and_then(|(server, chan)| {
            let (messages, me) = (server.messages.get(&chan.info.id)?, server.me.as_ref()?);
            let after = self.browsing.then_some(self.cursor);
            let start = self.unread_start()?;
            unread::first_mention(messages, start, after, chan.info.kind, me)
        });
        if let Some(index) = here {
            return self.move_cursor(index);
        }
        let current = self.channel().map(|(_, chan)| chan.info.id.clone());
        let mut places = Vec::new();
        let mut here = 0;
        for (s, server) in self.servers.iter().enumerate() {
            for (t, team) in server.teams.iter().enumerate() {
                for (c, chan) in team.chans.iter().flatten().enumerate() {
                    let open = s == self.server && Some(&chan.info.id) == current.as_ref();
                    if (s, t, c) == (self.server, self.team, team.cur) {
                        here = places.len();
                    }
                    places.push((s, t, c, chan.mentions() && !open));
                }
            }
        }
        match unread::next_marked(places.len(), here, |i| places[i].3) {
            Some(i) => {
                let (s, t, c, _) = places[i];
                (self.server, self.team) = (s, t);
                self.goal = Goal::Mention;
                self.go_channel(c);
            }
            None => self.notice = "No unread mention".into(),
        }
    }

    fn mark_read(&mut self) {
        let Some((_, chan)) = self.channel() else {
            return;
        };
        let id = chan.info.id.clone();
        if let Some(entry) = &mut self.entry {
            entry.unread = 0;
            entry.boundary = None;
        }
        if let Some(server) = self.servers.get_mut(self.server) {
            server.mark_viewed(&id);
        }
        self.send_view(self.server, id, None);
        self.notice = "Marked as read".into();
    }

    fn fetch_channels(&self, i: usize, team: TeamId, after: Duration) {
        let Some(client) = self.servers.get(i).and_then(|s| s.client.clone()) else {
            return;
        };
        self.spawn(i, async move {
            tokio::time::sleep(after).await;
            let channels = client.channels(&team).await;
            let members = client.members(&team).await;
            match (channels, members) {
                (Ok(channels), Ok(members)) => Loaded::Channels {
                    team,
                    channels,
                    members,
                },
                (Err(e), _) | (_, Err(e)) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn fetch_hidden(&mut self) {
        if self
            .servers
            .iter()
            .flat_map(|s| &s.teams)
            .any(|t| t.loading)
        {
            return;
        }
        let order = std::iter::once(self.server).chain(0..self.servers.len());
        let next = order.into_iter().find_map(|i| {
            let teams = &mut self.servers.get_mut(i)?.teams;
            let team = teams.iter_mut().find(|t| t.chans.is_none())?;
            team.loading = true;
            Some((i, team.info.id.clone()))
        });
        if let Some((i, id)) = next {
            self.fetch_channels(i, id, HIDDEN_TEAM_EVERY);
        }
    }

    fn fetch_posts(&mut self, i: usize, id: ChannelId) {
        let Some(server) = self.servers.get_mut(i) else {
            return;
        };
        let Some(client) = server.client.clone() else {
            return;
        };
        if !server.loading.insert(id.clone()) {
            return;
        }
        let (watcher, room) = (client.clone(), id.clone());
        self.spawn(i, async move {
            let names = watcher.changed_keys(&room).await;
            match names.is_empty() {
                true => Loaded::Viewed,
                false => Loaded::Said(format!(
                    "The keys of {} changed: :verify or :trust them",
                    names.join(", ")
                )),
            }
        });
        self.spawn(i, async move {
            match client.posts(&id, None, model::PAGE as u32).await {
                Ok(list) => Loaded::Posts { channel: id, list },
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn start_sync(&mut self, i: usize) {
        let tx = self.tx.clone();
        let Some(server) = self.servers.get_mut(i) else {
            return;
        };
        let Some(client) = server.client.clone() else {
            return;
        };
        let (actions, receiver) = unbounded_channel();
        server.actions = Some(actions);
        server.sync = Some(tokio::spawn(sync::run(
            client,
            sync::BACKOFF,
            receiver,
            move |event| tx.send(Msg::Sync(i, event)).is_ok(),
        )));
    }

    fn connect(&self, i: usize, name: &str, url: &str, credentials: Credentials) {
        let (name, url) = (name.to_owned(), url.to_owned());
        self.spawn(i, async move {
            match open(&name, &url, credentials).await {
                Ok(session) => Loaded::Connected(Ok(Box::new(session))),
                Err(e) => Loaded::Connected(Err(e.to_string())),
            }
        });
    }

    fn load_teams(&self, i: usize) {
        let Some(client) = self.servers.get(i).and_then(|s| s.client.clone()) else {
            return;
        };
        self.spawn(i, async move {
            match client.teams().await {
                Ok(teams) => Loaded::Teams(teams),
                Err(e) => Loaded::Failed(e.to_string()),
            }
        });
    }

    fn fetch_users(&self, i: usize, ids: Vec<UserId>) {
        let Some(client) = self.servers.get(i).and_then(|s| s.client.clone()) else {
            return;
        };
        for chunk in ids.chunks(100) {
            let (client, chunk) = (client.clone(), chunk.to_vec());
            self.spawn(i, async move {
                match client.users(&chunk).await {
                    Ok(users) => Loaded::Users(users),
                    Err(e) => Loaded::Failed(e.to_string()),
                }
            });
        }
    }

    fn spawn(&self, server: usize, work: impl Future<Output = Loaded> + Send + 'static) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Msg::Loaded(server, work.await));
        });
    }
}

async fn open(name: &str, url: &str, credentials: Credentials) -> Result<Session, Error> {
    let client = Client::connect(name, url, credentials).await?;
    let me = client.me().await?;
    Ok(Session {
        client: Arc::new(client),
        me,
    })
}

#[cfg(test)]
pub mod tests {
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};
    use serde::de::DeserializeOwned;
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::api::types::{Post, PostList, Reaction, Status};
    use crate::ui::tests::screen;
    use unicode_width::UnicodeWidthStr;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    pub fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
    }

    pub fn press_code(app: &mut App, code: KeyCode) {
        app.update(Msg::Key(key(code)));
    }

    pub fn fixture() -> App {
        let (app, rx) = fixture_rx();
        std::mem::forget(rx);
        app
    }

    pub fn fixture_rx() -> (App, tokio::sync::mpsc::UnboundedReceiver<Msg>) {
        let (tx, rx) = unbounded_channel();
        let config = ServerConfig {
            name: "work".into(),
            url: "https://chat.example.org".into(),
            method: crate::config::Method::Password,
            login: "paul".into(),
            password: None,
            password_cmd: None,
        };
        let mut app = App::new(
            vec![(config, Err("offline".into()))],
            PathBuf::from("/nonexistent/servers.toml"),
            tx,
        );
        app.notice.clear();
        app.settings.copy = Some("true".into());
        app.settings.open = crate::config::Open {
            image: "true".into(),
            video: "true".into(),
            browser: "true".into(),
            text: "true".into(),
            other: "true".into(),
        };
        app.servers[0].me = Some(json_user("u1", "paul"));
        app.loaded(
            0,
            Loaded::Users(vec![json_user("u1", "paul"), json_user("u2", "ana")]),
        );
        app.loaded(
            0,
            Loaded::Teams(vec![serde_json::from_value(serde_json::json!(
                {"id": "t1", "name": "eng", "display_name": "Engineering"}
            ))
            .unwrap()]),
        );
        app.loaded(
            0,
            Loaded::Channels {
                team: TeamId("t1".into()),
                channels: json("channels"),
                members: json("members"),
            },
        );
        let list: PostList = json("posts");
        app.loaded(
            0,
            Loaded::Posts {
                channel: ChannelId("c3".into()),
                list,
            },
        );
        (app, rx)
    }

    pub fn invited(kind: &str) -> App {
        let mut app = fixture();
        let channels = serde_json::from_value(serde_json::json!([
            {"id": "c9", "type": kind, "display_name": "Club", "name": "club",
             "last_post_at": 9, "invited": "u2"}
        ]))
        .unwrap();
        app.loaded(
            0,
            Loaded::Channels {
                team: TeamId("t1".into()),
                channels,
                members: Vec::new(),
            },
        );
        app
    }

    pub fn json<T: DeserializeOwned>(name: &str) -> T {
        let text = std::fs::read_to_string(format!("tests/fixtures/{name}.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    pub fn json_user(id: &str, name: &str) -> User {
        serde_json::from_value(serde_json::json!({"id": id, "username": name})).unwrap()
    }

    #[test]
    fn first_k_reveals_and_j_from_the_latest_hides() {
        let mut app = fixture();
        assert!(!app.browsing);
        press(&mut app, "h");
        assert!(app.browsing);
        assert_eq!(app.cursor, 1);
        press(&mut app, "h");
        assert_eq!(app.cursor, 0);
        press(&mut app, "ll");
        assert!(!app.browsing);
    }

    #[test]
    fn g_and_capital_g_reach_the_ends() {
        let mut app = fixture();
        press(&mut app, "g");
        assert_eq!(app.cursor, 0);
        press(&mut app, "G");
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn q_quits_but_closes_help_first() {
        let mut app = fixture();
        press(&mut app, "?q");
        assert!(!app.quit);
        press(&mut app, "q");
        assert!(app.quit);
    }

    #[test]
    fn unknown_commands_are_reported_and_quit_works() {
        let mut app = fixture();
        press(&mut app, ":nope");
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.notice, "Unknown command: nope");
        press(&mut app, ":quit");
        press_code(&mut app, KeyCode::Enter);
        assert!(app.quit);
    }

    #[test]
    fn channel_keys_stop_at_the_ends() {
        let mut app = fixture();
        press(&mut app, "K");
        assert_eq!(app.servers[0].teams[0].cur, 0);
        press(&mut app, "JJJJJJ");
        assert_eq!(app.servers[0].teams[0].cur, 3);
    }

    pub fn post(id: &str, channel: &str, user: &str, text: &str, ms: i64) -> Post {
        serde_json::from_value(serde_json::json!({
            "id": id, "create_at": ms, "user_id": user, "channel_id": channel, "message": text
        }))
        .unwrap()
    }

    pub fn posted(app: &mut App, post: Post, mentions: &[&str]) {
        let mentions = mentions.iter().map(|m| UserId((*m).into())).collect();
        app.apply_event(0, Event::Posted { post, mentions });
    }

    fn reaction(user: &str, post: &str) -> Reaction {
        serde_json::from_value(serde_json::json!({
            "user_id": user, "post_id": post, "emoji_name": "tada"
        }))
        .unwrap()
    }

    fn text(app: &mut App) -> String {
        screen(app, 80, 14).join("\n")
    }

    fn tabs(app: &mut App) -> String {
        screen(app, 80, 14).remove(0)
    }

    #[test]
    fn a_new_post_in_the_current_channel_appears_and_the_view_follows() {
        let mut app = fixture();
        posted(
            &mut app,
            post("p10", "c3", "u2", "late news", 1_700_000_300_000),
            &[],
        );
        assert!(text(&mut app).contains("late news"));
        assert_eq!((app.cursor, app.browsing), (2, false));
        press(&mut app, "hh");
        assert_eq!(app.cursor, 1);
        posted(
            &mut app,
            post("p11", "c3", "u2", "later news", 1_700_000_400_000),
            &[],
        );
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn a_mention_elsewhere_runs_the_notify_command_but_the_open_channel_and_old_posts_do_not() {
        let mut app = fixture();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        app.settings.notify = Some(format!("printf '%s|%s' \"$1\" \"$2\" > {}", out.display()));
        let now = chrono::Utc::now().timestamp_millis();
        posted(&mut app, post("n1", "c3", "u2", "open room", now), &["u1"]);
        posted(
            &mut app,
            post("n2", "c1", "u2", "old news", now - 600_000),
            &["u1"],
        );
        posted(&mut app, post("n3", "c1", "u2", "no call", now), &[]);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!out.exists());
        posted(&mut app, post("n4", "c1", "u2", "@paul ping", now), &["u1"]);
        let said = wait_for_file(&out);
        assert!(
            said.starts_with("Town Square|") && said.ends_with(": @paul ping"),
            "{said}"
        );
    }

    #[test]
    fn search_results_are_a_page_to_scroll_and_close() {
        let mut app = fixture();
        app.loaded(
            0,
            Loaded::Found {
                text: "news".into(),
                channel: ChannelId("c3".into()),
                run: PostList::default(),
                list: PostList {
                    order: vec![PostId("p1".into())],
                    posts: HashMap::from([(
                        PostId("p1".into()),
                        post("p1", "c3", "u2", "late\nnews", 1_700_000_300_000),
                    )]),
                },
                read: 40,
            },
        );
        assert_eq!(app.mode(), "RESULTS");
        let page = text(&mut app);
        assert!(
            page.contains("1 found for \"news\" in the last 40 messages"),
            "{page}"
        );
        assert!(page.contains("late news"), "{page}");
        press(&mut app, "jk");
        press(&mut app, "q");
        assert_eq!(app.mode(), "NORMAL");
        app.loaded(
            0,
            Loaded::Found {
                text: "zzz".into(),
                channel: ChannelId("c3".into()),
                run: PostList::default(),
                list: PostList::default(),
                read: 40,
            },
        );
        assert_eq!(app.notice, "No message holds \"zzz\" in the last 40");
    }

    #[test]
    fn enter_on_a_hit_opens_it_in_its_channel_and_a_hit_too_old_stays_listed() {
        let mut app = fixture();
        let channel = app.channel().map(|(_, c)| c.info.id.clone()).unwrap();
        let posts = |ids: &[(&str, i64)]| PostList {
            order: ids.iter().map(|(id, _)| PostId((*id).into())).collect(),
            posts: ids
                .iter()
                .map(|(id, ms)| {
                    let id = PostId((*id).into());
                    (id.clone(), post(&id.0, &channel.0, "u2", "pizza", *ms))
                })
                .collect(),
        };
        let found = |app: &mut App, hits: &[(&str, i64)], run: &[(&str, i64)]| {
            app.loaded(
                0,
                Loaded::Found {
                    text: "pizza".into(),
                    channel: channel.clone(),
                    list: posts(hits),
                    run: posts(run),
                    read: 40,
                },
            );
        };
        found(
            &mut app,
            &[("h2", 1_700_000_900_000), ("h1", 1_700_000_100_000)],
            &[
                ("h2", 1_700_000_900_000),
                ("m", 1_700_000_500_000),
                ("h1", 1_700_000_100_000),
            ],
        );
        press(&mut app, "j");
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.mode(), "NORMAL");
        let messages = &app.servers[0].messages[&channel];
        assert_eq!(messages[app.cursor].id, PostId("h1".into()));
        assert!(app.browsing);
        found(
            &mut app,
            &[("gone", 1_600_000_000_000)],
            &[("h2", 1_700_000_900_000)],
        );
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.mode(), "RESULTS");
        assert!(app.notice.contains("Too old"), "{}", app.notice);
    }

    #[test]
    fn a_post_elsewhere_counts_a_mention_and_viewing_clears_the_tab() {
        let mut app = fixture();
        assert!(tabs(&mut app).contains("•#Town Square!"));
        posted(
            &mut app,
            post("p12", "c1", "u2", "@paul ping", 1_700_000_500_000),
            &["u1"],
        );
        let member = app.servers[0].teams[0].chans.as_ref().unwrap()[0]
            .member
            .clone()
            .unwrap();
        assert_eq!(member.mention_count, 2);
        app.apply_event(0, Event::Viewed(ChannelId("c1".into())));
        assert!(tabs(&mut app).contains(" #Town Square "));
    }

    #[test]
    fn my_own_post_from_another_device_is_not_unread() {
        let mut app = fixture();
        app.servers[0].teams[0].chans.as_mut().unwrap()[1].member = None;
        posted(
            &mut app,
            post("p13", "c1", "u1", "from my phone", 1_700_000_600_000),
            &[],
        );
        assert!(!tabs(&mut app).contains("•"));
    }

    fn members_viewed_up_to(ms: i64) -> Vec<ChannelMember> {
        let mut members: Vec<ChannelMember> = json("members");
        members[0].msg_count = ms;
        members[0].mention_count = 0;
        members
    }

    #[test]
    fn entering_a_channel_clears_its_tab_and_a_refresh_with_the_server_state_agrees() {
        let mut app = fixture();
        press(&mut app, "J");
        let member = app.servers[0].teams[0].chans.as_ref().unwrap()[1]
            .member
            .clone()
            .unwrap();
        assert_eq!((member.msg_count, member.mention_count), (2, 0));
        press(&mut app, "K");
        assert!(tabs(&mut app).contains(" #Town Square "));
        app.loaded(
            0,
            Loaded::Channels {
                team: TeamId("t1".into()),
                channels: json("channels"),
                members: members_viewed_up_to(2),
            },
        );
        assert!(tabs(&mut app).contains(" #Town Square "));
        assert_eq!(app.servers[0].teams[0].cur, 0);
    }

    fn enter_town_square(app: &mut App) {
        press(app, "J");
        let list: PostList = json("posts");
        app.loaded(
            0,
            Loaded::Posts {
                channel: ChannelId("c1".into()),
                list,
            },
        );
    }

    #[test]
    fn the_separator_and_the_cursor_mark_the_first_unread_until_the_next_visit() {
        let mut app = fixture();
        enter_town_square(&mut app);
        assert!(text(&mut app).contains("new messages"));
        assert_eq!((app.cursor, app.browsing), (1, true));
        press(&mut app, "KJ");
        assert!(!text(&mut app).contains("new messages"));
    }

    #[test]
    fn c_drops_the_separator_for_this_visit() {
        let mut app = fixture();
        enter_town_square(&mut app);
        press(&mut app, "c");
        assert_eq!(app.notice, "Marked as read");
        assert!(!text(&mut app).contains("new messages"));
    }

    #[test]
    fn n_goes_to_the_next_unread_channel_then_says_when_there_is_none() {
        let mut app = fixture();
        press(&mut app, "n");
        assert_eq!(app.servers[0].teams[0].cur, 1);
        let list: PostList = json("posts");
        app.loaded(
            0,
            Loaded::Posts {
                channel: ChannelId("c1".into()),
                list,
            },
        );
        press(&mut app, "n");
        assert_eq!(app.notice, "No other unread channel in this team");
        assert_eq!(app.servers[0].teams[0].cur, 1);
    }

    #[test]
    fn capital_n_goes_to_the_next_mention_then_says_when_there_is_none() {
        let mut app = fixture();
        press(&mut app, "N");
        assert_eq!(app.servers[0].teams[0].cur, 1);
        let list: PostList = json("posts");
        app.loaded(
            0,
            Loaded::Posts {
                channel: ChannelId("c1".into()),
                list,
            },
        );
        assert_eq!(app.cursor, 1);
        press(&mut app, "N");
        assert_eq!(app.notice, "No unread mention");
    }

    #[test]
    fn a_post_elsewhere_flags_its_tab_and_one_in_the_open_channel_does_not() {
        let mut app = fixture();
        app.servers[0].teams[0].chans.as_mut().unwrap()[0].member =
            serde_json::from_value(serde_json::json!({"channel_id": "c3", "msg_count": 0}))
                .unwrap();
        press(&mut app, "J");
        posted(
            &mut app,
            post("p20", "c3", "u2", "elsewhere", 1_700_000_900_000),
            &[],
        );
        assert!(tabs(&mut app).contains("•+Secret"));
        posted(
            &mut app,
            post("p21", "c1", "u2", "right here", 1_700_000_950_000),
            &[],
        );
        assert!(!tabs(&mut app).contains("•#Town Square"));
    }

    #[test]
    fn edits_deletes_and_reactions_change_the_screen() {
        let mut app = fixture();
        press(&mut app, "a");
        let mut edited = post("p2", "c3", "u2", "hi there", 1_700_000_060_000);
        edited.edit_at = 1_700_000_070_000;
        app.apply_event(0, Event::Edited(edited));
        let shown = text(&mut app);
        assert!(shown.contains("hi there *"), "{shown}");
        app.apply_event(0, Event::ReactionAdded(reaction("u1", "p2")));
        assert!(text(&mut app).contains("🎉 1"));
        app.apply_event(0, Event::ReactionRemoved(reaction("u1", "p2")));
        assert!(!text(&mut app).contains("🎉 1"));
        press(&mut app, "hh");
        app.apply_event(
            0,
            Event::Deleted(post("p1", "c3", "u1", "", 1_700_000_000_000)),
        );
        assert!(!text(&mut app).contains("hello"));
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn a_dropped_stream_shows_offline_until_it_is_back() {
        let mut app = fixture();
        app.sync(0, Sync::Down("connection closed".into()));
        assert_eq!(app.notice, "work: connection closed");
        app.notice.clear();
        assert!(screen(&mut app, 80, 14)
            .last()
            .unwrap()
            .ends_with("offline  2/2  100%  ? help"));
        app.sync(0, Sync::Up { resync: false });
        assert!(!screen(&mut app, 80, 14).last().unwrap().contains("offline"));
    }

    fn page(posts: &[(&str, i64)]) -> PostList {
        let order: Vec<&str> = posts.iter().rev().map(|(id, _)| *id).collect();
        let posts: serde_json::Map<String, serde_json::Value> = posts
            .iter()
            .map(|(id, ms)| {
                let post = serde_json::json!({
                    "id": id, "create_at": ms, "user_id": "u2", "channel_id": "c3", "message": id
                });
                ((*id).to_owned(), post)
            })
            .collect();
        serde_json::from_value(serde_json::json!({"order": order, "posts": posts})).unwrap()
    }

    fn older(app: &mut App, before: &str, posts: &[(&str, i64)], asked: usize) {
        app.loaded(
            0,
            Loaded::Older {
                channel: ChannelId("c3".into()),
                before: PostId(before.into()),
                list: Ok(page(posts)),
                asked,
            },
        );
    }

    fn ids(app: &App) -> Vec<String> {
        app.messages()
            .unwrap()
            .iter()
            .map(|m| m.id.0.clone())
            .collect()
    }

    const OLD: [(&str, i64); 2] = [("o1", 1_699_999_000_000), ("o2", 1_699_999_100_000)];

    #[test]
    fn an_older_page_lands_the_cursor_above_the_old_top_or_keeps_it_on_its_message() {
        let mut app = fixture();
        press(&mut app, "hh");
        assert_eq!(app.cursor, 0);
        older(&mut app, "p1", &OLD, 60);
        assert_eq!(ids(&app), ["o1", "o2", "p1", "p2"]);
        assert_eq!(app.cursor, 1);

        let mut app = fixture();
        press(&mut app, "h");
        older(&mut app, "p1", &OLD, 60);
        assert_eq!(app.cursor, 3);
    }

    #[test]
    fn overlapping_and_stale_pages_change_nothing_wrong() {
        let mut app = fixture();
        older(&mut app, "p1", &[OLD[0], ("p1", 1_700_000_000_000)], 60);
        assert_eq!(ids(&app), ["o1", "p1", "p2"]);
        older(&mut app, "p1", &[OLD[1]], 60);
        assert_eq!(ids(&app), ["o1", "p1", "p2"]);
    }

    #[test]
    fn a_short_page_ends_the_history() {
        let mut app = fixture();
        app.servers[0].complete.clear();
        older(&mut app, "p1", &OLD, 60);
        assert!(text(&mut app).contains("Start of +Secret"));
        press(&mut app, "hhhhh");
        assert_eq!(app.notice, "Start of the channel");
    }

    #[test]
    fn a_full_page_leaves_the_history_open_and_a_failed_one_can_be_retried() {
        let mut app = fixture();
        app.servers[0].complete.clear();
        app.servers[0].paging.insert(ChannelId("c3".into()));
        older(&mut app, "p1", &OLD, 2);
        assert!(!app.servers[0].complete.contains(&ChannelId("c3".into())));
        app.servers[0].paging.insert(ChannelId("c3".into()));
        app.loaded(
            0,
            Loaded::Older {
                channel: ChannelId("c3".into()),
                before: PostId("o1".into()),
                list: Err("timeout".into()),
                asked: 60,
            },
        );
        assert!(app.servers[0].paging.is_empty());
        assert_eq!(app.notice, "work: timeout");
    }

    #[test]
    fn a_refresh_of_the_newest_page_keeps_the_older_history() {
        let mut app = fixture();
        older(&mut app, "p1", &OLD, 60);
        let list: PostList = json("posts");
        app.loaded(
            0,
            Loaded::Posts {
                channel: ChannelId("c3".into()),
                list,
            },
        );
        assert_eq!(ids(&app), ["o1", "o2", "p1", "p2"]);
    }

    fn crowd(app: &mut App, count: usize) {
        let base = 1_600_000_000_000;
        let list = (0..count)
            .map(|n| {
                Message::from_post(&post(
                    &format!("m{n}"),
                    "c3",
                    "u2",
                    "x",
                    base + n as i64 * 1000,
                ))
            })
            .collect();
        app.servers[0].messages.insert(ChannelId("c3".into()), list);
        app.servers[0].complete.clear();
    }

    #[test]
    fn a_live_post_over_the_cap_drops_the_oldest_and_keeps_the_cursor_on_its_message() {
        let mut app = fixture();
        crowd(&mut app, 500);
        (app.browsing, app.cursor) = (true, 10);
        posted(
            &mut app,
            post("new", "c3", "u2", "x", 1_700_000_000_000),
            &[],
        );
        assert_eq!((app.count(), app.cursor), (500, 9));
        assert_eq!(app.messages().unwrap()[0].id.0, "m1");
    }

    #[test]
    fn leaving_a_long_channel_trims_it_to_the_newest_page() {
        let mut app = fixture();
        crowd(&mut app, 100);
        app.servers[0].complete.insert(ChannelId("c3".into()));
        press(&mut app, "J");
        let id = ChannelId("c3".into());
        assert_eq!(app.servers[0].messages[&id].len(), model::PAGE);
        assert!(!app.servers[0].complete.contains(&id));
    }

    #[test]
    fn h_and_l_select_the_nearest_message_on_screen_when_the_selection_is_off_it() {
        for (key, above) in [("h", false), ("l", false), ("h", true), ("l", true)] {
            let mut app = fixture();
            crowd(&mut app, 60);
            app.reset_view();
            crate::ui::tests::screen(&mut app, 60, 10);
            press(&mut app, "h");
            let selected = app.cursor;
            press(&mut app, &"k".repeat(40));
            if above {
                app.cursor = 0;
            }
            crate::ui::tests::screen(&mut app, 60, 10);
            let (first, last) = app.visible;
            assert!(first < last, "{:?}", app.visible);
            assert_eq!(app.cursor, if above { 0 } else { selected });
            press(&mut app, key);
            assert_eq!(app.cursor, if above { first } else { last });
            press(&mut app, "l");
            assert_eq!(
                app.cursor,
                if above { first + 1 } else { (last + 1).min(59) }
            );
        }
    }

    fn add_home(app: &mut App) {
        let mut server = Server::new("home".into());
        server.me = Some(json_user("u1", "paul"));
        app.servers.push(server);
        let team = serde_json::from_value(serde_json::json!(
            {"id": "t9", "display_name": "Family"}
        ))
        .unwrap();
        app.loaded(1, Loaded::Teams(vec![team]));
        let channels = serde_json::from_value(serde_json::json!([
            {"id": "h1", "type": "O", "display_name": "Garden", "name": "garden"},
            {"id": "h2", "type": "O", "display_name": "Kitchen", "name": "kitchen"}
        ]))
        .unwrap();
        app.loaded(
            1,
            Loaded::Channels {
                team: TeamId("t9".into()),
                channels,
                members: Vec::new(),
            },
        );
    }

    #[test]
    fn only_the_shown_team_and_the_visited_ones_load_at_start() {
        let mut app = fixture();
        let team = |id: &str| -> crate::api::types::Team {
            serde_json::from_value(serde_json::json!({"id": id, "display_name": id})).unwrap()
        };
        app.servers[0].teams[0].chans = None;
        app.loaded(0, Loaded::Teams(vec![team("t1"), team("t2"), team("t3")]));
        let loading: Vec<bool> = app.servers[0].teams.iter().map(|t| t.loading).collect();
        assert_eq!(loading, [true, false, false]);
    }

    #[test]
    fn hidden_teams_load_one_after_another_in_the_background() {
        let mut app = fixture();
        let team = |id: &str| -> crate::api::types::Team {
            serde_json::from_value(serde_json::json!({"id": id, "display_name": id})).unwrap()
        };
        app.servers[0].teams[0].chans = None;
        app.loaded(0, Loaded::Teams(vec![team("t1"), team("t2"), team("t3")]));
        app.loaded(
            0,
            Loaded::Channels {
                team: TeamId("t1".into()),
                channels: Vec::new(),
                members: Vec::new(),
            },
        );
        let loading: Vec<bool> = app.servers[0].teams.iter().map(|t| t.loading).collect();
        assert_eq!(loading, [false, true, false]);
    }

    #[test]
    fn f_filters_the_team_and_enter_goes_to_the_channel() {
        let mut app = fixture();
        press(&mut app, "f");
        assert_eq!(app.mode(), "SEARCH");
        press(&mut app, "town");
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.mode(), "NORMAL");
        assert_eq!(app.servers[0].teams[0].cur, 1);
    }

    #[test]
    fn esc_closes_the_finder_without_moving_and_keys_are_text_in_it() {
        let mut app = fixture();
        press(&mut app, "fjk");
        assert_eq!(app.finder.as_ref().unwrap().line.text(), "jk");
        press_code(&mut app, KeyCode::Esc);
        assert!(app.finder.is_none());
        assert_eq!(app.servers[0].teams[0].cur, 0);
    }

    #[test]
    fn tab_wraps_around_the_matches_and_typing_goes_back_to_the_best() {
        let mut app = fixture();
        press(&mut app, "f");
        for _ in 0..4 {
            press_code(&mut app, KeyCode::Tab);
        }
        assert_eq!(app.finder.as_ref().unwrap().selected, 0);
        press_code(&mut app, KeyCode::BackTab);
        assert_eq!(app.finder.as_ref().unwrap().selected, 3);
        press(&mut app, "t");
        assert_eq!(app.finder.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn capital_f_reaches_other_servers_and_f_does_not() {
        let mut app = fixture();
        add_home(&mut app);
        press(&mut app, "fgard");
        assert!(app.finder_targets().is_empty());
        press_code(&mut app, KeyCode::Esc);
        press(&mut app, "Fgard");
        assert_eq!(app.finder_targets().len(), 1);
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(
            (app.server, app.team, app.servers[1].teams[0].cur),
            (1, 0, 0)
        );
        press(&mut app, "FKitchen");
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.servers[1].teams[0].cur, 1);
    }

    #[test]
    fn at_lists_the_conversations_and_a_hash_names_the_team() {
        let mut app = fixture();
        add_home(&mut app);
        press(&mut app, "F@");
        assert_eq!(app.finder_targets(), [(0, 0, 2), (0, 0, 3)]);
        press_code(&mut app, KeyCode::Esc);
        press(&mut app, "Fhome#k");
        assert_eq!(app.finder_targets(), [(1, 0, 1)]);
    }

    #[test]
    fn a_draft_is_kept_per_channel_and_a_failed_send_gives_the_text_back() {
        let mut app = fixture();
        press(&mut app, "idraft one");
        press_code(&mut app, KeyCode::Esc);
        assert_eq!(app.mode(), "NORMAL");
        press(&mut app, "i");
        assert_eq!(app.compose.as_ref().unwrap().line.text(), "draft one");
        press_code(&mut app, KeyCode::Esc);
        press(&mut app, "Ji");
        assert_eq!(app.compose.as_ref().unwrap().line.text(), "");
        press_code(&mut app, KeyCode::Esc);
        app.loaded(
            0,
            Loaded::NotSent {
                channel: ChannelId("c1".into()),
                text: "lost words".into(),
                error: "timeout".into(),
            },
        );
        assert_eq!(app.notice, "work: timeout (kept as a draft)");
        press(&mut app, "i");
        assert_eq!(app.compose.as_ref().unwrap().line.text(), "lost words");
    }

    #[test]
    fn a_paste_never_sends_and_loses_its_line_breaks() {
        let mut app = fixture();
        press(&mut app, "ix ");
        app.update(Msg::Paste("a\nb\r\nc".into()));
        assert_eq!(app.compose.as_ref().unwrap().line.text(), "x a b  c");
        app.update(Msg::Key(key(KeyCode::Esc)));
        app.update(Msg::Paste("ignored".into()));
        assert!(app.compose.is_none() && app.input.is_none());
    }

    #[test]
    fn typing_is_announced_once_per_input_session_and_never_while_editing() {
        let mut app = fixture();
        let (actions, mut seen) = unbounded_channel();
        app.servers[0].actions = Some(actions);
        press(&mut app, "iabc");
        assert!(matches!(seen.try_recv(), Ok(Action::Typing { .. })));
        assert!(seen.try_recv().is_err());
        press_code(&mut app, KeyCode::Esc);
        press(&mut app, "id");
        assert!(seen.try_recv().is_ok());
        press_code(&mut app, KeyCode::Esc);
        app.cursor = 0;
        press(&mut app, "ex");
        assert!(seen.try_recv().is_err());
    }

    #[test]
    fn who_is_typing_shows_on_the_status_line_until_they_stop_or_expire() {
        let mut app = fixture();
        let (channel, ana) = (ChannelId("c3".into()), UserId("u2".into()));
        app.apply_event(
            0,
            Event::Typing {
                channel: channel.clone(),
                user: ana.clone(),
            },
        );
        let status = screen(&mut app, 80, 14).pop().unwrap();
        assert!(status.contains("✎ ana"), "{status:?}");
        posted(
            &mut app,
            post("p9", "c3", "u2", "done", 1_700_000_900_000),
            &[],
        );
        assert!(!screen(&mut app, 80, 14).pop().unwrap().contains("✎"));
        let old = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        app.servers[0].typing.insert(channel, vec![(ana, old)]);
        assert!(!screen(&mut app, 80, 14).pop().unwrap().contains("✎"));
        let me = UserId("u1".into());
        app.apply_event(
            0,
            Event::Typing {
                channel: ChannelId("c3".into()),
                user: me,
            },
        );
        assert!(!screen(&mut app, 80, 14).pop().unwrap().contains("✎"));
    }

    pub fn ctrl(c: char) -> Msg {
        Msg::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    pub fn editor(app: &mut App, body: &str) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("editor.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        app.settings.editor = Some(format!("sh {}", path.display()));
        dir
    }

    fn until_screen(app: &mut App, wanted: &str) -> Vec<String> {
        for _ in 0..100 {
            let lines = screen(app, 60, 16);
            if lines.iter().any(|l| l.contains(wanted)) {
                return lines;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        panic!("{wanted:?} never appeared: {:?}", screen(app, 60, 16));
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn ctrl_g_from_normal_mode_opens_the_editor_in_the_lower_half() {
        let mut app = fixture();
        let _dir = editor(&mut app, "printf 'IN-THE-EDITOR'; sleep 30");
        app.update(ctrl('g'));
        assert_eq!(app.mode(), "EDITOR");
        let lines = until_screen(&mut app, "IN-THE-EDITOR");
        let row = lines
            .iter()
            .position(|l| l.contains("IN-THE-EDITOR"))
            .unwrap();
        assert!(row >= lines.len() / 2, "{row}");
        assert!(
            lines.iter().take(row).any(|l| l.contains("hello")),
            "{lines:?}"
        );
        assert!(lines.last().unwrap().starts_with(" EDITOR"));
        let (pane_rows, _) = app.pane.as_ref().unwrap().size();
        assert_eq!(pane_rows, term::split(16).1);
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn the_conversation_stays_alive_above_the_editor() {
        let mut app = fixture();
        let _dir = editor(&mut app, "printf 'EDITING'; sleep 30");
        app.update(ctrl('g'));
        until_screen(&mut app, "EDITING");
        posted(
            &mut app,
            post(
                "p50",
                "c3",
                "u2",
                "arrived while editing",
                1_700_000_900_000,
            ),
            &[],
        );
        until_screen(&mut app, "arrived while editing");
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn keys_go_to_the_editor_and_its_result_lands_in_the_input_line() {
        let (mut app, mut rx) = fixture_rx();
        let _dir = editor(
            &mut app,
            "IFS= read -r a\nprintf '%s\\n%s\\n' \"$a\" second > \"$1\"",
        );
        app.update(ctrl('g'));
        press(&mut app, "q");
        assert!(!app.quit);
        press(&mut app, "ix");
        press_code(&mut app, KeyCode::Enter);
        while app.pane.is_some() {
            let msg = rx.blocking_recv().unwrap();
            app.update(msg);
        }
        assert_eq!(app.mode(), "INSERT");
        assert_eq!(app.compose.as_ref().unwrap().line.text(), "qix\nsecond");
        let row = screen(&mut app, 60, 12).iter().nth_back(1).unwrap().clone();
        assert_eq!(row, " > qix⏎second");
    }

    #[test]
    fn an_empty_result_from_normal_mode_cancels_and_keeps_the_draft() {
        let (mut app, mut rx) = fixture_rx();
        press(&mut app, "idraft");
        press_code(&mut app, KeyCode::Esc);
        let _dir = editor(&mut app, ": > \"$1\"");
        app.update(ctrl('g'));
        while app.pane.is_some() {
            let msg = rx.blocking_recv().unwrap();
            app.update(msg);
        }
        assert!(app.compose.is_none());
        press(&mut app, "i");
        assert_eq!(app.compose.as_ref().unwrap().line.text(), "draft");
        let (mut app, mut rx) = fixture_rx();
        let _dir = editor(&mut app, ": > \"$1\"");
        app.update(ctrl('g'));
        while app.pane.is_some() {
            let msg = rx.blocking_recv().unwrap();
            app.update(msg);
        }
        assert_eq!(app.mode(), "NORMAL");
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn a_failing_editor_says_so_and_a_normal_mode_start_is_undone() {
        let (mut app, mut rx) = fixture_rx();
        let _dir = editor(&mut app, "exit 1");
        app.update(ctrl('g'));
        while app.pane.is_some() {
            let msg = rx.blocking_recv().unwrap();
            app.update(msg);
        }
        assert!(
            app.notice.starts_with("Editor: the editor exited"),
            "{}",
            app.notice
        );
        assert_eq!(app.mode(), "NORMAL");
    }

    #[test]
    fn ctrl_keys_do_nothing_in_normal_mode_and_ctrl_g_needs_a_plain_screen() {
        let mut app = fixture();
        app.update(ctrl('d'));
        assert!(app.notice.is_empty() && app.awaiting.is_none());
        press(&mut app, "f");
        app.update(ctrl('g'));
        assert!(app.pane.is_none());
        assert_eq!(app.mode(), "SEARCH");
    }

    #[ignore = "needs a pseudo-terminal"]
    #[test]
    fn a_pasted_text_goes_to_the_editor_not_to_the_line() {
        let mut app = fixture();
        let _dir = editor(&mut app, "sleep 30");
        app.update(ctrl('g'));
        app.update(Msg::Paste("pasted".into()));
        assert_eq!(app.compose.as_ref().unwrap().line.text(), "");
    }

    #[test]
    fn a_channel_the_server_counts_as_read_shows_no_separator_whatever_its_old_timestamps() {
        let mut app = fixture();
        app.servers[0].teams[0].chans.as_mut().unwrap()[1]
            .member
            .as_mut()
            .unwrap()
            .msg_count = 2;
        enter_town_square(&mut app);
        assert!(!text(&mut app).contains("new messages"));
        assert_eq!((app.cursor, app.browsing), (1, false));
    }

    #[test]
    fn the_separator_stays_on_its_post_while_new_posts_arrive() {
        let mut app = fixture();
        enter_town_square(&mut app);
        for (n, id) in ["n1", "n2"].into_iter().enumerate() {
            let ms = 1_700_000_100_000 + n as i64 * 1000;
            posted(&mut app, post(id, "c1", "u2", "more", ms), &[]);
        }
        let shown = text(&mut app);
        assert_eq!(shown.matches("new messages").count(), 1, "{shown}");
        let lines: Vec<&str> = shown.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.contains("new messages"))
            .unwrap();
        assert!(lines[at + 1].contains("hi"), "{shown}");
    }

    #[test]
    fn the_picker_closes_on_esc_and_x_does_nothing_without_a_message_and_x_repeats_nothing_at_first(
    ) {
        let mut app = fixture();
        press(&mut app, "x");
        press_code(&mut app, KeyCode::Esc);
        assert_eq!(app.mode(), "NORMAL");
        press(&mut app, "X");
        assert_eq!(app.notice, "No recent reaction: pick one with x");
        press(&mut app, "J");
        press(&mut app, "J");
        press(&mut app, "x");
        assert_eq!(app.mode(), "NORMAL");
        let bottom = screen(&mut app, 80, 12).pop().unwrap();
        assert!(!bottom.starts_with(" REACT"));
    }

    #[test]
    fn y_and_capital_y_copy_a_message_or_a_range_through_the_register_and_the_clipboard() {
        let mut app = fixture();
        press(&mut app, "y");
        assert_eq!(app.register, "hi");
        assert_eq!(app.clipboard, [clipboard::osc52("hi")]);
        assert_eq!(app.notice, "Copied 1 message");
        let time = app.messages().unwrap()[1].at.format("%H:%M").to_string();
        press(&mut app, "Y");
        assert_eq!(app.register, format!("[{time}] ana: hi"));
        (app.cursor, app.browsing) = (0, true);
        press(&mut app, "Vl");
        assert_eq!(app.mode(), "VISUAL");
        press(&mut app, "y");
        assert_eq!(app.register, "hello :tada:\nhi");
        assert_eq!(app.notice, "Copied 2 messages");
        assert_eq!(app.mode(), "NORMAL");
        press(&mut app, "Vl");
        press_code(&mut app, KeyCode::Esc);
        assert_eq!(app.mode(), "NORMAL");
    }

    #[test]
    fn p_puts_the_register_in_the_input_and_complains_when_it_is_empty() {
        let mut app = fixture();
        press(&mut app, "p");
        assert_eq!(app.notice, "Nothing to paste: copy a message with y first");
        assert!(app.compose.is_none());
        (app.cursor, app.browsing) = (0, true);
        press(&mut app, "Vly");
        press(&mut app, "p");
        assert_eq!(app.mode(), "INSERT");
        assert_eq!(
            app.compose.as_ref().unwrap().line.text(),
            "hello :tada:\nhi"
        );
    }

    #[test]
    fn m_shows_scrolls_unfocuses_and_hides_the_column_and_refuses_a_narrow_screen() {
        let mut app = fixture();
        screen(&mut app, 60, 14);
        press(&mut app, "m");
        assert_eq!(app.notice, "Screen too narrow for the users list");
        assert!(!app.users.shown);
        screen(&mut app, 100, 14);
        press(&mut app, "m");
        assert!(app.users.shown && app.users.focus);
        assert_eq!(app.mode(), "USERS");
        press(&mut app, "jj");
        assert_eq!(app.users.top, 2);
        press(&mut app, "q");
        assert_eq!(app.mode(), "NORMAL");
        assert!(app.users.shown && !app.quit);
        press(&mut app, "mm");
        assert!(!app.users.shown);
    }

    #[test]
    fn the_column_lists_who_is_around_first_and_follows_status_events() {
        let mut app = fixture();
        let channel = ChannelId("c3".into());
        screen(&mut app, 100, 14);
        press(&mut app, "m");
        app.servers[0].members.insert(
            channel.clone(),
            vec![UserId("u1".into()), UserId("u2".into())],
        );
        app.servers[0]
            .status
            .insert(UserId("u2".into()), Status::Online);
        let shown = screen(&mut app, 100, 14).join("\n");
        assert!(
            shown.contains("present — 1") && shown.contains("● ana"),
            "{shown}"
        );
        assert!(
            shown.contains("absent — 1") && shown.contains("○ paul"),
            "{shown}"
        );
        assert!(shown.contains("hello"), "{shown}");
        app.apply_event(
            0,
            Event::Status {
                user: UserId("u1".into()),
                status: Status::Away,
            },
        );
        let shown = screen(&mut app, 100, 14).join("\n");
        assert!(
            shown.contains("present — 2") && shown.contains("● paul"),
            "{shown}"
        );
        let row = shown.lines().find(|l| l.contains("hello")).unwrap();
        assert!(row.contains("│") || row.width() <= 100, "{row}");
    }

    fn program(dir: &tempfile::TempDir, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        format!("sh {}", path.display())
    }

    fn wait_for_file(path: &std::path::Path) -> String {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if !text.is_empty() {
                    return text;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("{} never appeared", path.display());
    }

    fn message_with(app: &mut App, id: &str, text: &str, files: &[(&str, &str, &str)]) {
        let mut post = post(id, "c3", "u2", text, 1_700_000_800_000);
        let files: Vec<serde_json::Value> = files
            .iter()
            .map(|(fid, name, mime)| serde_json::json!({"id": fid, "name": name, "mime_type": mime, "size": 4}))
            .collect();
        post.metadata = serde_json::from_value(serde_json::json!({"files": files})).unwrap();
        posted(app, post, &[]);
    }

    #[test]
    fn o_opens_a_single_link_in_the_browser_and_says_when_there_is_nothing() {
        let mut app = fixture();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        app.settings.open.browser = program(
            &dir,
            "browser.sh",
            &format!("printf '%s' \"$1\" > {}", out.display()),
        );
        (app.cursor, app.browsing) = (0, true);
        press(&mut app, "o");
        assert_eq!(app.notice, "Nothing to open");
        message_with(&mut app, "p80", "read https://x.org/page, then reply", &[]);
        app.cursor = app.count() - 1;
        press(&mut app, "o");
        assert_eq!(wait_for_file(&out), "https://x.org/page");
        assert_eq!(app.notice, "Opened https://x.org/page");
    }

    #[test]
    fn several_things_make_a_picker_and_a_digit_chooses() {
        let mut app = fixture();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        app.settings.open.browser = program(
            &dir,
            "browser.sh",
            &format!("printf '%s' \"$1\" > {}", out.display()),
        );
        message_with(&mut app, "p81", "one https://a.org and https://b.org", &[]);
        press(&mut app, "o");
        assert_eq!(app.mode(), "OPEN");
        let bottom = screen(&mut app, 100, 12).pop().unwrap();
        assert!(
            bottom.starts_with(" OPEN  1 https://a.org") && bottom.contains("2 https://b.org"),
            "{bottom:?}"
        );
        press(&mut app, "z");
        assert_eq!(app.mode(), "OPEN");
        press(&mut app, "2");
        assert_eq!(app.mode(), "NORMAL");
        assert_eq!(wait_for_file(&out), "https://b.org");
        press(&mut app, "o");
        press_code(&mut app, KeyCode::Esc);
        assert_eq!(app.mode(), "NORMAL");
    }

    #[test]
    fn reacting_turns_the_reaction_display_on() {
        let mut app = fixture();
        assert!(!app.reactions);
        app.loaded(
            0,
            Loaded::Reacted(Reaction {
                user_id: UserId("u1".into()),
                post_id: PostId("p2".into()),
                emoji_name: "heart".into(),
            }),
        );
        assert!(app.reactions);
        press(&mut app, "a");
        assert!(!app.reactions);
        app.loaded(
            0,
            Loaded::Unreacted(Reaction {
                user_id: UserId("u1".into()),
                post_id: PostId("p2".into()),
                emoji_name: "heart".into(),
            }),
        );
        assert!(app.reactions);
    }

    #[test]
    fn yank_hands_the_text_to_the_clipboard_command_and_reports_its_failure() {
        let (mut app, mut rx) = fixture_rx();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("copied");
        app.settings.copy = Some(format!("cat > {}", out.display()));
        press(&mut app, "y");
        assert_eq!(wait_for_file(&out), "hi");
        app.settings.copy = Some("exit 2".into());
        press(&mut app, "y");
        let msg = rx.blocking_recv().unwrap();
        app.update(msg);
        assert!(
            app.notice.starts_with("Copy failed: exit 2 exited"),
            "{}",
            app.notice
        );
    }
}
