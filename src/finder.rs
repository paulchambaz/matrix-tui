//! Fuzzy matching and the list of channels the finder shows, with no I/O.

use std::cmp::Reverse;
use std::collections::HashSet;

use crate::api::types::ChannelKind;
use crate::model::Server;

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Where the letters of `query` appear in order in `text`, ignoring case, with a score.
/// The score is higher for letters that follow each other, that start a word, that come
/// early, and for short texts.
pub fn fuzzy(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    let letters: Vec<char> = text.chars().collect();
    let (mut score, mut hits, mut next) = (0, Vec::new(), 0);
    for want in query.chars() {
        let found = (next..letters.len()).find(|&i| lower(letters[i]) == lower(want))?;
        score += 10;
        if hits.last().is_some_and(|&p| found == p + 1) {
            score += 15;
        }
        if found == 0 || !letters[found - 1].is_alphanumeric() {
            score += 20;
        }
        hits.push(found);
        next = found + 1;
    }
    score -= hits.first().map_or(0, |&first| first as i32);
    score -= letters.len() as i32;
    if query.to_lowercase() == text.to_lowercase() {
        score += 100;
    }
    Some((score, hits))
}

/// Matches one word over a scope and a name at once, as if the text were `scope#name`:
/// the first letters are found in the scope and the others in the name, trying every split.
pub fn fuzzy_both(query: &str, scope: &str, name: &str) -> Option<(i32, Vec<usize>, Vec<usize>)> {
    let letters: Vec<char> = query.chars().collect();
    let mut best: Option<(i32, Vec<usize>, Vec<usize>)> = None;
    for split in 1..letters.len() {
        let head: String = letters[..split].iter().collect();
        let tail: String = letters[split..].iter().collect();
        let (Some((a, scope_hits)), Some((b, name_hits))) =
            (fuzzy(&head, scope), fuzzy(&tail, name))
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(score, ..)| a + b > *score) {
            best = Some((a + b, scope_hits, name_hits));
        }
    }
    best
}

/// A channel the finder may list, wherever it is.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub server: usize,
    pub team: usize,
    pub chan: usize,
    pub scope: String,
    pub name: String,
    pub kind: ChannelKind,
    pub unread: bool,
}

impl Candidate {
    /// Whether it is a direct or group message, which `@x` looks for.
    pub fn direct(&self) -> bool {
        matches!(self.kind, ChannelKind::Direct | ChannelKind::Group)
    }
}

/// A candidate that matched, with the letters that did.
#[derive(Debug)]
pub struct Entry<'a> {
    pub candidate: &'a Candidate,
    pub score: i32,
    pub name_hits: Vec<usize>,
    pub scope_hits: Vec<usize>,
}

impl Entry<'_> {
    fn name(&mut self, query: &str) -> bool {
        let Some((score, hits)) = fuzzy(query, &self.candidate.name) else {
            return false;
        };
        self.score += score;
        self.name_hits.extend(hits);
        true
    }

    fn scope(&mut self, query: &str) -> bool {
        let Some((score, hits)) = fuzzy(query, &self.candidate.scope) else {
            return false;
        };
        self.score += score;
        self.scope_hits.extend(hits);
        true
    }

    fn word(&mut self, word: &str, all: bool) -> bool {
        if let Some(rest) = word.strip_prefix('@') {
            return self.candidate.direct() && (rest.is_empty() || self.name(rest));
        }
        if let Some((scope, name)) = word.split_once('#') {
            return (scope.is_empty() || self.scope(scope)) && (name.is_empty() || self.name(name));
        }
        if self.name(word) {
            return true;
        }
        if !all {
            return false;
        }
        if let Some((score, hits)) = fuzzy(word, &self.candidate.scope) {
            self.score += score - 20;
            self.scope_hits.extend(hits);
            return true;
        }
        let both = fuzzy_both(word, &self.candidate.scope, &self.candidate.name);
        let Some((score, scope_hits, name_hits)) = both else {
            return false;
        };
        self.score += score - 10;
        self.scope_hits.extend(scope_hits);
        self.name_hits.extend(name_hits);
        true
    }
}

/// The channels the finder searches: those of the team at `here`, or of every team of every server.
/// A direct or group message shown in several teams of a server is listed once.
pub fn candidates(servers: &[Server], here: (usize, usize), all: bool) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    let mut list = Vec::new();
    for (s, server) in servers.iter().enumerate() {
        for (t, team) in server.teams.iter().enumerate() {
            if !all && (s, t) != here {
                continue;
            }
            let scope = format!("{}:{}", server.name, team.info.display_name);
            for (c, chan) in team.chans.iter().flatten().enumerate() {
                if !seen.insert((s, chan.info.id.clone())) {
                    continue;
                }
                list.push(Candidate {
                    server: s,
                    team: t,
                    chan: c,
                    scope: scope.clone(),
                    name: server.name_of(chan),
                    kind: chan.info.kind,
                    unread: chan.unread(),
                });
            }
        }
    }
    list
}

