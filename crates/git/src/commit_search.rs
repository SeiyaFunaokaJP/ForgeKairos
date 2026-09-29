use crate::Oid;
use crate::repository::{Branch, LogSource, SearchCommitArgs, TagInfo};
use std::collections::HashSet;
use std::ops::Range;
use std::str::FromStr;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

/// A commit search stops collecting commits here: a search that finds more is too broad to go
/// through one by one, and a `変更:` search would otherwise keep reading diffs to the end.
pub const COMMIT_SEARCH_RESULT_LIMIT: usize = 10_000;

/// What a `xxx:` condition in the commit search box narrows the search by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommitSearchConditionKind {
    Author,
    Tag,
    Branch,
    Path,
    Since,
    Until,
    Change,
    Merge,
    Sha,
}

impl CommitSearchConditionKind {
    pub const ALL: [Self; 9] = [
        Self::Author,
        Self::Tag,
        Self::Branch,
        Self::Path,
        Self::Since,
        Self::Until,
        Self::Change,
        Self::Merge,
        Self::Sha,
    ];

    /// The words that introduce the condition, before the colon. The English ones are matched
    /// ignoring ASCII case.
    fn prefix_words(self) -> &'static [&'static str] {
        match self {
            Self::Author => &["author", "著者"],
            Self::Tag => &["tag", "タグ"],
            Self::Branch => &["branch", "ブランチ"],
            Self::Path => &["path", "file", "ファイル"],
            Self::Since => &["since", "after", "以降"],
            Self::Until => &["until", "before", "以前"],
            Self::Change => &["change", "変更"],
            Self::Merge => &["merge", "マージ"],
            Self::Sha => &["sha"],
        }
    }

    /// The name the condition goes by in the search box's chips and messages, in the display
    /// language.
    pub fn label(self) -> &'static str {
        match self {
            Self::Author => i18n::tc!("commit search", "Author"),
            Self::Tag => i18n::tc!("commit search", "Tag"),
            Self::Branch => i18n::tc!("commit search", "Branch"),
            Self::Path => i18n::tc!("commit search", "File"),
            Self::Since => i18n::tc!("commit search", "Since"),
            Self::Until => i18n::tc!("commit search", "Until"),
            Self::Change => i18n::tc!("commit search", "Change"),
            Self::Merge => i18n::tc!("commit search", "Merge"),
            Self::Sha => "sha",
        }
    }

    /// The word the search box writes before the colon: the label, when the query takes it,
    /// and otherwise the English word, so that a condition written in a language the query
    /// does not read still reads back.
    pub fn query_prefix(self) -> &'static str {
        let label = self.label();
        let words = self.prefix_words();
        words
            .iter()
            .find(|word| word.eq_ignore_ascii_case(label))
            .or_else(|| words.first())
            .copied()
            .unwrap_or(label)
    }
}

/// One `xxx:value` condition found in a commit search query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitSearchCondition {
    pub kind: CommitSearchConditionKind,
    pub value: String,
    /// Where the condition, prefix and quotes included, sits in the query.
    pub range: Range<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedCommitSearchQuery {
    /// The words that are not conditions, to look for in commit messages.
    pub text: String,
    pub conditions: Vec<CommitSearchCondition>,
}

/// Whether a search that has filters also lists merge commits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MergeFilter {
    #[default]
    Include,
    Exclude,
    Only,
}

impl MergeFilter {
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let is = |words: &[&str]| words.iter().any(|word| word.eq_ignore_ascii_case(value));
        if is(&["yes", "include", "はい", "含む"]) {
            Some(Self::Include)
        } else if is(&["no", "exclude", "いいえ", "除く"]) {
            Some(Self::Exclude)
        } else if is(&["only", "だけ", "のみ"]) {
            Some(Self::Only)
        } else {
            None
        }
    }

    /// The value the advanced search form writes into the query, in the display language when
    /// [`Self::parse`] reads it back.
    pub fn query_value(self) -> &'static str {
        let (translated, english) = match self {
            Self::Include => (i18n::tc!("commit search", "yes"), "yes"),
            Self::Exclude => (i18n::tc!("commit search", "no"), "no"),
            Self::Only => (i18n::tc!("commit search", "only"), "only"),
        };
        if Self::parse(translated) == Some(self) {
            translated
        } else {
            english
        }
    }
}

fn is_colon(character: char) -> bool {
    matches!(character, ':' | '：')
}

/// The condition `text` starts with, and how many bytes its prefix and colon take.
fn match_condition_prefix(text: &str) -> Option<(CommitSearchConditionKind, usize)> {
    CommitSearchConditionKind::ALL.iter().find_map(|kind| {
        kind.prefix_words().iter().find_map(|word| {
            let candidate = text.get(..word.len())?;
            if !candidate.eq_ignore_ascii_case(word) {
                return None;
            }
            let colon = text.get(word.len()..)?.chars().next()?;
            is_colon(colon).then_some((*kind, word.len() + colon.len_utf8()))
        })
    })
}

fn next_whitespace(text: &str, from: usize) -> usize {
    text.get(from..)
        .and_then(|rest| rest.find(char::is_whitespace))
        .map_or(text.len(), |offset| from + offset)
}

