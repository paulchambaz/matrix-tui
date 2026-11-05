//! Reads Matrix events, as the server sends them, into the internal types.

use serde_json::{json, Value};

use super::types::{ChannelId, FileInfo, Metadata, Post, PostId, Reaction, UserId};

const UNDECRYPTABLE: &str = "[unable to decrypt]";

/// What one event says.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// A new message, or a line about the room itself.
    Post(Post),
    /// A new text for a message, from the sender of the edit.
    Edit { sender: UserId, post: Post },
    /// A reaction, with the id of the event that carries it.
    React { event: PostId, reaction: Reaction },
    /// The removal of a message, an edit or a reaction.
    Redact(PostId),
}

fn text<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(value, |v, key| v.get(key))?.as_str()
}

/// The name of an emoji as the interface writes it, from the key a reaction carries.
pub fn emoji_name(key: &str) -> String {
    let plain = key.trim_end_matches('\u{fe0f}');
    emojis::get(key)
        .or_else(|| emojis::get(plain))
        .and_then(|e| e.shortcode())
        .map_or_else(|| key.to_owned(), str::to_owned)
}

/// The key a reaction carries for an emoji name; names that are not emojis stay as they are.
pub fn emoji_key(name: &str) -> String {
    emojis::get_by_shortcode(name).map_or_else(|| name.to_owned(), |e| e.as_str().to_owned())
}

/// The part of a user id before the colon, without the `@`.
pub fn localpart(user: &str) -> &str {
    user.trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or_default()
}

fn body_of(content: &Value, replying: bool) -> String {
    let body = text(content, &["body"]).unwrap_or_default();
    let body = match replying {
        true => {
            let quoted = body.lines().take_while(|l| l.starts_with('>')).count();
            let rest = body.lines().skip(quoted);
            let rest = rest.skip_while(|l| quoted > 0 && l.is_empty());
            rest.collect::<Vec<_>>().join("\n")
        }
        false => body.to_owned(),
    };
    match (text(content, &["msgtype"]), text(content, &["geo_uri"])) {
        (Some("m.emote"), _) => format!("* {body}"),
        (Some("m.location"), Some(geo)) if geo == body => format!("📍 {geo}"),
        (Some("m.location"), Some(geo)) => format!("📍 {body}: {geo}"),
        _ => body,
    }
}

fn rich(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| text(value.get(0)?, &["body"]))
}

fn words(value: &Value) -> Option<&str> {
    [
        "m.text",
        "org.matrix.msc1767.text",
        "org.matrix.msc1767.message",
    ]
    .iter()
    .find_map(|key| rich(value.get(key)?))
}

fn poll_of(content: &Value) -> Option<String> {
    let start = content
        .get("m.poll.start")
        .or_else(|| content.get("org.matrix.msc3381.poll.start"))?;
    let answers = start.get("answers")?.as_array()?;
    let mut lines = vec![format!("📊 {}", words(start.get("question")?)?)];
    lines.extend(
        answers
            .iter()
            .enumerate()
            .filter_map(|(i, a)| Some(format!("{}. {}", i + 1, words(a)?))),
    );
    Some(lines.join("\n"))
}

fn file_of(content: &Value) -> Option<FileInfo> {
    let source = match (content.get("file"), text(content, &["url"])) {
        (Some(file), _) => json!({ "file": file }),
        (None, Some(url)) => json!({ "url": url }),
        _ => return None,
    };
    let body = text(content, &["body"]).unwrap_or("file");
    Some(FileInfo {
        id: source.to_string(),
        name: text(content, &["filename"]).unwrap_or(body).to_owned(),
        mime_type: text(content, &["info", "mimetype"])
            .unwrap_or_default()
            .to_owned(),
        size: content
            .pointer("/info/size")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
    })
}

fn mentions(content: &Value, body: &str, me: &str) -> bool {
    let listed = content
        .pointer("/m.mentions/user_ids")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(me)));
    let room = content.pointer("/m.mentions/room").and_then(Value::as_bool) == Some(true);
    listed || room || body.contains(me)
}

