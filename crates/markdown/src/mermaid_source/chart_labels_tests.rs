use super::*;
use crate::mermaid_source::encode_label;
use mermaid_render::{MermaidTheme, render_to_svg, render_to_svg_with_geometry};

fn lookup(source: &str, item: LabelItem, rendered: &str) -> MermaidLabelLookup {
    find_item_label(source, &item, rendered)
}

/// Writes `new_label` over the span the way the preview does, and checks that
/// merman still draws the result.
fn apply(source: &str, span: &MermaidLabelSpan, new_label: &str) -> String {
    let replaced = source
        .get(span.range.clone())
        .expect("a span addresses its own source");
    let written = encode_label(new_label, span.quoting, replaced);
    let mut edited = source.to_string();
    edited.replace_range(span.range.clone(), &written);
    render_to_svg(&edited, &MermaidTheme::default())
        .unwrap_or_else(|error| panic!("{edited:?} should still render: {error:#}"));
    edited
}

fn token<'a>(source: &'a str, span: &MermaidLabelSpan) -> &'a str {
    &source[span.range.clone()]
}

#[test]
fn a_pie_slice_is_its_quoted_label() {
    let source = "pie title \u{30da}\u{30c3}\u{30c8}\n  \"\u{72ac}\" : 386\n  '\u{732b}' : 85\n";
    let dog = lookup(source, LabelItem::PieSlice { index: 0 }, "\u{72ac}").expect("found");
    assert_eq!(token(source, &dog), "\"\u{72ac}\"");
    assert_eq!(dog.label, "\u{72ac}");
    assert_eq!(dog.quoting, MermaidLabelQuoting::PieLabel);
    assert_eq!(
        apply(source, &dog, "\u{30a4}\u{30cc} \"\u{5c0f}\" \\"),
        "pie title \u{30da}\u{30c3}\u{30c8}\n  \"\u{30a4}\u{30cc} \\\"\u{5c0f}\\\" \\\\\" : 386\n  '\u{732b}' : 85\n"
    );

    // A label quoted with `'` is rewritten with `"`, since the span covers
    // the quotes as well.
    let cat = lookup(source, LabelItem::PieSlice { index: 1 }, "\u{732b}").expect("found");
    assert_eq!(token(source, &cat), "'\u{732b}'");
    assert_eq!(
        apply(source, &cat, "\u{30cd}\u{30b3}"),
        "pie title \u{30da}\u{30c3}\u{30c8}\n  \"\u{72ac}\" : 386\n  \"\u{30cd}\u{30b3}\" : 85\n"
    );
}

/// merman draws only the first of several slices with the same label, and the
/// legend counts the ones it draws.
#[test]
fn pie_slices_are_counted_the_way_merman_keeps_them() {
    let source = "pie\n  \"a\" : 1\n  \"b\" : 2\n  \"a\" : 3\n  \"c\" : 4\n";
    let c = lookup(source, LabelItem::PieSlice { index: 2 }, "c").expect("found");
    assert_eq!(token(source, &c), "\"c\"");
    assert_eq!(
        lookup(source, LabelItem::PieSlice { index: 3 }, "c"),
        Err(MermaidLabelRefusal::NotLocated)
    );
    // Renaming the first `a` would bring the hidden one on screen.
    assert_eq!(
        lookup(source, LabelItem::PieSlice { index: 0 }, "a"),
        Err(MermaidLabelRefusal::Duplicated)
    );
    let b = lookup(source, LabelItem::PieSlice { index: 1 }, "b").expect("found");
    assert_eq!(token(source, &b), "\"b\"");
    // A rendered text that does not belong to the slice counted means the
    // count drifted, and nothing may be written.
    assert_eq!(
        lookup(source, LabelItem::PieSlice { index: 1 }, "c"),
        Err(MermaidLabelRefusal::NotLocated)
    );
}

