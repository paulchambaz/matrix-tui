//! Servers, teams, channels and messages, with the rules that read them.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Local, TimeZone};

use crate::api::types::{
    Channel, ChannelId, ChannelKind, ChannelMember, Post, PostId, PostList, Reaction, Status,
    Team as WireTeam, User, UserId,
};
use crate::api::Client;
use crate::sync::Action;

/// Posts asked for at a time.
pub const PAGE: usize = 60;
/// Most messages kept for one channel.
pub const MAX_MESSAGES: usize = 500;
/// How long someone counts as typing after their last typing event.
const TYPING: Duration = Duration::from_secs(5);
const GROUP_WINDOW_MS: i64 = 5 * 60 * 1000;
const PALETTE: [u8; 12] = [1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 14];

/// The users who reacted to a message with one emoji.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reactions {
    pub name: String,
    pub users: Vec<UserId>,
}

/// A file attached to a post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
}

/// A post, ready to be shown.
#[derive(Debug, Clone)]
pub struct Message {
    pub id: PostId,
    pub at: DateTime<Local>,
    pub ms: i64,
    pub user: UserId,
    pub text: String,
    pub root: Option<PostId>,
    pub edited: bool,
    pub system: bool,
    pub mentioned: bool,
    pub files: Vec<Attachment>,
    pub reactions: Vec<Reactions>,
}

impl Message {
    /// Reads a post as the server sent it.
    pub fn from_post(post: &Post) -> Self {
        Self {
            id: post.id.clone(),
            at: Local
                .timestamp_millis_opt(post.create_at)
                .single()
                .unwrap_or_default(),
            ms: post.create_at,
            user: post.user_id.clone(),
            text: post.message.clone(),
            root: Some(post.root_id.clone())
                .filter(|r| !r.is_empty())
                .map(PostId),
            edited: post.edit_at > 0,
            system: post.kind.starts_with("system_"),
            mentioned: post.mentioned,
            files: post
                .metadata
                .files
                .iter()
                .flatten()
                .map(|f| Attachment {
                    id: f.id.clone(),
                    name: f.name.clone(),
                    mime: f.mime_type.clone(),
                    size: f.size,
                })
                .collect(),
            reactions: {
                let mut groups = Vec::new();
                for r in post.metadata.reactions.iter().flatten() {
                    add_reaction(&mut groups, r);
                }
                groups
            },
        }
    }
}

fn add_reaction(groups: &mut Vec<Reactions>, reaction: &Reaction) {
    match groups.iter_mut().find(|g| g.name == reaction.emoji_name) {
        Some(group) if group.users.contains(&reaction.user_id) => {}
        Some(group) => group.users.push(reaction.user_id.clone()),
        None => groups.push(Reactions {
            name: reaction.emoji_name.clone(),
            users: vec![reaction.user_id.clone()],
        }),
    }
}

/// Puts a message in its place by time, replacing the one with the same id.
pub fn insert_message(list: &mut Vec<Message>, message: Message) {
    match list.iter().position(|m| m.id == message.id) {
        Some(i) => list[i] = message,
        None => {
            let at = list.partition_point(|m| m.ms <= message.ms);
            list.insert(at, message);
        }
    }
}

/// Adds a page of messages, replacing the ones already known by id.
pub fn merge_page(list: &mut Vec<Message>, page: Vec<Message>) {
    for message in page {
        insert_message(list, message);
    }
}

/// Puts the newest page in place of what was known for the same time span.
/// Returns false when it does not meet the known messages, which are then dropped as stale.
pub fn replace_window(list: &mut Vec<Message>, page: Vec<Message>) -> bool {
    let (Some(first), Some(last)) = (page.first().map(|m| m.ms), page.last().map(|m| m.ms)) else {
        list.clear();
        return false;
    };
    let meets = list.last().is_none_or(|known| known.ms >= first);
    if !meets {
        list.clear();
    }
    list.retain(|m| m.ms < first || m.ms > last);
    merge_page(list, page);
    meets
}

