//! The synchronization loop: what the server sends, as events, with reconnection.

use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::ruma::OwnedRoomId;
use serde_json::Value;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::api::types::{ChannelId, Post, Reaction, Status, UserId};
use crate::api::verify::Check;
use crate::api::{self, json_of, Change, Client};

const LONG_POLL: Duration = Duration::from_secs(30);

/// How long to wait between attempts to reconnect: doubling from `min` up to `max`.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub min: Duration,
    pub max: Duration,
}

/// The waits used for real.
pub const BACKOFF: Backoff = Backoff {
    min: Duration::from_secs(1),
    max: Duration::from_secs(30),
};

/// Something that happened on the server.
#[derive(Debug, Clone)]
pub enum Event {
    Posted { post: Post, mentions: Vec<UserId> },
    Edited(Post),
    Deleted(Post),
    ReactionAdded(Reaction),
    ReactionRemoved(Reaction),
    Viewed(ChannelId),
    Typing { channel: ChannelId, user: UserId },
    Status { user: UserId, status: Status },
    Invited(String),
    Verification(Check),
}

/// Something to tell the server while the loop runs.
#[derive(Debug, Clone)]
pub enum Action {
    /// The reader is typing in a channel.
    Typing { channel: ChannelId },
}

/// What the loop reports.
#[derive(Debug)]
pub enum Sync {
    /// The server answers; `resync` is set when it is not the first time.
    Up {
        resync: bool,
    },
    /// The server stopped answering, with the reason.
    Down(String),
    Event(Box<Event>),
}

/// The event that tells a change, with the reader among the mentioned when the message calls them.
pub fn event_of(change: Change, me: &UserId) -> Event {
    match change {
        Change::Posted(post) => Event::Posted {
            mentions: if post.mentioned {
                vec![me.clone()]
            } else {
                Vec::new()
            },
            post,
        },
        Change::Edited(post) => Event::Edited(post),
        Change::Deleted(post) => Event::Deleted(post),
        Change::Reacted(reaction) => Event::ReactionAdded(reaction),
        Change::Unreacted(reaction) => Event::ReactionRemoved(reaction),
    }
}

/// The events that a typing notice or a read receipt of a room tells.
pub fn ephemeral(channel: &ChannelId, event: &Value, me: &UserId) -> Vec<Event> {
    let content = event.get("content");
    match event.get("type").and_then(Value::as_str) {
        Some("m.typing") => content
            .and_then(|c| c.get("user_ids"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|user| *user != me.0)
            .map(|user| Event::Typing {
                channel: channel.clone(),
                user: UserId(user.to_owned()),
            })
            .collect(),
        Some("m.receipt") => {
            let mine = content
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|events| events.values())
                .filter_map(|kinds| kinds.get("m.read")?.get(&me.0))
                .next()
                .is_some();
            mine.then(|| Event::Viewed(channel.clone()))
                .into_iter()
                .collect()
        }
        _ => Vec::new(),
    }
}

/// The presence a presence event tells.
pub fn presence(event: &Value) -> Option<Event> {
    let user = event.get("sender")?.as_str()?;
    let word = event.pointer("/content/presence")?.as_str()?;
    let status = match word {
        "online" => Status::Online,
        "unavailable" => Status::Away,
        "org.matrix.msc3026.busy" => Status::DoNotDisturb,
        _ => Status::Offline,
    };
    Some(Event::Status {
        user: UserId(user.to_owned()),
        status,
    })
}

async fn invitations<'a>(
    client: &Client,
    ids: impl Iterator<Item = &'a matrix_sdk::ruma::OwnedRoomId>,
) -> Vec<Event> {
    let mut events = Vec::new();
    for id in ids {
        let Some(room) = client.sdk().get_room(id) else {
            continue;
        };
        let name = room
            .display_name()
            .await
            .map_or_else(|_| id.to_string(), |n| n.to_string());
        events.push(Event::Invited(format!("Invited to {name}")));
    }
    events
}

fn events_of(client: &Client, response: matrix_sdk::sync::SyncResponse) -> Vec<Event> {
    let me = client.myself();
    let mut events = Vec::new();
    for (room, update) in response.rooms.joined {
        let channel = ChannelId(OwnedRoomId::to_string(&room));
        let timeline = update
            .timeline
            .events
            .iter()
            .filter_map(|e| json_of(e.raw()));
        for event in timeline {
            if let Some(change) = client.ingest(&channel, &event, &me.0) {
                events.push(event_of(change, me));
            }
        }
        for event in update.ephemeral.iter().filter_map(json_of) {
            events.extend(ephemeral(&channel, &event, me));
        }
    }
    events.extend(
        response
            .presence
            .iter()
            .filter_map(json_of)
            .filter_map(|e| presence(&e)),
    );
    events
}