#[test]
fn a_pie_label_keeps_its_value_and_comments() {
    // merman cuts every pie line at `%%`, so a trailing comment is not part of
    // the value, and `;` separates nothing: the rest of the line is ignored.
    let source = "pie showData\n  %% \u{30b3}\u{30e1}\u{30f3}\u{30c8}\n  \"\u{72ac}\" : 386 %% \u{591a}\u{3044}\n  \"a\" : 1; \"b\" : 2\n";
    let dog = lookup(source, LabelItem::PieSlice { index: 0 }, "\u{72ac} [386]").expect("found");
    assert_eq!(
        apply(source, &dog, "\u{30a4}\u{30cc}"),
        "pie showData\n  %% \u{30b3}\u{30e1}\u{30f3}\u{30c8}\n  \"\u{30a4}\u{30cc}\" : 386 %% \u{591a}\u{3044}\n  \"a\" : 1; \"b\" : 2\n"
    );
    let a = lookup(source, LabelItem::PieSlice { index: 1 }, "a [1]").expect("found");
    assert_eq!(token(source, &a), "\"a\"");
    assert_eq!(
        lookup(source, LabelItem::PieSlice { index: 2 }, "b"),
        Err(MermaidLabelRefusal::NotLocated),
        "the `\"b\"` after `;` is not a slice"
    );
}

#[test]
fn a_pie_title_is_the_last_one_written() {
    let header = "pie showData title \u{30da}\u{30c3}\u{30c8} %% \u{6ce8}\n  \"a\" : 1\n";
    let title = lookup(header, LabelItem::Title, "\u{30da}\u{30c3}\u{30c8}").expect("found");
    assert_eq!(token(header, &title), "\u{30da}\u{30c3}\u{30c8}");
    assert_eq!(
        apply(header, &title, "\u{52d5}\u{7269}"),
        "pie showData title \u{52d5}\u{7269} %% \u{6ce8}\n  \"a\" : 1\n"
    );

    let body = "pie title first\n  title  second  \n  \"a\" : 1\n";
    let title = lookup(body, LabelItem::Title, "second").expect("found");
    assert_eq!(token(body, &title), "second");
    assert_eq!(
        apply(body, &title, "\u{4e8c}\u{756a}\u{76ee}"),
        "pie title first\n  title  \u{4e8c}\u{756a}\u{76ee}  \n  \"a\" : 1\n"
    );

    assert_eq!(
        lookup("pie\n  \"a\" : 1\n", LabelItem::Title, ""),
        Err(MermaidLabelRefusal::NotLocated)
    );
}

#[test]
fn a_pie_label_that_merman_would_merge_or_cut_is_refused() {
    let reserved = reserved_labels(
        "pie\n  \"a\" : 1\n  \"b\" : 2\n",
        &LabelItem::PieSlice { index: 0 },
    );
    assert_eq!(reserved, vec!["b".to_string()]);
    assert!(write_refusal(MermaidLabelQuoting::PieLabel, "b", &reserved).is_some());
    assert!(write_refusal(MermaidLabelQuoting::PieLabel, "a", &reserved).is_none());
    assert!(write_refusal(MermaidLabelQuoting::PieLabel, "50%%", &reserved).is_some());
    assert!(write_refusal(MermaidLabelQuoting::PieLabel, "  ", &reserved).is_some());
    assert!(write_refusal(MermaidLabelQuoting::PieTitle, "x %% y", &[]).is_some());
    assert!(write_refusal(MermaidLabelQuoting::PieTitle, "\u{52d5}\u{7269}", &[]).is_none());
}