/// Drops the oldest messages beyond `max` and says how many went.
pub fn cap(list: &mut Vec<Message>, max: usize) -> usize {
    let extra = list.len().saturating_sub(max);
    list.drain(..extra);
    extra
}

/// Applies an edited post, keeping the reactions already known.
pub fn edit_message(list: &mut [Message], post: &Post) {
    if let Some(m) = list.iter_mut().find(|m| m.id == post.id) {
        m.text.clone_from(&post.message);
        m.edited = true;
    }
}

/// Removes a message and says where it was.
pub fn remove_message(list: &mut Vec<Message>, id: &PostId) -> Option<usize> {
    let i = list.iter().position(|m| &m.id == id)?;
    list.remove(i);
    Some(i)
}

/// Records a reaction on its message.
pub fn react(list: &mut [Message], reaction: &Reaction) {
    if let Some(m) = list.iter_mut().find(|m| m.id == reaction.post_id) {
        add_reaction(&mut m.reactions, reaction);
    }
}

/// Takes a reaction off its message; an emoji nobody uses disappears.
pub fn unreact(list: &mut [Message], reaction: &Reaction) {
    let Some(m) = list.iter_mut().find(|m| m.id == reaction.post_id) else {
        return;
    };
    for group in m
        .reactions
        .iter_mut()
        .filter(|g| g.name == reaction.emoji_name)
    {
        group.users.retain(|u| u != &reaction.user_id);
    }
    m.reactions.retain(|g| !g.users.is_empty());
}

/// Posts of a page oldest first, without the deleted ones.
pub fn messages_of(list: &PostList) -> Vec<Message> {
    let mut messages: Vec<Message> = list
        .ordered()
        .filter(|p| p.delete_at == 0)
        .map(Message::from_post)
        .collect();
    messages.sort_by_key(|m| m.ms);
    messages
}

/// Whether `message` continues the group of `prev`, so its author and time are not repeated.
pub fn group_with_previous(prev: &Message, message: &Message) -> bool {
    let gap = message.ms - prev.ms;
    prev.user == message.user
        && prev.at.date_naive() == message.at.date_naive()
        && (0..GROUP_WINDOW_MS).contains(&gap)
}

/// Index of the first message of the group that holds message `i`.
pub fn group_start(messages: &[Message], mut i: usize) -> usize {
    while i > 0 && group_with_previous(&messages[i - 1], &messages[i]) {
        i -= 1;
    }
    i
}

/// A day written for a separator line: Today, Yesterday, or the date.
pub fn day_label(day: DateTime<Local>, now: DateTime<Local>) -> String {
    let (today, date) = (now.date_naive(), day.date_naive());
    if date == today {
        return "Today".into();
    }
    if today.pred_opt() == Some(date) {
        return "Yesterday".into();
    }
    match day.year() == now.year() {
        true => day.format("%A %-d %B").to_string(),
        false => day.format("%A %-d %B %Y").to_string(),
    }
}

/// A stable ANSI color index for a user.
pub fn user_color(id: &UserId) -> u8 {
    let hash =
        id.0.bytes()
            .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    PALETTE[hash as usize % PALETTE.len()]
}

/// A channel with the reader's membership.
#[derive(Debug, Clone)]
pub struct Chan {
    pub info: Channel,
    pub member: Option<ChannelMember>,
}

impl Chan {
    /// How many posts the reader has not read, by the server's own counters.
    pub fn unread_count(&self) -> usize {
        let read = self.member.as_ref().map_or(i64::MAX, |m| m.msg_count);
        usize::try_from(self.info.total_msg_count - read).unwrap_or(0)
    }

    /// Whether the tab should be marked: unread posts, or a mention in a muted channel.
    pub fn unread(&self) -> bool {
        let Some(member) = &self.member else {
            return false;
        };
        let muted = member.notify_props.mark_unread == "mention";
        self.mentions() || (!muted && self.unread_count() > 0)
    }

    /// Whether someone mentioned the reader since the last view.
    pub fn mentions(&self) -> bool {
        self.member.as_ref().is_some_and(|m| m.mention_count > 0)
    }
}

