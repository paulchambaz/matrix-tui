//! The filter written after the options: clauses joined by `and`, groups of them by `or`.

use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};

use crate::api::types::{ChannelKind, PostId, User};
use crate::cli::Error;
use crate::model::Message;
use crate::unread::mentions_me;

/// One test on a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Test {
    /// Written by one of these users, `me` standing for the reader.
    From(Vec<String>),
    /// Calls the reader.
    Mention,
    /// In a direct or group conversation.
    Direct,
    /// Not read yet.
    Unread,
    /// Newer than a time, in milliseconds.
    After(i64),
    /// Older than a time, in milliseconds.
    Before(i64),
    /// Contains a lowercase text.
    Body(String),
    /// In the thread of a post.
    Thread(PostId),
}

/// A test, possibly turned around by `not`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clause {
    pub negated: bool,
    pub test: Test,
}

/// What a message is tested against besides itself.
#[derive(Debug)]
pub struct Context<'a> {
    pub me: &'a User,
    pub kind: ChannelKind,
    pub author: &'a str,
    pub viewed: i64,
}

/// Groups of clauses, a message satisfying every clause of one of them; none means every message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query(Vec<Vec<Clause>>);

fn bad(text: impl Into<String>) -> Error {
    Error::BadQuery(text.into())
}

/// Reads the words after the options.
///
/// # Errors
/// Fails on an unknown clause, a missing value, a bad time or a missing `and` or `or`.
pub fn parse(words: &[String], now: DateTime<Local>) -> Result<Query, Error> {
    let mut words = words.iter().map(String::as_str).peekable();
    let mut groups: Vec<Vec<Clause>> = Vec::new();
    while words.peek().is_some() {
        if let Some(last) = groups.last() {
            match words.next() {
                Some("and") if !last.is_empty() => {}
                Some("or") => groups.push(Vec::new()),
                Some(other) => return Err(bad(format!("expected `and` or `or` before `{other}`"))),
                None => {}
            }
        } else {
            groups.push(Vec::new());
        }
        let negated = words.next_if_eq(&"not").is_some();
        let word = words.next().ok_or_else(|| bad("a clause is missing"))?;
        let test = match word {
            "mention" => Test::Mention,
            "unread" => Test::Unread,
            "dm" => Test::Direct,
            "from" => Test::From(
                value(&mut words, word)?
                    .split(',')
                    .map(|name| name.trim_start_matches('@').to_lowercase())
                    .collect(),
            ),
            "after" => Test::After(time(value(&mut words, word)?, now)?),
            "before" => Test::Before(time(value(&mut words, word)?, now)?),
            "body" => Test::Body(value(&mut words, word)?.to_lowercase()),
            "thread" => Test::Thread(PostId(value(&mut words, word)?.to_owned())),
            other => return Err(bad(format!("unknown clause `{other}`"))),
        };
        if let Some(group) = groups.last_mut() {
            group.push(Clause { negated, test });
        }
    }
    Ok(Query(groups))
}

fn value<'a>(words: &mut impl Iterator<Item = &'a str>, clause: &str) -> Result<&'a str, Error> {
    words
        .next()
        .ok_or_else(|| bad(format!("`{clause}` needs a value")))
}