const GANTT: &str = concat!(
    "gantt\n",
    "  title \u{8a08}\u{753b}\n",
    "  dateFormat YYYY-MM-DD\n",
    "  %% \u{5168}\u{884c}\u{30b3}\u{30e1}\u{30f3}\u{30c8}\n",
    "  section \u{8a2d}\u{8a08}<br>\u{5de5}\u{7a0b}\n",
    "  \u{8981}\u{4ef6}\u{5b9a}\u{7fa9} :a1, 2024-01-01, 10d\n",
    "  \u{9032}\u{6357} 50%% \u{5b8c}\u{4e86} :after a1, 3d\n",
    "  section \u{5b9f}\u{88c5}\n",
    "  \u{4f5c}\u{308b} :done, 2024-01-20, 5d; \u{7121}\u{8996}\n",
    "  \u{30ea}\u{30ea}\u{30fc}\u{30b9} :milestone, m1, 2024-01-25, 0d\n",
);

#[test]
fn gantt_tasks_are_found_by_the_id_merman_gives_them() {
    let first = lookup(
        GANTT,
        LabelItem::GanttTask {
            task_id: "a1".into(),
        },
        "\u{8981}\u{4ef6}\u{5b9a}\u{7fa9} ",
    )
    .expect("found");
    assert_eq!(token(GANTT, &first), "\u{8981}\u{4ef6}\u{5b9a}\u{7fa9}");
    assert_eq!(first.quoting, MermaidLabelQuoting::GanttTask);

    // A `%%` inside a task line belongs to the task; only a line that opens
    // with one is a comment.
    let second = lookup(
        GANTT,
        LabelItem::GanttTask {
            task_id: "task1".into(),
        },
        "\u{9032}\u{6357} 50%% \u{5b8c}\u{4e86}",
    )
    .expect("found");
    assert_eq!(
        token(GANTT, &second),
        "\u{9032}\u{6357} 50%% \u{5b8c}\u{4e86}"
    );

    let third = lookup(
        GANTT,
        LabelItem::GanttTask {
            task_id: "task2".into(),
        },
        "\u{4f5c}\u{308b}",
    )
    .expect("found");
    let edited = apply(GANTT, &third, "\u{4f5c}\u{308a}\u{76f4}\u{3059}");
    assert_eq!(
        edited,
        GANTT.replace(
            "  \u{4f5c}\u{308b} :done",
            "  \u{4f5c}\u{308a}\u{76f4}\u{3059} :done"
        )
    );

    let milestone = lookup(
        GANTT,
        LabelItem::GanttTask {
            task_id: "m1".into(),
        },
        "\u{30ea}\u{30ea}\u{30fc}\u{30b9}",
    )
    .expect("found");
    assert_eq!(token(GANTT, &milestone), "\u{30ea}\u{30ea}\u{30fc}\u{30b9}");

    assert_eq!(
        lookup(
            GANTT,
            LabelItem::GanttTask {
                task_id: "task3".into()
            },
            ""
        ),
        Err(MermaidLabelRefusal::NotLocated)
    );
    // The rendered name has to be the one the source gives the task.
    assert_eq!(
        lookup(
            GANTT,
            LabelItem::GanttTask {
                task_id: "a1".into()
            },
            "\u{4f5c}\u{308b}"
        ),
        Err(MermaidLabelRefusal::NotLocated)
    );
}