/// A team of a server, with its channels once they are loaded.
#[derive(Debug, Clone)]
pub struct Team {
    pub info: WireTeam,
    pub chans: Option<Vec<Chan>>,
    pub loading: bool,
    pub cur: usize,
}

impl Team {
    /// Puts the channels back in order, keeping the selection on the same channel.
    pub fn resort(&mut self) {
        let Some(chans) = self.chans.as_mut() else {
            return;
        };
        let kept = chans.get(self.cur).map(|c| c.info.id.clone());
        chans.sort_by_key(|c| Reverse(c.info.last_post_at));
        if let Some(i) = kept.and_then(|id| chans.iter().position(|c| c.info.id == id)) {
            self.cur = i;
        }
    }
}

/// Puts channels in order of their latest message, newest first.
pub fn order_channels(channels: Vec<Channel>, members: Vec<ChannelMember>) -> Vec<Chan> {
    let mut members: HashMap<_, _> = members
        .into_iter()
        .map(|m| (m.channel_id.clone(), m))
        .collect();
    let mut channels: Vec<Channel> = channels.into_iter().filter(|c| c.delete_at == 0).collect();
    channels.sort_by_key(|c| Reverse(c.last_post_at));

    channels
        .into_iter()
        .map(|info| Chan {
            member: members.remove(&info.id),
            info,
        })
        .collect()
}

/// The state of the event stream of a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Up,
    Down,
}

/// A member of a channel and whether they are around.
#[derive(Debug)]
pub struct Member<'a> {
    pub user: &'a UserId,
    pub name: &'a str,
    pub status: Status,
}

/// The sign written before a channel name.
pub fn glyph(kind: ChannelKind) -> char {
    match kind {
        ChannelKind::Public => '#',
        ChannelKind::Private => '+',
        ChannelKind::Direct | ChannelKind::Group => '@',
    }
}

/// A logged-in server, or one that is still connecting.
#[derive(Debug)]
pub struct Server {
    pub name: String,
    pub client: Option<Arc<Client>>,
    pub me: Option<User>,
    pub users: HashMap<UserId, User>,
    pub asked: HashSet<UserId>,
    pub teams: Vec<Team>,
    pub messages: HashMap<ChannelId, Vec<Message>>,
    pub loading: HashSet<ChannelId>,
    pub refreshed: HashSet<ChannelId>,
    pub complete: HashSet<ChannelId>,
    pub paging: HashSet<ChannelId>,
    pub typing: HashMap<ChannelId, Vec<(UserId, Instant)>>,
    pub members: HashMap<ChannelId, Vec<UserId>>,
    pub members_loading: HashSet<ChannelId>,
    pub status: HashMap<UserId, Status>,
    pub actions: Option<tokio::sync::mpsc::UnboundedSender<Action>>,
    pub sync: Option<tokio::task::JoinHandle<()>>,
    pub link: Link,
}

impl Server {
    /// A server that is not connected yet.
    pub fn new(name: String) -> Self {
        Self {
            name,
            client: None,
            me: None,
            users: HashMap::new(),
            asked: HashSet::new(),
            teams: Vec::new(),
            messages: HashMap::new(),
            loading: HashSet::new(),
            refreshed: HashSet::new(),
            complete: HashSet::new(),
            paging: HashSet::new(),
            typing: HashMap::new(),
            members: HashMap::new(),
            members_loading: HashSet::new(),
            status: HashMap::new(),
            actions: None,
            sync: None,
            link: Link::Connecting,
        }
    }

    /// Whether the server was removed, which leaves an empty slot so that indexes stay valid.
    pub fn is_gone(&self) -> bool {
        self.name.is_empty()
    }

    /// The name of a user, or `?` while it is being fetched.
    pub fn username(&self, id: &UserId) -> &str {
        self.users.get(id).map_or("?", |u| u.username.as_str())
    }

    /// The tab label of a channel: `#` public, `+` private, `@` direct and group.
    pub fn label(&self, chan: &Chan) -> String {
        format!("{}{}", glyph(chan.info.kind), self.name_of(chan))
    }

