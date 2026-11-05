//! Matrix client with a persistent session, end-to-end encryption and a cache of what it has read.
//!
//! Everything it does is a read except `view`, `create_post`, `edit_post`, `delete_post`,
//! `react`, `unreact`, `create_direct`, `join`, `leave`, `set_status`, `pin` and `unpin`.

/// Account actions: recovery key, sessions, password and trust.
pub mod account;
/// Reading of Matrix events.
pub mod convert;
mod oidc;
/// Creating spaces and moving rooms in and out of them.
pub mod space;
/// The types shared with the rest of the program.
pub mod types;
/// Verification of sessions by comparing emojis.
pub mod verify;

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError, RwLock};
use std::time::Duration;

use chrono::Utc;
use directories::ProjectDirs;
use futures_util::future::join_all;
use futures_util::stream::{self, StreamExt};
use matrix_sdk::attachment::{
    AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo, BaseVideoInfo,
};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::encryption::recovery::RecoveryError;
use matrix_sdk::encryption::secret_storage::SecretStorageError;
use matrix_sdk::media::{MediaError, MediaFormat, MediaRequestParameters};
use matrix_sdk::notification_settings::RoomNotificationMode;
use matrix_sdk::room::{MessagesOptions, Room};
use matrix_sdk::ruma::api::client::presence::{get_presence, set_presence};
use matrix_sdk::ruma::api::client::room::create_room;
use matrix_sdk::ruma::api::error::{Error as ApiError, ErrorBody, ErrorKind, StandardErrorBody};
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::relation::Annotation;
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::events::room::message::{
    AddMentions, ForwardThread, LocationMessageEventContent, MessageType,
    OriginalSyncRoomMessageEvent, Relation, ReplacementMetadata, ReplyMetadata, ReplyWithinThread,
    RoomMessageEventContent,
};
use matrix_sdk::ruma::events::room::pinned_events::RoomPinnedEventsEventContent;
use matrix_sdk::ruma::events::{AnyInitialStateEvent, InitialStateEvent};
use matrix_sdk::ruma::matrix_uri::MatrixId;
use matrix_sdk::ruma::presence::PresenceState;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{
    uint, EventId, MatrixToUri, MatrixUri, OwnedEventId, OwnedUserId, RoomId, UInt,
    UserId as MatrixUserId,
};
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::{RoomMemberships, RoomState};
use serde_json::Value;
use thiserror::Error;

use crate::config::Credentials;
use types::{
    Channel, ChannelId, ChannelKind, ChannelMember, Metadata, Post, PostId, PostList, Reaction,
    Status, Team, TeamId, User, UserId, UserStatus,
};

const HOME: &str = "home";
const NEWEST_PAGES: usize = 8;

/// How many pages of history a search reads, and how many messages a page holds.
const SEARCH_PAGES: usize = 20;
const SEARCH_PAGE: u32 = 100;

/// How many hits end a search early.
const SEARCH_HITS: usize = 50;

/// How many rooms are read from the server at the same time when a team loads.
const LOAD_AT_ONCE: usize = 8;

/// Everything that can go wrong talking to a server.
#[derive(Debug, Error)]
pub enum Error {
    /// The server refused the login or password.
    #[error("login refused: {0}")]
    Login(String),
    /// The server could not be reached.
    #[error("server unreachable: {0}")]
    Unreachable(String),
    /// The server or the local store failed or answered with an error.
    #[error("{0}")]
    Server(String),
}

fn failed(e: impl Display) -> Error {
    Error::Server(e.to_string())
}

fn recovery_failure(e: RecoveryError) -> Error {
    match e {
        RecoveryError::SecretStorage(SecretStorageError::MissingKeyInfo { .. }) => {
            Error::Server("This account has no recovery key".into())
        }
        RecoveryError::SecretStorage(SecretStorageError::SecretStorageKey(_)) => {
            Error::Server("This is not the recovery key of this account".into())
        }
        e => failed(e),
    }
}

fn upload_failure(e: matrix_sdk::Error) -> Error {
    match e {
        matrix_sdk::Error::Media(MediaError::MediaTooLargeToUpload { max, .. }) => {
            Error::Server(format!("File too large: the server accepts {max} bytes"))
        }
        e => failed(e),
    }
}

/// The media type a file name suggests, from a short list of extensions.
fn mime_of(path: &Path) -> mime::Mime {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let name = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => "",
    };
    name.parse().unwrap_or(mime::APPLICATION_OCTET_STREAM)
}

impl From<matrix_sdk::HttpError> for Error {
    fn from(e: matrix_sdk::HttpError) -> Self {
        match e {
            matrix_sdk::HttpError::Reqwest(e) => Self::Unreachable(e.to_string()),
            e => match e.as_client_api_error() {
                Some(refusal) => Self::Server(server_message(refusal)),
                None => failed(e),
            },
        }
    }
}

fn notify_of(
    mode: Option<RoomNotificationMode>,
    direct: bool,
    mentions: i64,
) -> (&'static str, i64) {
    match (mode, direct) {
        (Some(RoomNotificationMode::Mute), _)
        | (Some(RoomNotificationMode::MentionsAndKeywordsOnly), true) => ("mention", 0),
        (Some(RoomNotificationMode::MentionsAndKeywordsOnly), false) => ("mention", mentions),
        _ => ("all", mentions),
    }
}

fn server_message(refusal: &ApiError) -> String {
    match &refusal.body {
        ErrorBody::Standard(StandardErrorBody {
            kind: ErrorKind::Forbidden,
            ..
        }) => "You don't have the rights to do that here".into(),
        ErrorBody::Standard(StandardErrorBody { kind, message, .. }) => {
            let errcode = kind.errcode().to_string();
            let reason = message
                .strip_prefix(&format!("{errcode}: "))
                .unwrap_or(message);
            match reason.is_empty() {
                true => errcode,
                false => reason.to_owned(),
            }
        }
        _ => refusal.to_string(),
    }
}

impl From<matrix_sdk::Error> for Error {
    fn from(e: matrix_sdk::Error) -> Self {
        match e {
            matrix_sdk::Error::Http(e) => (*e).into(),
            e => failed(e),
        }
    }
}

macro_rules! failures {
    ($($error:ty),*) => {
        $(impl From<$error> for Error {
            fn from(e: $error) -> Self {
                failed(e)
            }
        })*
    };
}

impl From<matrix_sdk::ClientBuildError> for Error {
    fn from(e: matrix_sdk::ClientBuildError) -> Self {
        match e {
            matrix_sdk::ClientBuildError::Http(e) => e.into(),
            e => failed(e),
        }
    }
}

failures!(matrix_sdk::IdParseError, std::io::Error, serde_json::Error);

/// What the cache knows about a message beyond the message itself.
#[derive(Debug, Default)]
struct Cache {
    senders: HashMap<PostId, UserId>,
    edits: HashMap<PostId, (UserId, Post)>,
    reactions: HashMap<PostId, Vec<(PostId, Reaction)>>,
    tokens: HashMap<PostId, String>,
    last: HashMap<ChannelId, (i64, PostId)>,
    names: HashMap<UserId, String>,
    read: HashMap<ChannelId, ReadState>,
    seen: HashMap<PostId, i64>,
}

