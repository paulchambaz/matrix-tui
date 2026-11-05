//! Tests of what the client sends to the server and asks of it, against a mock server.

use std::sync::OnceLock;

use serde_json::{json, Value};
use wiremock::matchers::{method, path_regex, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;

const ROOM: &str = "!r:x.org";
const ME: &str = "@me:x.org";

fn home() {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().expect("temporary home");
        std::env::set_var("HOME", dir.path());
        std::env::set_var("XDG_DATA_HOME", dir.path().join("data"));
        dir
    });
}

fn event(id: &str, sender: &str, body: &str, ts: i64) -> Value {
    json!({
        "type": "m.room.message", "event_id": id, "sender": sender, "room_id": ROOM,
        "origin_server_ts": ts, "content": {"msgtype": "m.text", "body": body},
    })
}

fn state(kind: &str, key: &str, content: Value) -> Value {
    json!({
        "type": kind, "state_key": key, "sender": ME, "event_id": format!("$s-{kind}-{key}"),
        "room_id": ROOM, "origin_server_ts": 1, "content": content,
    })
}

async fn answer(server: &MockServer, verb: &str, route: &str, body: Value) {
    Mock::given(method(verb))
        .and(path_regex(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn world(name: &str) -> (MockServer, Client) {
    let credentials = Credentials::Password {
        login: "me".into(),
        password: "pw".into(),
    };
    world_with(name, credentials).await
}

async fn world_with(name: &str, credentials: Credentials) -> (MockServer, Client) {
    home();
    let server = MockServer::start().await;
    answer(
        &server,
        "GET",
        r"/versions$",
        json!({"versions": ["v1.11"], "unstable_features": {}}),
    )
    .await;
    answer(
        &server,
        "POST",
        r"/login$",
        json!({"user_id": ME, "access_token": "t", "device_id": "D"}),
    )
    .await;
    answer(
        &server,
        "POST",
        r"/keys/upload$",
        json!({"one_time_key_counts": {"signed_curve25519": 50}}),
    )
    .await;
    answer(
        &server,
        "POST",
        r"/keys/query$",
        json!({"device_keys": {}, "failures": {}}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path_regex(r"/state/m\.room\.encryption/$"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"errcode": "M_NOT_FOUND", "error": "not found"})),
        )
        .mount(&server)
        .await;
    let timeline: Vec<Value> = (1..=4)
        .map(|i| event(&format!("$m{i}"), "@you:x.org", &format!("message {i}"), i))
        .collect();
    let events = vec![
        state("m.room.create", "", json!({"room_version": "11"})),
        state("m.room.name", "", json!({"name": "general"})),
        state("m.room.member", ME, json!({"membership": "join"})),
        state("m.room.member", "@you:x.org", json!({"membership": "join"})),
        state(
            "m.room.power_levels",
            "",
            json!({"users": {ME: 100}, "events_default": 0, "state_default": 50}),
        ),
    ];
    answer(
        &server,
        "GET",
        r"/sync$",
        json!({
            "next_batch": "s1",
            "rooms": {"join": {ROOM: {
                "state": {"events": events},
                "timeline": {"events": timeline, "limited": false, "prev_batch": "t0"},
            }}},
            "device_one_time_keys_count": {"signed_curve25519": 50},
        }),
    )
    .await;
    oauth(&server).await;
    let client = Client::connect(name, &server.uri(), credentials)
        .await
        .expect("connected to the mock server");
    (server, client)
}

async fn oauth(server: &MockServer) {
    let issuer = format!("{}/", server.uri());
    let at = |route: &str| format!("{issuer}{route}");
    answer(
        server,
        "GET",
        r"/auth_metadata$",
        json!({
            "issuer": issuer, "authorization_endpoint": at("oauth2/authorize"),
            "token_endpoint": at("oauth2/token"), "registration_endpoint": at("oauth2/registration"),
            "response_types_supported": ["code"], "response_modes_supported": ["query", "fragment"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"], "revocation_endpoint": at("oauth2/revoke"),
        }),
    )
    .await;
    answer(
        server,
        "POST",
        r"/oauth2/registration$",
        json!({"client_id": "mx-client", "client_id_issued_at": 1}),
    )
    .await;
    answer(
        server,
        "POST",
        r"/oauth2/token$",
        json!({"access_token": "at", "expires_in": 300, "refresh_token": "rt", "token_type": "Bearer"}),
    )
    .await;
    answer(
        server,
        "GET",
        r"/account/whoami$",
        json!({"user_id": ME, "device_id": "D"}),
    )
    .await;
}

async fn sent(server: &MockServer, verb: &str, route: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method.as_str() == verb && r.url.path().contains(route))
        .collect()
}

fn body_of(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap_or_default()
}

fn room() -> ChannelId {
    ChannelId(ROOM.into())
}

#[tokio::test]
async fn sending_posts_the_text_and_returns_the_new_post() {
    let (server, client) = world("send").await;
    answer(
        &server,
        "PUT",
        r"/send/m\.room\.message/",
        json!({"event_id": "$sent"}),
    )
    .await;
    let post = client
        .create_post(&room(), "hello", None)
        .await
        .expect("sent");
    let bodies = sent(&server, "PUT", "/send/m.room.message/").await;
    assert_eq!(bodies.len(), 1);
    let content = body_of(&bodies[0]);
    assert_eq!(content["body"], "hello");
    assert!(content.get("m.relates_to").is_none());
    assert_eq!(post.id, PostId("$sent".into()));
    assert_eq!(post.message, "hello");
    assert_eq!(post.channel_id, room());
    assert_eq!(post.root_id, "");
}

#[tokio::test]
async fn a_reply_points_at_its_message_with_no_quoted_fallback() {
    let (server, client) = world("reply").await;
    answer(
        &server,
        "PUT",
        r"/send/m\.room\.message/",
        json!({"event_id": "$sent"}),
    )
    .await;
    answer(
        &server,
        "GET",
        r"/event/",
        event("$m1", "@you:x.org", "message 1", 1),
    )
    .await;
    let root = PostId("$m1".into());
    let post = client
        .create_post(&room(), "yes", Some(&root))
        .await
        .expect("sent");
    let bodies = sent(&server, "PUT", "/send/m.room.message/").await;
    assert_eq!(bodies.len(), 1);
    let content = body_of(&bodies[0]);
    assert_eq!(content["m.relates_to"]["m.in_reply_to"]["event_id"], "$m1");
    assert!(!content["body"]
        .as_str()
        .unwrap_or_default()
        .starts_with("> "));
    assert_eq!(post.root_id, "$m1");
}

#[tokio::test]
async fn a_reply_inside_a_thread_stays_in_that_thread() {
    let (server, client) = world("thread-reply").await;
    answer(
        &server,
        "PUT",
        r"/send/m\.room\.message/",
        json!({"event_id": "$sent"}),
    )
    .await;
    let mut inside = event("$m2", "@you:x.org", "in the thread", 2);
    inside["content"]["m.relates_to"] = json!({
        "rel_type": "m.thread", "event_id": "$m1", "is_falling_back": true,
        "m.in_reply_to": {"event_id": "$m1"}
    });
    answer(&server, "GET", r"/event/", inside).await;
    let post = client
        .create_post(&room(), "yes", Some(&PostId("$m2".into())))
        .await
        .expect("sent");
    let bodies = sent(&server, "PUT", "/send/m.room.message/").await;
    let relation = &body_of(&bodies[0])["m.relates_to"];
    assert_eq!(relation["rel_type"], "m.thread");
    assert_eq!(relation["event_id"], "$m1");
    assert_eq!(relation["m.in_reply_to"]["event_id"], "$m2");
    assert_ne!(relation["is_falling_back"], true);
    assert_eq!(post.root_id, "$m1");
}

#[tokio::test]
async fn a_thread_is_its_root_and_the_messages_in_it() {
    let (server, client) = world("thread-list").await;
    answer(
        &server,
        "GET",
        r"/event/",
        event("$m1", "@you:x.org", "root", 1),
    )
    .await;
    let mut inside = event("$m2", "@you:x.org", "inside", 2);
    inside["content"]["m.relates_to"] = json!({"rel_type": "m.thread", "event_id": "$m1"});
    answer(&server, "GET", r"/relations/", json!({"chunk": [inside]})).await;
    let list = client
        .thread(&room(), &PostId("$m1".into()))
        .await
        .expect("thread");
    assert_eq!(list.order.len(), 2);
    assert_eq!(list.posts[&PostId("$m2".into())].root_id, "$m1");
}

#[tokio::test]
async fn deleting_a_message_redacts_that_event() {
    let (server, client) = world("delete").await;
    answer(&server, "PUT", r"/redact/", json!({"event_id": "$red"})).await;
    client
        .delete_post(&room(), &PostId("$m1".into()))
        .await
        .expect("redacted");
    let redactions = sent(&server, "PUT", "/redact/").await;
    assert_eq!(redactions.len(), 1);
    assert!(redactions[0].url.path().contains("/redact/$m1/"));
    assert!(sent(&server, "DELETE", "/").await.is_empty());
}

#[tokio::test]
async fn members_are_invited_kicked_banned_and_unbanned_by_their_id() {
    let (server, client) = world("members").await;
    for verb in ["invite", "kick", "ban", "unban"] {
        answer(&server, "POST", &format!("/{verb}$"), json!({})).await;
    }
    let you = UserId("@you:x.org".into());
    client.invite(&room(), &you).await.expect("invited");
    client.kick(&room(), &you).await.expect("kicked");
    client.ban(&room(), &you).await.expect("banned");
    client.unban(&room(), &you).await.expect("unbanned");
    for verb in ["invite", "kick", "ban", "unban"] {
        let calls = sent(&server, "POST", &format!("/{verb}")).await;
        assert_eq!(calls.len(), 1, "{verb}");
        assert_eq!(body_of(&calls[0])["user_id"], "@you:x.org", "{verb}");
    }
}

#[tokio::test]
async fn renaming_and_the_topic_are_state_events_of_the_room() {
    let (server, client) = world("rename").await;
    answer(&server, "PUT", r"/state/", json!({"event_id": "$s"})).await;
    client.rename(&room(), "lounge").await.expect("renamed");
    client.set_topic(&room(), "tea").await.expect("topic set");
    let names = sent(&server, "PUT", "/state/m.room.name").await;
    assert_eq!(body_of(&names[0])["name"], "lounge");
    let topics = sent(&server, "PUT", "/state/m.room.topic").await;
    assert_eq!(body_of(&topics[0])["topic"], "tea");
}

#[tokio::test]
async fn a_new_nick_is_the_display_name_of_the_profile() {
    let (server, client) = world("nick").await;
    answer(&server, "PUT", r"/displayname$", json!({})).await;
    client.set_nick("Paul").await.expect("renamed");
    let calls = sent(&server, "PUT", "/displayname").await;
    assert_eq!(body_of(&calls[0])["displayname"], "Paul");
}

#[tokio::test]
async fn a_private_room_is_invite_only_and_encrypted() {
    let (server, client) = world("private").await;
    answer(
        &server,
        "POST",
        r"/createRoom$",
        json!({"room_id": "!new:x.org"}),
    )
    .await;
    let channel = client.create_room("club", false).await.expect("created");
    assert_eq!(channel.id.0, "!new:x.org");
    let calls = sent(&server, "POST", "/createRoom").await;
    let request = body_of(&calls[0]);
    assert_eq!(request["name"], "club");
    assert_eq!(request["preset"], "private_chat");
    assert_eq!(request["initial_state"][0]["type"], "m.room.encryption");
}

#[tokio::test]
async fn a_space_is_a_room_of_type_space() {
    let (server, client) = world("space").await;
    answer(
        &server,
        "POST",
        r"/createRoom$",
        json!({"room_id": "!new:x.org"}),
    )
    .await;
    client.create_space("club").await.expect("created");
    let calls = sent(&server, "POST", "/createRoom").await;
    let request = body_of(&calls[0]);
    assert_eq!(request["name"], "club");
    assert_eq!(request["creation_content"]["type"], "m.space");
}

#[tokio::test]
async fn a_room_enters_a_space_through_a_child_event_and_leaves_with_an_empty_one() {
    let (server, client) = world("child").await;
    answer(
        &server,
        "PUT",
        r"/state/m.space.child/",
        json!({"event_id": "$e"}),
    )
    .await;
    let (space, child) = (TeamId(ROOM.into()), ChannelId("!child:x.org".into()));
    client.space_add(&space, &child).await.expect("added");
    client.space_remove(&space, &child).await.expect("removed");
    let calls = sent(&server, "PUT", "/state/m.space.child/").await;
    assert!(calls[0].url.path().ends_with("/m.space.child/!child:x.org"));
    assert_eq!(body_of(&calls[0])["via"][0], "x.org");
    assert_eq!(body_of(&calls[1]), json!({}));
    let home = TeamId("home".into());
    assert!(client.space_add(&home, &child).await.is_err());
}

#[tokio::test]
async fn a_public_room_is_open_and_not_encrypted() {
    let (server, client) = world("public").await;
    answer(
        &server,
        "POST",
        r"/createRoom$",
        json!({"room_id": "!new:x.org"}),
    )
    .await;
    client.create_room("agora", true).await.expect("created");
    let calls = sent(&server, "POST", "/createRoom").await;
    let request = body_of(&calls[0]);
    assert_eq!(request["preset"], "public_chat");
    assert!(request["initial_state"]
        .as_array()
        .is_none_or(Vec::is_empty));
}

#[tokio::test]
async fn a_group_is_a_direct_room_that_invites_everyone() {
    let (server, client) = world("group").await;
    answer(
        &server,
        "POST",
        r"/createRoom$",
        json!({"room_id": "!new:x.org"}),
    )
    .await;
    answer(&server, "PUT", r"/account_data/", json!({})).await;
    let others = [UserId("@a:x.org".into()), UserId("@b:x.org".into())];
    client.create_group(&others).await.expect("created");
    let calls = sent(&server, "POST", "/createRoom").await;
    let request = body_of(&calls[0]);
    assert_eq!(request["is_direct"], true);
    assert_eq!(request["invite"], json!(["@a:x.org", "@b:x.org"]));
    assert_eq!(request["initial_state"][0]["type"], "m.room.encryption");
}

#[tokio::test]
async fn a_matrix_to_link_names_the_room_it_points_at() {
    let (server, client) = world("link").await;
    answer(
        &server,
        "GET",
        r"/directory/room/",
        json!({"room_id": ROOM, "servers": []}),
    )
    .await;
    let team = TeamId("home".into());
    let found = client
        .channel_named(&team, "https://matrix.to/#/%23general:x.org")
        .await
        .expect("resolved");
    assert_eq!(found.id, room());
    let found = client
        .channel_named(&team, "matrix:r/general:x.org")
        .await
        .expect("resolved");
    assert_eq!(found.id, room());
}

#[tokio::test]
async fn removing_a_reaction_redacts_the_reaction_not_the_message() {
    let (server, client) = world("react").await;
    answer(
        &server,
        "PUT",
        r"/send/m\.reaction/",
        json!({"event_id": "$react"}),
    )
    .await;
    answer(&server, "PUT", r"/redact/", json!({"event_id": "$red"})).await;
    let post = PostId("$m1".into());
    client.react(&room(), &post, "+1").await.expect("reacted");
    let reactions = sent(&server, "PUT", "/send/m.reaction/").await;
    assert_eq!(reactions.len(), 1);
    let relation = &body_of(&reactions[0])["m.relates_to"];
    assert_eq!(relation["rel_type"], "m.annotation");
    assert_eq!(relation["event_id"], "$m1");
    assert_eq!(relation["key"], "👍");

    assert!(client.unreact(&room(), &post, "tada").await.is_err());
    assert!(sent(&server, "PUT", "/redact/").await.is_empty());

    client
        .unreact(&room(), &post, "+1")
        .await
        .expect("unreacted");
    let redactions = sent(&server, "PUT", "/redact/").await;
    assert_eq!(redactions.len(), 1);
    assert!(redactions[0].url.path().contains("/redact/$react/"));

    assert!(client.unreact(&room(), &post, "+1").await.is_err());
    assert_eq!(sent(&server, "PUT", "/redact/").await.len(), 1);
}

fn page(ids: &[(&str, i64)], end: Option<&str>) -> Value {
    let chunk: Vec<Value> = ids
        .iter()
        .map(|(id, ts)| event(id, "@you:x.org", id, *ts))
        .collect();
    let mut body = json!({"start": "t", "chunk": chunk, "state": []});
    if let Some(end) = end {
        body["end"] = json!(end);
    }
    body
}

async fn messages(server: &MockServer, from: Option<&str>, body: Value) {
    let given = Mock::given(method("GET")).and(path_regex(r"/messages$"));
    let given = match from {
        Some(token) => given.and(query_param("from", token)),
        None => given.and(query_param_is_missing("from")),
    };
    given
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn the_previous_page_is_asked_with_the_token_of_the_page_before() {
    let (server, client) = world("pages").await;
    messages(&server, None, page(&[("$m4", 4), ("$m3", 3)], Some("t1"))).await;
    messages(
        &server,
        Some("t1"),
        page(&[("$m2", 2), ("$m1", 1)], Some("t0")),
    )
    .await;
    let newest = client.posts(&room(), None, 2).await.expect("first page");
    assert_eq!(
        newest.order,
        vec![PostId("$m4".into()), PostId("$m3".into())]
    );
    let older = client
        .posts(&room(), Some(&PostId("$m3".into())), 2)
        .await
        .expect("previous page");
    assert_eq!(
        older.order,
        vec![PostId("$m2".into()), PostId("$m1".into())]
    );
    assert!(sent(&server, "GET", "/context/").await.is_empty());
    let asked = sent(&server, "GET", "/messages").await;
    assert_eq!(asked.len(), 2);
    assert_eq!(
        asked[1]
            .url
            .query_pairs()
            .find(|(k, _)| k == "from")
            .map(|(_, v)| v.into_owned()),
        Some("t1".into())
    );
}

#[tokio::test]
async fn without_a_known_token_the_page_starts_from_the_context_of_the_message() {
    let (server, client) = world("context").await;
    answer(
        &server,
        "GET",
        r"/context/",
        json!({
            "start": "t5", "end": "t6",
            "event": event("$m3", "@you:x.org", "message 3", 3),
            "events_before": [], "events_after": [], "state": [],
        }),
    )
    .await;
    messages(
        &server,
        Some("t5"),
        page(&[("$m2", 2), ("$m1", 1)], Some("t0")),
    )
    .await;
    let older = client
        .posts(&room(), Some(&PostId("$m3".into())), 2)
        .await
        .expect("previous page");
    assert_eq!(
        older.order,
        vec![PostId("$m2".into()), PostId("$m1".into())]
    );
    let context = sent(&server, "GET", "/context/").await;
    assert_eq!(context.len(), 1);
    assert!(context[0].url.path().contains("/context/$m3"));
}

#[tokio::test]
async fn a_message_the_server_cannot_place_gives_an_empty_page() {
    let (server, client) = world("nowhere").await;
    answer(
        &server,
        "GET",
        r"/context/",
        json!({
            "event": event("$m3", "@you:x.org", "message 3", 3),
            "events_before": [], "events_after": [], "state": [],
        }),
    )
    .await;
    let older = client
        .posts(&room(), Some(&PostId("$m3".into())), 2)
        .await
        .expect("empty page");
    assert!(older.order.is_empty());
    assert!(sent(&server, "GET", "/messages").await.is_empty());
}

fn secret_storage(path: &str) -> String {
    format!(r"/user/[^/]+/account_data/m\.secret_storage\.{path}$")
}

#[tokio::test]
async fn recovering_on_an_account_without_a_recovery_key_says_so() {
    let (server, client) = world("norecovery").await;
    Mock::given(method("GET"))
        .and(path_regex(secret_storage("default_key")))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"errcode": "M_NOT_FOUND", "error": "not found"})),
        )
        .mount(&server)
        .await;
    let e = client.recover("whatever").await.expect_err("no recovery");
    assert_eq!(e.to_string(), "This account has no recovery key");
}

