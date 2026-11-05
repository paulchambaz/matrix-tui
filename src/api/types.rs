//! The types shared by the Matrix client, the model and the interface.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

macro_rules! id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);
    };
}

id!(UserId, "Identifier of a user.");
id!(TeamId, "Identifier of a team.");
id!(ChannelId, "Identifier of a channel.");
id!(PostId, "Identifier of a post.");

/// A Matrix account.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: UserId,
    pub username: String,
}

/// Whether a user is around.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Status {
    Online,
    Away,
    DoNotDisturb,
    #[default]
    Offline,
}

impl Status {
    /// The server's word for a status.
    pub fn word(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Away => "away",
            Self::DoNotDisturb => "dnd",
            Self::Offline => "offline",
        }
    }

    /// Reads the server's word for a status; anything unknown counts as offline.
    pub fn parse(word: &str) -> Self {
        match word {
            "online" => Self::Online,
            "away" => Self::Away,
            "dnd" => Self::DoNotDisturb,
            _ => Self::Offline,
        }
    }
}

/// The status of one user, as the server sends it.
#[derive(Debug, Clone, Deserialize)]
pub struct UserStatus {
    pub user_id: UserId,
    pub status: String,
}

/// A team the reader belongs to.
#[derive(Debug, Clone, Deserialize)]
pub struct Team {
    pub id: TeamId,
    #[serde(default)]
    pub name: String,
    pub display_name: String,
}

/// The four kinds of channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelKind {
    /// Public channel, shown with `#`.
    #[serde(rename(deserialize = "O", serialize = "public"))]
    Public,
    /// Private channel, shown with `+`.
    #[serde(rename(deserialize = "P", serialize = "private"))]
    Private,
    /// Direct message between two users, shown with `@`.
    #[serde(rename(deserialize = "D", serialize = "direct"))]
    Direct,
    /// Group message, shown with `@`.
    #[serde(rename(deserialize = "G", serialize = "group"))]
    Group,
}

/// A channel, direct message or group message.
#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    #[serde(rename = "type")]
    pub kind: ChannelKind,
    #[serde(default)]
    pub display_name: String,
    pub name: String,
    #[serde(default)]
    pub last_post_at: i64,
    #[serde(default)]
    pub total_msg_count: i64,
    #[serde(default)]
    pub delete_at: i64,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub peer: Option<UserId>,
    #[serde(default)]
    pub invited: Option<UserId>,
}

/// Notification settings of one membership.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NotifyProps {
    #[serde(default)]
    pub mark_unread: String,
}

/// The reader's membership of a channel, which carries the read state.
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelMember {
    pub channel_id: ChannelId,
    #[serde(default)]
    pub msg_count: i64,
    #[serde(default)]
    pub mention_count: i64,
    #[serde(default)]
    pub last_viewed_at: i64,
    #[serde(default)]
    pub notify_props: NotifyProps,
}

/// One user's reaction to a post.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Reaction {
    pub user_id: UserId,
    pub post_id: PostId,
    pub emoji_name: String,
}

/// A file attached to a post.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FileInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub size: i64,
}

/// Extra data the server sends along with a post.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub reactions: Option<Vec<Reaction>>,
    #[serde(default)]
    pub files: Option<Vec<FileInfo>>,
}

/// A message. An empty `root_id` means it is not a reply.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Post {
    pub id: PostId,
    pub create_at: i64,
    #[serde(default)]
    pub edit_at: i64,
    #[serde(default)]
    pub delete_at: i64,
    pub user_id: UserId,
    pub channel_id: ChannelId,
    #[serde(default)]
    pub root_id: String,
    #[serde(default)]
    pub message: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub metadata: Metadata,
    #[serde(default)]
    pub mentioned: bool,
}

/// A page of posts; `order` lists their ids newest first.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PostList {
    pub order: Vec<PostId>,
    pub posts: HashMap<PostId, Post>,
}

impl PostList {
    /// The posts in the order the server listed them, newest first.
    pub fn ordered(&self) -> impl Iterator<Item = &Post> {
        self.order.iter().filter_map(|id| self.posts.get(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posts_come_newest_first_with_metadata() {
        let list: PostList =
            serde_json::from_str(include_str!("../../tests/fixtures/posts.json")).unwrap();
        let posts: Vec<_> = list.ordered().collect();
        assert_eq!(posts[0].id, PostId("p2".into()));
        assert_eq!(posts[0].root_id, "p1");
        assert!(posts[0].edit_at > 0);
        let files = posts[0].metadata.files.as_deref().unwrap_or_default();
        assert_eq!(files[0].name, "a.png");
        assert!(posts[1].metadata.files.is_none());
    }

    #[test]
    fn channels_cover_all_kinds() {
        let channels: Vec<Channel> =
            serde_json::from_str(include_str!("../../tests/fixtures/channels.json")).unwrap();
        let kinds: Vec<_> = channels.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                ChannelKind::Public,
                ChannelKind::Direct,
                ChannelKind::Private,
                ChannelKind::Group
            ]
        );
        assert_eq!(channels[1].peer, Some(UserId("u2".into())));
    }

    #[test]
    fn members_carry_read_state() {
        let members: Vec<ChannelMember> =
            serde_json::from_str(include_str!("../../tests/fixtures/members.json")).unwrap();
        assert_eq!(members[0].mention_count, 1);
        assert_eq!(members[0].last_viewed_at, 1_700_000_000_000);
        assert_eq!(members[0].notify_props.mark_unread, "all");
    }
}