/// How much of a room the reader has not read, and when they last did.
#[derive(Debug, Clone, Copy, Default)]
struct ReadState {
    unread: i64,
    mentions: i64,
    viewed: i64,
}

impl Cache {
    fn apply(&mut self, room: &ChannelId, item: convert::Item) -> Option<Change> {
        match item {
            convert::Item::Post(mut post) => {
                let at = post.create_at;
                self.senders.insert(post.id.clone(), post.user_id.clone());
                if self.last.get(room).is_none_or(|(last, _)| *last <= at) && post.kind.is_empty() {
                    self.last.insert(room.clone(), (at, post.id.clone()));
                }
                if let Some((sender, edit)) = self.edits.get(&post.id) {
                    if *sender == post.user_id {
                        post.message.clone_from(&edit.message);
                        post.edit_at = edit.edit_at;
                    }
                }
                let reactions: Vec<Reaction> = self
                    .reactions
                    .get(&post.id)
                    .into_iter()
                    .flatten()
                    .map(|(_, r)| r.clone())
                    .collect();
                post.metadata = Metadata {
                    reactions: Some(reactions).filter(|r| !r.is_empty()),
                    files: post.metadata.files,
                };
                Some(Change::Posted(post))
            }
            convert::Item::Edit { sender, post } => {
                if self.senders.get(&post.id).is_some_and(|s| *s != sender) {
                    return None;
                }
                let newer = self
                    .edits
                    .get(&post.id)
                    .is_none_or(|(_, old)| old.edit_at <= post.edit_at);
                if newer {
                    self.edits.insert(post.id.clone(), (sender, post.clone()));
                }
                Some(Change::Edited(post))
            }
            convert::Item::React { event, reaction } => {
                let list = self.reactions.entry(reaction.post_id.clone()).or_default();
                if list.iter().any(|(id, _)| *id == event) {
                    return None;
                }
                list.push((event, reaction.clone()));
                Some(Change::Reacted(reaction))
            }
            convert::Item::Redact(target) => {
                let found = self.reactions.values_mut().find_map(|list| {
                    let i = list.iter().position(|(id, _)| *id == target)?;
                    Some(list.remove(i).1)
                });
                if let Some(reaction) = found {
                    return Some(Change::Unreacted(reaction));
                }
                self.edits.remove(&target);
                self.senders.remove(&target);
                Some(Change::Deleted(Post {
                    id: target,
                    channel_id: room.clone(),
                    ..Post::default()
                }))
            }
        }
    }
}

/// A change to a room, as one event tells it.
#[derive(Debug, Clone)]
pub enum Change {
    Posted(Post),
    Edited(Post),
    Deleted(Post),
    Reacted(Reaction),
    Unreacted(Reaction),
}

/// A connection to one server.
pub struct Client {
    sdk: RwLock<matrix_sdk::Client>,
    me: UserId,
    url: String,
    credentials: Credentials,
    dir: PathBuf,
    cache: Mutex<Cache>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("me", &self.me).finish()
    }
}

fn data_dir(name: &str) -> Result<PathBuf, Error> {
    let dirs = ProjectDirs::from("", "", "mx").ok_or_else(|| failed("no home directory"))?;
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(dirs.data_dir().join(safe))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    Ok(file.write_all(bytes)?)
}

fn room_id(id: &ChannelId) -> Result<matrix_sdk::ruma::OwnedRoomId, Error> {
    Ok(RoomId::parse(&id.0)?)
}

fn event_id(id: &PostId) -> Result<OwnedEventId, Error> {
    Ok(EventId::parse(&id.0)?)
}

fn encrypted() -> Raw<AnyInitialStateEvent> {
    InitialStateEvent::with_empty_state_key(RoomEncryptionEventContent::with_recommended_defaults())
        .to_raw_any()
}

fn user_id(id: &UserId) -> Result<OwnedUserId, Error> {
    Ok(MatrixUserId::parse(&id.0)?)
}

fn unlinked(name: &str) -> String {
    let id = MatrixToUri::parse(name)
        .map(|link| link.id().clone())
        .or_else(|_| MatrixUri::parse(name).map(|link| link.id().clone()));
    match id {
        Ok(MatrixId::Room(id)) => id.to_string(),
        Ok(MatrixId::RoomAlias(alias)) => alias.to_string(),
        _ => name.to_owned(),
    }
}

/// The JSON of a raw event, when it is valid.
pub fn json_of<T>(raw: &matrix_sdk::ruma::serde::Raw<T>) -> Option<Value> {
    serde_json::from_str(raw.json().get()).ok()
}

/// The way every synchronization asks, without the member lists of big rooms.
pub fn sync_settings() -> SyncSettings {
    SyncSettings::default().filter(
        matrix_sdk::ruma::api::client::sync::sync_events::v3::Filter::FilterDefinition(
            matrix_sdk::ruma::api::client::filter::FilterDefinition::with_lazy_loading(),
        ),
    )
}

async fn build(dir: &Path, url: &str) -> Result<matrix_sdk::Client, Error> {
    Ok(matrix_sdk::Client::builder()
        .server_name_or_homeserver_url(url)
        .sqlite_store(dir.join("store"), None)
        .handle_refresh_tokens()
        .build()
        .await?)
}