#[tokio::test]
async fn verifying_an_account_without_cross_signing_keys_says_so() {
    let (_server, client) = world("nocross").await;
    let e = client.verify().await.expect_err("no keys");
    assert_eq!(
        e.to_string(),
        "This account has no cross-signing keys; set them up in another client"
    );
}

#[tokio::test]
async fn recovering_with_a_wrong_key_says_it_is_not_the_key() {
    let (server, client) = world("wrongkey").await;
    answer(
        &server,
        "GET",
        &secret_storage("default_key"),
        json!({"key": "K"}),
    )
    .await;
    answer(
        &server,
        "GET",
        &secret_storage("key\\.K"),
        json!({"algorithm": "m.secret_storage.v1.aes-hmac-sha2", "iv": "AAAAAAAAAAAAAAAAAAAAAA", "mac": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}),
    )
    .await;
    for key in ["wrongkey", "not a key at all !!"] {
        let e = client.recover(key).await.expect_err("wrong key");
        assert_eq!(
            e.to_string(),
            "This is not the recovery key of this account"
        );
    }
}

async fn media(server: &MockServer, limit: u64) {
    answer(
        server,
        "GET",
        r"/media/(v3/)?config",
        json!({"m.upload.size": limit}),
    )
    .await;
    answer(
        server,
        "POST",
        r"/media/v3/upload",
        json!({"content_uri": "mxc://x.org/abc"}),
    )
    .await;
    answer(
        server,
        "PUT",
        r"/send/m\.room\.message/",
        json!({"event_id": "$sent"}),
    )
    .await;
}