fn time(word: &str, now: DateTime<Local>) -> Result<i64, Error> {
    if let Some(ago) = duration(word) {
        return Ok((now - ago).timestamp_millis());
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(word) {
        return Ok(at.timestamp_millis());
    }
    NaiveDateTime::parse_from_str(word, "%Y-%m-%dT%H:%M")
        .or_else(|_| {
            NaiveDate::parse_from_str(word, "%Y-%m-%d").map(|d| d.and_time(NaiveTime::MIN))
        })
        .ok()
        .and_then(|local| Local.from_local_datetime(&local).earliest())
        .map(|at| at.timestamp_millis())
        .ok_or_else(|| bad(format!("`{word}` is not a time")))
}

fn duration(word: &str) -> Option<Duration> {
    let (count, minutes) = [('m', 1), ('h', 60), ('d', 1440)]
        .into_iter()
        .find_map(|(unit, minutes)| word.strip_suffix(unit).map(|count| (count, minutes)))?;
    let count: i64 = count.parse::<u32>().ok()?.into();
    Duration::try_minutes(count * minutes)
}

impl Clause {
    fn holds(&self, message: &Message, context: &Context) -> bool {
        let found = match &self.test {
            Test::From(names) => names.iter().any(|name| {
                if name == "me" {
                    message.user == context.me.id
                } else {
                    name.eq_ignore_ascii_case(context.author)
                }
            }),
            Test::Mention => mentions_me(message, context.kind, context.me),
            Test::Direct => matches!(context.kind, ChannelKind::Direct | ChannelKind::Group),
            Test::Unread => message.ms > context.viewed && message.user != context.me.id,
            Test::After(at) => message.ms > *at,
            Test::Before(at) => message.ms < *at,
            Test::Body(text) => message.text.to_lowercase().contains(text.as_str()),
            Test::Thread(id) => message.id == *id || message.root.as_ref() == Some(id),
        };
        found != self.negated
    }
}

impl Query {
    /// Whether a message satisfies every clause of one group.
    pub fn matches(&self, message: &Message, context: &Context) -> bool {
        self.0.is_empty()
            || self
                .0
                .iter()
                .any(|group| group.iter().all(|clause| clause.holds(message, context)))
    }

    /// The time in milliseconds at or before which no message can match.
    pub fn since(&self, viewed: i64) -> i64 {
        self.0
            .iter()
            .map(|group| {
                group
                    .iter()
                    .filter(|clause| !clause.negated)
                    .filter_map(|clause| match clause.test {
                        Test::After(at) => Some(at),
                        Test::Unread => Some(viewed),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0)
            })
            .min()
            .unwrap_or(0)
    }

    /// Whether every group of the query asks for unread messages.
    pub fn unread(&self) -> bool {
        !self.0.is_empty()
            && self.0.iter().all(|group| {
                group
                    .iter()
                    .any(|clause| !clause.negated && clause.test == Test::Unread)
            })
    }

    /// The post whose thread is asked for.
    pub fn thread(&self) -> Option<&PostId> {
        self.0
            .iter()
            .flatten()
            .find_map(|clause| match &clause.test {
                Test::Thread(id) => Some(id),
                _ => None,
            })
    }

    /// Points the thread clauses at the root post of the thread.
    pub fn root_thread_at(&mut self, root: &PostId) {
        for clause in self.0.iter_mut().flatten() {
            if let Test::Thread(id) = &mut clause.test {
                id.clone_from(root);
            }
        }
    }

    /// Checks that a query for channels holds nothing but `unread` and `mention`.
    ///
    /// # Errors
    /// Fails on any other clause.
    pub fn for_channels(&self) -> Result<(), Error> {
        if self
            .0
            .iter()
            .flatten()
            .all(|clause| matches!(clause.test, Test::Unread | Test::Mention))
        {
            Ok(())
        } else {
            Err(bad("channels take only `unread` and `mention`"))
        }
    }

    /// Whether a channel with this read state satisfies a query for channels.
    pub fn selects(&self, unread: bool, mention: bool) -> bool {
        self.0.is_empty()
            || self.0.iter().any(|group| {
                group.iter().all(|clause| {
                    let found = match clause.test {
                        Test::Unread => unread,
                        _ => mention,
                    };
                    found != clause.negated
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::api::types::UserId;

    fn now() -> DateTime<Local> {
        Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap().into()
    }

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    fn query(text: &str) -> Query {
        parse(&words(text), now()).unwrap()
    }

    fn me() -> User {
        User {
            id: UserId("u1".into()),
            username: "paul".into(),
        }
    }

    fn message(user: &str, text: &str, ms: i64) -> Message {
        Message {
            id: PostId(format!("p{ms}")),
            at: Local.timestamp_millis_opt(ms).unwrap(),
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

    fn holds(query: &Query, message: &Message, author: &str, kind: ChannelKind) -> bool {
        let me = me();
        let context = Context {
            me: &me,
            kind,
            author,
            viewed: 100,
        };
        query.matches(message, &context)
    }

    #[test]
    fn an_empty_query_keeps_everything() {
        let all = query("");
        assert!(holds(
            &all,
            &message("u2", "hi", 1),
            "ana",
            ChannelKind::Public
        ));
    }

    #[test]
    fn clauses_are_joined_by_and() {
        let q = query("from ana and after 2026-01-01 and not mention");
        assert_eq!(q.0.len(), 1);
        assert_eq!(q.0[0].len(), 3);
        assert!(q.0[0][2].negated);
        assert!(parse(&words("unread mention"), now()).is_err());
        assert!(parse(&words("unread and"), now()).is_err());
        assert!(parse(&words("unread or"), now()).is_err());
        assert!(parse(&words("or unread"), now()).is_err());
        assert!(parse(&words("unread and or mention"), now()).is_err());
        assert!(parse(&words("and unread"), now()).is_err());
    }

    #[test]
    fn and_binds_tighter_than_or() {
        let q = query("dm and unread or mention and not from me or body x");
        assert_eq!(q.0.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 2, 1]);
    }

    #[test]
    fn dm_and_unread_or_mention_and_unread() {
        let q = query("dm and unread or mention and unread");
        let new = |text: &str, ms| message("u2", text, ms);
        assert!(holds(&q, &new("hi", 101), "ana", ChannelKind::Direct));
        assert!(holds(&q, &new("hi", 101), "ana", ChannelKind::Group));
        assert!(holds(
            &q,
            &new("ping @paul", 101),
            "ana",
            ChannelKind::Public
        ));
        assert!(!holds(&q, &new("hi", 101), "ana", ChannelKind::Public));
        assert!(!holds(&q, &new("hi", 100), "ana", ChannelKind::Direct));
        assert!(!holds(
            &q,
            &new("ping @paul", 100),
            "ana",
            ChannelKind::Public
        ));
    }

    #[test]
    fn unknown_and_unfinished_clauses_are_refused() {
        for text in ["nope", "from", "after", "after tomorrow", "body", "thread"] {
            assert!(matches!(
                parse(&words(text), now()),
                Err(Error::BadQuery(_))
            ));
        }
    }

    #[test]
    fn times_are_dates_instants_or_durations_back() {
        let hour = 3_600_000;
        let at = now().timestamp_millis();
        assert_eq!(time("90m", now()).unwrap(), at - 90 * 60_000);
        assert_eq!(time("12h", now()).unwrap(), at - 12 * hour);
        assert_eq!(time("7d", now()).unwrap(), at - 7 * 24 * hour);
        assert_eq!(
            time("2026-11-04T12:00:00Z", now()).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0)
                .unwrap()
                .timestamp_millis()
        );
        assert!(time("2026-11-04", now()).is_ok());
        assert!(time("2026-11-04T09:00", now()).is_ok());
        assert!(time("-5d", now()).is_err());
        assert!(time("soon", now()).is_err());
    }

    #[test]
    fn from_knows_me_and_ignores_case_and_at() {
        let q = query("from me,@ANA");
        assert!(holds(
            &q,
            &message("u1", "x", 1),
            "paul",
            ChannelKind::Public
        ));
        assert!(holds(
            &q,
            &message("u2", "x", 1),
            "ana",
            ChannelKind::Public
        ));
        assert!(!holds(
            &q,
            &message("u3", "x", 1),
            "bob",
            ChannelKind::Public
        ));
    }

    #[test]
    fn mention_follows_the_rule_of_the_interface() {
        let q = query("mention");
        let call = message("u2", "ping @paul", 1);
        assert!(holds(&q, &call, "ana", ChannelKind::Public));
        assert!(!holds(
            &q,
            &message("u2", "hello", 1),
            "ana",
            ChannelKind::Public
        ));
        assert!(holds(
            &q,
            &message("u2", "hello", 1),
            "ana",
            ChannelKind::Direct
        ));
        assert!(!holds(
            &q,
            &message("u1", "hello", 1),
            "paul",
            ChannelKind::Direct
        ));
    }

    #[test]
    fn unread_is_newer_than_the_last_view_and_not_mine() {
        let q = query("unread");
        assert!(holds(
            &q,
            &message("u2", "x", 101),
            "ana",
            ChannelKind::Public
        ));
        assert!(!holds(
            &q,
            &message("u2", "x", 100),
            "ana",
            ChannelKind::Public
        ));
        assert!(!holds(
            &q,
            &message("u1", "x", 101),
            "paul",
            ChannelKind::Public
        ));
    }

    #[test]
    fn not_turns_a_clause_around() {
        let q = query("not unread and not from me");
        assert!(holds(
            &q,
            &message("u2", "x", 50),
            "ana",
            ChannelKind::Public
        ));
        assert!(!holds(
            &q,
            &message("u2", "x", 150),
            "ana",
            ChannelKind::Public
        ));
        assert!(!holds(
            &q,
            &message("u1", "x", 50),
            "paul",
            ChannelKind::Public
        ));
    }

    #[test]
    fn body_ignores_case_and_after_before_bound_the_time() {
        let q = query("body DePloy and after 2026-11-04T09:00 and before 2026-11-04T18:00");
        let noon = Utc
            .with_ymd_and_hms(2026, 11, 4, 12, 0, 0)
            .unwrap()
            .with_timezone(&Local)
            .timestamp_millis();
        assert!(holds(
            &q,
            &message("u2", "the Deploy", noon),
            "ana",
            ChannelKind::Public
        ));
        assert!(!holds(
            &q,
            &message("u2", "the build", noon),
            "ana",
            ChannelKind::Public
        ));
        assert!(!holds(
            &q,
            &message("u2", "the deploy", 1),
            "ana",
            ChannelKind::Public
        ));
    }

    #[test]
    fn a_thread_holds_its_root_and_its_replies() {
        let q = query("thread p1");
        let mut reply = message("u2", "x", 2);
        reply.root = Some(PostId("p1".into()));
        let mut root = message("u2", "x", 1);
        root.id = PostId("p1".into());
        assert!(holds(&q, &root, "ana", ChannelKind::Public));
        assert!(holds(&q, &reply, "ana", ChannelKind::Public));
        assert!(!holds(
            &q,
            &message("u2", "x", 3),
            "ana",
            ChannelKind::Public
        ));
    }

    #[test]
    fn the_fetch_can_stop_at_the_time_nothing_older_can_match() {
        assert_eq!(query("from me").since(100), 0);
        assert_eq!(query("unread").since(100), 100);
        assert_eq!(query("not unread").since(100), 0);
        let after = query("after 2026-01-01").since(100);
        assert_eq!(
            query("after 2026-01-01 and unread").since(100),
            after.max(100)
        );
    }

    #[test]
    fn the_fetch_of_an_or_goes_back_as_far_as_its_widest_group() {
        assert_eq!(query("unread or mention").since(100), 0);
        assert_eq!(query("unread or dm and unread").since(100), 100);
        let after = query("after 2026-01-01").since(100);
        assert_eq!(
            query("unread or after 2026-01-01").since(100),
            after.min(100)
        );
    }

    #[test]
    fn only_a_positive_unread_lifts_the_default_limit() {
        assert!(query("dm and unread or mention and unread").unread());
        assert!(!query("dm and unread or mention").unread());
        assert!(query("unread and mention").unread());
        assert!(!query("not unread").unread());
        assert!(!query("mention").unread());
    }

    #[test]
    fn channels_accept_only_unread_and_mention() {
        assert!(query("unread and not mention").for_channels().is_ok());
        assert!(query("from me").for_channels().is_err());
        let q = query("unread and not mention");
        assert!(q.selects(true, false));
        assert!(!q.selects(true, true));
        assert!(!q.selects(false, false));
        let either = query("unread or mention");
        assert!(either.selects(true, false) && either.selects(false, true));
        assert!(!either.selects(false, false));
        assert!(query("dm and unread").for_channels().is_err());
    }

    #[test]
    fn a_thread_is_pointed_at_its_root() {
        let mut q = query("thread p2");
        assert_eq!(q.thread(), Some(&PostId("p2".into())));
        q.root_thread_at(&PostId("p1".into()));
        assert_eq!(q.thread(), Some(&PostId("p1".into())));
    }
}