fn member_line(event: &Value) -> Option<String> {
    let content = event.get("content")?;
    let previous = event.pointer("/unsigned/prev_content");
    let subject = text(event, &["state_key"])?;
    let was = previous.and_then(|p| text(p, &["membership"]));
    let name = text(content, &["displayname"])
        .or_else(|| previous.and_then(|p| text(p, &["displayname"])))
        .unwrap_or_else(|| localpart(subject));
    let by_self = text(event, &["sender"]) == Some(subject);
    let action = match (text(content, &["membership"])?, was) {
        ("join", Some("join")) => return None,
        ("join", _) => "joined the room",
        ("leave", Some("invite")) if by_self => "declined the invitation",
        ("leave", _) if by_self => "left the room",
        ("leave", Some("ban")) => "was unbanned",
        ("leave", _) => "was removed from the room",
        ("invite", _) => "was invited",
        ("ban", _) => "was banned",
        _ => return None,
    };
    Some(format!("{name} {action}"))
}

fn post(room: &ChannelId, event: &Value, me: &str) -> Option<Post> {
    let id = text(event, &["event_id"])?;
    let content = event.get("content")?;
    let replying = content.pointer("/m.relates_to/m.in_reply_to").is_some();
    let threaded = text(content, &["m.relates_to", "rel_type"]) == Some("m.thread");
    let root = threaded
        .then(|| text(content, &["m.relates_to", "event_id"]))
        .flatten()
        .or_else(|| text(content, &["m.relates_to", "m.in_reply_to", "event_id"]))
        .unwrap_or_default();
    let (message, kind, files, user) = match text(event, &["type"])? {
        "m.room.message" | "m.sticker" => {
            let file = file_of(content);
            let body = body_of(content, replying);
            let caption = match &file {
                Some(f) if f.name == body => String::new(),
                _ => body,
            };
            (
                caption,
                String::new(),
                file.into_iter().collect::<Vec<_>>(),
                text(event, &["sender"])?.to_owned(),
            )
        }
        "m.poll.start" | "org.matrix.msc3381.poll.start" => (
            poll_of(content)?,
            String::new(),
            Vec::new(),
            text(event, &["sender"])?.to_owned(),
        ),
        "m.room.encrypted" => (
            UNDECRYPTABLE.to_owned(),
            String::new(),
            Vec::new(),
            text(event, &["sender"])?.to_owned(),
        ),
        "m.room.member" => (
            member_line(event)?,
            "system_member".to_owned(),
            Vec::new(),
            text(event, &["state_key"])?.to_owned(),
        ),
        "m.room.topic" => (
            format!(
                "changed the topic to: {}",
                text(content, &["topic"]).unwrap_or_default()
            ),
            "system_topic".to_owned(),
            Vec::new(),
            text(event, &["sender"])?.to_owned(),
        ),
        "m.room.name" => (
            format!(
                "renamed the room to {}",
                text(content, &["name"]).unwrap_or_default()
            ),
            "system_name".to_owned(),
            Vec::new(),
            text(event, &["sender"])?.to_owned(),
        ),
        _ => return None,
    };
    let sender = text(event, &["sender"])?;
    Some(Post {
        id: PostId(id.to_owned()),
        create_at: event.get("origin_server_ts")?.as_i64()?,
        user_id: UserId(user),
        channel_id: room.clone(),
        root_id: root.to_owned(),
        mentioned: sender != me && kind.is_empty() && mentions(content, &message, me),
        message,
        kind,
        metadata: Metadata {
            reactions: None,
            files: Some(files).filter(|f| !f.is_empty()),
        },
        ..Post::default()
    })
}