fn file(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).expect("file");
    (dir, path)
}

#[tokio::test]
async fn uploading_a_png_posts_the_bytes_then_an_image_event() {
    let (server, client) = world("upload-png").await;
    media(&server, 1 << 20).await;
    let (_dir, path) = file("cat.PNG", b"pixels");
    client.upload(&room(), &path).await.expect("sent");
    let uploads = sent(&server, "POST", "/media/v3/upload").await;
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].body, b"pixels");
    assert_eq!(
        uploads[0].headers.get("content-type").map(|v| v.as_bytes()),
        Some(&b"image/png"[..])
    );
    let events = sent(&server, "PUT", "/send/m.room.message/").await;
    assert_eq!(events.len(), 1);
    let content = body_of(&events[0]);
    assert_eq!(content["msgtype"], "m.image");
    assert_eq!(content["url"], "mxc://x.org/abc");
    assert_eq!(content["body"], "cat.PNG");
    assert_eq!(content["info"]["size"], 6);
}

#[tokio::test]
async fn an_unknown_extension_is_sent_as_a_plain_file() {
    let (server, client) = world("upload-bin").await;
    media(&server, 1 << 20).await;
    let (_dir, path) = file("blob.xyz", b"data");
    client.upload(&room(), &path).await.expect("sent");
    let uploads = sent(&server, "POST", "/media/v3/upload").await;
    assert_eq!(
        uploads[0].headers.get("content-type").map(|v| v.as_bytes()),
        Some(&b"application/octet-stream"[..])
    );
    let content = body_of(&sent(&server, "PUT", "/send/m.room.message/").await[0]);
    assert_eq!(content["msgtype"], "m.file");
}