    /// The name of a channel as the tabs write it, without its sign.
    pub fn name_of(&self, chan: &Chan) -> String {
        let info = &chan.info;
        let name = if info.display_name.is_empty() {
            &info.name
        } else {
            &info.display_name
        };
        match info.kind {
            ChannelKind::Direct => match self.other_of(info) {
                Some(other) => self.username(&other).to_owned(),
                None => "…".into(),
            },
            _ => name.clone(),
        }
    }

    /// The members of a channel who are around and those who are not, each by name.
    pub fn presence(&self, channel: &ChannelId) -> (Vec<Member<'_>>, Vec<Member<'_>>) {
        let ids = self.members.get(channel).into_iter().flatten();
        let mut all: Vec<Member> = ids
            .map(|user| Member {
                user,
                name: self.username(user),
                status: self.status.get(user).copied().unwrap_or_default(),
            })
            .collect();
        let rank = |status: Status| match status {
            Status::Online => 0,
            Status::Away => 1,
            Status::DoNotDisturb => 2,
            Status::Offline => 3,
        };
        all.sort_by_key(|m| (rank(m.status), m.name.to_lowercase()));
        all.into_iter().partition(|m| m.status != Status::Offline)
    }

    /// Notes that a user is typing in a channel.
    pub fn typed(&mut self, channel: &ChannelId, user: &UserId, at: Instant) {
        let list = self.typing.entry(channel.clone()).or_default();
        match list.iter_mut().find(|(u, _)| u == user) {
            Some(entry) => entry.1 = at,
            None => list.push((user.clone(), at)),
        }
    }

    /// Notes that a user stopped typing, for example because their message arrived.
    pub fn stopped(&mut self, channel: &ChannelId, user: &UserId) {
        if let Some(list) = self.typing.get_mut(channel) {
            list.retain(|(u, _)| u != user);
        }
    }

    /// The names of the users who typed in a channel in the last few seconds.
    pub fn typing_names(&self, channel: &ChannelId, now: Instant) -> Vec<&str> {
        let list = self.typing.get(channel).into_iter().flatten();
        list.filter(|(_, at)| now.saturating_duration_since(*at) < TYPING)
            .map(|(user, _)| self.username(user))
            .collect()
    }

    /// Keeps only the newest page of a channel that is no longer open.
    pub fn trim(&mut self, channel: &ChannelId) {
        let Some(list) = self.messages.get_mut(channel) else {
            return;
        };
        if cap(list, PAGE) > 0 {
            self.complete.remove(channel);
        }
    }

    /// Records that a channel was viewed: nothing in it is unread any more.
    pub fn mark_viewed(&mut self, channel: &ChannelId) {
        let chans = self
            .teams
            .iter_mut()
            .flat_map(|t| t.chans.iter_mut().flatten());
        for Chan { info, member } in chans.filter(|c| &c.info.id == channel) {
            if let Some(member) = member {
                member.msg_count = member.msg_count.max(info.total_msg_count);
                member.mention_count = 0;
            }
        }
    }

    /// The other person of a direct channel.
    pub fn other_of(&self, info: &Channel) -> Option<UserId> {
        info.peer.clone()
    }

