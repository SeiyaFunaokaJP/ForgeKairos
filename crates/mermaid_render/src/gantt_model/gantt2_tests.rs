//! Where the lines the form does not show end up, how a deleted task's
//! references are written down as dates, and which statements survive a round
//! trip byte for byte.

use super::gantt_dates::{TaskTime, task_times};
use super::*;
use crate::{MermaidTheme, render_to_svg};

fn parsed(source: &str) -> GanttDocument {
    parse_gantt(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
}

/// Parses, emits, parses again and emits again, and returns the first emit.
/// The two texts are compared directly, because comparing the documents alone
/// would miss a line that both of them lost.
fn round_trip(source: &str) -> String {
    let first = emit_gantt(&parsed(source));
    let second = emit_gantt(&parsed(&first));
    assert_eq!(first, second, "emitting twice changed the source");
    first
}

fn drawing_of(source: &str) -> String {
    let svg = render_to_svg(source, &MermaidTheme::default())
        .unwrap_or_else(|error| panic!("`{source}` should render: {error}"));
    assert!(
        !svg.contains("Syntax error in text"),
        "`{source}` rendered an error diagram"
    );
    let mut out = String::new();
    let mut rest = svg.as_str();
    while let Some(at) = rest.find("merman-") {
        out.push_str(rest.get(..at).unwrap_or_default());
        out.push_str("merman-x");
        rest = rest.get(at + "merman-".len()..).unwrap_or_default();
        rest = rest.trim_start_matches(|character: char| character.is_ascii_digit());
    }
    out.push_str(rest);
    out
}

fn times(source: &str) -> Vec<TaskTime> {
    task_times(source).unwrap_or_else(|error| panic!("`{source}` should resolve: {error}"))
}

/// Every line the form does not show comes back where it was written, and
/// the chart draws the same.
#[test]
fn a_preserved_line_comes_back_where_it_was_written() {
    for source in [
        "gantt\n    dateFormat YYYY-MM-DD\n    %% 上のメモ\n    section 設計\n    %% 見出しのメモ\n    A :a, 2026-01-05, 3d\n    %% A のメモ\n    todayMarker off\n    B :b, after a, 2d\n    section 実装\n    C :c, after b, 1d\n    %% 最後のメモ",
        // Without a `section` line, and with a multi-line block between tasks.
        "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-05, 3d\n    accDescr {\n        説明\n    }\n    B :b, after a, 2d",
    ] {
        assert_eq!(round_trip(source), source);
        assert_eq!(drawing_of(source), drawing_of(&round_trip(source)));
    }
}

/// A line belongs to the element right above it, so moving that element moves
/// the line with it.
#[test]
fn a_preserved_line_moves_with_the_element_above_it() {
    let mut document = parsed(
        "gantt\n    dateFormat YYYY-MM-DD\n    section 前半\n    %% 前半の見出し\n    A :a, 2026-01-05, 3d\n    %% A のメモ\n    B :b, 2026-01-06, 2d\n    section 後半\n    C :c, 2026-01-07, 1d",
    );
    if let Some(section) = document.sections.first_mut() {
        section.tasks.swap(0, 1);
    }
    assert_eq!(
        emit_gantt(&document),
        "gantt\n    dateFormat YYYY-MM-DD\n    section 前半\n    %% 前半の見出し\n    B :b, 2026-01-06, 2d\n    A :a, 2026-01-05, 3d\n    %% A のメモ\n    section 後半\n    C :c, 2026-01-07, 1d"
    );

    document.sections.swap(0, 1);
    assert_eq!(
        emit_gantt(&document),
        "gantt\n    dateFormat YYYY-MM-DD\n    section 後半\n    C :c, 2026-01-07, 1d\n    section 前半\n    %% 前半の見出し\n    B :b, 2026-01-06, 2d\n    A :a, 2026-01-05, 3d\n    %% A のメモ"
    );
}

/// Deleting an element leaves its lines where they were: below whatever now
/// sits above them.
#[test]
fn a_deleted_element_leaves_its_lines_behind() {
    let source = "gantt\n    dateFormat YYYY-MM-DD\n    %% 上\n    A :a, 2026-01-05, 3d\n    %% A の下\n    section S\n    %% S の見出し\n    B :b, 2026-01-06, 2d\n    %% B の下\n    C :c, 2026-01-07, 1d\n    %% C の下\n    section T\n    %% T の見出し\n    D :d, 2026-01-08, 1d";

    // A task after another one: its line joins that one's.
    let mut document = parsed(source);
    document.remove_task(1, 1).expect("removable");
    assert_eq!(
        emit_gantt(&document),
        "gantt\n    dateFormat YYYY-MM-DD\n    %% 上\n    A :a, 2026-01-05, 3d\n    %% A の下\n    section S\n    %% S の見出し\n    B :b, 2026-01-06, 2d\n    %% B の下\n    %% C の下\n    section T\n    %% T の見出し\n    D :d, 2026-01-08, 1d"
    );

    // The first task of a named section: its line stays under the header.
    let mut document = parsed(source);
    document.remove_task(1, 0).expect("removable");
    assert_eq!(
        emit_gantt(&document),
        "gantt\n    dateFormat YYYY-MM-DD\n    %% 上\n    A :a, 2026-01-05, 3d\n    %% A の下\n    section S\n    %% S の見出し\n    %% B の下\n    C :c, 2026-01-07, 1d\n    %% C の下\n    section T\n    %% T の見出し\n    D :d, 2026-01-08, 1d"
    );

    // The only task of the nameless band: its line goes to the top.
    let mut document = parsed(source);
    document.remove_task(0, 0).expect("removable");
    let emitted = emit_gantt(&document);
    assert_eq!(
        emitted,
        "gantt\n    dateFormat YYYY-MM-DD\n    %% 上\n    %% A の下\n    section S\n    %% S の見出し\n    B :b, 2026-01-06, 2d\n    %% B の下\n    C :c, 2026-01-07, 1d\n    %% C の下\n    section T\n    %% T の見出し\n    D :d, 2026-01-08, 1d"
    );
    // The band itself stays in the form, empty, and is not written.
    assert_eq!(round_trip(&emitted), emitted);

    // A whole section: its header's lines and its tasks' lines stay, below
    // the last task of the section above it.
    let mut document = parsed(source);
    document.remove_section(1).expect("removable");
    let emitted = emit_gantt(&document);
    assert_eq!(
        emitted,
        "gantt\n    dateFormat YYYY-MM-DD\n    %% 上\n    A :a, 2026-01-05, 3d\n    %% A の下\n    %% S の見出し\n    %% B の下\n    %% C の下\n    section T\n    %% T の見出し\n    D :d, 2026-01-08, 1d"
    );
    assert_eq!(parsed(&emitted), document);

    // The first section of a chart without a nameless band: to the top.
    let mut document = parsed(
        "gantt\n    section S\n    %% S の見出し\n    A :a, 2026-01-05, 3d\n    section T\n    B :b, 2026-01-06, 2d",
    );
    document.remove_section(0).expect("removable");
    let emitted = emit_gantt(&document);
    assert_eq!(
        emitted,
        "gantt\n    %% S の見出し\n    section T\n    B :b, 2026-01-06, 2d"
    );
    assert_eq!(parsed(&emitted), document);
}

/// Deleting a task another one starts `after` writes down the moment it ended,
/// whatever it ended with, and the drawing of the task left behind does not
/// move.
#[test]
fn a_deleted_reference_becomes_the_moment_it_resolved_to() {
    for (source, expected) in [
        // A length on a date that itself follows another task.
        (
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :b, after a, 1w\n    C :c, after b, 5d",
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    C :c, 2026-01-11, 5d",
        ),
        // A start that points forwards, at a task written below it.
        (
            "gantt\n    dateFormat YYYY-MM-DD\n    A :2026-01-01, 3d\n    B :b, after x, 2d\n    X :x, 2026-01-10, 1d\n    C :c, after b, 5d",
            "gantt\n    dateFormat YYYY-MM-DD\n    A :2026-01-01, 3d\n    X :x, 2026-01-10, 1d\n    C :c, 2026-01-13, 5d",
        ),
        // Hours in a format with a time.
        (
            "gantt\n    dateFormat YYYY-MM-DD HH:mm\n    A :a, 2026-01-06 09:00, 2h\n    B :b, after a, 90m\n    C :c, after b, 1h",
            "gantt\n    dateFormat YYYY-MM-DD HH:mm\n    A :a, 2026-01-06 09:00, 2h\n    C :c, 2026-01-06 12:30, 1h",
        ),
        // Day first.
        (
            "gantt\n    dateFormat DD-MM-YYYY\n    A :a, 30-01-2026, 3d\n    B :b, after a, 2d\n    C :c, after b, 1d",
            "gantt\n    dateFormat DD-MM-YYYY\n    A :a, 30-01-2026, 3d\n    C :c, 04-02-2026, 1d",
        ),
        // Milliseconds since the epoch.
        (
            "gantt\n    dateFormat x\n    A :a, 1767225600000, 1d\n    B :b, after a, 1d\n    C :c, after b, 1d",
            "gantt\n    dateFormat x\n    A :a, 1767225600000, 1d\n    C :c, 1767398400000, 1d",
        ),
    ] {
        let before = times(source);
        let mut document = parsed(source);
        document
            .remove_task(0, 1)
            .unwrap_or_else(|error| panic!("`{source}` should allow the deletion: {error}"))
            .expect("there is such a task");
        let emitted = emit_gantt(&document);
        assert_eq!(emitted, expected);
        assert_eq!(document.structure_error(), None);
        let after = times(&emitted);
        assert_eq!(
            after.last(),
            before.last(),
            "the task after the deleted one moved:\n{emitted}"
        );
        assert_eq!(round_trip(&emitted), emitted);
    }
}

/// A chart without a `dateFormat` reads a start through JavaScript's `Date`,
/// which a plain `YYYY-MM-DD` spells.
#[test]
fn a_chart_without_a_date_format_gets_a_plain_date() {
    let source = "gantt\n    A :a, 2026-01-01, 3d\n    B :b, after a, 2d\n    C :c, after b, 1d";
    let before = times(source);
    let mut document = parsed(source);
    document.remove_task(0, 1).expect("removable");
    let emitted = emit_gantt(&document);
    assert_eq!(
        emitted,
        "gantt\n    A :a, 2026-01-01, 3d\n    C :c, 2026-01-06, 1d"
    );
    assert_eq!(times(&emitted).last(), before.last());
}

/// The rest of an `after` list is kept when it still resolves to the same
/// moment, and replaced by a date when the deleted task was the one that ended
/// last.
#[test]
fn the_rest_of_a_reference_list_is_kept_only_while_it_means_the_same() {
    let source = "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :b, 2026-01-01, 1d\n    C :c, after a b, 5d";

    let mut document = parsed(source);
    document.remove_task(0, 1).expect("removable");
    assert_eq!(
        emit_gantt(&document),
        "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    C :c, after a, 5d"
    );

    let mut document = parsed(source);
    document.remove_task(0, 0).expect("removable");
    assert_eq!(
        emit_gantt(&document),
        "gantt\n    dateFormat YYYY-MM-DD\n    B :b, 2026-01-01, 1d\n    C :c, 2026-01-04, 5d"
    );
}

/// `until` becomes the moment the deleted task started, spelled as a date that
/// `inclusiveEndDates` and `excludes` read back to the same end.
#[test]
fn a_deleted_until_becomes_the_end_it_resolved_to() {
    for (source, expected) in [
        (
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    B :b, after a, 2d\n    C :c, 2026-01-02, until b",
            "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-01, 3d\n    C :c, 2026-01-02, 2026-01-04",
        ),
        // Mermaid adds a day to an end date here, so the date is a day earlier.
        (
            "gantt\n    dateFormat YYYY-MM-DD\n    inclusiveEndDates\n    A :a, 2026-01-01, 3d\n    B :b, after a, 2d\n    C :c, 2026-01-02, until b",
            "gantt\n    dateFormat YYYY-MM-DD\n    inclusiveEndDates\n    A :a, 2026-01-01, 3d\n    C :c, 2026-01-02, 2026-01-03",
        ),
        // `excludes` pushed the end past the weekend; a plain date is read as
        // exactly the end it names.
        (
            "gantt\n    dateFormat YYYY-MM-DD\n    excludes weekends\n    A :a, 2026-01-05, 5d\n    B :b, after a, 2d\n    C :c, 2026-01-08, until b",
            "gantt\n    dateFormat YYYY-MM-DD\n    excludes weekends\n    A :a, 2026-01-05, 5d\n    C :c, 2026-01-08, 2026-01-14",
        ),
    ] {
        let before = times(source);
        let mut document = parsed(source);
        document
            .remove_task(0, 1)
            .unwrap_or_else(|error| panic!("`{source}` should allow the deletion: {error}"));
        let emitted = emit_gantt(&document);
        assert_eq!(times(&emitted).last(), before.last(), "{emitted}");
        assert_eq!(emitted, expected);
    }
}

/// An `until` that `excludes` pushed past a weekend is drawn to the day before
/// the weekend, while the same end written as a date is drawn to the end
/// itself. The moments agree, the bars do not, so the deletion is refused.
#[test]
fn a_deleted_until_whose_bar_would_grow_is_refused() {
    let source = "gantt
    dateFormat YYYY-MM-DD
    excludes weekends
    X :x, 2026-01-10, 1d
    C :c, 2026-01-06, until x";
    let before = times(source);
    let drawn = before.last().copied().expect("C");
    assert_ne!(
        drawn.drawn_end, drawn.end,
        "the example has to end on skipped days to test anything"
    );
    let mut document = parsed(source);
    let error = document
        .remove_task(0, 0)
        .expect_err("a date end would draw C longer");
    assert!(format!("{error}").contains("`C`"), "{error}");
    assert_eq!(emit_gantt(&document), source);
}

/// A `click` naming the id `merman` gives a task without one would follow
/// whichever task ends up in that place, so such a chart is not opened.
#[test]
fn a_click_on_an_automatic_id_is_refused() {
    let source = "gantt
    dateFormat YYYY-MM-DD
    A :2026-01-01, 1d
    B :2026-01-03, 1d
    C :2026-01-05, 1d
    click task2 href \"https://example.com\"";
    let error = parse_gantt(source).expect_err("the link would move with a deletion");
    assert!(format!("{error}").contains("task2"), "{error}");

    // An explicit id spelled the same way is an ordinary id.
    let source = "gantt
    dateFormat YYYY-MM-DD
    A :task2, 2026-01-01, 1d
    click task2 href \"https://example.com\"";
    assert_eq!(round_trip(source), source);
}

/// When the chart's `dateFormat` cannot spell the moment, nothing is removed
/// and the reason says which task would move.
#[test]
fn a_deletion_that_would_move_a_task_is_refused() {
    let source = "gantt\n    dateFormat MMM D YYYY\n    A :a, Jan 1 2026, 3d\n    B :b, after a, 2d\n    C :c, after b, 1d";
    let mut document = parsed(source);
    let error = document
        .remove_task(0, 1)
        .expect_err("a start in this format cannot be written");
    let message = format!("{error}");
    assert!(message.contains("`C`"), "{message}");
    assert!(message.contains("`B`"), "{message}");
    assert_eq!(
        emit_gantt(&document),
        source,
        "a refused deletion changes nothing"
    );

    // A section whose task is pointed at from outside it goes the same way.
    let sectioned = "gantt\n    dateFormat MMM D YYYY\n    A :a, Jan 1 2026, 3d\n    section 後半\n    B :b, after a, 2d\n    section 最後\n    C :c, after b, 1d";
    let mut document = parsed(sectioned);
    let error = document
        .remove_section(1)
        .expect_err("`C` would have no date to start on");
    assert!(format!("{error}").contains("セクション `後半`"), "{error}");
    assert_eq!(emit_gantt(&document), sectioned);

    let mut document = parsed(source);
    // Something nothing points at goes without asking `merman` anything.
    document
        .remove_task(0, 2)
        .expect("nothing points at C")
        .expect("there is such a task");
}

/// A `click` is held where it was written, follows a rename of the task it
/// names, and may not end up above that task.
#[test]
fn a_click_is_preserved_and_follows_its_task() {
    let source = "gantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-05, 3d\n    click a href \"https://example.com\"\n    B :b, after a, 2d\n    click a,b call callback()";
    assert_eq!(round_trip(source), source);
    assert_eq!(drawing_of(source), drawing_of(&round_trip(source)));

    let mut document = parsed(source);
    document.rename_references(&[("a".to_string(), "req".to_string())]);
    if let Some(task) = document
        .sections
        .first_mut()
        .and_then(|section| section.tasks.first_mut())
    {
        task.id = Some("req".to_string());
    }
    assert_eq!(
        emit_gantt(&document),
        "gantt\n    dateFormat YYYY-MM-DD\n    A :req, 2026-01-05, 3d\n    click req href \"https://example.com\"\n    B :b, after req, 2d\n    click req,b call callback()"
    );
    assert_eq!(document.structure_error(), None);

    // Moving `A` below the line that gives it its link would take the link
    // away, so the form stops there.
    let mut document = parsed(source);
    if let Some(section) = document.sections.first_mut() {
        section.tasks.swap(0, 1);
    }
    let error = document.structure_error().expect("a click above its task");
    assert!(error.contains("click"), "{error}");

    // Deleting the task leaves the line, which then names nothing.
    let mut document = parsed(source);
    document.remove_task(0, 1).expect("removable");
    assert!(emit_gantt(&document).contains("click a,b call callback()"));
    assert_eq!(document.structure_error(), None);
}

/// A name that merely opens with a keyword is a task, the way `merman` reads
/// the line.
#[test]
fn a_task_may_open_with_a_keyword_that_mermaid_does_not_read_there() {
    let source = "gantt\n    dateFormat YYYY-MM-DD\n    sectional review :a, 2026-01-05, 3d\n    accTitle x :b, 2026-01-05, 1d\n    displayMode compact :c, 2026-01-05, 1d\n    weekdays :d, 2026-01-05, 1d";
    assert_eq!(round_trip(source), source);
    let document = parsed(source);
    assert_eq!(
        document
            .tasks()
            .map(|task| task.label.as_str())
            .collect::<Vec<_>>(),
        vec![
            "sectional review",
            "accTitle x",
            "displayMode compact",
            "weekdays"
        ]
    );
    let drawing = drawing_of(source);
    for label in ["sectional review", "displayMode compact", "weekdays"] {
        assert!(
            drawing.contains(label),
            "`{label}` should be drawn as a task"
        );
    }
}

/// `displayMode` lives in the configuration, which is held verbatim; written
/// in the body it is explained rather than opened.
#[test]
fn display_mode_is_kept_in_the_configuration() {
    for source in [
        "---\ndisplayMode: compact\n---\ngantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-05, 3d\n    B :b, 2026-01-06, 2d",
        "%%{init: {\"gantt\": {\"displayMode\": \"compact\"}}}%%\ngantt\n    dateFormat YYYY-MM-DD\n    A :a, 2026-01-05, 3d",
    ] {
        assert_eq!(round_trip(source), source);
        drawing_of(source);
    }
    let error = parse_gantt("gantt\n    displayMode compact\n    A :a, 2026-01-05, 3d")
        .expect_err("displayMode is not a statement");
    assert!(format!("{error}").contains("フロントマター"), "{error}");
}

/// Every statement and tag a gantt chart can carry comes back byte for byte
/// and draws the same, so none of them is lost or changes its meaning.
#[test]
fn every_gantt_construct_survives_a_round_trip() {
    for source in [
        "gantt\n    dateFormat YYYY-MM-DD\n    A :done, a, 2026-01-05, 3d\n    B :active, b, after a, 2d\n    C :crit, milestone, c, after b, 0d\n    D :vert, d, 2026-01-09, 0d\n    E :crit, active, e, after c, 1d",
        "gantt\n    dateFormat YYYY-MM-DD\n    axisFormat %d\n    excludes weekends, 2026-01-07\n    includes 2026-01-10\n    weekday monday\n    weekend friday\n    tickInterval 1day\n    todayMarker stroke-width:5px,stroke:#0f0\n    inclusiveEndDates\n    topAxis\n    A :a, 2026-01-05, 3d\n    B :b, after a, 2d",
        "gantt\n    todayMarker off\n    A :a, 2026-01-05, 3d",
        "%%{init: {\"theme\": \"forest\"}}%%\ngantt\n    title 図\n    dateFormat YYYY-MM-DD\n    accTitle: 概要\n    accDescr: 説明\n    A :a, 2026-01-05, 3d\n    click a href \"https://example.com\"",
        "gantt\n    dateFormat YYYY-MM-DD\n    section 設計\n    A :a, 2026-01-05, 3d\n    B :b, 2026-01-02, until a\n    C :after a b, 1d\n    D :2d",
    ] {
        let emitted = round_trip(source);
        assert_eq!(emitted, source);
        assert_eq!(drawing_of(source), drawing_of(&emitted));
    }
}