/// Splits a commit search query into its conditions (`著者:山田`, `tag:v1.0`, ...) and the text
/// left over for the commit messages.
///
/// A value can be quoted (`著者:"山田 太郎"`) to hold a space; unquoted it ends at the first space,
/// fullwidth ones included. A word that only looks like a condition stays text: an unknown prefix
/// (`fix: typo` still searches the messages for `fix: typo`) and a prefix with nothing after it,
/// so that a half-typed `著者:` cannot quietly turn into "every commit".
pub fn parse_commit_search_query(query: &str) -> ParsedCommitSearchQuery {
    let mut conditions = Vec::new();
    let mut index = 0;
    while index < query.len() {
        let Some(rest) = query.get(index..) else {
            break;
        };
        let Some(token_offset) = rest.find(|character: char| !character.is_whitespace()) else {
            break;
        };
        let token_start = index + token_offset;
        if let Some((kind, prefix_len)) = query.get(token_start..).and_then(match_condition_prefix)
        {
            let value_start = token_start + prefix_len;
            let after_prefix = query.get(value_start..).unwrap_or_default();
            let (value, end) = if let Some(quoted) = after_prefix.strip_prefix('"') {
                match quoted.find('"') {
                    Some(close) => (&quoted[..close], value_start + 1 + close + 1),
                    None => (quoted, query.len()),
                }
            } else {
                let end = next_whitespace(query, value_start);
                (query.get(value_start..end).unwrap_or_default(), end)
            };
            let value = value.trim();
            if !value.is_empty() {
                conditions.push(CommitSearchCondition {
                    kind,
                    value: value.to_string(),
                    range: token_start..end,
                });
                index = end;
                continue;
            }
        }
        index = next_whitespace(query, token_start);
    }

    ParsedCommitSearchQuery {
        text: text_outside(query, conditions.iter().map(|condition| &condition.range)),
        conditions,
    }
}

/// The query with the given ranges cut out, the pieces left joined by one space.
fn text_outside<'a>(query: &str, ranges: impl Iterator<Item = &'a Range<usize>>) -> String {
    let mut pieces = Vec::new();
    let mut start = 0;
    for range in ranges {
        pieces.push(query.get(start..range.start).unwrap_or_default());
        start = range.end;
    }
    pieces.push(query.get(start..).unwrap_or_default());
    pieces
        .into_iter()
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `kind:value` as the query spells it, quoted when the value holds a space. `None` for a value
/// the query cannot hold: one with a `"` that would also need quoting.
pub fn format_commit_search_condition(
    kind: CommitSearchConditionKind,
    value: &str,
) -> Option<String> {
    let value = value.trim();
    let needs_quotes = value.contains(char::is_whitespace) || value.starts_with('"');
    if needs_quotes && value.contains('"') {
        return None;
    }
    let prefix = kind.query_prefix();
    Some(if needs_quotes {
        format!("{prefix}:\"{value}\"")
    } else {
        format!("{prefix}:{value}")
    })
}

/// The query without the condition at `range`, as the search box's chip × leaves it.
pub fn remove_commit_search_condition(query: &str, range: Range<usize>) -> String {
    let before = query.get(..range.start).unwrap_or(query).trim_end();
    let after = query.get(range.end..).unwrap_or_default().trim_start();
    match (before.is_empty(), after.is_empty()) {
        (true, _) => after.to_string(),
        (_, true) => before.to_string(),
        _ => format!("{before} {after}"),
    }
}

/// The query with every condition of `kind` replaced by one for `value`, or taken out when
/// `value` is empty. The new condition goes at the end, after the words already there.
pub fn set_commit_search_condition(
    query: &str,
    kind: CommitSearchConditionKind,
    value: &str,
) -> Option<String> {
    let parsed = parse_commit_search_query(query);
    let mut rest = query.to_string();
    for condition in parsed.conditions.iter().rev() {
        if condition.kind == kind {
            rest = remove_commit_search_condition(&rest, condition.range.clone());
        }
    }
    let rest = rest.trim();
    if value.trim().is_empty() {
        return Some(rest.to_string());
    }
    let condition = format_commit_search_condition(kind, value)?;
    Some(if rest.is_empty() {
        condition
    } else {
        format!("{rest} {condition}")
    })
}

/// Which end of a period a date in the search box stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateBound {
    Since,
    Until,
}

fn normalize_date_text(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| match character {
            '０'..='９' => {
                char::from_digit(character as u32 - '０' as u32, 10).unwrap_or(character)
            }
            '／' => '/',
            '－' | '−' | 'ー' => '-',
            '：' => ':',
            '　' => ' ',
            other => other,
        })
        .collect()
}

fn subtract_months(date: Date, months: i64) -> Option<Date> {
    let total = i64::from(date.year()) * 12 + i64::from(u8::from(date.month())) - 1 - months;
    let year = i32::try_from(total.div_euclid(12)).ok()?;
    let month = Month::try_from(u8::try_from(total.rem_euclid(12) + 1).ok()?).ok()?;
    let day = date.day().min(month.length(year));
    Date::from_calendar_date(year, month, day).ok()
}