#[tokio::test]
async fn a_file_over_the_server_limit_is_refused_before_sending() {
    let (server, client) = world("upload-big").await;
    media(&server, 3).await;
    let (_dir, path) = file("big.txt", b"too long");
    let error = client.upload(&room(), &path).await.expect_err("refused");
    assert!(error.to_string().contains("accepts 3 bytes"), "{error}");
    assert!(sent(&server, "POST", "/media/v3/upload").await.is_empty());
    assert!(sent(&server, "PUT", "/send/m.room.message/")
        .await
        .is_empty());
}

#[tokio::test]
async fn a_missing_file_says_so_and_asks_nothing() {
    let (server, client) = world("upload-missing").await;
    media(&server, 1 << 20).await;
    let error = client
        .upload(&room(), std::path::Path::new("/nonexistent/a.png"))
        .await
        .expect_err("missing");
    assert!(error.to_string().contains("/nonexistent/a.png"), "{error}");
    assert!(sent(&server, "POST", "/media/v3/upload").await.is_empty());
}

#[tokio::test]
async fn sso_opens_the_browser_then_trades_the_token_for_a_session() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("temporary directory");
    let script = dir.path().join("browser.sh");
    let seen = dir.path().join("address");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$1\" > {}\nback=$(echo \"$1\" | sed 's/.*redirectUrl=//;s/&.*//;s/%3A/:/g;s/%2F/\\//g')\ncurl -s -o /dev/null \"$back?loginToken=abc\"\n",
            seen.display()
        ),
    )
    .expect("script written");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("script executable");
    let credentials = Credentials::Sso {
        browser: script.display().to_string(),
    };
    let (server, _client) = world_with("sso", credentials).await;
    let address = std::fs::read_to_string(&seen).expect("the browser was started");
    assert!(address.contains("/login/sso/redirect"));
    assert!(address.contains("redirectUrl=http"));
    let logins = sent(&server, "POST", "/login").await;
    assert_eq!(logins.len(), 1);
    assert_eq!(body_of(&logins[0])["type"], "m.login.token");
    assert_eq!(body_of(&logins[0])["token"], "abc");
}