    /// Ids among `wanted` that were never asked for, marked as asked.
    pub fn unknown<'a>(&mut self, wanted: impl Iterator<Item = &'a UserId>) -> Vec<UserId> {
        let mut ids = Vec::new();
        for id in wanted {
            if !self.users.contains_key(id) && self.asked.insert(id.clone()) {
                ids.push(id.clone());
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(user: &str, ms: i64) -> Message {
        Message {
            id: PostId(format!("p{ms}")),
            at: Local.timestamp_millis_opt(ms).single().unwrap(),
            ms,
            user: UserId(user.into()),
            text: String::new(),
            root: None,
            edited: false,
            system: false,
            files: Vec::new(),
            reactions: Vec::new(),
            mentioned: false,
        }
    }

    fn channel(id: &str, kind: ChannelKind, name: &str, last: i64) -> Channel {
        Channel {
            id: ChannelId(id.into()),
            kind,
            display_name: name.into(),
            name: name.into(),
            last_post_at: last,
            total_msg_count: last,
            delete_at: 0,
            header: String::new(),
            purpose: String::new(),
            peer: None,
            invited: None,
        }
    }

    fn member(id: &str, read: i64, mentions: i64, mark: &str) -> ChannelMember {
        serde_json::from_value(serde_json::json!({
            "channel_id": id, "msg_count": read, "mention_count": mentions,
            "notify_props": {"mark_unread": mark}
        }))
        .unwrap()
    }

    fn chan(last_post: i64, member: Option<ChannelMember>) -> Chan {
        Chan {
            info: channel("c", ChannelKind::Public, "c", last_post),
            member,
        }
    }

    const NOON: i64 = 1_700_000_000_000;

    #[test]
    fn same_author_within_five_minutes_groups() {
        assert!(group_with_previous(
            &msg("a", NOON),
            &msg("a", NOON + 60_000)
        ));
        assert!(!group_with_previous(
            &msg("a", NOON),
            &msg("b", NOON + 60_000)
        ));
        assert!(!group_with_previous(
            &msg("a", NOON),
            &msg("a", NOON + 6 * 60_000)
        ));
        assert!(!group_with_previous(&msg("a", NOON), &msg("a", NOON - 1)));
    }

    #[test]
    fn group_start_walks_back() {
        let list = [
            msg("a", NOON),
            msg("a", NOON + 1000),
            msg("b", NOON + 2000),
            msg("b", NOON + 3000),
        ];
        assert_eq!(group_start(&list, 1), 0);
        assert_eq!(group_start(&list, 3), 2);
    }

    #[test]
    fn days_are_named() {
        let now = Local.with_ymd_and_hms(2026, 7, 23, 12, 0, 0).unwrap();
        let day = |y, m, d| Local.with_ymd_and_hms(y, m, d, 9, 0, 0).unwrap();
        assert_eq!(day_label(day(2026, 7, 23), now), "Today");
        assert_eq!(day_label(day(2026, 7, 22), now), "Yesterday");
        assert_eq!(day_label(day(2026, 7, 14), now), "Tuesday 14 July");
        assert_eq!(day_label(day(2025, 7, 14), now), "Monday 14 July 2025");
    }

    #[test]
    fn colors_are_stable_ansi() {
        let id = UserId("abc".into());
        assert_eq!(user_color(&id), user_color(&id));
        assert!(PALETTE.contains(&user_color(&id)));
    }

    #[test]
    fn unread_follows_the_servers_counters_and_mutes() {
        assert!(chan(10, Some(member("c", 5, 0, "all"))).unread());
        assert!(!chan(5, Some(member("c", 5, 0, "all"))).unread());
        assert!(!chan(10, Some(member("c", 5, 0, "mention"))).unread());
        assert!(chan(10, Some(member("c", 5, 2, "mention"))).unread());
        assert!(!chan(10, None).unread());
        assert_eq!(chan(10, Some(member("c", 7, 0, "all"))).unread_count(), 3);
        assert_eq!(chan(10, Some(member("c", 12, 0, "all"))).unread_count(), 0);
        assert_eq!(chan(10, None).unread_count(), 0);
    }

    #[test]
    fn viewing_a_channel_clears_it_everywhere_it_is_listed() {
        let mut server = Server::new("work".into());
        let team = |chans| Team {
            info: serde_json::from_value(serde_json::json!({"id": "t", "display_name": "T"}))
                .unwrap(),
            chans: Some(chans),
            loading: false,
            cur: 0,
        };
        let c = chan(10, Some(member("c", 5, 2, "all")));
        server.teams = vec![team(vec![c.clone()]), team(vec![c])];
        server.mark_viewed(&ChannelId("c".into()));
        let all = server.teams.iter().flat_map(|t| t.chans.iter().flatten());
        assert!(all.clone().count() == 2 && all.into_iter().all(|c| !c.unread() && !c.mentions()));
    }

    #[test]
    fn channels_are_ordered_by_latest_message() {
        let channels = vec![
            channel("c1", ChannelKind::Public, "Zed", 3),
            channel("c2", ChannelKind::Direct, "", 9),
            channel("c3", ChannelKind::Private, "Alpha", 0),
            channel("c4", ChannelKind::Direct, "", 5),
        ];
        let ids: Vec<_> = order_channels(channels, vec![])
            .iter()
            .map(|c| c.info.id.0.clone())
            .collect();
        assert_eq!(ids, ["c2", "c4", "c1", "c3"]);
    }

    #[test]
    fn direct_channels_are_labelled_with_the_other_user() {
        let mut server = Server::new("work".into());
        let user = |id: &str, name: &str| User {
            id: UserId(id.into()),
            username: name.into(),
        };
        server.me = Some(user("u1", "paul"));
        server.users.insert(UserId("u2".into()), user("u2", "ana"));
        let dm = Chan {
            info: Channel {
                peer: Some(UserId("u2".into())),
                ..channel("c", ChannelKind::Direct, "", 0)
            },
            member: None,
        };
        assert_eq!(server.label(&dm), "@ana");
        assert_eq!(server.label(&chan(0, None)), "#c");
    }

    #[test]
    fn unknown_users_are_asked_once() {
        let mut server = Server::new("work".into());
        let ids = [UserId("u2".into()), UserId("u2".into())];
        assert_eq!(server.unknown(ids.iter()).len(), 1);
        assert!(server.unknown(ids.iter()).is_empty());
    }

    fn reaction(user: &str, post: &str, name: &str) -> Reaction {
        Reaction {
            user_id: UserId(user.into()),
            post_id: PostId(post.into()),
            emoji_name: name.into(),
        }
    }

    #[test]
    fn messages_are_inserted_in_order_without_duplicates() {
        let mut list = vec![msg("a", 10), msg("a", 30)];
        insert_message(&mut list, msg("b", 20));
        insert_message(&mut list, msg("b", 20));
        assert_eq!(list.iter().map(|m| m.ms).collect::<Vec<_>>(), [10, 20, 30]);
        assert_eq!(remove_message(&mut list, &PostId("p20".into())), Some(1));
        assert_eq!(remove_message(&mut list, &PostId("p20".into())), None);
    }

    #[test]
    fn reactions_group_by_emoji_and_vanish_when_empty() {
        let mut list = vec![msg("a", 10)];
        react(&mut list, &reaction("u1", "p10", "+1"));
        react(&mut list, &reaction("u2", "p10", "+1"));
        react(&mut list, &reaction("u2", "p10", "+1"));
        react(&mut list, &reaction("u1", "p10", "tada"));
        assert_eq!(list[0].reactions[0].users.len(), 2);
        assert_eq!(list[0].reactions.len(), 2);
        unreact(&mut list, &reaction("u1", "p10", "tada"));
        unreact(&mut list, &reaction("u1", "p10", "+1"));
        assert_eq!(list[0].reactions.len(), 1);
        assert_eq!(list[0].reactions[0].users, [UserId("u2".into())]);
    }

    fn ms_of(list: &[Message]) -> Vec<i64> {
        list.iter().map(|m| m.ms).collect()
    }

    #[test]
    fn pages_merge_in_order_without_duplicates() {
        let mut list = vec![msg("a", 30), msg("a", 40)];
        merge_page(&mut list, vec![msg("a", 10), msg("a", 20), msg("a", 30)]);
        merge_page(&mut list, vec![msg("a", 10), msg("a", 20)]);
        assert_eq!(ms_of(&list), [10, 20, 30, 40]);
    }

    #[test]
    fn the_newest_page_replaces_its_window_and_keeps_older_history() {
        let mut list = vec![
            msg("a", 10),
            msg("a", 20),
            msg("a", 30),
            msg("a", 40),
            msg("a", 50),
        ];
        let kept = replace_window(&mut list, vec![msg("a", 30), msg("a", 50), msg("a", 60)]);
        assert!(kept);
        assert_eq!(ms_of(&list), [10, 20, 30, 50, 60]);
    }

    #[test]
    fn a_window_that_misses_the_known_messages_starts_over() {
        let mut list = vec![msg("a", 10), msg("a", 20)];
        assert!(!replace_window(
            &mut list,
            vec![msg("a", 90), msg("a", 100)]
        ));
        assert_eq!(ms_of(&list), [90, 100]);
        assert!(!replace_window(&mut list, vec![]));
        assert!(list.is_empty());
    }

    #[test]
    fn newer_live_posts_survive_a_refresh() {
        let mut list = vec![msg("a", 10), msg("a", 20), msg("a", 70)];
        replace_window(&mut list, vec![msg("a", 10), msg("a", 20), msg("a", 30)]);
        assert_eq!(ms_of(&list), [10, 20, 30, 70]);
    }

    #[test]
    fn the_cap_drops_the_oldest_and_trim_forgets_completeness() {
        let mut list: Vec<Message> = (1..=5).map(|i| msg("a", i)).collect();
        assert_eq!(cap(&mut list, 3), 2);
        assert_eq!(ms_of(&list), [3, 4, 5]);
        assert_eq!(cap(&mut list, 3), 0);
        let mut server = Server::new("work".into());
        let id = ChannelId("c".into());
        server
            .messages
            .insert(id.clone(), (1..=100).map(|i| msg("a", i)).collect());
        server.complete.insert(id.clone());
        server.trim(&id);
        assert_eq!(server.messages[&id].len(), PAGE);
        assert_eq!(server.messages[&id][0].ms, 41);
        assert!(!server.complete.contains(&id));
        server.messages.insert(id.clone(), vec![msg("a", 1)]);
        server.complete.insert(id.clone());
        server.trim(&id);
        assert!(server.complete.contains(&id));
    }

    #[test]
    fn typing_expires_and_stops_with_a_message() {
        let mut server = Server::new("work".into());
        let (channel, ana, bob) = (
            ChannelId("c".into()),
            UserId("u2".into()),
            UserId("u3".into()),
        );
        server.users.insert(
            ana.clone(),
            User {
                id: ana.clone(),
                username: "ana".into(),
            },
        );
        let now = Instant::now();
        server.typed(&channel, &ana, now);
        server.typed(&channel, &bob, now - Duration::from_secs(9));
        server.typed(&channel, &ana, now);
        assert_eq!(server.typing_names(&channel, now), ["ana"]);
        assert!(server
            .typing_names(&channel, now + Duration::from_secs(6))
            .is_empty());
        server.stopped(&channel, &ana);
        assert!(server.typing_names(&channel, now).is_empty());
    }

    #[test]
    fn members_are_listed_present_first_by_status_then_name() {
        let mut server = Server::new("work".into());
        let channel = ChannelId("c".into());
        let names = [
            ("u1", "zed", Status::Online),
            ("u2", "bob", Status::Away),
            ("u3", "amy", Status::Online),
            ("u4", "eve", Status::DoNotDisturb),
            ("u5", "dan", Status::Offline),
            ("u6", "cat", Status::Offline),
        ];
        for (id, name, status) in names {
            let user = UserId(id.into());
            server.users.insert(
                user.clone(),
                User {
                    id: user.clone(),
                    username: name.into(),
                },
            );
            server.status.insert(user.clone(), status);
            server
                .members
                .entry(channel.clone())
                .or_default()
                .push(user);
        }
        server
            .members
            .get_mut(&channel)
            .unwrap()
            .push(UserId("u7".into()));
        let (present, absent) = server.presence(&channel);
        let name = |list: &[Member]| list.iter().map(|m| m.name.to_owned()).collect::<Vec<_>>();
        assert_eq!(name(&present), ["amy", "zed", "bob", "eve"]);
        assert_eq!(name(&absent), ["?", "cat", "dan"]);
        assert!(server.presence(&ChannelId("none".into())).0.is_empty());
    }
}