async fn act(client: &Client, action: Action) {
    let Action::Typing { channel } = action;
    let room = matrix_sdk::ruma::RoomId::parse(&channel.0)
        .ok()
        .and_then(|id| client.sdk().get_room(&id));
    if let Some(room) = room {
        let _ = room.typing_notice(true).await;
    }
}

/// Keeps one server's synchronization going for as long as `send` accepts what it reports.
pub async fn run(
    client: Arc<Client>,
    backoff: Backoff,
    mut actions: UnboundedReceiver<Action>,
    send: impl Fn(Sync) -> bool,
) {
    let (mut wait, mut first, mut up) = (backoff.min, true, false);
    let (found, mut checks) = mpsc::unbounded_channel();
    client.watch(found);
    let pending: Vec<_> = client
        .sdk()
        .invited_rooms()
        .iter()
        .map(|r| r.room_id().to_owned())
        .collect();
    for event in invitations(&client, pending.iter()).await {
        if !send(Sync::Event(Box::new(event))) {
            return;
        }
    }
    loop {
        let settings = api::sync_settings().timeout(LONG_POLL);
        let sdk = client.sdk();
        let sync = sdk.sync_once(settings);
        tokio::pin!(sync);
        let result = loop {
            tokio::select! {
                Some(action) = actions.recv() => act(&client, action).await,
                Some(check) = checks.recv() => {
                    if !send(Sync::Event(Box::new(Event::Verification(check)))) {
                        return;
                    }
                }
                result = &mut sync => break result,
            }
        };
        match result {
            Ok(response) => {
                if !up {
                    (wait, up) = (backoff.min, true);
                    if !send(Sync::Up { resync: !first }) {
                        return;
                    }
                    first = false;
                    while actions.try_recv().is_ok() {}
                }
                let invited = invitations(&client, response.rooms.invited.keys()).await;
                for event in invited.into_iter().chain(events_of(&client, response)) {
                    if !send(Sync::Event(Box::new(event))) {
                        return;
                    }
                }
            }
            Err(e) => {
                if e.client_api_error_kind().is_some_and(|k| {
                    matches!(
                        k,
                        matrix_sdk::ruma::api::error::ErrorKind::UnknownToken { .. }
                    )
                }) {
                    let _ = client.relogin().await;
                }
                up = false;
                if !send(Sync::Down(e.to_string())) {
                    return;
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(backoff.max);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn me() -> UserId {
        UserId("@me:x.org".into())
    }

    fn room() -> ChannelId {
        ChannelId("!r:x.org".into())
    }

    #[test]
    fn typing_names_everyone_but_the_reader() {
        let event = json!({
            "type": "m.typing",
            "content": {"user_ids": ["@me:x.org", "@ana:x.org"]},
        });
        let events = ephemeral(&room(), &event, &me());
        assert!(
            matches!(events.as_slice(), [Event::Typing { user, .. }] if user.0 == "@ana:x.org")
        );
    }

    #[test]
    fn only_my_own_read_receipt_marks_the_channel_viewed() {
        let receipt = |user: &str| {
            json!({
                "type": "m.receipt",
                "content": {"$e": {"m.read": {user: {"ts": 1}}}},
            })
        };
        assert!(ephemeral(&room(), &receipt("@ana:x.org"), &me()).is_empty());
        assert!(matches!(
            ephemeral(&room(), &receipt("@me:x.org"), &me()).as_slice(),
            [Event::Viewed(c)] if *c == room()
        ));
    }

    #[test]
    fn presence_words_become_statuses() {
        let event = |word: &str| json!({"sender": "@ana:x.org", "content": {"presence": word}});
        let status = |word| match presence(&event(word)) {
            Some(Event::Status { status, .. }) => status,
            other => panic!("not a status: {other:?}"),
        };
        assert_eq!(status("online"), Status::Online);
        assert_eq!(status("unavailable"), Status::Away);
        assert_eq!(status("org.matrix.msc3026.busy"), Status::DoNotDisturb);
        assert_eq!(status("offline"), Status::Offline);
    }

    #[test]
    fn a_post_that_mentions_the_reader_lists_them() {
        let post = Post {
            mentioned: true,
            ..Post::default()
        };
        let Event::Posted { mentions, .. } = event_of(Change::Posted(post), &me()) else {
            panic!("not a post");
        };
        assert_eq!(mentions, [me()]);
        let quiet = event_of(Change::Posted(Post::default()), &me());
        assert!(matches!(quiet, Event::Posted { mentions, .. } if mentions.is_empty()));
    }
}