#[tokio::test]
async fn a_browser_that_cannot_start_names_the_address_to_open() {
    home();
    let server = MockServer::start().await;
    answer(
        &server,
        "GET",
        r"/versions$",
        json!({"versions": ["v1.11"], "unstable_features": {}}),
    )
    .await;
    let credentials = Credentials::Sso {
        browser: "/nonexistent/browser".into(),
    };
    let error = Client::connect("sso-broken", &server.uri(), credentials)
        .await
        .expect_err("the browser does not exist");
    assert!(error.to_string().contains("open http"));
}

#[tokio::test]
async fn an_sso_session_that_expired_asks_for_a_restart_and_sends_nothing() {
    let (server, mut client) = world("sso-expired").await;
    client.credentials = Credentials::Sso {
        browser: "true".into(),
    };
    let before = sent(&server, "POST", "/login").await.len();
    let error = client.relogin().await.expect_err("no live browser login");
    assert!(error.to_string().contains("restart mx"));
    assert_eq!(sent(&server, "POST", "/login").await.len(), before);
}

#[tokio::test]
async fn a_search_keeps_the_messages_that_hold_the_text_whatever_its_case() {
    let (server, client) = world("search").await;
    let chunk: Vec<Value> = [("$c", "PIZZA again"), ("$b", "soup"), ("$a", "Pizza time")]
        .iter()
        .zip(1..)
        .map(|((id, body), ts)| event(id, "@you:x.org", body, ts))
        .collect();
    messages(
        &server,
        None,
        json!({"start": "t", "chunk": chunk, "state": []}),
    )
    .await;
    let (found, run, read) = client.search(&room(), "pizza").await.expect("searched");
    assert_eq!(found.order, vec![PostId("$c".into()), PostId("$a".into())]);
    assert_eq!(run.order.len(), 3);
    assert_eq!(read, 3);
}