#[test]
fn gantt_sections_and_the_title_are_found_by_what_is_drawn() {
    let section = lookup(
        GANTT,
        LabelItem::GanttSection {
            name: "\u{8a2d}\u{8a08}\n\u{5de5}\u{7a0b}".into(),
        },
        "\u{8a2d}\u{8a08}\n\u{5de5}\u{7a0b}",
    )
    .expect("found");
    assert_eq!(
        token(GANTT, &section),
        "\u{8a2d}\u{8a08}<br>\u{5de5}\u{7a0b}"
    );
    assert_eq!(section.label, "\u{8a2d}\u{8a08}<br>\u{5de5}\u{7a0b}");
    assert_eq!(
        apply(GANTT, &section, "\u{8a2d}\u{8a08}"),
        GANTT.replace(
            "section \u{8a2d}\u{8a08}<br>\u{5de5}\u{7a0b}",
            "section \u{8a2d}\u{8a08}"
        )
    );

    let title = lookup(GANTT, LabelItem::Title, "\u{8a08}\u{753b}").expect("found");
    assert_eq!(token(GANTT, &title), "\u{8a08}\u{753b}");
    assert_eq!(
        apply(GANTT, &title, "\u{65b0}\u{8a08}\u{753b}"),
        GANTT.replace("title \u{8a08}\u{753b}", "title \u{65b0}\u{8a08}\u{753b}")
    );

    let reserved = reserved_labels(
        GANTT,
        &LabelItem::GanttSection {
            name: "\u{5b9f}\u{88c5}".into(),
        },
    );
    assert_eq!(
        reserved,
        vec!["\u{8a2d}\u{8a08}<br>\u{5de5}\u{7a0b}".to_string()]
    );
    assert!(
        write_refusal(
            MermaidLabelQuoting::GanttSection,
            "\u{8a2d}\u{8a08}<br>\u{5de5}\u{7a0b}",
            &reserved
        )
        .is_some()
    );
    assert!(
        write_refusal(
            MermaidLabelQuoting::GanttSection,
            "\u{8a66}\u{9a13}",
            &reserved
        )
        .is_none()
    );
}

#[test]
fn gantt_labels_that_would_change_meaning_are_refused() {
    let duplicated_sections =
        "gantt\n  section A\n  x :2024-01-01, 1d\n  section A\n  y :2024-01-02, 1d\n";
    assert_eq!(
        lookup(
            duplicated_sections,
            LabelItem::GanttSection { name: "A".into() },
            "A"
        ),
        Err(MermaidLabelRefusal::Duplicated)
    );

    // An id written by hand may collide with the one merman numbers another
    // task with.
    let colliding_ids = "gantt\n  x :2024-01-01, 1d\n  y :task1, 2024-01-02, 1d\n";
    assert_eq!(
        lookup(
            colliding_ids,
            LabelItem::GanttTask {
                task_id: "task1".into()
            },
            "x"
        ),
        Err(MermaidLabelRefusal::Duplicated)
    );

    for label in ["title x", "section y", "a:b", "click z", "%% c", ""] {
        assert!(
            write_refusal(MermaidLabelQuoting::GanttTask, label, &[]).is_some(),
            "{label:?} would not stay a task name"
        );
    }
    assert!(
        write_refusal(
            MermaidLabelQuoting::GanttTask,
            "\u{8a2d}\u{8a08} #1; \u{7d9a}\u{304d}",
            &[]
        )
        .is_none()
    );
    assert!(write_refusal(MermaidLabelQuoting::GanttTitle, " ", &[]).is_some());
}

const ER: &str = concat!(
    "erDiagram\n",
    "  \u{9867}\u{5ba2}[\"\u{304a}\u{5ba2}\u{69d8}\"] {\n",
    "    string \u{540d}\u{524d} PK \"\u{6c0f}\u{540d}\"\n",
    "    %% \u{30b3}\u{30e1}\u{30f3}\u{30c8}\n",
    "    int age\n",
    "  }\n",
    "  \u{9867}\u{5ba2} ||--o{ \u{6ce8}\u{6587} : places\n",
    "  \u{6ce8}\u{6587} {\n",
    "    int id PK\n",
    "  }\n",
    "  \u{9867}\u{5ba2} {\n",
    "    string email UK\n",
    "  }\n",
);