async fn restored(dir: &Path, url: &str) -> Result<Option<matrix_sdk::Client>, Error> {
    let Ok(bytes) = std::fs::read(dir.join("session.json")) else {
        return Ok(None);
    };
    let matrix = serde_json::from_slice::<MatrixSession>(&bytes).ok();
    let sdk = build(dir, url).await?;
    match matrix {
        Some(session) => {
            sdk.matrix_auth()
                .restore_session(session, RoomLoadSettings::default())
                .await?;
        }
        None => match oidc::restore(&sdk, &bytes).await {
            Some(restored) => restored?,
            None => return Ok(None),
        },
    }
    oidc::keep_saved(&sdk, &dir.join("session.json"));
    match sdk.whoami().await {
        Ok(_) => Ok(Some(sdk)),
        Err(e)
            if matches!(
                e.client_api_error_kind(),
                Some(matrix_sdk::ruma::api::error::ErrorKind::UnknownToken { .. })
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

const SSO_WAIT: Duration = Duration::from_secs(300);

async fn sso_login(
    auth: &matrix_sdk::authentication::matrix::MatrixAuth,
    browser: &str,
) -> matrix_sdk::Result<()> {
    let (failure, failed_to_open) = tokio::sync::oneshot::channel();
    let program = browser.to_owned();
    let login = auth
        .login_sso(|address| async move {
            let hint = address.clone();
            crate::open::start(&program, &address, move |outcome| {
                if let Some(why) = outcome {
                    let _ = failure.send(format!("{why}; open {hint} yourself"));
                }
            })?;
            Ok(())
        })
        .initial_device_display_name("mx")
        .send();
    let waited = tokio::time::timeout(SSO_WAIT, async {
        tokio::select! {
            sent = login => sent.map(drop),
            Ok(why) = failed_to_open => Err(matrix_sdk::Error::UnknownError(why.into())),
        }
    })
    .await;
    waited.unwrap_or_else(|_| {
        Err(matrix_sdk::Error::UnknownError(
            "Timed out waiting for the browser".into(),
        ))
    })
}

async fn fresh_login(
    dir: &Path,
    url: &str,
    credentials: &Credentials,
) -> Result<matrix_sdk::Client, Error> {
    let _ = std::fs::remove_dir_all(dir.join("store"));
    let sdk = build(dir, url).await?;
    let auth = sdk.matrix_auth();
    let sent = match credentials {
        Credentials::Password { login, password } => auth
            .login_username(login, password)
            .initial_device_display_name("mx")
            .send()
            .await
            .map(drop),
        Credentials::Sso { browser } => sso_login(&auth, browser).await,
        Credentials::Oidc { browser } => oidc::login(&sdk, browser).await,
    };
    if let Err(e) = sent {
        return Err(match e {
            matrix_sdk::Error::Http(e) => match *e {
                matrix_sdk::HttpError::Reqwest(e) => Error::Unreachable(e.to_string()),
                e => Error::Login(e.to_string()),
            },
            e => failed(e),
        });
    }
    let session = match credentials {
        Credentials::Oidc { .. } => oidc::saved(&sdk)?,
        _ => serde_json::to_vec(
            &auth
                .session()
                .ok_or_else(|| failed("the server sent no session"))?,
        )?,
    };
    write_private(&dir.join("session.json"), &session)?;
    oidc::keep_saved(&sdk, &dir.join("session.json"));
    Ok(sdk)
}

impl Client {
    /// Opens a session, restoring the saved one or logging in with the password.
    ///
    /// # Errors
    /// Fails when the server is unreachable or refuses the login.
    pub async fn connect(name: &str, url: &str, credentials: Credentials) -> Result<Self, Error> {
        let dir = data_dir(name)?;
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let sdk = restored(&dir, url).await?;
        let sdk = match sdk {
            Some(sdk) => sdk,
            None => fresh_login(&dir, url, &credentials).await?,
        };
        let me = sdk
            .user_id()
            .ok_or_else(|| failed("the server sent no user id"))?
            .to_string();
        sdk.sync_once(sync_settings().timeout(Duration::ZERO))
            .await?;
        Ok(Self {
            sdk: RwLock::new(sdk),
            me: UserId(me),
            url: url.to_owned(),
            credentials,
            dir,
            cache: Mutex::new(Cache::default()),
        })
    }

    /// Logs in again with the password on a new device, or asks to restart for SSO, after the server dropped the session.
    ///
    /// # Errors
    /// Fails when the server refuses the login or cannot be reached.
    pub async fn relogin(&self) -> Result<(), Error> {
        if matches!(
            self.credentials,
            Credentials::Sso { .. } | Credentials::Oidc { .. }
        ) {
            return Err(Error::Login(
                "Session expired: restart mx to sign in again".into(),
            ));
        }
        let fresh = fresh_login(&self.dir, &self.url, &self.credentials).await?;
        *self.sdk.write().unwrap_or_else(PoisonError::into_inner) = fresh;
        Ok(())
    }

    /// The underlying client, for the synchronization loop.
    pub fn sdk(&self) -> matrix_sdk::Client {
        self.sdk
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The reader's own account.
    pub fn myself(&self) -> &UserId {
        &self.me
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn room(&self, id: &ChannelId) -> Result<Room, Error> {
        self.sdk()
            .get_room(&room_id(id)?)
            .ok_or_else(|| failed("unknown room"))
    }

    async fn profile_name(&self, id: &UserId) -> String {
        if let Some(name) = self.cache().names.get(id) {
            return name.clone();
        }
        let name = match MatrixUserId::parse(&id.0) {
            Ok(user) => self
                .sdk()
                .account()
                .fetch_user_profile_of(&user)
                .await
                .ok()
                .and_then(|p| p.data.get("displayname")?.as_str().map(str::to_owned)),
            Err(_) => None,
        }
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| convert::localpart(&id.0).to_owned());
        self.cache().names.insert(id.clone(), name.clone());
        name
    }

    /// The reader's own account.
    ///
    /// # Errors
    /// Fails when the reader's id is not valid.
    pub async fn me(&self) -> Result<User, Error> {
        Ok(User {
            username: self.profile_name(&self.me).await,
            id: self.me.clone(),
        })
    }

    async fn children(&self) -> HashMap<String, HashSet<String>> {
        let mut spaces = HashMap::new();
        for space in self
            .sdk()
            .joined_rooms()
            .into_iter()
            .filter(|r| r.is_space())
        {
            let events = space
                .get_state_events(matrix_sdk::ruma::events::StateEventType::SpaceChild)
                .await
                .unwrap_or_default();
            let children = events
                .iter()
                .filter_map(|e| match e {
                    matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState::Sync(raw) => {
                        json_of(raw)
                    }
                    matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState::Stripped(
                        raw,
                    ) => json_of(raw),
                })
                .filter(|e| {
                    e.pointer("/content/via")
                        .and_then(Value::as_array)
                        .is_some_and(|via| !via.is_empty())
                })
                .filter_map(|e| e.get("state_key")?.as_str().map(str::to_owned))
                .collect();
            spaces.insert(space.room_id().to_string(), children);
        }
        spaces
    }

    /// The teams the reader belongs to: the spaces, and Home for the rooms in none.
    ///
    /// # Errors
    /// Never fails for now; kept fallible like the other reads.
    pub async fn teams(&self) -> Result<Vec<Team>, Error> {
        let mut teams = vec![Team {
            id: TeamId(HOME.into()),
            name: HOME.into(),
            display_name: "Home".into(),
        }];
        for space in self
            .sdk()
            .joined_rooms()
            .into_iter()
            .filter(|r| r.is_space())
        {
            let name = space
                .display_name()
                .await
                .map_or_else(|_| space.room_id().to_string(), |n| n.to_string());
            teams.push(Team {
                id: TeamId(space.room_id().to_string()),
                name: space
                    .canonical_alias()
                    .map_or_else(|| name.clone(), |a| a.to_string()),
                display_name: name,
            });
        }
        Ok(teams)
    }

    async fn rooms_of(&self, team: &TeamId) -> Vec<Room> {
        let spaces = self.children().await;
        let mut rooms = Vec::new();
        for room in self
            .sdk()
            .joined_rooms()
            .into_iter()
            .filter(|r| !r.is_space())
        {
            let id = room.room_id().to_string();
            let inside = match team.0.as_str() {
                HOME => !spaces.values().any(|c| c.contains(&id)),
                space => spaces.get(space).is_some_and(|c| c.contains(&id)),
            };
            if inside || room.is_direct().await.unwrap_or(false) {
                rooms.push(room);
            }
        }
        if team.0 == HOME {
            rooms.extend(self.sdk().invited_rooms());
        }
        rooms
    }

    async fn group_name(&self, room: &Room) -> String {
        let Ok(members) = room.members(RoomMemberships::ACTIVE).await else {
            return String::new();
        };
        let mut names: Vec<String> = members
            .iter()
            .filter(|m| m.user_id().as_str() != self.me.0)
            .map(|m| {
                m.display_name().filter(|n| !n.is_empty()).map_or_else(
                    || convert::localpart(m.user_id().as_str()).to_owned(),
                    str::to_owned,
                )
            })
            .collect();
        names.sort();
        names.join(", ")
    }

    async fn channel_of(&self, room: &Room) -> Channel {
        let direct = room.is_direct().await.unwrap_or(false);
        let targets = room.direct_targets();
        let peer = (direct && targets.len() == 1)
            .then(|| targets.iter().next().and_then(|t| t.as_user_id()))
            .flatten()
            .map(|u| UserId(u.to_string()));
        let kind = match (direct, peer.is_some(), room.is_public()) {
            (true, true, _) => ChannelKind::Direct,
            (true, false, _) => ChannelKind::Group,
            (false, _, Some(true)) => ChannelKind::Public,
            _ => ChannelKind::Private,
        };
        let id = ChannelId(room.room_id().to_string());
        let invited = room.state() == RoomState::Invited;
        let inviter = match invited {
            true => room.invite_details().await.ok().and_then(|d| d.inviter),
            false => None,
        };
        let (last, unread) = match invited {
            true => (Utc::now().timestamp_millis(), 0),
            false => (
                self.latest(room).await.0,
                self.read_state(room, kind).await.unread,
            ),
        };
        let named = match (kind, invited) {
            (ChannelKind::Group, false) => self.group_name(room).await,
            _ => String::new(),
        };
        let display_name = match named.is_empty() {
            true => room
                .display_name()
                .await
                .map(|n| n.to_string())
                .unwrap_or_default(),
            false => named,
        };
        Channel {
            display_name,
            name: room
                .canonical_alias()
                .map_or_else(|| id.0.clone(), |a| a.to_string()),
            kind,
            last_post_at: last,
            total_msg_count: unread,
            delete_at: 0,
            header: room.topic().unwrap_or_default(),
            purpose: String::new(),
            peer,
            invited: invited
                .then(|| UserId(inviter.map_or_else(String::new, |m| m.user_id().to_string()))),
            id,
        }
    }

    async fn viewed_at(&self, room: &Room) -> i64 {
        let Ok(me) = user_id(&self.me) else {
            return 0;
        };
        let receipt = room
            .load_user_receipt(
                matrix_sdk::ruma::events::receipt::ReceiptType::Read,
                &matrix_sdk::ruma::events::receipt::ReceiptThread::Unthreaded,
                &me,
            )
            .await
            .ok()
            .flatten();
        let Some((event, _)) = receipt else {
            return 0;
        };
        let key = PostId(event.to_string());
        if let Some(at) = self.cache().seen.get(&key) {
            return *at;
        }
        let at = room
            .load_or_fetch_event(&event, None)
            .await
            .ok()
            .and_then(|e| json_of(e.raw())?.get("origin_server_ts")?.as_i64())
            .unwrap_or_default();
        self.cache().seen.insert(key, at);
        at
    }

    async fn latest(&self, room: &Room) -> (i64, Option<PostId>) {
        let id = ChannelId(room.room_id().to_string());
        let known = self.cache().last.get(&id).cloned();
        if known.is_none() {
            let _ = self.posts(&id, None, 60).await;
        }
        let last = self.cache().last.get(&id).cloned();
        last.map_or((0, None), |(at, event)| (at, Some(event)))
    }

    async fn read_state(&self, room: &Room, kind: ChannelKind) -> ReadState {
        let id = ChannelId(room.room_id().to_string());
        let viewed = self.viewed_at(room).await;
        let mut state = ReadState {
            viewed,
            ..ReadState::default()
        };
        if self.latest(room).await.0 > viewed {
            let list = self.posts(&id, None, 60).await.unwrap_or_default();
            let fresh = list
                .ordered()
                .filter(|p| p.create_at > viewed)
                .filter(|p| p.user_id != self.me && p.kind.is_empty());
            let (unread, mentioned) =
                fresh.fold((0, 0), |(n, m), p| (n + 1, m + i64::from(p.mentioned)));
            let direct = matches!(kind, ChannelKind::Direct | ChannelKind::Group);
            state.unread = unread;
            state.mentions = if direct { unread } else { mentioned };
        }
        self.cache().read.insert(id, state);
        state
    }

    /// The reader's channels in a team, including direct and group messages.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn channels(&self, team: &TeamId) -> Result<Vec<Channel>, Error> {
        let rooms = self.rooms_of(team).await;
        Ok(stream::iter(rooms)
            .map(|r| async move { self.channel_of(&r).await })
            .buffered(LOAD_AT_ONCE)
            .collect()
            .await)
    }

    async fn member_of(&self, room: &Room) -> ChannelMember {
        let id = ChannelId(room.room_id().to_string());
        if room.state() == RoomState::Invited {
            return ChannelMember {
                channel_id: id,
                msg_count: 0,
                mention_count: 1,
                last_viewed_at: 0,
                notify_props: types::NotifyProps {
                    mark_unread: "all".into(),
                },
            };
        }
        let known = self.cache().read.get(&id).copied();
        let state = match known {
            Some(state) => state,
            None => {
                let kind = self.channel_of(room).await.kind;
                self.read_state(room, kind).await
            }
        };
        let direct = room.is_direct().await.unwrap_or(false);
        let (mark_unread, mention_count) =
            notify_of(room.notification_mode().await, direct, state.mentions);
        ChannelMember {
            channel_id: id,
            msg_count: 0,
            mention_count,
            last_viewed_at: state.viewed,
            notify_props: types::NotifyProps {
                mark_unread: mark_unread.into(),
            },
        }
    }

    /// The reader's memberships in a team, which hold the unread counts.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn members(&self, team: &TeamId) -> Result<Vec<ChannelMember>, Error> {
        let rooms = self.rooms_of(team).await;
        Ok(stream::iter(rooms)
            .map(|r| async move { self.member_of(&r).await })
            .buffered(LOAD_AT_ONCE)
            .collect()
            .await)
    }

    /// Marks a channel as read up to its latest event.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn view(&self, channel: &ChannelId, _prev: Option<&ChannelId>) -> Result<(), Error> {
        use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
        use matrix_sdk::ruma::events::receipt::ReceiptThread;
        let room = self.room(channel)?;
        let Some(latest) = self.latest(&room).await.1 else {
            return Ok(());
        };
        let latest = event_id(&latest)?;
        room.send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, latest.clone())
            .await?;
        room.send_single_receipt(ReceiptType::FullyRead, ReceiptThread::Unthreaded, latest)
            .await?;
        Ok(())
    }

    /// Creates a message, as a reply to `root` when there is one.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn create_post(
        &self,
        channel: &ChannelId,
        message: &str,
        root: Option<&PostId>,
    ) -> Result<Post, Error> {
        let room = self.room(channel)?;
        let mut content = RoomMessageEventContent::text_markdown(message);
        let mut thread_root = None;
        if let Some(root) = root {
            let target = event_id(root)?;
            let event = room.load_or_fetch_event(&target, None).await?;
            let sender = event
                .raw()
                .get_field::<OwnedUserId>("sender")
                .map_err(failed)?
                .ok_or_else(|| failed("the message has no sender"))?;
            let thread = match event
                .raw()
                .deserialize_as_unchecked::<OriginalSyncRoomMessageEvent>()
                .map(|e| e.content.relates_to)
            {
                Ok(Some(Relation::Thread(t))) => Some(t),
                _ => None,
            };
            let metadata = ReplyMetadata::new(&target, &sender, thread.as_ref());
            content = match &thread {
                Some(t) => {
                    thread_root = Some(t.event_id.to_string());
                    content.make_for_thread(metadata, ReplyWithinThread::Yes, AddMentions::Yes)
                }
                None => content.make_reply_to(metadata, ForwardThread::No, AddMentions::Yes),
            };
        }
        let sent = room.send(content).await?;
        let post = Post {
            id: PostId(sent.response.event_id.to_string()),
            create_at: Utc::now().timestamp_millis(),
            user_id: self.me.clone(),
            channel_id: channel.clone(),
            root_id: thread_root
                .or_else(|| root.map(|r| r.0.clone()))
                .unwrap_or_default(),
            message: message.to_owned(),
            ..Post::default()
        };
        self.cache()
            .senders
            .insert(post.id.clone(), self.me.clone());
        Ok(post)
    }

    /// Sends a file or an image to a channel, read whole into memory.
    ///
    /// # Errors
    /// Fails when the file cannot be read, is over the size the server accepts, or on network errors.
    pub async fn upload(&self, channel: &ChannelId, path: &Path) -> Result<(), Error> {
        let room = self.room(channel)?;
        let data = tokio::fs::read(path)
            .await
            .map_err(|e| failed(format!("Cannot read {}: {e}", path.display())))?;
        let name = path
            .file_name()
            .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        let mime = mime_of(path);
        let size = UInt::try_from(data.len()).ok();
        let info = match mime.type_() {
            mime::IMAGE => AttachmentInfo::Image(BaseImageInfo {
                size,
                ..BaseImageInfo::default()
            }),
            mime::VIDEO => AttachmentInfo::Video(BaseVideoInfo {
                size,
                ..BaseVideoInfo::default()
            }),
            mime::AUDIO => AttachmentInfo::Audio(BaseAudioInfo {
                size,
                ..BaseAudioInfo::default()
            }),
            _ => AttachmentInfo::File(BaseFileInfo { size }),
        };
        let config = AttachmentConfig::new().info(info);
        room.send_attachment(name, &mime, data, config)
            .await
            .map_err(upload_failure)?;
        Ok(())
    }

    /// Replaces the text of one of the reader's messages.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn edit_post(
        &self,
        channel: &ChannelId,
        id: &PostId,
        message: &str,
    ) -> Result<Post, Error> {
        let room = self.room(channel)?;
        let content = RoomMessageEventContent::text_markdown(message)
            .make_replacement(ReplacementMetadata::new(event_id(id)?, None));
        room.send(content).await?;
        Ok(Post {
            id: id.clone(),
            channel_id: channel.clone(),
            message: message.to_owned(),
            edit_at: Utc::now().timestamp_millis(),
            ..Post::default()
        })
    }

    /// Deletes one of the reader's messages.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn delete_post(&self, channel: &ChannelId, id: &PostId) -> Result<(), Error> {
        self.room(channel)?
            .redact(&event_id(id)?, None, None)
            .await?;
        Ok(())
    }

    /// Adds the reader's reaction to a message.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn react(
        &self,
        channel: &ChannelId,
        post: &PostId,
        emoji: &str,
    ) -> Result<Reaction, Error> {
        let room = self.room(channel)?;
        let key = convert::emoji_key(emoji);
        let content = ReactionEventContent::new(Annotation::new(event_id(post)?, key));
        let sent = room.send(content).await?;
        let reaction = Reaction {
            user_id: self.me.clone(),
            post_id: post.clone(),
            emoji_name: emoji.to_owned(),
        };
        let event = PostId(sent.response.event_id.to_string());
        self.cache()
            .reactions
            .entry(post.clone())
            .or_default()
            .push((event, reaction.clone()));
        Ok(reaction)
    }

    /// Takes the reader's reaction off a message.
    ///
    /// # Errors
    /// Fails on network or server errors, and when the reaction is not known.
    pub async fn unreact(
        &self,
        channel: &ChannelId,
        post: &PostId,
        emoji: &str,
    ) -> Result<(), Error> {
        let found = {
            let mut cache = self.cache();
            let list = cache.reactions.entry(post.clone()).or_default();
            list.iter()
                .position(|(_, r)| r.user_id == self.me && r.emoji_name == emoji)
                .map(|i| list.remove(i).0)
        };
        let event = found.ok_or_else(|| failed("reaction not found"))?;
        self.room(channel)?
            .redact(&event_id(&event)?, None, None)
            .await?;
        Ok(())
    }

    /// Opens the direct conversation with a user, which posts nothing.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn create_direct(&self, other: &UserId) -> Result<Channel, Error> {
        let other = user_id(other)?;
        let room = match self.sdk().get_dm_room(&other) {
            Some(room) => room,
            None => self.sdk().create_dm(&other).await?,
        };
        Ok(self.channel_of(&room).await)
    }

    /// Opens a conversation with several users, which posts nothing.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn create_group(&self, others: &[UserId]) -> Result<Channel, Error> {
        let mut request = create_room::v3::Request::new();
        request.invite = others.iter().map(user_id).collect::<Result<_, _>>()?;
        request.is_direct = true;
        request.preset = Some(create_room::v3::RoomPreset::TrustedPrivateChat);
        request.initial_state = vec![encrypted()];
        let room = self.sdk().create_room(request).await?;
        Ok(self.channel_of(&room).await)
    }

    /// Creates a room that only invited users join, encrypted unless `public`.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn create_room(&self, name: &str, public: bool) -> Result<Channel, Error> {
        let mut request = create_room::v3::Request::new();
        request.name = Some(name.to_owned());
        if public {
            request.preset = Some(create_room::v3::RoomPreset::PublicChat);
        } else {
            request.preset = Some(create_room::v3::RoomPreset::PrivateChat);
            request.initial_state = vec![encrypted()];
        }
        let room = self.sdk().create_room(request).await?;
        Ok(self.channel_of(&room).await)
    }

    /// Invites a user into a room.
    ///
    /// # Errors
    /// Fails on network or server errors, and when the reader may not invite.
    pub async fn invite(&self, channel: &ChannelId, user: &UserId) -> Result<(), Error> {
        self.room(channel)?
            .invite_user_by_id(&user_id(user)?)
            .await?;
        Ok(())
    }

    /// Removes a user from a room, who may come back when invited.
    ///
    /// # Errors
    /// Fails on network or server errors, and when the reader may not kick.
    pub async fn kick(&self, channel: &ChannelId, user: &UserId) -> Result<(), Error> {
        self.room(channel)?.kick_user(&user_id(user)?, None).await?;
        Ok(())
    }

    /// Removes a user from a room and bars them from joining it.
    ///
    /// # Errors
    /// Fails on network or server errors, and when the reader may not ban.
    pub async fn ban(&self, channel: &ChannelId, user: &UserId) -> Result<(), Error> {
        self.room(channel)?.ban_user(&user_id(user)?, None).await?;
        Ok(())
    }

    /// Lets a banned user join a room again.
    ///
    /// # Errors
    /// Fails on network or server errors, and when the reader may not unban.
    pub async fn unban(&self, channel: &ChannelId, user: &UserId) -> Result<(), Error> {
        self.room(channel)?
            .unban_user(&user_id(user)?, None)
            .await?;
        Ok(())
    }

    /// Gives a room a new name.
    ///
    /// # Errors
    /// Fails on network or server errors, and when the reader may not rename.
    pub async fn rename(&self, channel: &ChannelId, name: &str) -> Result<(), Error> {
        self.room(channel)?.set_name(name.to_owned()).await?;
        Ok(())
    }

    /// Gives a room a new topic.
    ///
    /// # Errors
    /// Fails on network or server errors, and when the reader may not change it.
    pub async fn set_topic(&self, channel: &ChannelId, topic: &str) -> Result<(), Error> {
        self.room(channel)?.set_room_topic(topic).await?;
        Ok(())
    }

    /// Changes the name the reader goes by on the server.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn set_nick(&self, name: &str) -> Result<(), Error> {
        self.sdk().account().set_display_name(Some(name)).await?;
        self.cache().names.insert(self.me.clone(), name.to_owned());
        Ok(())
    }

    /// The messages of a room that hold some text, every message read, and how many were read.
    ///
    /// Both lists are newest first; the second one is what lets a hit be opened in place.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn search(
        &self,
        channel: &ChannelId,
        text: &str,
    ) -> Result<(PostList, PostList, usize), Error> {
        let room = self.room(channel)?;
        let wanted = text.to_lowercase();
        let me = self.me.0.clone();
        let mut list = PostList::default();
        let mut run = PostList::default();
        let mut read = 0;
        let mut from = None;
        for _ in 0..SEARCH_PAGES {
            let mut options = MessagesOptions::backward().from(from.as_deref());
            options.limit = SEARCH_PAGE.into();
            let page = room.messages(options).await?;
            for event in &page.chunk {
                let Some(Change::Posted(post)) =
                    json_of(event.raw()).and_then(|e| self.ingest(channel, &e, &me))
                else {
                    continue;
                };
                read += 1;
                if post.kind.is_empty() && post.message.to_lowercase().contains(&wanted) {
                    list.order.push(post.id.clone());
                    list.posts.insert(post.id.clone(), post.clone());
                }
                run.order.push(post.id.clone());
                run.posts.insert(post.id.clone(), post);
            }
            match page.end {
                Some(end) if list.order.len() < SEARCH_HITS => from = Some(end),
                _ => break,
            }
        }
        Ok((list, run, read))
    }

    /// Tells the server's moderators that a message is not acceptable.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn report(
        &self,
        channel: &ChannelId,
        post: &PostId,
        reason: &str,
    ) -> Result<(), Error> {
        let reason = Some(reason).filter(|r| !r.is_empty()).map(str::to_owned);
        self.room(channel)?
            .report_content(event_id(post)?, reason)
            .await?;
        Ok(())
    }

    /// Hides everything a user says from now on, in every room.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn ignore(&self, user: &UserId) -> Result<(), Error> {
        self.sdk().account().ignore_user(&user_id(user)?).await?;
        Ok(())
    }

    /// Shows a user again after `ignore`.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn unignore(&self, user: &UserId) -> Result<(), Error> {
        self.sdk().account().unignore_user(&user_id(user)?).await?;
        Ok(())
    }

    /// Sends a place as a message.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn send_location(
        &self,
        channel: &ChannelId,
        latitude: &str,
        longitude: &str,
    ) -> Result<(), Error> {
        let geo = format!("geo:{latitude},{longitude}");
        let place = LocationMessageEventContent::new(geo.clone(), geo);
        let content = RoomMessageEventContent::new(MessageType::Location(place));
        self.room(channel)?.send(content).await?;
        Ok(())
    }

    /// Unlocks the keys of older encrypted messages with the recovery key.
    ///
    /// # Errors
    /// Fails on network or server errors, and when the key is wrong.
    pub async fn recover(&self, key: &str) -> Result<(), Error> {
        let sdk = self.sdk();
        sdk.encryption()
            .recovery()
            .recover(key.trim())
            .await
            .map_err(recovery_failure)?;
        let backups = sdk.encryption().backups();
        for room in sdk.joined_rooms() {
            if room.encryption_state().is_encrypted() {
                let _ = backups.download_room_keys_for_room(room.room_id()).await;
            }
        }
        Ok(())
    }

    /// Makes the reader a member of a room.
    ///
    /// # Errors
    /// Fails on network or server errors, and when the room cannot be joined.
    pub async fn join(&self, channel: &ChannelId) -> Result<(), Error> {
        self.sdk()
            .join_room_by_id_or_alias(
                <&matrix_sdk::ruma::RoomOrAliasId>::try_from(channel.0.as_str())?,
                &[],
            )
            .await?;
        Ok(())
    }

    /// Takes the reader out of a room.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn leave(&self, channel: &ChannelId) -> Result<(), Error> {
        let room = self.room(channel)?;
        room.leave().await?;
        room.forget().await?;
        Ok(())
    }

    /// Sets the presence of the reader.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn set_status(&self, status: Status) -> Result<(), Error> {
        let state = match status {
            Status::Online => PresenceState::Online,
            Status::Away => PresenceState::Unavailable,
            Status::DoNotDisturb => PresenceState::from("org.matrix.msc3026.busy"),
            Status::Offline => PresenceState::Offline,
        };
        self.sdk()
            .send(set_presence::v3::Request::new(user_id(&self.me)?, state))
            .await?;
        Ok(())
    }

    /// Pins a message to its room, or unpins it.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn pin(&self, channel: &ChannelId, post: &PostId, pinned: bool) -> Result<(), Error> {
        let room = self.room(channel)?;
        let target = event_id(post)?;
        let mut ids = room.pinned_event_ids().unwrap_or_default();
        ids.retain(|id| id != &target);
        if pinned {
            ids.push(target);
        }
        room.send_state_event(RoomPinnedEventsEventContent::new(ids))
            .await?;
        Ok(())
    }

    /// The account with this name: a full id, or a name on the reader's own server.
    ///
    /// # Errors
    /// Fails on network or server errors, and when nobody has that name.
    pub async fn user_named(&self, name: &str) -> Result<User, Error> {
        let name = name.trim_start_matches('@');
        let id = match name.contains(':') {
            true => format!("@{name}"),
            false => {
                let server = self.me.0.split_once(':').map_or("", |(_, s)| s);
                format!("@{name}:{server}")
            }
        };
        let user = MatrixUserId::parse(&id)?;
        self.sdk().account().fetch_user_profile_of(&user).await?;
        let id = UserId(user.to_string());
        Ok(User {
            username: self.profile_name(&id).await,
            id,
        })
    }

    /// A room by its alias, its id or its name among the rooms the reader is in.
    ///
    /// # Errors
    /// Fails on network or server errors, and when there is no such room.
    pub async fn channel_named(&self, _team: &TeamId, name: &str) -> Result<Channel, Error> {
        let name = unlinked(name);
        let name = name.as_str();
        if name.starts_with('!') || name.starts_with('#') {
            let id = match name.starts_with('#') {
                true => self
                    .sdk()
                    .resolve_room_alias(<&matrix_sdk::ruma::RoomAliasId>::try_from(name)?)
                    .await?
                    .room_id
                    .to_string(),
                false => name.to_owned(),
            };
            if let Ok(room) = self.room(&ChannelId(id.clone())) {
                return Ok(self.channel_of(&room).await);
            }
            return Ok(Channel {
                id: ChannelId(id),
                kind: ChannelKind::Public,
                display_name: name.to_owned(),
                name: name.to_owned(),
                last_post_at: 0,
                total_msg_count: 0,
                delete_at: 0,
                header: String::new(),
                purpose: String::new(),
                peer: None,
                invited: None,
            });
        }
        for room in self.sdk().joined_rooms() {
            let channel = self.channel_of(&room).await;
            let alias = channel.name.trim_start_matches('#').split(':').next();
            if channel.display_name.eq_ignore_ascii_case(name)
                || alias.is_some_and(|a| a.eq_ignore_ascii_case(name))
            {
                return Ok(channel);
            }
        }
        Err(failed(format!("no room named {name}")))
    }

    /// The members of a room.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn channel_users(&self, channel: &ChannelId) -> Result<Vec<User>, Error> {
        let room = self.room(channel)?;
        if room.state() == RoomState::Invited {
            return Ok(Vec::new());
        }
        let members = room.members(RoomMemberships::JOIN).await?;
        let mut users = Vec::new();
        for member in members {
            let id = UserId(member.user_id().to_string());
            let username = member
                .display_name()
                .filter(|n| !n.is_empty())
                .map_or_else(|| convert::localpart(&id.0).to_owned(), str::to_owned);
            self.cache().names.insert(id.clone(), username.clone());
            users.push(User { id, username });
        }
        Ok(users)
    }

    /// Whether each of the users is around.
    ///
    /// # Errors
    /// Never fails: a user whose presence is unknown counts as offline.
    pub async fn statuses(&self, ids: &[UserId]) -> Result<Vec<UserStatus>, Error> {
        let ask = |id: UserId| async move {
            let status = match user_id(&id) {
                Ok(user) => self
                    .sdk()
                    .send(get_presence::v3::Request::new(user))
                    .await
                    .map_or(Status::Offline, |r| match r.presence.as_str() {
                        "online" => Status::Online,
                        "unavailable" => Status::Away,
                        "org.matrix.msc3026.busy" => Status::DoNotDisturb,
                        _ => Status::Offline,
                    }),
                Err(_) => Status::Offline,
            };
            UserStatus {
                user_id: id,
                status: status.word().to_owned(),
            }
        };
        Ok(join_all(ids.iter().cloned().map(ask)).await)
    }

    /// The content of an attached file.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn download(&self, file: &str) -> Result<Vec<u8>, Error> {
        let request = MediaRequestParameters {
            source: serde_json::from_str(file)?,
            format: MediaFormat::File,
        };
        Ok(self.sdk().media().get_media_content(&request, true).await?)
    }

    /// Keeps what an event tells and says what it changes.
    pub fn ingest(&self, room: &ChannelId, event: &Value, me: &str) -> Option<Change> {
        let item = convert::read(room, event, me)?;
        self.cache().apply(room, item)
    }

    async fn token_before(&self, room: &Room, post: &PostId) -> Result<Option<String>, Error> {
        if let Some(token) = self.cache().tokens.get(post) {
            return Ok(Some(token.clone()));
        }
        let context = room
            .event_with_context(&event_id(post)?, false, uint!(0), None)
            .await?;
        Ok(context.prev_batch_token)
    }

    /// A page of messages, newest first, optionally those older than `before`.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn posts(
        &self,
        channel: &ChannelId,
        before: Option<&PostId>,
        per_page: u32,
    ) -> Result<PostList, Error> {
        let room = self.room(channel)?;
        if room.state() == RoomState::Invited {
            return Ok(PostList::default());
        }
        let mut from = match before {
            Some(post) => match self.token_before(&room, post).await? {
                Some(token) => Some(token),
                None => return Ok(PostList::default()),
            },
            None => None,
        };
        let wanted = per_page as usize;
        let mut list = PostList::default();
        let me = self.me.0.clone();
        for _ in 0..NEWEST_PAGES {
            let mut options = MessagesOptions::backward().from(from.as_deref());
            options.limit = per_page.into();
            let page = room.messages(options).await?;
            for event in &page.chunk {
                let Some(Change::Posted(post)) =
                    json_of(event.raw()).and_then(|e| self.ingest(channel, &e, &me))
                else {
                    continue;
                };
                list.order.push(post.id.clone());
                list.posts.insert(post.id.clone(), post);
            }
            match page.end {
                Some(end) if list.order.len() < wanted => from = Some(end),
                Some(end) => {
                    if let Some(oldest) = list.order.last() {
                        self.cache().tokens.insert(oldest.clone(), end);
                    }
                    break;
                }
                None => break,
            }
        }
        Ok(list)
    }

    /// A message with the replies to it, newest first.
    ///
    /// # Errors
    /// Fails on network or server errors.
    pub async fn thread(&self, channel: &ChannelId, post: &PostId) -> Result<PostList, Error> {
        let room = self.room(channel)?;
        let me = self.me.0.clone();
        let mut list = PostList::default();
        let target = event_id(post)?;
        let root = room.load_or_fetch_event(&target, None).await?;
        let events = std::iter::once(root.raw().clone());
        let replies = room
            .relations(
                target.clone(),
                matrix_sdk::room::RelationsOptions::default(),
            )
            .await;
        let replies = replies.map(|r| r.chunk).unwrap_or_default();
        let replies = replies.iter().map(|e| e.raw().clone());
        for raw in events.chain(replies) {
            if let Some(Change::Posted(post)) =
                json_of(&raw).and_then(|e| self.ingest(channel, &e, &me))
            {
                list.order.push(post.id.clone());
                list.posts.insert(post.id.clone(), post);
            }
        }
        Ok(list)
    }

    /// Resolves user ids into accounts.
    ///
    /// # Errors
    /// Never fails: a user without a profile is known by the first part of the id.
    pub async fn users(&self, ids: &[UserId]) -> Result<Vec<User>, Error> {
        let ask = |id: UserId| async move {
            User {
                username: self.profile_name(&id).await,
                id,
            }
        };
        Ok(join_all(ids.iter().cloned().map(ask)).await)
    }
}