#[tokio::test]
async fn a_report_names_the_event_and_the_reason() {
    let (server, client) = world("report").await;
    answer(&server, "POST", r"/report/", json!({})).await;
    client
        .report(&room(), &PostId("$m2".into()), "spam")
        .await
        .expect("reported");
    let calls = sent(&server, "POST", "/report/").await;
    assert!(calls[0].url.path().ends_with("/report/$m2"));
    assert_eq!(body_of(&calls[0])["reason"], "spam");
}

#[tokio::test]
async fn ignoring_a_user_writes_the_ignored_user_list() {
    let (server, client) = world("ignore").await;
    answer(&server, "PUT", r"/account_data/", json!({})).await;
    let you = UserId("@you:x.org".into());
    client.ignore(&you).await.expect("ignored");
    client.unignore(&you).await.expect("unignored");
    let calls = sent(&server, "PUT", "/account_data/m.ignored_user_list").await;
    assert_eq!(calls.len(), 1);
    assert!(body_of(&calls[0])["ignored_users"]["@you:x.org"].is_object());
}

#[tokio::test]
async fn a_location_is_a_message_with_a_geo_uri() {
    let (server, client) = world("location").await;
    answer(
        &server,
        "PUT",
        r"/send/m\.room\.message/",
        json!({"event_id": "$loc"}),
    )
    .await;
    client
        .send_location(&room(), "48.85", "2.35")
        .await
        .expect("sent");
    let calls = sent(&server, "PUT", "/send/m.room.message/").await;
    let content = body_of(&calls[0]);
    assert_eq!(content["msgtype"], "m.location");
    assert_eq!(content["geo_uri"], "geo:48.85,2.35");
}

