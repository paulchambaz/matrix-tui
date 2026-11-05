//! A fake Matrix on `127.0.0.1` with invented people and messages, and the real `mx` run against it.
//!
//! It records the demo gif of the README and lets anyone try `mx` without a server. Nothing leaves the
//! machine and the real configuration is never read: `mx` gets a throwaway home directory.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SERVER: &str = "demo.local";
const LONG_POLL_CAP: Duration = Duration::from_secs(20);
const MINUTE: i64 = 60_000;

type Shared = Arc<Mutex<World>>;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

fn lock(shared: &Shared) -> MutexGuard<'_, World> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

fn user_id(name: &str) -> String {
    format!("@{name}:{SERVER}")
}

fn room_id(key: &str) -> String {
    format!("!{key}:{SERVER}")
}

fn key_of(id: &str) -> &str {
    id.trim_start_matches('!')
        .split(':')
        .next()
        .unwrap_or_default()
}

fn name_of(id: &str) -> &str {
    id.trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or_default()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let hex = bytes
            .get(at + 1..at + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (byte, hex) {
            (b'%', Some(decoded)) => {
                out.push(decoded);
                at += 3;
            }
            _ => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Room {
    key: String,
    name: String,
    topic: String,
    public: bool,
    space: bool,
    parent: Option<String>,
    other: Option<String>,
    created: u64,
    unread: Option<usize>,
    read: Option<String>,
}

struct Event {
    pos: u64,
    room: String,
    body: Value,
}

struct World {
    me: String,
    people: Vec<(String, String)>,
    rooms: Vec<Room>,
    events: Vec<Event>,
    ephemeral: Vec<(u64, String, Value)>,
    direct_at: u64,
    pos: u64,
    next: u64,
    scripted: Option<(String, String)>,
}

impl World {
    fn new(me: &str, people: &[(&str, &str)]) -> Self {
        let mut all = vec![(me.to_owned(), "online".to_owned())];
        all.extend(
            people
                .iter()
                .map(|(name, state)| ((*name).to_owned(), (*state).to_owned())),
        );
        Self {
            me: me.to_owned(),
            people: all,
            rooms: Vec::new(),
            events: Vec::new(),
            ephemeral: Vec::new(),
            direct_at: 0,
            pos: 0,
            next: 0,
            scripted: None,
        }
    }

    fn bump(&mut self) -> u64 {
        self.pos += 1;
        self.pos
    }

    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    fn knows(&self, name: &str) -> bool {
        self.people.iter().any(|(n, _)| n == name)
    }

    fn room(&self, key: &str) -> Option<&Room> {
        self.rooms.iter().find(|r| r.key == key)
    }

    fn open(&mut self, key: &str, name: &str, topic: &str) -> &mut Room {
        let created = self.bump();
        self.rooms.push(Room {
            key: key.to_owned(),
            name: name.to_owned(),
            topic: topic.to_owned(),
            public: false,
            space: false,
            parent: None,
            other: None,
            created,
            unread: None,
            read: None,
        });
        self.rooms.last_mut().unwrap_or_else(|| unreachable!())
    }

    fn space(&mut self, key: &str, name: &str) {
        self.open(key, name, "").space = true;
    }

    fn channel(&mut self, space: &str, public: bool, key: &str, name: &str, topic: &str) {
        let room = self.open(key, name, topic);
        room.public = public;
        room.parent = Some(space.to_owned());
    }

    fn direct(&mut self, other: &str) -> String {
        let key = format!("dm-{other}");
        if self.room(&key).is_none() {
            self.open(&key, "", "").other = Some(other.to_owned());
            self.direct_at = self.pos;
        }
        key
    }

    fn add(&mut self, room: &str, sender: &str, kind: &str, content: Value, ago: i64) -> String {
        let id = self.id("$e");
        let pos = self.bump();
        self.events.push(Event {
            pos,
            room: room.to_owned(),
            body: json!({
                "event_id": id, "room_id": room_id(room), "sender": user_id(sender),
                "type": kind, "origin_server_ts": now() - ago, "content": content,
            }),
        });
        id
    }

    fn post(&mut self, room: &str, user: &str, root: &str, message: &str, ago: i64) -> String {
        let mut content = json!({"msgtype": "m.text", "body": message});
        if !root.is_empty() {
            content["m.relates_to"] = json!({
                "rel_type": "m.thread", "event_id": root, "is_falling_back": true,
                "m.in_reply_to": {"event_id": root},
            });
        }
        if message.contains(&format!("@{}", self.me)) {
            content["m.mentions"] = json!({"user_ids": [user_id(&self.me)]});
        }
        self.add(room, user, "m.room.message", content, ago * MINUTE)
    }

    fn file(
        &mut self,
        room: &str,
        user: &str,
        name: &str,
        mime: &str,
        size: i64,
        ago: i64,
    ) -> String {
        let content = json!({
            "msgtype": "m.file", "body": name, "filename": name,
            "url": format!("mxc://{SERVER}/{name}"), "info": {"mimetype": mime, "size": size},
        });
        self.add(room, user, "m.room.message", content, ago * MINUTE)
    }

    fn room_of(&self, event: &str) -> Option<String> {
        let found = self.events.iter().find(|e| e.body["event_id"] == event);
        found.map(|e| e.room.clone())
    }

    fn react(&mut self, event: &str, user: &str, emoji: &str) -> Option<String> {
        let room = self.room_of(event)?;
        let key = emojis::get_by_shortcode(emoji)
            .map_or_else(|| emoji.to_owned(), |e| e.as_str().to_owned());
        let content = json!({
            "m.relates_to": {"rel_type": "m.annotation", "event_id": event, "key": key},
        });
        Some(self.add(&room, user, "m.reaction", content, 0))
    }

    fn edited(&mut self, event: &str, message: &str) -> Option<String> {
        let room = self.room_of(event)?;
        let sender = self
            .events
            .iter()
            .find(|e| e.body["event_id"] == event)
            .map(|e| name_of(e.body["sender"].as_str().unwrap_or_default()).to_owned())?;
        let content = json!({
            "msgtype": "m.text", "body": format!("* {message}"),
            "m.new_content": {"msgtype": "m.text", "body": message},
            "m.relates_to": {"rel_type": "m.replace", "event_id": event},
        });
        Some(self.add(&room, &sender, "m.room.message", content, MINUTE))
    }

    fn redact(&mut self, room: &str, event: &str) -> String {
        if let Some(found) = self.events.iter_mut().find(|e| e.body["event_id"] == event) {
            found.body["content"] = json!({});
            found.body["unsigned"] = json!({"redacted_because": {}});
        }
        let me = self.me.clone();
        let id = self.add(room, &me, "m.room.redaction", json!({}), 0);
        if let Some(found) = self.events.iter_mut().find(|e| e.body["event_id"] == id) {
            found.body["redacts"] = json!(event);
        }
        id
    }

    fn typing(&mut self, room: &str, users: &[String]) {
        let pos = self.bump();
        let ids: Vec<String> = users.iter().map(|u| user_id(u)).collect();
        let event = json!({"type": "m.typing", "content": {"user_ids": ids}});
        self.ephemeral.push((pos, room.to_owned(), event));
    }

    fn unread(&mut self, room: &str, count: usize) {
        if let Some(found) = self.rooms.iter_mut().find(|r| r.key == room) {
            found.unread = Some(count);
        }
    }

    fn finish(&mut self) {
        self.events
            .sort_by_key(|e| e.body["origin_server_ts"].as_i64());
        let me = user_id(&self.me);
        let reads: Vec<Option<String>> = self
            .rooms
            .iter()
            .map(|room| {
                let all: Vec<&Value> = self
                    .events
                    .iter()
                    .filter(|e| e.room == room.key)
                    .map(|e| &e.body)
                    .collect();
                let theirs: Vec<usize> = all
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e["type"] == "m.room.message" && e["sender"] != me)
                    .filter(|(_, e)| {
                        e.pointer("/content/m.relates_to/rel_type") != Some(&json!("m.replace"))
                    })
                    .map(|(at, _)| at)
                    .collect();
                let last_read = match room.unread {
                    None => all.len().checked_sub(1),
                    Some(count) => theirs
                        .len()
                        .checked_sub(count)
                        .and_then(|first| theirs.get(first))
                        .and_then(|at| at.checked_sub(1)),
                };
                last_read
                    .and_then(|at| all.get(at))
                    .and_then(|e| e["event_id"].as_str().map(str::to_owned))
            })
            .collect();
        for (room, read) in self.rooms.iter_mut().zip(reads) {
            room.read = read;
        }
    }

    fn members(&self, room: &Room) -> Vec<String> {
        match &room.other {
            Some(other) => vec![self.me.clone(), other.clone()],
            None => self.people.iter().map(|(name, _)| name.clone()).collect(),
        }
    }

    fn state(&self, room: &Room) -> Vec<Value> {
        let at = now() - 30 * 24 * 60 * MINUTE;
        let key = &room.key;
        let creator = user_id(&self.me);
        let event = |kind: &str, state_key: &str, content: Value| {
            json!({
                "event_id": format!("$st-{key}-{kind}-{state_key}"), "room_id": room_id(key),
                "sender": creator, "type": kind, "state_key": state_key,
                "origin_server_ts": at, "content": content,
            })
        };
        let mut create = json!({"room_version": "11"});
        if room.space {
            create["type"] = json!("m.space");
        }
        let mut events = vec![
            event("m.room.create", "", create),
            event(
                "m.room.power_levels",
                "",
                json!({"users": {creator.clone(): 100}, "users_default": 0, "events_default": 0,
                       "state_default": 50, "redact": 50, "invite": 0}),
            ),
            event(
                "m.room.join_rules",
                "",
                json!({"join_rule": if room.public { "public" } else { "invite" }}),
            ),
        ];
        if room.other.is_none() {
            events.push(event("m.room.name", "", json!({"name": room.name})));
        }
        if !room.topic.is_empty() {
            events.push(event("m.room.topic", "", json!({"topic": room.topic})));
        }
        if room.other.is_none() && !room.space {
            events.push(event(
                "m.room.canonical_alias",
                "",
                json!({"alias": format!("#{key}:{SERVER}")}),
            ));
        }
        for name in self.members(room) {
            events.push(event(
                "m.room.member",
                &user_id(&name),
                json!({"membership": "join", "displayname": name}),
            ));
        }
        if room.space {
            let children = self
                .rooms
                .iter()
                .filter(|r| r.parent.as_deref() == Some(key));
            for child in children {
                events.push(event(
                    "m.space.child",
                    &room_id(&child.key),
                    json!({"via": [SERVER]}),
                ));
            }
        }
        events
    }

    fn receipt(&self, room: &Room) -> Option<Value> {
        let read = room.read.as_ref()?;
        let at = self
            .events
            .iter()
            .find(|e| e.body["event_id"] == *read)
            .and_then(|e| e.body["origin_server_ts"].as_i64())?;
        Some(json!({
            "type": "m.receipt",
            "content": {read: {"m.read": {user_id(&self.me): {"ts": at}}}},
        }))
    }

    fn direct_content(&self) -> Value {
        let map: serde_json::Map<String, Value> = self
            .rooms
            .iter()
            .filter_map(|r| Some((user_id(r.other.as_ref()?), json!([room_id(&r.key)]))))
            .collect();
        Value::Object(map)
    }

    fn sync_body(&self, since: Option<u64>) -> Value {
        let after = since.unwrap_or(0);
        let mut joined = serde_json::Map::new();
        for room in &self.rooms {
            let fresh = since.is_none() || room.created > after;
            let timeline: Vec<&Value> = self
                .events
                .iter()
                .filter(|e| e.room == room.key && (fresh || e.pos > after))
                .map(|e| &e.body)
                .collect();
            let mut ephemeral: Vec<Value> = self
                .ephemeral
                .iter()
                .filter(|(pos, key, _)| *key == room.key && *pos > after)
                .map(|(_, _, event)| event.clone())
                .collect();
            if fresh {
                ephemeral.extend(self.receipt(room));
            }
            if !fresh && timeline.is_empty() && ephemeral.is_empty() {
                continue;
            }
            let mut entry = json!({
                "timeline": {"events": timeline, "limited": false, "prev_batch": "t0"},
                "ephemeral": {"events": ephemeral},
            });
            if fresh {
                let others: Vec<String> = self
                    .members(room)
                    .iter()
                    .filter(|n| **n != self.me)
                    .map(|n| user_id(n))
                    .collect();
                entry["state"] = json!({"events": self.state(room)});
                entry["summary"] = json!({
                    "m.heroes": others.iter().take(5).collect::<Vec<_>>(),
                    "m.joined_member_count": self.members(room).len(),
                    "m.invited_member_count": 0,
                });
            }
            joined.insert(room_id(&room.key), entry);
        }
        let mut account = Vec::new();
        if since.is_none() || self.direct_at > after {
            account.push(json!({"type": "m.direct", "content": self.direct_content()}));
        }
        json!({
            "next_batch": format!("s{}", self.pos),
            "rooms": {"join": joined},
            "account_data": {"events": account},
            "presence": {"events": []},
            "to_device": {"events": []},
            "device_lists": {"changed": [], "left": []},
            "device_one_time_keys_count": {"signed_curve25519": 50},
        })
    }

    fn page(&self, room: &str, from: Option<&str>, limit: usize) -> Value {
        let events: Vec<&Value> = self
            .events
            .iter()
            .filter(|e| e.room == room)
            .map(|e| &e.body)
            .collect();
        let token = |t: &str| t.strip_prefix('t').and_then(|n| n.parse::<usize>().ok());
        let end = from
            .and_then(token)
            .unwrap_or(events.len())
            .min(events.len());
        let start = end.saturating_sub(limit);
        let chunk: Vec<&Value> = events[start..end].iter().rev().copied().collect();
        let mut page = json!({"start": format!("t{end}"), "chunk": chunk, "state": []});
        if start > 0 {
            page["end"] = json!(format!("t{start}"));
        }
        page
    }

    fn context(&self, room: &str, event: &str) -> Option<Value> {
        let events: Vec<&Value> = self
            .events
            .iter()
            .filter(|e| e.room == room)
            .map(|e| &e.body)
            .collect();
        let at = events.iter().position(|e| e["event_id"] == event)?;
        Some(json!({
            "event": events[at], "events_before": [], "events_after": [], "state": [],
            "start": format!("t{at}"), "end": format!("t{}", at + 1),
        }))
    }

    fn presence(&self, name: &str) -> Option<Value> {
        let (_, state) = self.people.iter().find(|(n, _)| n == name)?;
        let word = match state.as_str() {
            "online" => "online",
            "away" => "unavailable",
            "dnd" => "org.matrix.msc3026.busy",
            _ => "offline",
        };
        Some(json!({"presence": word}))
    }
}

fn work() -> World {
    let mut w = World::new(
        "alex",
        &[
            ("maya", "online"),
            ("tomas", "away"),
            ("priya", "dnd"),
            ("jonas", "offline"),
            ("lena", "online"),
            ("sam", "online"),
        ],
    );
    w.space("eng", "Engineering");
    w.space("ux", "Design");
    w.channel(
        "eng",
        true,
        "general",
        "General",
        "Announcements and good mornings",
    );
    w.channel("eng", true, "releases", "Releases", "What ships, and when");
    w.channel(
        "eng",
        true,
        "backend",
        "Backend",
        "API, database and workers",
    );
    w.channel(
        "eng",
        true,
        "incidents",
        "Incidents",
        "Something is on fire",
    );
    w.channel(
        "eng",
        false,
        "roadmap",
        "Roadmap",
        "Not for everyone's eyes yet",
    );
    w.channel(
        "ux",
        true,
        "design-system",
        "Design System",
        "Tokens, components, docs",
    );
    w.channel(
        "ux",
        true,
        "research",
        "Research",
        "Interviews and findings",
    );
    w.channel("ux", true, "brand", "Brand", "Logos, colors, voice");
    for other in ["maya", "tomas", "priya"] {
        w.direct(other);
    }

    let hello = w.post("general", "lena", "", "Good morning everyone! :wave:", 213);
    w.react(&hello, "sam", "wave");
    w.react(&hello, "maya", "wave");
    w.post(
        "general",
        "sam",
        "",
        "Reminder: the all-hands is **Thursday at 15:00**. The agenda is in the wiki.",
        200,
    );
    w.post(
        "general",
        "jonas",
        "",
        "Anyone knows a good coffee place near the new office?",
        140,
    );
    let coffee = w.post(
        "general",
        "lena",
        "",
        "The one on Mill Street. Ask for the *flat white*.",
        138,
    );
    w.react(&coffee, "jonas", "coffee");
    w.react(&coffee, "sam", "+1");

    w.post(
        "releases",
        "sam",
        "",
        "Release **v2.14** is tagged. Changelog: https://git.example.org/acme/api/releases/v2.14",
        100,
    );
    w.post(
        "releases",
        "tomas",
        "",
        "Staging looks good, the smoke tests are green.",
        93,
    );
    w.post(
        "releases",
        "sam",
        "",
        "Deploying v2.14 to production in ten minutes :rocket:",
        13,
    );
    w.post(
        "releases",
        "priya",
        "",
        "Canary is clean so far, rolling out to 100%.",
        8,
    );
    w.unread("releases", 2);

    w.post(
        "incidents",
        "jonas",
        "",
        "Latency on `/orders` is back to normal after the cache flush.",
        66,
    );
    w.post(
        "incidents",
        "maya",
        "",
        "Postmortem draft: https://wiki.example.org/pm/2431",
        30,
    );
    w.post(
        "incidents",
        "priya",
        "",
        "Elevated 5xx on the payments worker, looking into it.",
        10,
    );
    w.post(
        "incidents",
        "jonas",
        "",
        "Seeing it too, the queue is backing up.",
        7,
    );
    w.post(
        "incidents",
        "priya",
        "",
        "@alex can you check the worker logs when you get a minute?",
        4,
    );
    w.unread("incidents", 3);

    let migration = w.post(
        "backend",
        "maya",
        "",
        "Morning! The **orders** migration is up for review: https://git.example.org/acme/api/pull/482",
        31,
    );
    let worry = w.post(
        "backend",
        "tomas",
        "",
        "Looking at it now. Does it lock the table? Adding a column to `orders` worries me.",
        29,
    );
    w.post("backend", "jonas", &migration, "Seen, thanks Maya!", 28);
    w.post(
        "backend",
        "maya",
        "",
        "No lock, the default is applied by the database without a rewrite. The core of it:\n```sql\nALTER TABLE orders\n  ADD COLUMN currency text DEFAULT 'EUR';\n```",
        26,
    );
    let notes = w.post(
        "backend",
        "tomas",
        "",
        "> the default is applied without a rewrite\nGood, that answers it. Two small things:\n- rename `currency` to `currency_code`\n- add a test for the _empty table_ case",
        23,
    );
    w.react(&notes, "maya", "+1");
    w.edited(
        &worry,
        "Looking at it now. Does it lock the table? Adding a column to `orders` worries me a bit.",
    );
    w.post("backend", "maya", "", "Both done. Benchmarks attached.", 16);
    let bench = w.file(
        "backend",
        "maya",
        "bench-results.csv",
        "text/csv",
        14_336,
        15,
    );
    w.react(&bench, "tomas", "tada");
    w.react(&bench, "priya", "heart");
    w.react(&bench, "sam", "tada");
    w.post(
        "backend",
        "priya",
        "",
        "Great work both of you! @alex can you review it before we deploy?",
        11,
    );

    w.post(
        "roadmap",
        "priya",
        "",
        "Draft for next quarter is in the doc. Comments welcome until Friday.",
        500,
    );
    w.post(
        "roadmap",
        "sam",
        "",
        "I would move the search work before the billing rewrite.",
        466,
    );

    w.post(
        "dm-maya",
        "maya",
        "",
        "Do you have five minutes later to go over the Q3 plan?",
        23,
    );
    w.unread("dm-maya", 1);
    w.post(
        "dm-tomas",
        "alex",
        "",
        "Thanks for the review yesterday.",
        300,
    );
    w.post("dm-tomas", "tomas", "", "Anytime :+1:", 296);
    w.post("dm-priya", "priya", "", "Lunch at 12:30?", 166);
    w.post("dm-priya", "alex", "", "Sure, see you there.", 165);

    w.post(
        "design-system",
        "lena",
        "",
        "New **button** tokens are published to the registry.",
        266,
    );
    w.post(
        "design-system",
        "sam",
        "",
        "Nice! Does it cover the `danger` variant?",
        260,
    );
    w.post(
        "research",
        "lena",
        "",
        "Interview notes from this week are in the shared folder.",
        400,
    );
    w.post(
        "brand",
        "sam",
        "",
        "The new wordmark is final, exports coming.",
        666,
    );
    w.scripted = Some(("backend".to_owned(), "priya".to_owned()));
    w.finish();
    w
}

fn community() -> World {
    let mut w = World::new(
        "alex",
        &[
            ("nora", "online"),
            ("kai", "away"),
            ("elena", "online"),
            ("bram", "offline"),
        ],
    );
    w.space("club", "Open Source Club");
    w.channel(
        "club",
        true,
        "announcements",
        "Announcements",
        "Meetups and releases",
    );
    w.channel("club", true, "help", "Help", "Ask anything, be kind");
    w.channel(
        "club",
        true,
        "show-and-tell",
        "Show and Tell",
        "What did you build?",
    );
    w.direct("nora");
    w.post(
        "announcements",
        "nora",
        "",
        "The next meetup is on the **12th**, at the library. Pizza included.",
        1000,
    );
    w.post(
        "help",
        "kai",
        "",
        "How do you test code that talks to a socket?",
        60,
    );
    w.post(
        "help",
        "elena",
        "",
        "I bind to port 0 and hand the address to the code under test.",
        56,
    );
    w.post("help", "kai", "", "That is neat, trying it now :eyes:", 53);
    w.unread("help", 2);
    w.post(
        "show-and-tell",
        "bram",
        "",
        "A tiny terminal chat client, finally usable! https://example.org/chat",
        200,
    );
    w.finish();
    w
}

fn handle(shared: &Shared, method: &str, target: &str, body: &Value) -> Option<Value> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query: HashMap<String, String> = query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect();
    let segments: Vec<String> = path
        .trim_start_matches('/')
        .split('/')
        .map(decode)
        .collect();
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    let mut w = lock(shared);
    let me = w.me.clone();
    Some(match (method, segments.as_slice()) {
        ("GET", ["_matrix", "client", "versions"]) => json!({
            "versions": ["v1.1", "v1.2", "v1.3", "v1.4", "v1.5", "v1.6", "v1.7", "v1.8", "v1.9", "v1.10", "v1.11"],
            "unstable_features": {},
        }),
        ("GET", [_, _, _, "login"]) => json!({"flows": [{"type": "m.login.password"}]}),
        ("POST", [_, _, _, "login"]) => json!({
            "user_id": user_id(&me), "access_token": "demo", "device_id": "DEMO", "home_server": SERVER,
        }),
        ("POST", [_, _, _, "keys", "upload"]) => {
            json!({"one_time_key_counts": {"signed_curve25519": 50}})
        }
        ("POST", [_, _, _, "keys", "query"]) => json!({"device_keys": {}, "failures": {}}),
        ("POST", [_, _, _, "keys", "claim"]) => json!({"one_time_keys": {}, "failures": {}}),
        ("GET", [_, _, _, "pushrules"] | [_, _, _, "pushrules", ""]) => json!({"global": {}}),
        ("GET", [_, _, _, "profile", user]) => {
            let name = name_of(user);
            w.knows(name).then(|| json!({"displayname": name}))?
        }
        ("GET", [_, _, _, "presence", user, "status"]) => w.presence(name_of(user))?,
        ("PUT", [_, _, _, "presence", user, "status"]) => {
            let state = match body["presence"].as_str().unwrap_or("online") {
                "unavailable" => "away",
                "org.matrix.msc3026.busy" => "dnd",
                "offline" => "offline",
                _ => "online",
            };
            let name = name_of(user).to_owned();
            if let Some(entry) = w.people.iter_mut().find(|(n, _)| *n == name) {
                entry.1 = state.to_owned();
            }
            json!({})
        }
        ("GET", [_, _, _, "user", _, "account_data", "m.direct"]) => w.direct_content(),
        ("PUT", [_, _, _, "user", _, "account_data", _]) => json!({}),
        ("GET", [_, _, _, "rooms", room, "messages"]) => {
            let limit = query
                .get("limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(10);
            w.page(key_of(room), query.get("from").map(String::as_str), limit)
        }
        ("GET", [_, _, _, "rooms", room, "context", event]) => w.context(key_of(room), event)?,
        ("GET", [_, _, _, "rooms", room, "event", event]) => w
            .events
            .iter()
            .find(|e| e.room == key_of(room) && e.body["event_id"] == *event)
            .map(|e| e.body.clone())?,
        ("GET", [_, _, _, "rooms", room, "relations", event, ..]) => {
            let chunk: Vec<&Value> = w
                .events
                .iter()
                .filter(|e| e.room == key_of(room))
                .filter(|e| e.body.pointer("/content/m.relates_to/event_id") == Some(&json!(event)))
                .map(|e| &e.body)
                .collect();
            json!({"chunk": chunk})
        }
        ("GET", [_, _, _, "rooms", room, "members"]) => {
            let found = w.room(key_of(room))?;
            let chunk: Vec<Value> = w
                .state(found)
                .into_iter()
                .filter(|e| e["type"] == "m.room.member")
                .collect();
            json!({"chunk": chunk})
        }
        ("PUT", [_, _, _, "rooms", room, "send", kind, _]) => {
            let key = key_of(room).to_owned();
            w.room(&key)?;
            let id = w.add(&key, &me, kind, body.clone(), 0);
            let edit = body.pointer("/m.relates_to/rel_type") == Some(&json!("m.replace"));
            if *kind == "m.room.message" && !edit {
                if let Some(found) = w.rooms.iter_mut().find(|r| r.key == key) {
                    found.read = Some(id.clone());
                }
                if w.scripted.as_ref().is_some_and(|(r, _)| *r == key) {
                    if let Some((key, who)) = w.scripted.take() {
                        tokio::spawn(answer(shared.clone(), key, who, id.clone()));
                    }
                }
            }
            json!({"event_id": id})
        }
        ("PUT", [_, _, _, "rooms", room, "redact", event, _]) => {
            json!({"event_id": w.redact(key_of(room), event)})
        }
        ("PUT", [_, _, _, "rooms", _, "state", ..]) => json!({"event_id": "$state"}),
        ("PUT", [_, _, _, "rooms", _, "typing", _]) => json!({}),
        ("POST", [_, _, _, "rooms", room, "receipt", _, event]) => {
            if let Some(found) = w.rooms.iter_mut().find(|r| r.key == key_of(room)) {
                found.read = Some((*event).to_owned());
            }
            json!({})
        }
        ("POST", [_, _, _, "rooms", _, "read_markers" | "leave" | "forget"]) => json!({}),
        ("POST", [_, _, _, "createRoom"]) => {
            let other = body["invite"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(|id| name_of(id).to_owned())
                .find(|name| w.knows(name))?;
            let key = w.direct(&other);
            w.direct_at = w.pos;
            json!({"room_id": room_id(&key)})
        }
        ("POST", [_, _, _, "join", room]) => {
            w.room(key_of(room))?;
            json!({"room_id": room})
        }
        _ => return None,
    })
}

async fn sync(shared: &Shared, target: &str) -> Value {
    let query: HashMap<String, String> = target
        .split_once('?')
        .map_or("", |(_, q)| q)
        .split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect();
    let since = query
        .get("since")
        .and_then(|s| s.strip_prefix('s'))
        .and_then(|s| s.parse::<u64>().ok());
    let wait = query
        .get("timeout")
        .and_then(|t| t.parse::<u64>().ok())
        .map_or(Duration::ZERO, Duration::from_millis)
        .min(LONG_POLL_CAP);
    let started = Instant::now();
    loop {
        {
            let w = lock(shared);
            if since.is_none_or(|s| w.pos > s) || started.elapsed() >= wait {
                return w.sync_body(since);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn answer(shared: Shared, room: String, who: String, reply_to: String) {
    tokio::time::sleep(Duration::from_millis(700)).await;
    lock(&shared).typing(&room, std::slice::from_ref(&who));
    tokio::time::sleep(Duration::from_millis(2600)).await;
    {
        let mut w = lock(&shared);
        w.typing(&room, &[]);
        let text = "Perfect, thanks Alex! :tada: I will watch the dashboards when it goes out.";
        w.post(&room, &who, "", text, 0);
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    lock(&shared).react(&reply_to, "maya", "+1");
}

struct Request {
    method: String,
    target: String,
    body: Value,
}

async fn read_request(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<Request> {
    let end = loop {
        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let length = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_owned)
        })
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < end + length {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
        buffer.extend_from_slice(&chunk[..n]);
    }
    let body = serde_json::from_slice(&buffer[end..end + length]).unwrap_or(Value::Null);
    buffer.drain(..end + length);
    let mut words = head.split_whitespace();
    Some(Request {
        method: words.next()?.to_owned(),
        target: words.next()?.to_owned(),
        body,
    })
}

async fn connection(mut stream: TcpStream, shared: Shared) {
    let mut buffer = Vec::new();
    while let Some(request) = read_request(&mut stream, &mut buffer).await {
        let path = request.target.split('?').next().unwrap_or_default();
        let answer = match (request.method.as_str(), path.ends_with("/sync")) {
            ("GET", true) => Some(sync(&shared, &request.target).await),
            _ => handle(&shared, &request.method, &request.target, &request.body),
        };
        let (status, body) = match answer {
            Some(value) => ("200 OK", value),
            None => (
                "404 Not Found",
                json!({"errcode": "M_NOT_FOUND", "error": "not found"}),
            ),
        };
        let body = body.to_string();
        let head = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        if stream
            .write_all(format!("{head}{body}").as_bytes())
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn serve(listener: TcpListener, shared: Shared) {
    loop {
        if let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(connection(stream, shared.clone()));
        }
    }
}

fn main() -> Result<()> {
    let home = tempfile::tempdir()?;
    let (ports, found) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async {
            let (Ok(a), Ok(b)) = (
                TcpListener::bind("127.0.0.1:0").await,
                TcpListener::bind("127.0.0.1:0").await,
            ) else {
                return;
            };
            let ports = ports.send((
                a.local_addr().map(|a| a.port()).unwrap_or(0),
                b.local_addr().map(|a| a.port()).unwrap_or(0),
            ));
            if ports.is_ok() {
                tokio::join!(
                    serve(a, Arc::new(Mutex::new(work()))),
                    serve(b, Arc::new(Mutex::new(community())))
                );
            }
        });
    });
    let (work_port, club_port) = found.recv().context("the fake servers did not start")?;

    let config = home.path().join("mx");
    std::fs::create_dir_all(&config)?;
    let servers = config.join("servers.toml");
    std::fs::write(
        &servers,
        format!(
            "[[server]]\nname = \"work\"\nurl = \"http://127.0.0.1:{work_port}\"\nlogin = \"alex\"\npassword = \"demo\"\n\n\
             [[server]]\nname = \"club\"\nurl = \"http://127.0.0.1:{club_port}\"\nlogin = \"alex\"\npassword = \"demo\"\n"
        ),
    )?;
    std::fs::set_permissions(&servers, std::fs::Permissions::from_mode(0o600))?;

    let beside = std::env::current_exe()?;
    let program = beside
        .parent()
        .and_then(|examples| examples.parent())
        .map(|target| target.join("mx"))
        .context("cannot find the mx binary")?;
    Command::new(&program)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_DATA_HOME", home.path().join("data"))
        .status()
        .with_context(|| format!("running {}", program.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(shared: &Shared, path: &str) -> Value {
        handle(
            shared,
            "GET",
            &format!("/_matrix/client/v3/{path}"),
            &Value::Null,
        )
        .unwrap_or_else(|| panic!("{path} is not served"))
    }

    #[test]
    fn every_read_the_app_makes_is_served() {
        for world in [work(), community()] {
            let shared: Shared = Arc::new(Mutex::new(world));
            let sync = lock(&shared).sync_body(None);
            let rooms = sync["rooms"]["join"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            assert!(!rooms.is_empty());
            for (room, entry) in &rooms {
                let members = entry["state"]["events"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|e| e["type"] == "m.room.member")
                    .count();
                assert!(members >= 2, "{room} has no members");
                assert!(
                    get(&shared, &format!("rooms/{room}/messages?dir=b&limit=60"))["chunk"]
                        .is_array()
                );
                assert!(get(&shared, &format!("rooms/{room}/members"))["chunk"].is_array());
            }
            assert_eq!(
                get(&shared, "profile/@alex:demo.local")["displayname"],
                "alex"
            );
            assert_eq!(
                get(&shared, "presence/@alex:demo.local/status")["presence"],
                "online"
            );
            assert!(sync["account_data"]["events"][0]["content"].is_object());
        }
    }

    #[test]
    fn unread_counts_follow_the_seeded_receipt() {
        let shared: Shared = Arc::new(Mutex::new(work()));
        let w = lock(&shared);
        let room = w
            .room("releases")
            .map(|r| r.key.clone())
            .unwrap_or_default();
        let read = w
            .room(&room)
            .and_then(|r| r.read.clone())
            .unwrap_or_default();
        let viewed = w
            .events
            .iter()
            .find(|e| e.body["event_id"] == read)
            .and_then(|e| e.body["origin_server_ts"].as_i64())
            .unwrap_or_default();
        let unread = w
            .events
            .iter()
            .filter(|e| e.room == room && e.body["sender"] != user_id("alex"))
            .filter(|e| e.body["origin_server_ts"].as_i64().unwrap_or_default() > viewed)
            .count();
        assert_eq!(unread, 2);
    }

    #[test]
    fn a_written_message_is_kept_and_read_back() {
        let shared: Shared = Arc::new(Mutex::new(work()));
        let path = "/_matrix/client/v3/rooms/!general:demo.local/send/m.room.message/t1";
        let body = json!({"msgtype": "m.text", "body": "hello"});
        let sent = handle(&shared, "PUT", path, &body);
        assert!(sent.is_some_and(|s| s["event_id"].is_string()));
        let page = get(&shared, "rooms/!general:demo.local/messages?dir=b&limit=1");
        assert_eq!(page["chunk"][0]["content"]["body"], "hello");
    }

    #[test]
    fn a_direct_room_is_created_once() {
        let shared: Shared = Arc::new(Mutex::new(work()));
        let body = json!({"invite": ["@jonas:demo.local"], "is_direct": true});
        let path = "/_matrix/client/v3/createRoom";
        let first = handle(&shared, "POST", path, &body);
        let again = handle(&shared, "POST", path, &body);
        assert!(first.is_some() && first == again);
    }

    #[test]
    fn a_sync_after_a_write_brings_only_what_is_new() {
        let shared: Shared = Arc::new(Mutex::new(work()));
        let before = lock(&shared).pos;
        let path = "/_matrix/client/v3/rooms/!general:demo.local/send/m.room.message/t1";
        let body = json!({"msgtype": "m.text", "body": "hi"});
        let _ = handle(&shared, "PUT", path, &body);
        let delta = lock(&shared).sync_body(Some(before));
        let rooms = delta["rooms"]["join"].as_object().map(serde_json::Map::len);
        assert_eq!(rooms, Some(1));
        let after = lock(&shared).pos;
        let quiet = lock(&shared).sync_body(Some(after));
        assert_eq!(
            quiet["rooms"]["join"].as_object().map(serde_json::Map::len),
            Some(0)
        );
    }

    #[tokio::test]
    async fn the_long_poll_returns_empty_when_nothing_happens() {
        let shared: Shared = Arc::new(Mutex::new(work()));
        let pos = lock(&shared).pos;
        let target = format!("/_matrix/client/v3/sync?since=s{pos}&timeout=150");
        let started = Instant::now();
        let body = sync(&shared, &target).await;
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert_eq!(
            body["rooms"]["join"].as_object().map(serde_json::Map::len),
            Some(0)
        );
    }
}