#[cfg(test)]
mod wire_tests;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ME: &str = "@me:x.org";

    fn refusal(kind: ErrorKind, message: &str) -> ApiError {
        let body = ErrorBody::Standard(StandardErrorBody::new(kind, message.into()));
        ApiError::new(matrix_sdk::reqwest::StatusCode::BAD_REQUEST, body)
    }

    #[test]
    fn muting_silences_everything_and_mentions_only_keeps_the_mentions_of_rooms() {
        use RoomNotificationMode::{AllMessages, MentionsAndKeywordsOnly, Mute};
        assert_eq!(notify_of(None, false, 3), ("all", 3));
        assert_eq!(notify_of(Some(AllMessages), true, 3), ("all", 3));
        assert_eq!(notify_of(Some(Mute), false, 3), ("mention", 0));
        assert_eq!(notify_of(Some(Mute), true, 3), ("mention", 0));
        assert_eq!(
            notify_of(Some(MentionsAndKeywordsOnly), false, 3),
            ("mention", 3)
        );
        assert_eq!(
            notify_of(Some(MentionsAndKeywordsOnly), true, 3),
            ("mention", 0)
        );
    }

    #[test]
    fn a_refusal_for_lack_of_rights_is_said_plainly() {
        let e = refusal(
            ErrorKind::Forbidden,
            "M_FORBIDDEN: Auth check failed: power",
        );
        assert_eq!(
            server_message(&e),
            "You don't have the rights to do that here"
        );
    }

    #[test]
    fn other_refusals_keep_the_reason_without_its_code() {
        let e = refusal(ErrorKind::NotFound, "M_NOT_FOUND: no such room");
        assert_eq!(server_message(&e), "no such room");
        assert_eq!(
            server_message(&refusal(ErrorKind::NotFound, "")),
            "M_NOT_FOUND"
        );
    }

    #[test]
    fn a_body_of_another_shape_keeps_the_status_and_the_text() {
        let body = ErrorBody::Json(json!({"x": 1}));
        let e = ApiError::new(matrix_sdk::reqwest::StatusCode::BAD_GATEWAY, body);
        assert_eq!(server_message(&e), "[502] {\"x\":1}");
    }

    #[test]
    fn a_media_type_comes_from_the_extension_in_any_case() {
        assert_eq!(mime_of(Path::new("a/b.JPG")), mime::IMAGE_JPEG);
        assert_eq!(mime_of(Path::new("notes.md")), mime::TEXT_PLAIN);
        assert_eq!(mime_of(Path::new("blob")), mime::APPLICATION_OCTET_STREAM);
    }

    fn room() -> ChannelId {
        ChannelId("!r:x.org".into())
    }

    fn feed(cache: &mut Cache, event: &Value) -> Option<Change> {
        cache.apply(&room(), convert::read(&room(), event, ME)?)
    }

    fn message(id: &str, sender: &str, body: &str, ts: i64) -> Value {
        json!({
            "type": "m.room.message", "event_id": id, "sender": sender,
            "origin_server_ts": ts, "content": {"msgtype": "m.text", "body": body},
        })
    }

    fn edit(id: &str, target: &str, sender: &str, body: &str, ts: i64) -> Value {
        json!({
            "type": "m.room.message", "event_id": id, "sender": sender,
            "origin_server_ts": ts,
            "content": {
                "msgtype": "m.text", "body": format!("* {body}"),
                "m.new_content": {"msgtype": "m.text", "body": body},
                "m.relates_to": {"rel_type": "m.replace", "event_id": target},
            },
        })
    }

    fn reaction(id: &str, target: &str, sender: &str, key: &str) -> Value {
        json!({
            "type": "m.reaction", "event_id": id, "sender": sender, "origin_server_ts": 5,
            "content": {"m.relates_to": {"rel_type": "m.annotation", "event_id": target, "key": key}},
        })
    }

    fn posted(change: Option<Change>) -> Post {
        match change {
            Some(Change::Posted(post)) => post,
            other => panic!("not a post: {other:?}"),
        }
    }

    #[test]
    fn an_edit_met_before_its_message_is_applied_when_the_message_comes() {
        let mut cache = Cache::default();
        feed(&mut cache, &edit("$e", "$a", "@ana:x.org", "fixed", 20));
        let post = posted(feed(&mut cache, &message("$a", "@ana:x.org", "typo", 10)));
        assert_eq!(post.message, "fixed");
        assert_eq!(post.edit_at, 20);
    }

    #[test]
    fn an_edit_from_someone_else_is_ignored() {
        let mut cache = Cache::default();
        posted(feed(&mut cache, &message("$a", "@ana:x.org", "mine", 10)));
        assert!(feed(&mut cache, &edit("$e", "$a", "@bob:x.org", "hijacked", 20)).is_none());
        feed(&mut cache, &edit("$e2", "$b", "@bob:x.org", "late", 30));
        let post = posted(feed(&mut cache, &message("$b", "@ana:x.org", "orig", 15)));
        assert_eq!(post.message, "orig");
    }

    #[test]
    fn a_live_edit_says_what_changed() {
        let mut cache = Cache::default();
        posted(feed(&mut cache, &message("$a", "@ana:x.org", "typo", 10)));
        let Some(Change::Edited(post)) =
            feed(&mut cache, &edit("$e", "$a", "@ana:x.org", "ok", 20))
        else {
            panic!("not an edit");
        };
        assert_eq!((post.id.0.as_str(), post.message.as_str()), ("$a", "ok"));
    }

    #[test]
    fn reactions_wait_for_their_message_and_leave_when_redacted() {
        let mut cache = Cache::default();
        let added = feed(&mut cache, &reaction("$r", "$a", "@bob:x.org", "🎉"));
        assert!(matches!(added, Some(Change::Reacted(r)) if r.emoji_name == "tada"));
        assert!(feed(&mut cache, &reaction("$r", "$a", "@bob:x.org", "🎉")).is_none());
        let post = posted(feed(&mut cache, &message("$a", "@ana:x.org", "hi", 10)));
        assert_eq!(post.metadata.reactions.unwrap_or_default().len(), 1);
        let redaction = json!({"type": "m.room.redaction", "redacts": "$r", "content": {}});
        let removed = feed(&mut cache, &redaction);
        assert!(matches!(removed, Some(Change::Unreacted(r)) if r.user_id.0 == "@bob:x.org"));
        let post = posted(feed(&mut cache, &message("$a", "@ana:x.org", "hi", 10)));
        assert!(post.metadata.reactions.is_none());
    }

    #[test]
    fn redacting_a_message_deletes_it_in_its_room() {
        let mut cache = Cache::default();
        posted(feed(&mut cache, &message("$a", "@ana:x.org", "oops", 10)));
        let redaction = json!({"type": "m.room.redaction", "redacts": "$a", "content": {}});
        let Some(Change::Deleted(post)) = feed(&mut cache, &redaction) else {
            panic!("not a deletion");
        };
        assert_eq!((post.id.0.as_str(), post.channel_id), ("$a", room()));
    }

    #[test]
    fn the_latest_message_ignores_system_lines_and_older_events() {
        let mut cache = Cache::default();
        posted(feed(&mut cache, &message("$new", "@ana:x.org", "new", 50)));
        posted(feed(&mut cache, &message("$old", "@ana:x.org", "old", 5)));
        let join = json!({
            "type": "m.room.member", "event_id": "$j", "sender": "@bob:x.org",
            "state_key": "@bob:x.org", "origin_server_ts": 99,
            "content": {"membership": "join"}, "unsigned": {"prev_content": {"membership": "invite"}},
        });
        posted(feed(&mut cache, &join));
        assert_eq!(cache.last.get(&room()), Some(&(50, PostId("$new".into()))));
    }
}