fn password_wanted(request: &Request) -> ResponseTemplate {
    let auth = &body_of(request)["auth"];
    if auth["type"] == "m.login.password" && auth["session"] == "s1" && auth["password"] == "pw" {
        return ResponseTemplate::new(200)
            .set_body_json(json!({"id_server_unbind_result": "success"}));
    }
    ResponseTemplate::new(401).set_body_json(json!({
        "flows": [{"stages": ["m.login.password"]}], "params": {}, "session": "s1",
    }))
}

async fn confirmed(server: &MockServer, verb: &str, route: &str) {
    Mock::given(method(verb))
        .and(path_regex(route))
        .respond_with(password_wanted)
        .mount(server)
        .await;
}

#[tokio::test]
async fn the_sessions_are_listed_newest_first_and_the_running_one_is_marked() {
    let (server, client) = world("sessions").await;
    answer(
        &server,
        "GET",
        r"/devices$",
        json!({"devices": [
            {"device_id": "OLD", "display_name": "phone", "last_seen_ts": 10},
            {"device_id": "D", "display_name": "mx", "last_seen_ts": 30, "last_seen_ip": "1.2.3.4"},
        ]}),
    )
    .await;
    let list = client.sessions().await.expect("listed");
    let ids: Vec<&str> = list.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["D", "OLD"]);
    assert!(list[0].current && !list[1].current);
    assert_eq!(list[0].ip, "1.2.3.4");
}