/// The candidates matching every word, best match first; the order is kept when there is no word.
/// A word is fuzzy-matched on the channel name, and when searching everywhere also on the scope;
/// `@x` only looks at direct and group messages, `a#b` wants `a` in the scope and `b` in the name.
pub fn entries<'a>(words: &[&str], candidates: &'a [Candidate], all: bool) -> Vec<Entry<'a>> {
    let mut found: Vec<Entry> = candidates
        .iter()
        .map(|candidate| Entry {
            candidate,
            score: 0,
            name_hits: Vec::new(),
            scope_hits: Vec::new(),
        })
        .filter_map(|mut entry| words.iter().all(|w| entry.word(w, all)).then_some(entry))
        .collect();
    if !words.is_empty() {
        found.sort_by_key(|e| Reverse(e.score));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(scope: &str, name: &str, kind: ChannelKind) -> Candidate {
        Candidate {
            server: 0,
            team: 0,
            chan: 0,
            scope: scope.into(),
            name: name.into(),
            kind,
            unread: false,
        }
    }

    fn names<'a>(found: &[Entry<'a>]) -> Vec<&'a str> {
        found.iter().map(|e| e.candidate.name.as_str()).collect()
    }

    #[test]
    fn letters_must_appear_in_order_ignoring_case() {
        assert_eq!(
            fuzzy("tsq", "Town Square").map(|r| r.1),
            Some(vec![0, 5, 6])
        );
        assert_eq!(fuzzy("qst", "Town Square"), None);
        assert_eq!(fuzzy("zz", "Pizza").map(|r| r.1), Some(vec![2, 3]));
        assert!(fuzzy("", "anything").is_some());
    }

    #[test]
    fn better_matches_score_higher() {
        let score = |q, t| fuzzy(q, t).unwrap().0;
        assert!(score("tow", "town") > score("tow", "tattoo-w"));
        assert!(score("sq", "squid") > score("sq", "town square"));
        assert!(score("town", "town") > score("town", "town square"));
        assert!(score("sq", "a square") > score("sq", "asqa"));
        assert!(score("a", "ab") > score("a", "abcdef"));
    }

    #[test]
    fn a_word_can_be_split_over_the_scope_and_the_name() {
        let (_, scope, name) = fuzzy_both("fanshit", "fan club", "shitpost").unwrap();
        assert_eq!(scope, [0, 1, 2]);
        assert_eq!(name, [0, 1, 2, 3]);
        assert!(fuzzy_both("zzz", "fan club", "shitpost").is_none());
        assert!(fuzzy_both("a", "a", "a").is_none());
    }

    fn sample() -> Vec<Candidate> {
        vec![
            cand("work:Engineering", "Town Square", ChannelKind::Public),
            cand("work:Engineering", "Secret", ChannelKind::Private),
            cand("work:Engineering", "ana", ChannelKind::Direct),
            cand("work:Engineering", "ana, bob", ChannelKind::Group),
            cand("home:Family", "Town Hall", ChannelKind::Public),
        ]
    }

    #[test]
    fn no_word_keeps_the_order_and_a_word_sorts_by_score() {
        let list = sample();
        assert_eq!(names(&entries(&[], &list, true)).len(), 5);
        assert_eq!(names(&entries(&[], &list, true))[0], "Town Square");
        let found = entries(&["to"], &list, true);
        assert_eq!(names(&found), ["Town Hall", "Town Square"]);
    }

    #[test]
    fn at_looks_only_at_direct_and_group_messages() {
        let list = sample();
        assert_eq!(names(&entries(&["@"], &list, false)), ["ana", "ana, bob"]);
        assert_eq!(names(&entries(&["@an"], &list, false)), ["ana", "ana, bob"]);
        assert_eq!(names(&entries(&["@bob"], &list, false)), ["ana, bob"]);
        assert!(entries(&["@town"], &list, false).is_empty());
    }

    #[test]
    fn a_hash_ties_the_scope_to_the_channel() {
        let list = sample();
        assert_eq!(names(&entries(&["eng#tow"], &list, true)), ["Town Square"]);
        assert_eq!(names(&entries(&["wk#tow"], &list, true)), ["Town Square"]);
        assert_eq!(names(&entries(&["home#tow"], &list, true)), ["Town Hall"]);
        assert_eq!(names(&entries(&["#sec"], &list, true)), ["Secret"]);
        assert_eq!(names(&entries(&["fam#"], &list, true)), ["Town Hall"]);
        assert!(entries(&["fam#sec"], &list, true).is_empty());
    }

    #[test]
    fn every_word_must_match_and_scope_words_only_count_everywhere() {
        let list = sample();
        assert_eq!(
            names(&entries(&["town", "sq"], &list, true)),
            ["Town Square"]
        );
        assert_eq!(names(&entries(&["family"], &list, true)), ["Town Hall"]);
        assert!(entries(&["family"], &list, false).is_empty());
        assert_eq!(names(&entries(&["famhall"], &list, true)), ["Town Hall"]);
    }

    #[test]
    fn hits_are_recorded_for_the_highlight() {
        let list = sample();
        let found = entries(&["eng#tsq"], &list, true);
        assert_eq!(found[0].name_hits, [0, 5, 6]);
        assert_eq!(found[0].scope_hits, [5, 6, 7]);
    }

    #[test]
    fn wide_characters_are_matched_by_character() {
        let list = vec![cand("a:b", "日本語-chat", ChannelKind::Public)];
        let found = entries(&["本c"], &list, false);
        assert_eq!(found[0].name_hits, [1, 4]);
    }
}