fn parse_relative_date(value: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
    let value = value.strip_suffix('前').unwrap_or(value).trim();
    let digits_end = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (digits, unit) = value.split_at(digits_end);
    let amount = digits
        .parse::<i64>()
        .ok()
        .filter(|amount| *amount <= 100_000)?;
    let unit = unit.trim().to_ascii_lowercase();
    let days = |days: i64| now.checked_sub(time::Duration::days(days));
    let months =
        |months: i64| subtract_months(now.date(), months).map(|date| now.replace_date(date));
    match unit.as_str() {
        "d" | "day" | "days" | "日" => days(amount),
        "w" | "week" | "weeks" | "週" | "週間" => days(amount.checked_mul(7)?),
        "m" | "month" | "months" | "か月" | "ヶ月" | "ヵ月" | "カ月" | "ケ月" | "箇月" => {
            months(amount)
        }
        "y" | "year" | "years" | "年" => months(amount.checked_mul(12)?),
        _ => None,
    }
}

fn parse_absolute_date(value: &str, bound: DateBound, offset: UtcOffset) -> Option<OffsetDateTime> {
    let (date_text, time_text) = match value.split_once([' ', 'T']) {
        Some((date, time)) => (date.trim(), Some(time.trim())),
        None => (value, None),
    };
    let mut parts = date_text.split(['-', '/']);
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = Month::try_from(parts.next()?.parse::<u8>().ok()?).ok()?;
    let day = parts.next()?.parse::<u8>().ok()?;
    if parts.next().is_some() || !(1000..=9999).contains(&year) {
        return None;
    }
    let date = Date::from_calendar_date(year, month, day).ok()?;
    let time = match time_text {
        Some(time_text) => {
            let mut parts = time_text.split(':');
            let hour = parts.next()?.parse::<u8>().ok()?;
            let minute = parts.next()?.parse::<u8>().ok()?;
            let second = match parts.next() {
                Some(second) => second.parse::<u8>().ok()?,
                None => 0,
            };
            if parts.next().is_some() {
                return None;
            }
            Time::from_hms(hour, minute, second).ok()?
        }
        // A day on its own covers all of it: `以前:2026-09-01` still finds that day's commits.
        None => match bound {
            DateBound::Since => Time::MIDNIGHT,
            DateBound::Until => Time::from_hms(23, 59, 59).ok()?,
        },
    };
    Some(PrimitiveDateTime::new(date, time).assume_offset(offset))
}

/// A date from the search box as seconds since the epoch: `2026-09-01`, `2026-09-01 12:00`, or a
/// span back from `now` (`3d` `2w` `1m` `1y`, `3日` `2週間` `1か月` `1年`).
///
/// This is worked out here rather than handed to git, whose date parser reads nearly any text as
/// some date and would turn a typo into a period nobody asked for.
pub fn parse_commit_search_date(value: &str, bound: DateBound, now: OffsetDateTime) -> Option<i64> {
    let value = normalize_date_text(value);
    parse_relative_date(&value, now)
        .or_else(|| parse_absolute_date(&value, bound, now.offset()))
        .map(OffsetDateTime::unix_timestamp)
}

/// `value` as an extended regular expression (what `git log -G` takes) that matches exactly that
/// text, spelling each ASCII letter as `[aA]` when case is ignored. Done here rather than with
/// `--regexp-ignore-case`, which older git does not apply to `-G`.
pub fn change_search_pattern(value: &str, case_sensitive: bool) -> String {
    let mut pattern = String::with_capacity(value.len() * 2);
    for character in value.chars() {
        if !case_sensitive && character.is_ascii_alphabetic() {
            pattern.push('[');
            pattern.push(character.to_ascii_lowercase());
            pattern.push(character.to_ascii_uppercase());
            pattern.push(']');
        } else {
            if matches!(
                character,
                '\\' | '.' | '^' | '$' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}'
            ) {
                pattern.push('\\');
            }
            pattern.push(character);
        }
    }
    pattern
}

/// The path of a `ファイル:` condition the way a pathspec spells it: `/` between the parts, also
/// when pasted from Windows, and relative to the repository. `None` when nothing is left, which
/// would name the whole repository rather than narrow the search.
pub fn normalize_search_path(value: &str) -> Option<String> {
    let path = value.trim().replace('\\', "/");
    let mut path = path.as_str();
    while let Some(rest) = path.strip_prefix("./").or_else(|| path.strip_prefix('/')) {
        path = rest;
    }
    (!path.is_empty() && path != ".").then(|| path.to_string())
}

/// Whether `name` is a full branch ref git takes as nothing but that ref: not an option, not a
/// range or other revision expression, following `git check-ref-format`.
pub fn is_searchable_branch_ref(name: &str) -> bool {
    let Some(short) = name
        .strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/remotes/"))
    else {
        return false;
    };
    !short.is_empty()
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && !name.ends_with('/')
        && !name.ends_with('.')
        && name
            .split('/')
            .all(|component| !component.starts_with('.') && !component.ends_with(".lock"))
        && !name.chars().any(|character| {
            character.is_ascii_control()
                || matches!(
                    character,
                    ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '\u{7f}'
                )
        })
}

/// What the graph knows that turns the conditions into a search.
pub struct CommitSearchEnvironment<'a> {
    pub now: OffsetDateTime,
    pub case_sensitive: bool,
    pub branches: &'a [Branch],
    /// `None` while the tag list is still loading.
    pub tags: Option<&'a [TagInfo]>,
    pub log_source: &'a LogSource,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommitSearchPlan {
    /// Nothing to match on, which finds nothing rather than every commit.
    Empty,
    /// A `タグ:` condition needs the tag list, which has not loaded yet.
    NeedsTags,
    /// Conditions that cannot be searched for, with the reason for each.
    Invalid(Vec<String>),
    Search(SearchCommitArgs),
}