#[tokio::test]
async fn ending_a_session_confirms_with_the_password_and_never_the_running_one() {
    let (server, client) = world("end-session").await;
    confirmed(&server, "POST", r"/delete_devices$").await;
    let error = client.end_session("D").await.expect_err("it is running");
    assert!(error.to_string().contains(":quit"));
    assert!(sent(&server, "POST", "/delete_devices").await.is_empty());
    client.end_session("OLD").await.expect("ended");
    let calls = sent(&server, "POST", "/delete_devices").await;
    assert_eq!(calls.len(), 2);
    assert!(body_of(&calls[0]).get("auth").is_none());
    assert_eq!(body_of(&calls[1])["devices"], json!(["OLD"]));
}

#[tokio::test]
async fn the_password_is_changed_after_the_server_asks_for_the_old_one() {
    let (server, client) = world("password").await;
    confirmed(&server, "POST", r"/account/password$").await;
    client.change_password("fresh").await.expect("changed");
    let calls = sent(&server, "POST", "/account/password").await;
    assert_eq!(calls.len(), 2);
    assert_eq!(body_of(&calls[1])["new_password"], "fresh");
    assert_eq!(body_of(&calls[1])["auth"]["identifier"]["user"], "me");
}

#[tokio::test]
async fn a_server_without_a_password_cannot_confirm_and_says_where_to_go() {
    let (server, mut client) = world("no-password").await;
    confirmed(&server, "POST", r"/account/password$").await;
    client.credentials = Credentials::Oidc {
        browser: "true".into(),
    };
    let error = client.change_password("fresh").await.expect_err("refused");
    assert!(error.to_string().contains("account page"));
}

#[tokio::test]
async fn closing_the_account_confirms_then_forgets_the_local_data() {
    let (server, client) = world("deactivate").await;
    confirmed(&server, "POST", r"/account/deactivate$").await;
    assert!(client.dir.exists());
    client.deactivate().await.expect("closed");
    assert_eq!(sent(&server, "POST", "/account/deactivate").await.len(), 2);
    assert!(!client.dir.exists());
}

#[tokio::test]
async fn two_accounts_on_one_server_keep_their_own_sessions() {
    let (server, first) = world("twin-a").await;
    let second = Client::connect(
        "twin-b",
        &server.uri(),
        Credentials::Password {
            login: "me".into(),
            password: "pw".into(),
        },
    )
    .await
    .expect("second account");
    assert_ne!(first.dir, second.dir);
    assert!(first.dir.join("session.json").exists());
    assert!(second.dir.join("session.json").exists());
}

#[tokio::test]
async fn an_oauth_login_registers_exchanges_the_code_and_the_session_comes_back() {
    use std::os::unix::fs::PermissionsExt;
    home();
    let dir = tempfile::tempdir().expect("temporary folder");
    let (script, seen) = (dir.path().join("browser.sh"), dir.path().join("seen"));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$1\" > {}\nback=$(echo \"$1\" | sed 's/.*redirect_uri=//;s/&.*//;s/%3A/:/g;s/%2F/\\//g')\nstate=$(echo \"$1\" | sed 's/.*[?&]state=//;s/&.*//')\ncurl -s -o /dev/null \"$back?code=abc&state=$state\"\n",
            seen.display()
        ),
    )
    .expect("script written");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("script executable");
    let credentials = Credentials::Oidc {
        browser: script.display().to_string(),
    };
    let (server, first) = world_with("oidc", credentials.clone()).await;
    let address = std::fs::read_to_string(&seen).expect("the browser was started");
    assert!(address.contains("/oauth2/authorize"));
    assert!(address.contains("client_id=mx-client"));
    assert_eq!(sent(&server, "POST", "/oauth2/registration").await.len(), 1);
    let saved = std::fs::read_to_string(first.dir.join("session.json")).expect("saved");
    assert!(saved.contains("mx-client") && saved.contains("\"access_token\":\"at\""));
    let again = Client::connect("oidc", &server.uri(), credentials)
        .await
        .expect("restored");
    assert_eq!(again.myself(), first.myself());
    assert_eq!(sent(&server, "POST", "/oauth2/registration").await.len(), 1);
    assert_eq!(sent(&server, "POST", "/oauth2/token").await.len(), 1);
}
