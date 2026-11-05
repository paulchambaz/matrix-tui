//! The rules behind the separator and the jump keys, with no I/O.

use crate::api::types::{ChannelKind, User};
use crate::model::Message;

/// Index of the first unread message when `unread` posts are unread, unless it is the first one shown.
pub fn first_unread(messages: &[Message], unread: usize) -> Option<usize> {
    let start = messages.len().saturating_sub(unread);
    (unread > 0 && start > 0).then_some(start)
}

fn names_me(text: &str, me: &str) -> bool {
    text.match_indices('@').any(|(at, _)| {
        let glued = text[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        let name = text[at + 1..]
            .split(|c: char| !(c.is_alphanumeric() || matches!(c, '.' | '_' | '-')))
            .next()
            .unwrap_or_default()
            .trim_end_matches(['.', '-'])
            .to_lowercase();
        !glued && (name == me.to_lowercase() || matches!(name.as_str(), "all" | "channel" | "here"))
    })
}

/// Whether a message calls the reader: it is not theirs and, in a conversation, any message does.
pub fn mentions_me(message: &Message, kind: ChannelKind, me: &User) -> bool {
    if message.user == me.id || message.system {
        return false;
    }
    if message.mentioned {
        return true;
    }
    match kind {
        ChannelKind::Direct | ChannelKind::Group => true,
        ChannelKind::Public | ChannelKind::Private => names_me(&message.text, &me.username),
    }
}

/// Index of the first message from `start` on, and after `after`, that calls the reader.
pub fn first_mention(
    messages: &[Message],
    start: usize,
    after: Option<usize>,
    kind: ChannelKind,
    me: &User,
) -> Option<usize> {
    let from = after.map_or(start, |a| (a + 1).max(start));
    messages
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, m)| mentions_me(m, kind, me))
        .map(|(i, _)| i)
}

/// The first index after `cur`, wrapping around and ending on `cur`, that `marked` accepts.
pub fn next_marked(len: usize, cur: usize, marked: impl Fn(usize) -> bool) -> Option<usize> {
    (1..=len)
        .map(|step| (cur + step) % len.max(1))
        .find(|&i| marked(i))
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use super::*;
    use crate::api::types::{PostId, UserId};

    fn msg(user: &str, text: &str, ms: i64) -> Message {
        Message {
            id: PostId(format!("p{ms}")),
            at: Local.timestamp_millis_opt(ms).single().unwrap(),
            ms,
            user: UserId(user.into()),
            text: text.into(),
            root: None,
            edited: false,
            system: false,
            mentioned: false,
            files: Vec::new(),
            reactions: Vec::new(),
        }
    }

    fn me() -> User {
        User {
            id: UserId("u1".into()),
            username: "Paul".into(),
        }
    }

    #[test]
    fn the_separator_sits_before_the_first_unread_post() {
        let list = [msg("a", "", 10), msg("a", "", 20), msg("a", "", 30)];
        assert_eq!(first_unread(&list, 2), Some(1));
        assert_eq!(first_unread(&list, 1), Some(2));
        assert_eq!(first_unread(&list, 3), None);
        assert_eq!(first_unread(&list, 40), None);
        assert_eq!(first_unread(&list, 0), None);
    }

    #[test]
    fn who_is_mentioned() {
        let public = ChannelKind::Public;
        let cases = [
            ("hi @paul", true),
            ("hi @PAUL.", true),
            ("hi @paul2", false),
            ("mail a@paul.org", false),
            ("@here lunch", true),
            ("@all", true),
            ("@channel!", true),
            ("nothing", false),
            ("@pau", false),
        ];
        for (text, expected) in cases {
            assert_eq!(
                mentions_me(&msg("u2", text, 1), public, &me()),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn conversations_always_call_but_never_for_my_own_or_system_posts() {
        let mut own = msg("u1", "@paul", 1);
        assert!(!mentions_me(&own, ChannelKind::Public, &me()));
        assert!(mentions_me(
            &msg("u2", "hello", 1),
            ChannelKind::Direct,
            &me()
        ));
        assert!(mentions_me(
            &msg("u2", "hello", 1),
            ChannelKind::Group,
            &me()
        ));
        own.user = UserId("u2".into());
        own.system = true;
        assert!(!mentions_me(&own, ChannelKind::Direct, &me()));
    }

    #[test]
    fn mentions_are_found_in_order_among_the_unread() {
        let list = [
            msg("u2", "@paul old", 10),
            msg("u2", "plain", 20),
            msg("u2", "@paul new", 30),
            msg("u2", "@paul newer", 40),
        ];
        let kind = ChannelKind::Public;
        assert_eq!(first_mention(&list, 1, None, kind, &me()), Some(2));
        assert_eq!(first_mention(&list, 1, Some(2), kind, &me()), Some(3));
        assert_eq!(first_mention(&list, 1, Some(3), kind, &me()), None);
        assert_eq!(first_mention(&list, 0, None, kind, &me()), Some(0));
        assert_eq!(first_mention(&list, 0, Some(0), kind, &me()), Some(2));
    }

    #[test]
    fn the_search_wraps_and_may_come_back_to_the_start() {
        assert_eq!(next_marked(5, 3, |i| i == 1), Some(1));
        assert_eq!(next_marked(5, 3, |i| i == 4), Some(4));
        assert_eq!(next_marked(5, 3, |i| i == 3), Some(3));
        assert_eq!(next_marked(5, 3, |_| false), None);
        assert_eq!(next_marked(0, 0, |_| true), None);
    }
}