/// Reads one event of a room; none when it says nothing the interface shows.
pub fn read(room: &ChannelId, event: &Value, me: &str) -> Option<Item> {
    let kind = text(event, &["type"])?;
    if event.pointer("/unsigned/redacted_because").is_some() {
        return None;
    }
    let content = event.get("content")?;
    let relation = content.get("m.relates_to");
    let target = relation.and_then(|r| text(r, &["event_id"]));
    match (kind, relation.and_then(|r| text(r, &["rel_type"]))) {
        ("m.room.message", Some("m.replace")) => {
            let new = content.get("m.new_content")?;
            Some(Item::Edit {
                sender: UserId(text(event, &["sender"])?.to_owned()),
                post: Post {
                    id: PostId(target?.to_owned()),
                    channel_id: room.clone(),
                    edit_at: event.get("origin_server_ts")?.as_i64()?,
                    message: body_of(new, false),
                    ..Post::default()
                },
            })
        }
        ("m.reaction", Some("m.annotation")) => Some(Item::React {
            event: PostId(text(event, &["event_id"])?.to_owned()),
            reaction: Reaction {
                user_id: UserId(text(event, &["sender"])?.to_owned()),
                post_id: PostId(target?.to_owned()),
                emoji_name: emoji_name(text(relation?, &["key"])?),
            },
        }),
        ("m.room.redaction", _) => {
            let target = text(event, &["redacts"]).or_else(|| text(content, &["redacts"]))?;
            Some(Item::Redact(PostId(target.to_owned())))
        }
        _ => post(room, event, me).map(Item::Post),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "@me:x.org";

    fn room() -> ChannelId {
        ChannelId("!r:x.org".into())
    }

    fn message(body: &str) -> Value {
        json!({
            "type": "m.room.message", "event_id": "$a", "sender": "@ana:x.org",
            "origin_server_ts": 1_700_000_000_000_i64,
            "content": {"msgtype": "m.text", "body": body},
        })
    }

    fn post_of(event: &Value) -> Post {
        match read(&room(), event, ME) {
            Some(Item::Post(post)) => post,
            other => panic!("not a post: {other:?}"),
        }
    }

    #[test]
    fn a_text_message_becomes_a_post() {
        let post = post_of(&message("hello"));
        assert_eq!(post.id, PostId("$a".into()));
        assert_eq!(post.user_id, UserId("@ana:x.org".into()));
        assert_eq!(post.message, "hello");
        assert_eq!(post.create_at, 1_700_000_000_000);
        assert!(post.root_id.is_empty() && !post.mentioned);
    }

    #[test]
    fn a_reply_names_its_target_and_loses_the_quoted_fallback() {
        let mut event = message("> <@bob:x.org> earlier\n\nanswer");
        event["content"]["m.relates_to"] = json!({"m.in_reply_to": {"event_id": "$b"}});
        let post = post_of(&event);
        assert_eq!(post.root_id, "$b");
        assert_eq!(post.message, "answer");
    }

    #[test]
    fn a_thread_message_names_the_root_of_the_thread_not_its_neighbour() {
        let mut event = message("in the thread");
        event["content"]["m.relates_to"] = json!({
            "rel_type": "m.thread",
            "event_id": "$root",
            "is_falling_back": true,
            "m.in_reply_to": {"event_id": "$previous"}
        });
        assert_eq!(post_of(&event).root_id, "$root");
    }

    #[test]
    fn a_location_shows_its_place_and_a_poll_its_question_and_answers() {
        let mut event = message("Café");
        event["content"] =
            json!({"msgtype": "m.location", "body": "Café", "geo_uri": "geo:48.8,2.3"});
        assert_eq!(post_of(&event).message, "📍 Café: geo:48.8,2.3");
        event["content"]["body"] = json!("geo:48.8,2.3");
        assert_eq!(post_of(&event).message, "📍 geo:48.8,2.3");
        event["type"] = json!("m.poll.start");
        event["content"] = json!({"m.poll.start": {
            "question": {"m.text": [{"body": "Lunch?"}]},
            "answers": [
                {"m.id": "a", "m.text": [{"body": "Pizza"}]},
                {"m.id": "b", "m.text": [{"body": "Soup"}]},
            ],
        }});
        assert_eq!(post_of(&event).message, "📊 Lunch?\n1. Pizza\n2. Soup");
        event["type"] = json!("m.poll.response");
        assert_eq!(read(&room(), &event, ME), None);
    }

    #[test]
    fn a_quote_that_is_not_a_reply_stays() {
        assert_eq!(
            post_of(&message("> quoted\n\ntext")).message,
            "> quoted\n\ntext"
        );
    }

    #[test]
    fn mentions_come_from_the_list_the_room_and_the_body() {
        let mut event = message("hi");
        event["content"]["m.mentions"] = json!({"user_ids": [ME]});
        assert!(post_of(&event).mentioned);
        event["content"]["m.mentions"] = json!({"room": true});
        assert!(post_of(&event).mentioned);
        assert!(post_of(&message("ping @me:x.org please")).mentioned);
        assert!(!post_of(&message("ping @you:x.org please")).mentioned);
    }

    #[test]
    fn my_own_messages_never_mention_me() {
        let mut event = message("note to @me:x.org");
        event["sender"] = json!(ME);
        assert!(!post_of(&event).mentioned);
    }

    #[test]
    fn a_file_carries_its_source_and_loses_a_caption_equal_to_its_name() {
        let event = json!({
            "type": "m.room.message", "event_id": "$f", "sender": "@ana:x.org",
            "origin_server_ts": 1,
            "content": {"msgtype": "m.image", "body": "a.png", "url": "mxc://x.org/abc",
                        "info": {"mimetype": "image/png", "size": 12}},
        });
        let post = post_of(&event);
        let files = post.metadata.files.unwrap_or_default();
        assert_eq!(files[0].name, "a.png");
        assert_eq!(files[0].mime_type, "image/png");
        assert_eq!(files[0].size, 12);
        assert_eq!(files[0].id, r#"{"url":"mxc://x.org/abc"}"#);
        assert!(post.message.is_empty());
    }

    #[test]
    fn an_edit_carries_the_new_text_and_its_target() {
        let mut event = message("* fixed");
        event["event_id"] = json!("$e");
        event["content"]["m.new_content"] = json!({"msgtype": "m.text", "body": "fixed"});
        event["content"]["m.relates_to"] = json!({"rel_type": "m.replace", "event_id": "$a"});
        let Some(Item::Edit { sender, post }) = read(&room(), &event, ME) else {
            panic!("not an edit");
        };
        assert_eq!(sender, UserId("@ana:x.org".into()));
        assert_eq!((post.id.0.as_str(), post.message.as_str()), ("$a", "fixed"));
        assert!(post.edit_at > 0);
    }

    #[test]
    fn a_reaction_is_named_like_the_interface_names_it() {
        let event = json!({
            "type": "m.reaction", "event_id": "$r", "sender": "@bob:x.org",
            "origin_server_ts": 1,
            "content": {"m.relates_to": {"rel_type": "m.annotation", "event_id": "$a", "key": "👍"}},
        });
        let Some(Item::React { event, reaction }) = read(&room(), &event, ME) else {
            panic!("not a reaction");
        };
        assert_eq!(event, PostId("$r".into()));
        assert_eq!(reaction.post_id, PostId("$a".into()));
        assert_eq!(emoji_key(&reaction.emoji_name), "👍");
    }

    #[test]
    fn a_redaction_names_its_target_in_either_place() {
        let old = json!({"type": "m.room.redaction", "redacts": "$a", "content": {}});
        let new = json!({"type": "m.room.redaction", "content": {"redacts": "$a"}});
        assert_eq!(
            read(&room(), &old, ME),
            Some(Item::Redact(PostId("$a".into())))
        );
        assert_eq!(
            read(&room(), &new, ME),
            Some(Item::Redact(PostId("$a".into())))
        );
    }

    #[test]
    fn a_redacted_event_says_nothing() {
        let mut event = message("gone");
        event["unsigned"] = json!({"redacted_because": {}});
        assert_eq!(read(&room(), &event, ME), None);
    }

    #[test]
    fn an_undecryptable_event_is_shown_as_such() {
        let event = json!({
            "type": "m.room.encrypted", "event_id": "$u", "sender": "@ana:x.org",
            "origin_server_ts": 1, "content": {"algorithm": "m.megolm.v1.aes-sha2"},
        });
        assert_eq!(post_of(&event).message, UNDECRYPTABLE);
    }

    #[test]
    fn joining_and_leaving_are_system_lines_but_a_rename_is_not() {
        let member = |sender: &str, content: Value, previous: Value| {
            json!({
                "type": "m.room.member", "event_id": "$m", "sender": sender,
                "state_key": "@ana:x.org", "origin_server_ts": 1, "content": content,
                "unsigned": {"prev_content": previous},
            })
        };
        let joined = post_of(&member(
            "@ana:x.org",
            json!({"membership": "join", "displayname": "Ana"}),
            json!({"membership": "invite"}),
        ));
        assert_eq!(joined.message, "Ana joined the room");
        assert!(joined.kind.starts_with("system_"));
        let left = post_of(&member(
            "@ana:x.org",
            json!({"membership": "leave"}),
            json!({"membership": "join", "displayname": "Ana"}),
        ));
        assert_eq!(left.message, "Ana left the room");
        let renamed = member(
            "@ana:x.org",
            json!({"membership": "join", "displayname": "Anna"}),
            json!({"membership": "join", "displayname": "Ana"}),
        );
        assert_eq!(read(&room(), &renamed, ME), None);
    }

    #[test]
    fn emoji_names_and_keys_go_both_ways() {
        assert_eq!(emoji_name("🎉"), "tada");
        assert_eq!(emoji_key("tada"), "🎉");
        assert_eq!(emoji_name("not-an-emoji"), "not-an-emoji");
        assert_eq!(localpart("@ana:x.org"), "ana");
    }
}
