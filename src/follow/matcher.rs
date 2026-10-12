//! Rule matching over guide rows (DESIGN §33 §5.4), on top of `textmatch` (fold, terms,
//! fixture, airing). OWNER: F-AB.
//!
//! One automaton over every rule's terms; a row's folded fields run through it once each, and
//! only the rules whose terms hit are checked further (leave-out words, highlights and
//! repeats, categories, "also needs", the shape). Pure: the scan feeds rows, the plan groups
//! the hits into events.

use crate::follow::{Airings, FollowRule, Shape};
use crate::model::FollowRuleId;
use crate::textmatch::airing::{AiringKind, airing_kind};
use crate::textmatch::fold::{fold, fold_append, has_token};
use crate::textmatch::terms::{Owner, TermSet};

/// One guide row as the scan reads it (borrowed text, folded by the matcher).
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    pub title: &'a str,
    pub sub_title: Option<&'a str>,
    pub description: Option<&'a str>,
    /// `\x1f`-joined guide categories.
    pub categories: Option<&'a str>,
    /// `ProgrammeFlags` bits.
    pub flags: u32,
    pub season: Option<i32>,
    pub episode: Option<i32>,
}

/// Why a row a rule's terms hit is not one of its matches (the preview's "left out: …").
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LeftOut {
    /// A "leave out" word (the word as the user wrote it).
    Without(String),
    Highlights,
    Repeat,
    /// Not in the rule's sports / categories.
    Category,
    /// None of the "also needs" words ("Qualifying", "Race").
    Missing,
    /// Not a fixture of the team ("Arsenal: The Invincibles").
    NotFixture,
    /// The title isn't the show.
    NotTitle,
    /// A spin-off ("Taskmaster NZ") while spin-offs are off.
    SpinOff,
    /// Not the live airing (an earlier airing of the same event was live or came first).
    NotLive,
    /// Already shown before (new episodes only).
    NotNew,
    /// Outside the rule's days or hours.
    Time,
    /// Not on a channel in the rule's scope.
    Channel,
}

impl LeftOut {
    /// The preview's wording ("left out: Women").
    pub fn label(&self) -> String {
        match self {
            LeftOut::Without(w) => w.clone(),
            LeftOut::Highlights => "highlights".into(),
            LeftOut::Repeat => "repeat".into(),
            LeftOut::Category => "other sport".into(),
            LeftOut::Missing => "not the sessions asked for".into(),
            LeftOut::NotFixture => "not a match".into(),
            LeftOut::NotTitle => "another programme".into(),
            LeftOut::SpinOff => "spin-off".into(),
            LeftOut::NotLive => "not live".into(),
            LeftOut::NotNew => "shown before".into(),
            LeftOut::Time => "outside the days or hours".into(),
            LeftOut::Channel => "not on the chosen channels".into(),
        }
    }
}

/// One rule, compiled.
#[derive(Debug)]
struct Compiled {
    id: FollowRuleId,
    terms: Vec<String>,
    with_any: Vec<String>,
    /// (folded, as written)
    without: Vec<(String, String)>,
    categories: Vec<String>,
    fields: crate::follow::spec::Fields,
    shape: Shape,
    airings: Airings,
    /// Highlights count (never for a spoiler-safe rule: highlights are results).
    highlights: bool,
}

/// Bits of the fields a term hit in.
const IN_TITLE: u8 = 1;
const IN_SUB: u8 = 2;
const IN_DESC: u8 = 4;
const IN_CATS: u8 = 8;

/// Reusable buffers of one scanning thread.
#[derive(Debug, Default)]
pub struct Scratch {
    title: String,
    sub: String,
    desc: String,
    cats: String,
    hits: Vec<Owner>,
    /// (rule index, field bits)
    cands: Vec<(u32, u8)>,
}

/// What one rule says about one row.
pub type Verdict = (usize, Result<AiringKind, LeftOut>);

/// The matcher over every given rule's terms (one automaton). Rule `i` is `rules[i]`.
#[derive(Debug, Default)]
pub struct Matcher {
    rules: Vec<Compiled>,
    terms: TermSet,
    /// One token of every term, ASCII case-insensitive, over the raw text: an ASCII field
    /// without any of them can't hold a term, so it isn't folded at all (most rows). `None`
    /// when a term is made only of tokens folding invents ("and" from "&", "v" from "vs").
    prefilter: Option<aho_corasick::AhoCorasick>,
    want_sub: bool,
    want_desc: bool,
    want_cats: bool,
}