#[test]
fn an_er_attribute_is_its_name_in_the_entities_blocks() {
    let item = |entity: &str, index| LabelItem::ErAttribute {
        entity: entity.into(),
        index,
    };
    let name = lookup(ER, item("\u{9867}\u{5ba2}", 0), "\u{540d}\u{524d}").expect("found");
    assert_eq!(token(ER, &name), "\u{540d}\u{524d}");
    assert_eq!(name.quoting, MermaidLabelQuoting::ErAttributeName);
    assert_eq!(
        apply(ER, &name, "\u{6c0f}\u{540d}"),
        ER.replace("string \u{540d}\u{524d} PK", "string \u{6c0f}\u{540d} PK")
    );

    let age = lookup(ER, item("\u{9867}\u{5ba2}", 1), "age").expect("found");
    assert_eq!(token(ER, &age), "age");
    // The second block of the same entity continues its count.
    let email = lookup(ER, item("\u{9867}\u{5ba2}", 2), "email").expect("found");
    assert_eq!(token(ER, &email), "email");
    let id = lookup(ER, item("\u{6ce8}\u{6587}", 0), "id").expect("found");
    assert_eq!(token(ER, &id), "id");

    assert_eq!(
        lookup(ER, item("\u{9867}\u{5ba2}", 3), ""),
        Err(MermaidLabelRefusal::NotLocated)
    );
    assert_eq!(
        lookup(ER, item("\u{9867}\u{5ba2}", 1), "\u{540d}\u{524d}"),
        Err(MermaidLabelRefusal::NotLocated),
        "a rendered name that is not the one counted means the count drifted"
    );

    for word in ["PK", "two words", "a,b", ""] {
        assert!(
            write_refusal(MermaidLabelQuoting::ErAttributeName, word, &[]).is_some(),
            "{word:?} is not an attribute name"
        );
    }
}

#[test]
fn an_er_block_this_reader_cannot_count_is_refused() {
    let item = |index| LabelItem::ErAttribute {
        entity: "A".into(),
        index,
    };
    // A block that opens on the line another one closed on.
    let reopened = "erDiagram\n  A { string x } A {\n    int y\n  }\n";
    assert_eq!(
        lookup(reopened, item(0), "x"),
        Err(MermaidLabelRefusal::NotLocated)
    );
    // A key where a type or a name should be.
    let key_as_name = "erDiagram\n  A {\n    string PK\n    int y\n  }\n";
    assert_eq!(
        lookup(key_as_name, item(0), "y"),
        Err(MermaidLabelRefusal::NotLocated)
    );
    // Another entity's one-line block does not stand in the way.
    let other = "erDiagram\n  B { string x }\n  A {\n    int y\n  }\n";
    let y = lookup(other, item(0), "y").expect("found");
    assert_eq!(token(other, &y), "y");
}

/// merman reads a block as a stream of words, not line by line. Counting by
/// lines put a click on one attribute onto another one of the same name: the
/// block of `OTHER` closed on its attribute's line and swallowed the first
/// `CUSTOMER` block, and a line holding two attributes counted as one.
#[test]
fn er_attributes_are_counted_the_way_merman_reads_the_block() {
    let closed_on_attribute_line = concat!(
        "erDiagram\n",
        "  OTHER {\n",
        "    string a }\n",
        "  CUSTOMER {\n",
        "    string name\n",
        "  }\n",
        "  CUSTOMER {\n",
        "    string name\n",
        "    string name\n",
        "  }\n",
    );
    let two_on_a_line = "erDiagram\n  CUSTOMER {\n    string x int x\n    string x\n  }\n";
    let one_line_and_decorated = concat!(
        "erDiagram\n",
        "  CUSTOMER { string x int y }\n",
        "  CUSTOMER[\"\u{9867}] \u{5ba2}\"] {\n",
        "    string z \"c } d\"\n",
        "    int w PK, FK\n",
        "  }\n",
        "  CUSTOMER:::hot {\n",
        "    int v\n",
        "  }\n",
    );
    // Where each attribute's name is, as (line, byte column).
    for (source, expected) in [
        (closed_on_attribute_line, vec![(4, 11), (7, 11), (8, 11)]),
        (two_on_a_line, vec![(2, 11), (2, 17), (3, 11)]),
        (
            one_line_and_decorated,
            vec![(1, 20), (1, 26), (3, 11), (4, 8), (7, 8)],
        ),
    ] {
        let (_, geometry) = render_to_svg_with_geometry(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("{source:?} should render: {error:#}"));
        for (index, (line, column)) in expected.into_iter().enumerate() {
            let item = LabelItem::ErAttribute {
                entity: "CUSTOMER".into(),
                index,
            };
            let rendered = &geometry
                .labels
                .iter()
                .find(|label| label.item == item)
                .unwrap_or_else(|| panic!("{item:?} should be mapped in {source:?}"))
                .label;
            let span = lookup(source, item.clone(), rendered)
                .unwrap_or_else(|refusal| panic!("{item:?} in {source:?}: {refusal:?}"));
            let before = &source[..span.range.start];
            let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
            assert_eq!(
                (before.matches('\n').count(), span.range.start - line_start),
                (line, column),
                "{item:?} in {source:?}"
            );
            assert_eq!(token(source, &span), rendered.as_str());
        }
    }
}