fn contains_text(haystack: &str, needle: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        haystack.contains(needle)
    } else {
        haystack.to_lowercase().contains(&needle.to_lowercase())
    }
}

/// Turns a parsed query into the search to run, or the reasons it cannot run.
///
/// Conditions are anded. A kind whose repeats git cannot and in one `git log` (a branch, a file,
/// changed text, a sha) is refused when repeated rather than quietly searched as "either".
pub fn plan_commit_search(
    parsed: &ParsedCommitSearchQuery,
    environment: &CommitSearchEnvironment,
) -> CommitSearchPlan {
    use CommitSearchConditionKind as Kind;

    let mut problems = Vec::new();
    let mut args = SearchCommitArgs {
        query: parsed.text.clone().into(),
        case_sensitive: environment.case_sensitive,
        ..SearchCommitArgs::default()
    };
    let mut needs_tags = false;

    for kind in [Kind::Branch, Kind::Path, Kind::Change, Kind::Sha] {
        if parsed
            .conditions
            .iter()
            .filter(|condition| condition.kind == kind)
            .count()
            > 1
        {
            problems.push(i18n::t!(
                "Only one \"{prefix}:\" condition can be given.",
                prefix = kind.query_prefix()
            ));
        }
    }

    let is_file_history = matches!(environment.log_source, LogSource::Path(_));
    let mut merges = None;
    for condition in &parsed.conditions {
        let value = condition.value.as_str();
        let prefix = condition.kind.query_prefix();
        match condition.kind {
            Kind::Author => args.authors.push(value.to_string().into()),
            Kind::Tag => {
                let Some(tags) = environment.tags else {
                    needs_tags = true;
                    continue;
                };
                let mut tagged = HashSet::new();
                let commits = tags
                    .iter()
                    .filter(|tag| contains_text(&tag.name, value, environment.case_sensitive))
                    .filter_map(|tag| Oid::from_str(&tag.commit_sha).ok())
                    .filter(|oid| tagged.insert(*oid))
                    .collect::<Vec<_>>();
                if commits.is_empty() {
                    problems.push(i18n::t!("No tag has \"{value}\" in its name."));
                    continue;
                }
                args.commits = Some(match args.commits.take() {
                    Some(previous) => previous
                        .into_iter()
                        .filter(|oid| tagged.contains(oid))
                        .collect(),
                    None => commits,
                });
            }
            Kind::Branch => {
                if !matches!(environment.log_source, LogSource::All) {
                    problems.push(i18n::t!(
                        "\"{prefix}:\" can only be used while the graph shows all branches."
                    ));
                    continue;
                }
                let branch = environment
                    .branches
                    .iter()
                    .filter(|branch| branch.name() == value)
                    .min_by_key(|branch| branch.is_remote());
                match branch {
                    Some(branch) if is_searchable_branch_ref(&branch.ref_name) => {
                        args.branch = Some(branch.ref_name.clone());
                    }
                    Some(_) => problems.push(i18n::t!(
                        "The name of branch \"{branch}\" cannot be used in a search.",
                        branch = value
                    )),
                    None => {
                        problems.push(i18n::t!("There is no branch \"{branch}\".", branch = value))
                    }
                }
            }
            Kind::Path => {
                if is_file_history {
                    problems.push(i18n::t!(
                        "\"{prefix}:\" cannot be used in a file's history."
                    ));
                } else if let Some(path) = normalize_search_path(value) {
                    args.path = Some(path.into());
                } else {
                    problems.push(i18n::t!(
                        "\"{prefix}:{value}\" names the whole repository, so it does not narrow \
                         the search. Write the name of a file or folder."
                    ));
                }
            }
            Kind::Since | Kind::Until => {
                let bound = if condition.kind == Kind::Since {
                    DateBound::Since
                } else {
                    DateBound::Until
                };
                let Some(timestamp) = parse_commit_search_date(value, bound, environment.now)
                else {
                    problems.push(i18n::t!(
                        "Cannot read the date in \"{prefix}:{value}\". Write it like 2026-09-01, \
                         \"2026-09-01 12:00\", or 3d, 2w, 1m, 1y."
                    ));
                    continue;
                };
                match bound {
                    DateBound::Since => {
                        args.since =
                            Some(args.since.map_or(timestamp, |since| since.max(timestamp)))
                    }
                    DateBound::Until => {
                        args.until =
                            Some(args.until.map_or(timestamp, |until| until.min(timestamp)))
                    }
                }
            }
            Kind::Change => args.change = Some(value.to_string().into()),
            Kind::Merge => match MergeFilter::parse(value) {
                Some(filter) if merges.is_none_or(|previous| previous == filter) => {
                    merges = Some(filter)
                }
                Some(_) => problems.push(i18n::t!(
                    "The \"{prefix}:\" conditions contradict each other."
                )),
                None => problems.push(i18n::t!(
                    "Cannot read \"{prefix}:{value}\". Write one of yes, no or only."
                )),
            },
            Kind::Sha => {
                if (4..=40).contains(&value.len())
                    && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    args.sha_prefix = Some(value.to_ascii_lowercase().into());
                } else {
                    problems.push(i18n::t!(
                        "Cannot read \"sha:{value}\". Write 4 to 40 hexadecimal digits."
                    ));
                }
            }
        }
    }
    args.merges = merges.unwrap_or_default();

    if let (Some(since), Some(until)) = (args.since, args.until)
        && since > until
    {
        problems.push(i18n::t!(
            "The period is reversed (\"{since_word}\" is later than \"{until_word}\").",
            since_word = Kind::Since.query_prefix(),
            until_word = Kind::Until.query_prefix()
        ));
    }

    if !problems.is_empty() {
        return CommitSearchPlan::Invalid(problems);
    }
    if needs_tags {
        return CommitSearchPlan::NeedsTags;
    }
    if !args.has_anything_to_match() {
        return CommitSearchPlan::Empty;
    }
    CommitSearchPlan::Search(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use CommitSearchConditionKind as Kind;
    use time::macros::datetime;

    fn conditions(query: &str) -> Vec<(Kind, String)> {
        parse_commit_search_query(query)
            .conditions
            .into_iter()
            .map(|condition| (condition.kind, condition.value))
            .collect()
    }

    fn text(query: &str) -> String {
        parse_commit_search_query(query).text
    }

    #[test]
    fn test_parse_conditions_in_english_and_japanese() {
        for (query, kind) in [
            ("author:alice", Kind::Author),
            ("著者:alice", Kind::Author),
            ("tag:alice", Kind::Tag),
            ("タグ:alice", Kind::Tag),
            ("branch:alice", Kind::Branch),
            ("ブランチ:alice", Kind::Branch),
            ("path:alice", Kind::Path),
            ("file:alice", Kind::Path),
            ("ファイル:alice", Kind::Path),
            ("since:alice", Kind::Since),
            ("after:alice", Kind::Since),
            ("以降:alice", Kind::Since),
            ("until:alice", Kind::Until),
            ("before:alice", Kind::Until),
            ("以前:alice", Kind::Until),
            ("change:alice", Kind::Change),
            ("変更:alice", Kind::Change),
            ("merge:alice", Kind::Merge),
            ("マージ:alice", Kind::Merge),
            ("sha:alice", Kind::Sha),
            ("AUTHOR:alice", Kind::Author),
            ("Tag：alice", Kind::Tag),
            ("ファイル：alice", Kind::Path),
        ] {
            assert_eq!(
                conditions(query),
                [(kind, "alice".to_string())],
                "unexpected conditions for {query:?}"
            );
            assert_eq!(text(query), "", "{query:?} leaves no message text");
        }
    }

    #[test]
    fn test_parse_keeps_text_that_only_looks_like_a_condition() {
        assert_eq!(conditions("fix: typo"), []);
        assert_eq!(text("fix: typo"), "fix: typo");
        assert_eq!(
            text("  fix  typo  "),
            "fix  typo",
            "text without conditions stays as typed"
        );
        assert_eq!(conditions("authority:alice"), []);
        assert_eq!(conditions("著者:"), []);
        assert_eq!(text("著者:"), "著者:");
        assert_eq!(conditions("著者: 直した"), []);
        assert_eq!(text("著者: 直した"), "著者: 直した");
        assert_eq!(conditions("author:\"\" x"), []);
        assert_eq!(
            conditions("著"),
            [],
            "a prefix cut short of a character boundary"
        );
        assert_eq!(conditions("タ"), []);
        assert_eq!(conditions("author::"), [(Kind::Author, ":".to_string())]);
        assert_eq!(
            conditions("prefix-author:alice"),
            [],
            "a prefix only starts a word"
        );
    }

    #[test]
    fn test_parse_quotes_multiple_conditions_and_positions() {
        let parsed =
            parse_commit_search_query("直した 著者:\"山田 太郎\" tag:v1　ファイル:src/ 修正");
        assert_eq!(
            parsed
                .conditions
                .iter()
                .map(|condition| (condition.kind, condition.value.as_str()))
                .collect::<Vec<_>>(),
            [
                (Kind::Author, "山田 太郎"),
                (Kind::Tag, "v1"),
                (Kind::Path, "src/"),
            ]
        );
        assert_eq!(parsed.text, "直した 修正");

        assert_eq!(
            conditions("author:alice author:bob"),
            [
                (Kind::Author, "alice".to_string()),
                (Kind::Author, "bob".to_string())
            ]
        );
        assert_eq!(
            conditions("著者:\"未閉じ の名前"),
            [(Kind::Author, "未閉じ の名前".to_string())]
        );
        assert_eq!(text("著者:\"山田\"直した"), "直した");

        let query = "fix 以降:\"2026-09-01 12:00\" end";
        let parsed = parse_commit_search_query(query);
        let range = parsed.conditions[0].range.clone();
        assert_eq!(&query[range], "以降:\"2026-09-01 12:00\"");
    }

    #[test]
    fn test_format_and_edit_conditions() {
        assert_eq!(
            format_commit_search_condition(Kind::Author, "山田 太郎").as_deref(),
            Some("author:\"山田 太郎\"")
        );
        assert_eq!(
            format_commit_search_condition(Kind::Path, "src/").as_deref(),
            Some("file:src/")
        );
        assert_eq!(
            format_commit_search_condition(Kind::Change, "a \"b\""),
            None
        );
        assert_eq!(
            format_commit_search_condition(Kind::Change, "a\"b").as_deref(),
            Some("change:a\"b")
        );

        let query = "直した 著者:山田 tag:v1";
        assert_eq!(
            set_commit_search_condition(query, Kind::Author, "鈴木 花子").as_deref(),
            Some("直した tag:v1 author:\"鈴木 花子\"")
        );
        assert_eq!(
            set_commit_search_condition(query, Kind::Tag, "").as_deref(),
            Some("直した 著者:山田")
        );
        assert_eq!(
            set_commit_search_condition("", Kind::Merge, "だけ").as_deref(),
            Some("merge:だけ")
        );
        for value in ["鈴木 花子", "src/a b.rs", "2026-09-01 12:00"] {
            for kind in Kind::ALL {
                let written = set_commit_search_condition("x", kind, value).unwrap();
                assert_eq!(
                    conditions(&written),
                    [(kind, value.to_string())],
                    "{written:?} must read back as what the form wrote"
                );
            }
        }

        let parsed = parse_commit_search_query(query);
        assert_eq!(
            remove_commit_search_condition(query, parsed.conditions[0].range.clone()),
            "直した tag:v1"
        );
        assert_eq!(
            remove_commit_search_condition(query, parsed.conditions[1].range.clone()),
            "直した 著者:山田"
        );
    }

    #[test]
    fn test_merge_filter_values() {
        for (value, expected) in [
            ("yes", Some(MergeFilter::Include)),
            ("YES", Some(MergeFilter::Include)),
            ("はい", Some(MergeFilter::Include)),
            ("no", Some(MergeFilter::Exclude)),
            ("いいえ", Some(MergeFilter::Exclude)),
            ("only", Some(MergeFilter::Only)),
            ("だけ", Some(MergeFilter::Only)),
            ("maybe", None),
            ("", None),
        ] {
            assert_eq!(MergeFilter::parse(value), expected, "{value:?}");
        }
        for filter in [
            MergeFilter::Include,
            MergeFilter::Exclude,
            MergeFilter::Only,
        ] {
            assert_eq!(MergeFilter::parse(filter.query_value()), Some(filter));
        }
    }

    /// A catalog in the user's configuration directory can translate the words into anything,
    /// including another condition's word, a word with a colon, or nothing the query reads.
    #[test]
    fn test_search_box_writes_words_the_query_reads_whatever_the_catalog() {
        struct EverythingCatalog(&'static str);
        impl i18n::Catalog for EverythingCatalog {
            fn locale(&self) -> &str {
                "xx"
            }
            fn lookup(&self, _context: Option<&str>, _msgid: &str) -> Option<&'static str> {
                Some(self.0)
            }
            fn lookup_plural(
                &self,
                _context: Option<&str>,
                _msgid: &str,
                _msgid_plural: &str,
                _count: u64,
            ) -> Option<&'static str> {
                None
            }
        }
        static CATALOGS: [EverythingCatalog; 8] = [
            EverythingCatalog("Autor"),
            EverythingCatalog("tag"),
            EverythingCatalog("マージ"),
            EverythingCatalog("only"),
            EverythingCatalog("a:b"),
            EverythingCatalog("\"x y\""),
            EverythingCatalog(""),
            EverythingCatalog(" "),
        ];
        for catalog in &CATALOGS {
            let _locale = i18n::test_support::LocaleOverride::new(Some(catalog));
            for kind in Kind::ALL {
                let written = set_commit_search_condition("x", kind, "abcd").unwrap();
                assert_eq!(
                    conditions(&written),
                    [(kind, "abcd".to_string())],
                    "{:?}: {written:?}",
                    catalog.0
                );
            }
            for filter in [
                MergeFilter::Include,
                MergeFilter::Exclude,
                MergeFilter::Only,
            ] {
                assert_eq!(
                    MergeFilter::parse(filter.query_value()),
                    Some(filter),
                    "{:?}",
                    catalog.0
                );
            }
        }
    }

    #[test]
    fn test_search_box_writes_the_display_language() {
        let _locale = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        assert_eq!(Kind::Author.label(), "著者");
        assert_eq!(
            format_commit_search_condition(Kind::Author, "山田 太郎").as_deref(),
            Some("著者:\"山田 太郎\"")
        );
        assert_eq!(
            set_commit_search_condition("直した", Kind::Merge, MergeFilter::Only.query_value())
                .as_deref(),
            Some("直した マージ:だけ")
        );
        for kind in Kind::ALL {
            let written = set_commit_search_condition("x", kind, "abcd").unwrap();
            assert_eq!(
                conditions(&written),
                [(kind, "abcd".to_string())],
                "{written:?} must read back as what the form wrote"
            );
        }
        for filter in [
            MergeFilter::Include,
            MergeFilter::Exclude,
            MergeFilter::Only,
        ] {
            assert_eq!(MergeFilter::parse(filter.query_value()), Some(filter));
        }
        assert_eq!(
            problems(plan(
                "branch:main branch:feature",
                Some(&[]),
                &LogSource::All
            )),
            ["「ブランチ:」は 1 つだけ指定できます。"]
        );
    }

    #[test]
    fn test_parse_dates() {
        let now = datetime!(2026-03-31 15:30:00 +09:00);
        let at = |value: &str, bound| parse_commit_search_date(value, bound, now);
        let local = |datetime: OffsetDateTime| Some(datetime.unix_timestamp());

        assert_eq!(
            at("2026-09-01", DateBound::Since),
            local(datetime!(2026-09-01 00:00:00 +09:00))
        );
        assert_eq!(
            at("2026-09-01", DateBound::Until),
            local(datetime!(2026-09-01 23:59:59 +09:00)),
            "a day on its own as the end of a period includes that day"
        );
        assert_eq!(
            at("2026/9/1 12:00", DateBound::Until),
            local(datetime!(2026-09-01 12:00:00 +09:00))
        );
        assert_eq!(
            at("2026-09-01T12:00:30", DateBound::Since),
            local(datetime!(2026-09-01 12:00:30 +09:00))
        );
        assert_eq!(
            at("２０２６－０９－０１", DateBound::Since),
            local(datetime!(2026-09-01 00:00:00 +09:00))
        );

        for (value, expected) in [
            ("3d", datetime!(2026-03-28 15:30:00 +09:00)),
            ("3日", datetime!(2026-03-28 15:30:00 +09:00)),
            ("3日前", datetime!(2026-03-28 15:30:00 +09:00)),
            ("３日", datetime!(2026-03-28 15:30:00 +09:00)),
            ("2w", datetime!(2026-03-17 15:30:00 +09:00)),
            ("2週間", datetime!(2026-03-17 15:30:00 +09:00)),
            ("1m", datetime!(2026-02-28 15:30:00 +09:00)),
            ("1か月", datetime!(2026-02-28 15:30:00 +09:00)),
            ("1ヶ月", datetime!(2026-02-28 15:30:00 +09:00)),
            ("13m", datetime!(2025-02-28 15:30:00 +09:00)),
            ("1y", datetime!(2025-03-31 15:30:00 +09:00)),
            ("1年", datetime!(2025-03-31 15:30:00 +09:00)),
            ("0d", now),
        ] {
            assert_eq!(at(value, DateBound::Since), local(expected), "{value:?}");
        }

        for value in [
            "",
            "yesterday",
            "3",
            "d",
            "3x",
            "-3d",
            "2026-13-01",
            "2026-02-30",
            "2026-09",
            "2026-09-01 25:00",
            "2026-09-01 12",
            "26-09-01",
            "99999999999d",
            "1.5d",
        ] {
            assert_eq!(
                at(value, DateBound::Since),
                None,
                "{value:?} must not be read"
            );
        }
    }

    #[test]
    fn test_normalize_search_path() {
        for (value, expected) in [
            ("src/", "src/"),
            ("src\\git\\lib.rs", "src/git/lib.rs"),
            (".\\src\\*.rs", "src/*.rs"),
            ("./a b.txt", "a b.txt"),
            ("/src", "src"),
            ("\\src", "src"),
            ("././src", "src"),
            ("ドキュメント\\読んで.md", "ドキュメント/読んで.md"),
        ] {
            assert_eq!(
                normalize_search_path(value).as_deref(),
                Some(expected),
                "{value:?}"
            );
        }
        for value in ["", " ", "/", "./", ".", "\\", ".\\", "//", "././"] {
            assert_eq!(
                normalize_search_path(value),
                None,
                "{value:?} is the whole repository"
            );
        }
    }

    #[test]
    fn test_change_search_pattern_escapes_regex() {
        assert_eq!(change_search_pattern("a.b*(c)", true), "a\\.b\\*\\(c\\)");
        assert_eq!(change_search_pattern("Ab1", false), "[aA][bB]1");
        assert_eq!(change_search_pattern("日本^$", false), "日本\\^\\$");
        assert_eq!(
            change_search_pattern("x|y?[z]{2}+\\", true),
            "x\\|y\\?\\[z\\]\\{2\\}\\+\\\\"
        );
    }

    #[test]
    fn test_searchable_branch_refs() {
        for name in [
            "refs/heads/main",
            "refs/heads/feature/日本語",
            "refs/remotes/origin/main",
        ] {
            assert!(is_searchable_branch_ref(name), "{name}");
        }
        for name in [
            "main",
            "--all",
            "refs/tags/v1",
            "refs/heads/",
            "refs/heads/a..b",
            "refs/heads/a@{1}",
            "refs/heads/a^",
            "refs/heads/a~1",
            "refs/heads/a:b",
            "refs/heads/.hidden",
            "refs/heads/a.lock",
            "refs/heads/a b",
            "refs/heads/a//b",
            "refs/heads/a/",
        ] {
            assert!(!is_searchable_branch_ref(name), "{name}");
        }
    }

    fn branch(ref_name: &str) -> Branch {
        Branch {
            is_head: false,
            ref_name: ref_name.to_string().into(),
            upstream: None,
            most_recent_commit: None,
        }
    }

    fn tag(name: &str, commit: u8) -> TagInfo {
        TagInfo {
            name: name.to_string(),
            commit_sha: Oid::from_bytes(&[commit; 20]).unwrap().to_string(),
            created_at_unix: None,
            subject: None,
            commit_subject: None,
            annotated: false,
        }
    }

    fn plan(query: &str, tags: Option<&[TagInfo]>, log_source: &LogSource) -> CommitSearchPlan {
        let branches = [
            branch("refs/heads/main"),
            branch("refs/heads/feature"),
            branch("refs/remotes/origin/feature"),
        ];
        plan_commit_search(
            &parse_commit_search_query(query),
            &CommitSearchEnvironment {
                now: datetime!(2026-09-25 12:00:00 +09:00),
                case_sensitive: false,
                branches: &branches,
                tags,
                log_source,
            },
        )
    }

    fn problems(plan: CommitSearchPlan) -> Vec<String> {
        match plan {
            CommitSearchPlan::Invalid(problems) => problems,
            other => panic!("expected problems, got {other:?}"),
        }
    }

    fn search(plan: CommitSearchPlan) -> SearchCommitArgs {
        match plan {
            CommitSearchPlan::Search(args) => args,
            other => panic!("expected a search, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_commit_search() {
        let tags = [tag("v1.0", 1), tag("v1.1", 2), tag("release-1.1", 2)];
        let all = LogSource::All;

        assert_eq!(plan("", Some(&tags), &all), CommitSearchPlan::Empty);
        assert_eq!(plan("   ", Some(&tags), &all), CommitSearchPlan::Empty);
        assert_eq!(
            plan("merge:yes", Some(&tags), &all),
            CommitSearchPlan::Empty,
            "including merges is no filter, and a search with none finds nothing"
        );
        assert_eq!(plan("tag:v1", None, &all), CommitSearchPlan::NeedsTags);

        let args = search(plan(
            "直した 著者:山田 author:yamada branch:feature ファイル:src/ 以降:2026-09-01 以前:2026-09-20 変更:foo マージ:いいえ sha:ABCD",
            Some(&tags),
            &all,
        ));
        assert_eq!(args.query.as_ref(), "直した");
        assert_eq!(args.authors, ["山田", "yamada"]);
        assert_eq!(args.branch.as_deref(), Some("refs/heads/feature"));
        assert_eq!(args.path.as_deref(), Some("src/"));
        assert_eq!(
            search(plan("file:src\\lib.rs", Some(&tags), &all))
                .path
                .as_deref(),
            Some("src/lib.rs"),
            "a path pasted from Windows is searched with `/`"
        );
        assert_eq!(
            args.since,
            Some(datetime!(2026-09-01 00:00:00 +09:00).unix_timestamp())
        );
        assert_eq!(
            args.until,
            Some(datetime!(2026-09-20 23:59:59 +09:00).unix_timestamp())
        );
        assert_eq!(args.change.as_deref(), Some("foo"));
        assert_eq!(args.merges, MergeFilter::Exclude);
        assert_eq!(args.sha_prefix.as_deref(), Some("abcd"));

        assert_eq!(
            search(plan("branch:origin/feature", Some(&tags), &all))
                .branch
                .as_deref(),
            Some("refs/remotes/origin/feature")
        );

        assert_eq!(
            search(plan("tag:V1.1", Some(&tags), &all)).commits,
            Some(vec![Oid::from_bytes(&[2; 20]).unwrap()])
        );
        let mut both = search(plan("tag:v1", Some(&tags), &all)).commits.unwrap();
        both.sort_by_key(|oid| oid.to_string());
        assert_eq!(
            both,
            [
                Oid::from_bytes(&[1; 20]).unwrap(),
                Oid::from_bytes(&[2; 20]).unwrap()
            ]
        );
        assert_eq!(
            search(plan("tag:v1 tag:release", Some(&tags), &all)).commits,
            Some(vec![Oid::from_bytes(&[2; 20]).unwrap()]),
            "two tags and: the commits both name"
        );
        assert_eq!(
            search(plan("tag:v1.0 tag:release", Some(&tags), &all)).commits,
            Some(vec![]),
        );

        let args = search(plan(
            "以降:2026-09-01 以降:2026-09-10 以前:2026-09-20 以前:2026-09-15",
            Some(&tags),
            &all,
        ));
        assert_eq!(
            (args.since, args.until),
            (
                Some(datetime!(2026-09-10 00:00:00 +09:00).unix_timestamp()),
                Some(datetime!(2026-09-15 23:59:59 +09:00).unix_timestamp())
            ),
            "repeated dates and into the narrowest period"
        );

        for (query, expected) in [
            ("branch:nope", "There is no branch \"nope\"."),
            ("tag:v9", "No tag has \"v9\" in its name."),
            ("以降:きのう", "Cannot read the date in \"since:きのう\"."),
            ("merge:maybe", "Cannot read \"merge:maybe\"."),
            (
                "merge:yes merge:no",
                "The \"merge:\" conditions contradict each other.",
            ),
            ("sha:xyz1", "Cannot read \"sha:xyz1\"."),
            ("sha:abc", "Cannot read \"sha:abc\"."),
            (
                "branch:main branch:feature",
                "Only one \"branch:\" condition can be given.",
            ),
            (
                "file:a file:b",
                "Only one \"file:\" condition can be given.",
            ),
            (
                "change:a change:b",
                "Only one \"change:\" condition can be given.",
            ),
            ("file:./", "\"file:./\" names the whole repository"),
            ("ファイル:/", "\"file:/\" names the whole repository"),
            (
                "以降:2026-09-10 以前:2026-09-01",
                "The period is reversed (\"since\" is later than \"until\").",
            ),
        ] {
            let problems = problems(plan(query, Some(&tags), &all));
            assert!(
                problems.iter().any(|problem| problem.starts_with(expected)),
                "{query:?} gave {problems:?}"
            );
        }

        let file_history = LogSource::Path(crate::repository::RepoPath::new("a.txt").unwrap());
        assert!(!problems(plan("file:b", Some(&tags), &file_history)).is_empty());
        let branch_graph = LogSource::Branch("main".into());
        assert!(!problems(plan("branch:feature", Some(&tags), &branch_graph)).is_empty());
    }
}