/// The prefilter over one token per term (the longest one folding can't invent).
fn prefilter(terms: &[String]) -> Option<aho_corasick::AhoCorasick> {
    let mut tokens: Vec<&str> = Vec::with_capacity(terms.len());
    for t in terms {
        let tok = t.split(' ').filter(|w| *w != "and" && *w != "v" && w.is_ascii()).max_by_key(|w| w.len())?;
        tokens.push(tok);
    }
    tokens.sort_unstable();
    tokens.dedup();
    aho_corasick::AhoCorasick::builder().ascii_case_insensitive(true).kind(Some(aho_corasick::AhoCorasickKind::DFA)).build(&tokens).ok()
}

impl Matcher {
    pub fn new(rules: &[FollowRule]) -> Self {
        let f = crate::textmatch::fold::folded;
        let mut pairs = Vec::new();
        let compiled: Vec<Compiled> = rules
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let terms: Vec<String> = r.spec.terms.iter().map(|t| f(t)).filter(|t| !t.is_empty()).collect();
                pairs.extend(terms.iter().map(|t| (t.clone(), i as Owner)));
                Compiled {
                    id: r.id,
                    terms,
                    with_any: r.spec.with_any.iter().map(|t| f(t)).filter(|t| !t.is_empty()).collect(),
                    without: r.spec.without.iter().map(|t| (f(t), t.trim().to_owned())).filter(|t| !t.0.is_empty()).collect(),
                    categories: r.spec.categories.iter().map(|t| f(t)).filter(|t| !t.is_empty()).collect(),
                    fields: r.spec.fields,
                    shape: r.spec.shape.clone(),
                    airings: r.spec.airings,
                    highlights: r.spec.include_highlights && !r.spoiler_safe,
                }
            })
            .collect();
        let all: Vec<String> = pairs.iter().map(|p| p.0.clone()).collect();
        Self {
            want_sub: compiled.iter().any(|c| c.fields.sub_title),
            want_desc: compiled.iter().any(|c| c.fields.description),
            want_cats: compiled.iter().any(|c| c.fields.categories),
            terms: TermSet::new(&pairs),
            prefilter: if all.is_empty() { None } else { prefilter(&all) },
            rules: compiled,
        }
    }

    /// `raw` may hold a term (always true for non-ASCII text: folding spells it differently).
    fn may_hold(&self, raw: &str) -> bool {
        match &self.prefilter {
            Some(p) => !raw.is_ascii() || p.is_match(raw),
            None => true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// The rule ids of `verdicts` (by index).
    pub fn rule_id(&self, index: usize) -> FollowRuleId {
        self.rules[index].id
    }

    /// The rules this row matches (shape, without, fields, airings applied).
    pub fn matches(&self, row: &Row<'_>, s: &mut Scratch) -> Vec<FollowRuleId> {
        let mut out = Vec::new();
        self.evaluate(row, s, &mut out);
        out.into_iter().filter(|(_, v)| v.is_ok()).map(|(i, _)| self.rules[i].id).collect()
    }

    /// Every rule whose terms hit this row, with its verdict (`out` cleared first).
    pub fn evaluate(&self, row: &Row<'_>, s: &mut Scratch, out: &mut Vec<Verdict>) {
        out.clear();
        if self.terms.is_empty() {
            return;
        }
        // fold a field only when its raw text may hold a term (the prefilter); the checks
        // below fold what they still need
        s.cands.clear();
        let title_folded = self.may_hold(row.title);
        if title_folded {
            fold(row.title, &mut s.title);
            self.collect(IN_TITLE, Field::Title, s);
        }
        let mut sub_folded = false;
        if self.want_sub
            && let Some(sub) = row.sub_title.filter(|t| self.may_hold(t))
        {
            fold(sub, &mut s.sub);
            sub_folded = true;
            self.collect(IN_SUB, Field::Sub, s);
        }
        let mut desc_folded = false;
        if self.want_desc
            && let Some(d) = row.description.filter(|t| self.may_hold(t))
        {
            fold(d, &mut s.desc);
            desc_folded = true;
            self.collect(IN_DESC, Field::Desc, s);
        }
        let mut cats_folded = false;
        if self.want_cats
            && let Some(c) = row.categories.filter(|t| self.may_hold(t))
        {
            fold_cats(c, &mut s.cats);
            cats_folded = true;
            self.collect(IN_CATS, Field::Cats, s);
        }
        if s.cands.is_empty() {
            return;
        }
        if !title_folded {
            fold(row.title, &mut s.title);
        }
        if !sub_folded {
            fold(row.sub_title.unwrap_or(""), &mut s.sub);
        }
        let airing = airing_kind(row.title, row.sub_title, row.flags);
        for k in 0..s.cands.len() {
            let (i, bits) = s.cands[k];
            let rule = &self.rules[i as usize];
            let allowed = (rule.fields.title as u8 * IN_TITLE)
                | (rule.fields.sub_title as u8 * IN_SUB)
                | (rule.fields.description as u8 * IN_DESC)
                | (rule.fields.categories as u8 * IN_CATS);
            if bits & allowed == 0 {
                continue;
            }
            if !rule.categories.is_empty() && !cats_folded {
                fold_cats(row.categories.unwrap_or(""), &mut s.cats);
                cats_folded = true;
            }
            if rule.fields.description && !rule.with_any.is_empty() && !desc_folded {
                fold(row.description.unwrap_or(""), &mut s.desc);
                desc_folded = true;
            }
            out.push((i as usize, self.check(rule, row, s, airing, desc_folded)));
        }
    }

    fn collect(&self, bit: u8, field: Field, s: &mut Scratch) {
        s.hits.clear();
        let text = match field {
            Field::Title => &s.title,
            Field::Sub => &s.sub,
            Field::Desc => &s.desc,
            Field::Cats => &s.cats,
        };
        self.terms.owners_into(text, &mut s.hits);
        for &o in &s.hits {
            match s.cands.iter_mut().find(|(i, _)| *i == o) {
                Some(c) => c.1 |= bit,
                None => s.cands.push((o, bit)),
            }
        }
    }

    fn check(&self, rule: &Compiled, row: &Row<'_>, s: &Scratch, airing: AiringKind, desc_folded: bool) -> Result<AiringKind, LeftOut> {
        if let Some((_, word)) = rule.without.iter().find(|(w, _)| has_token(&s.title, w) || has_token(&s.sub, w)) {
            return Err(LeftOut::Without(word.clone()));
        }
        if airing == AiringKind::Highlights && !rule.highlights {
            return Err(LeftOut::Highlights);
        }
        if airing == AiringKind::Repeat && rule.airings != Airings::Any {
            return Err(LeftOut::Repeat);
        }
        if !rule.categories.is_empty() && !rule.categories.iter().any(|c| category_in(&s.cats, c)) {
            return Err(LeftOut::Category);
        }
        if !rule.with_any.is_empty() {
            let found = |w: &str| {
                (rule.fields.title && has_token(&s.title, w))
                    || (rule.fields.sub_title && has_token(&s.sub, w))
                    || (rule.fields.description && desc_folded && has_token(&s.desc, w))
            };
            if !rule.with_any.iter().any(|w| found(w)) {
                return Err(LeftOut::Missing);
            }
        }
        match &rule.shape {
            Shape::Anywhere => {}
            Shape::Fixture => {
                let fixture = crate::textmatch::fixture::parse_fixture(row.title, row.sub_title).or_else(|| {
                    let d = row.description.filter(|_| rule.fields.description)?;
                    let first = d.split(". ").next().unwrap_or(d);
                    crate::textmatch::fixture::parse_fixture(first, None)
                });
                let Some((a, b)) = fixture else { return Err(LeftOut::NotFixture) };
                let (a, b) = (crate::textmatch::fold::folded(&a), crate::textmatch::fold::folded(&b));
                if !rule.terms.iter().any(|t| has_token(&a, t) || has_token(&b, t)) {
                    return Err(LeftOut::NotFixture);
                }
            }
            Shape::TitleIs { spin_offs } => {
                let core = crate::textmatch::event::core_title(row.title);
                let exact = rule.terms.contains(&core);
                let spin = rule.terms.iter().any(|t| core.len() > t.len() && core.starts_with(t.as_str()) && core.as_bytes()[t.len()] == b' ');
                match (exact, spin, spin_offs) {
                    (true, _, _) | (false, true, true) => {}
                    (false, true, false) => return Err(LeftOut::SpinOff),
                    (false, false, _) => return Err(LeftOut::NotTitle),
                }
            }
        }
        Ok(airing)
    }
}

#[derive(Clone, Copy)]
enum Field {
    Title,
    Sub,
    Desc,
    Cats,
}

/// `\x1f`-joined categories folded into one buffer.
fn fold_cats(c: &str, out: &mut String) {
    out.clear();
    for part in c.split('\x1f') {
        fold_append(part, out);
    }
}

/// A rule category in the row's folded categories, singular or plural ("Sport" / "Sports").
fn category_in(cats: &str, c: &str) -> bool {
    has_token(cats, c) || has_token(cats, &format!("{c}s")) || c.strip_suffix('s').is_some_and(|one| has_token(cats, one))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::follow::spec::Fields;
    use crate::follow::{FollowKind, FollowSpec};
    use crate::textmatch::airing::FLAG_LIVE;

    fn rule(id: i64, kind: FollowKind, terms: &[&str]) -> FollowRule {
        let mut r = FollowRule::draft("r", kind, terms.iter().map(|t| (*t).to_owned()).collect(), &Default::default(), 0);
        r.id = FollowRuleId(id);
        r
    }

    fn row<'a>(title: &'a str, sub: Option<&'a str>, desc: Option<&'a str>, flags: u32) -> Row<'a> {
        Row { title, sub_title: sub, description: desc, categories: Some("Sport\x1fFootball"), flags, season: None, episode: None }
    }

    fn verdict(m: &Matcher, r: Row<'_>) -> Option<Result<AiringKind, LeftOut>> {
        let (mut s, mut out) = (Scratch::default(), Vec::new());
        m.evaluate(&r, &mut s, &mut out);
        out.into_iter().next().map(|v| v.1)
    }

    /// The design's real UK guide strings (§9.1).
    #[test]
    fn team_rules_match_fixtures_of_the_senior_side_only() {
        let m = Matcher::new(&[rule(1, FollowKind::Team, &["Arsenal"])]);
        assert_eq!(verdict(&m, row("Premier League Football", Some("Arsenal v Chelsea"), None, FLAG_LIVE)), Some(Ok(AiringKind::Live)));
        assert_eq!(verdict(&m, row("Live: Arsenal vs. Chelsea", None, None, 0)), Some(Ok(AiringKind::Live)));
        assert_eq!(verdict(&m, row("Arsenal Women v Chelsea Women", None, None, 0)), Some(Err(LeftOut::Without("Women".into()))));
        assert_eq!(verdict(&m, row("Arsenal v Chelsea Women", None, None, 0)), Some(Err(LeftOut::Without("Women".into()))));
        assert_eq!(verdict(&m, row("Arsenal: The Invincibles", None, None, 0)), Some(Err(LeftOut::NotFixture)));
        assert_eq!(verdict(&m, row("Chelsea v Arsenal (R)", None, None, 0)), Some(Err(LeftOut::Repeat)));
        assert_eq!(verdict(&m, row("Match of the Day", None, Some("Arsenal v Chelsea and all the goals."), 0)), Some(Err(LeftOut::Highlights)));
        assert_eq!(verdict(&m, row("Arsenale Venezia v Roma", None, None, 0)), None, "whole tokens only");
        // a team named only in the description's first sentence
        assert_eq!(
            verdict(&m, row("Premier League Football", None, Some("Arsenal v Chelsea. Live from the Emirates."), 0)),
            Some(Ok(AiringKind::Unknown))
        );
        // Atlético with or without its accent
        let m = Matcher::new(&[rule(2, FollowKind::Team, &["Atletico Madrid"])]);
        assert_eq!(verdict(&m, row("LaLiga: Atlético Madrid v Sevilla", None, None, 0)), Some(Ok(AiringKind::Unknown)));
    }

    #[test]
    fn event_and_show_rules() {
        let mut f1 = rule(1, FollowKind::Event, &["Formula 1", "F1"]);
        f1.spec.with_any = vec!["Qualifying".into(), "Grand Prix".into(), "Sprint".into(), "Race".into()];
        f1.spec.without = vec!["Practice".into(), "Academy".into()];
        let m = Matcher::new(&[f1]);
        assert_eq!(verdict(&m, row("Formula 1: Live Japanese Grand Prix Qualifying", None, None, 0)), Some(Ok(AiringKind::Unknown)));
        assert_eq!(verdict(&m, row("F1 Academy", None, None, 0)), Some(Err(LeftOut::Without("Academy".into()))));
        assert_eq!(verdict(&m, row("The F1 Show", None, None, 0)), Some(Err(LeftOut::Missing)));
        assert_eq!(verdict(&m, row("Formula 1: Japanese GP Practice 3", None, None, 0)), Some(Err(LeftOut::Without("Practice".into()))));

        let show = rule(2, FollowKind::Show, &["Taskmaster"]);
        let m = Matcher::new(std::slice::from_ref(&show));
        assert_eq!(verdict(&m, row("Taskmaster", Some("Hot Dogs"), None, 0)), Some(Ok(AiringKind::Unknown)));
        assert_eq!(verdict(&m, row("New: Taskmaster", None, None, 0)), Some(Ok(AiringKind::New)));
        assert_eq!(verdict(&m, row("Taskmaster NZ", None, None, 0)), Some(Err(LeftOut::SpinOff)));
        assert_eq!(verdict(&m, row("Taskmaster: The Podcast Special", None, None, 0)), Some(Err(LeftOut::SpinOff)));
        let mut spin = show;
        spin.spec.shape = Shape::TitleIs { spin_offs: true };
        assert_eq!(verdict(&Matcher::new(&[spin]), row("Taskmaster NZ", None, None, 0)), Some(Ok(AiringKind::Unknown)));
    }

    /// The raw-text prefilter skips folding, never a match: punctuation, "&", "vs" and accents
    /// still reach the folded comparison.
    #[test]
    fn prefilter_never_hides_a_match() {
        let m = Matcher::new(&[rule(1, FollowKind::Custom, &["Man Utd", "Brighton & Hove Albion", "Atletico", "v"])]);
        assert!(m.prefilter.is_none(), "a term folding can invent disables the prefilter");
        let m = Matcher::new(&[rule(1, FollowKind::Custom, &["Man Utd", "Brighton & Hove Albion", "Atletico"])]);
        assert!(m.prefilter.is_some());
        let mut s = Scratch::default();
        for title in ["Man. Utd v Spurs", "BRIGHTON and Hove Albion", "Atlético Madrid", "LaLiga: ATLETICO"] {
            assert_eq!(m.matches(&row(title, None, None, 0), &mut s), [FollowRuleId(1)], "{title}");
        }
        assert!(m.matches(&row("Manchester", None, None, 0), &mut s).is_empty());
    }

    #[test]
    fn categories_fields_and_several_rules() {
        let mut england = rule(1, FollowKind::Team, &["England"]);
        england.spec.categories = vec!["Football".into()];
        let mut rugby = rule(2, FollowKind::Team, &["England"]);
        rugby.spec.categories = vec!["Rugby".into()];
        let mut anywhere = rule(3, FollowKind::Custom, &["Chelsea"]);
        anywhere.spec.fields = Fields { title: false, sub_title: true, description: false, categories: false };
        let m = Matcher::new(&[england, rugby, anywhere]);
        let (mut s, mut out) = (Scratch::default(), Vec::new());
        m.evaluate(&row("International Football", Some("England v Wales"), None, 0), &mut s, &mut out);
        out.sort_by_key(|v| v.0);
        assert_eq!(out, [(0, Ok(AiringKind::Unknown)), (1, Err(LeftOut::Category))]);
        // a term only in a field the rule doesn't look in is no hit at all
        m.evaluate(&row("Chelsea TV", None, None, 0), &mut s, &mut out);
        assert!(out.is_empty());
        assert_eq!(m.matches(&row("Football", Some("Chelsea v Spurs"), None, 0), &mut s), [FollowRuleId(3)]);
        assert!(Matcher::new(&[FollowRule { spec: FollowSpec::default(), ..rule(4, FollowKind::Custom, &[]) }]).is_empty());
    }
}