/// The SVG spells `&`, `<` and `"` as entity references, which the geometry
/// has to read back, or a label holding one never matches its source.
#[test]
fn labels_holding_xml_special_characters_resolve() {
    let pie = "pie\n  \"R&D <\u{958b}\u{767a}>\" : 3\n  \"\\\"q\\\"\" : 2\n";
    let gantt = "gantt\n  dateFormat YYYY-MM-DD\n  section S\n  R&D <\u{8a2d}\u{8a08}> \"x\" :2024-01-01, 3d\n";
    for (source, item, expected) in [
        (
            pie,
            LabelItem::PieSlice { index: 0 },
            "\"R&D <\u{958b}\u{767a}>\"",
        ),
        (pie, LabelItem::PieSlice { index: 1 }, "\"\\\"q\\\"\""),
        (
            gantt,
            LabelItem::GanttTask {
                task_id: "task1".into(),
            },
            "R&D <\u{8a2d}\u{8a08}> \"x\"",
        ),
    ] {
        let (_, geometry) = render_to_svg_with_geometry(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("{source:?} should render: {error:#}"));
        let rendered = &geometry
            .labels
            .iter()
            .find(|label| label.item == item)
            .unwrap_or_else(|| panic!("{item:?} should be mapped in {:?}", geometry.labels))
            .label;
        let span = lookup(source, item.clone(), rendered)
            .unwrap_or_else(|refusal| panic!("{item:?} drawn as {rendered:?}: {refusal:?}"));
        assert_eq!(token(source, &span), expected);
    }
}

/// Every label the renderer maps has to lead back to the token it is drawn
/// with, which is what ties the two halves of this feature together.
#[test]
fn every_mapped_label_resolves_to_its_token() {
    for source in [
        "pie showData title \u{30da}\u{30c3}\u{30c8}\n  \"\u{72ac}\" : 386\n  \"\u{732b}\" : 85\n  \"\u{72ac}\" : 3\n  \"\u{9ce5}\" : 15\n",
        GANTT,
        ER,
    ] {
        let (_, geometry) = render_to_svg_with_geometry(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("{source:?} should render: {error:#}"));
        assert!(!geometry.labels.is_empty(), "{source:?} maps no labels");
        for label in &geometry.labels {
            match lookup(source, label.item.clone(), &label.label) {
                Ok(span) => {
                    let expected = match &label.item {
                        LabelItem::PieSlice { .. } => format!("\"{}\"", span.label),
                        _ => span.label.clone(),
                    };
                    assert_eq!(token(source, &span), expected, "{label:?}");
                    match &label.item {
                        LabelItem::GanttSection { name } => {
                            assert_eq!(gantt_drawn_name(&span.label), *name)
                        }
                        LabelItem::PieSlice { .. } => {
                            assert!(label.label.starts_with(&span.label), "{label:?}")
                        }
                        _ => assert_eq!(span.label, label.label.trim(), "{label:?}"),
                    }
                }
                // The first of two slices labelled alike.
                Err(MermaidLabelRefusal::Duplicated) => {
                    assert_eq!(label.item, LabelItem::PieSlice { index: 0 })
                }
                Err(refusal) => panic!("{label:?} in {source:?} was refused: {refusal:?}"),
            }
        }
    }
}
