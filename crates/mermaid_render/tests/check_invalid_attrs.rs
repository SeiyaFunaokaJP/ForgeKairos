use gpui::Hsla;
use mermaid_render::MermaidTheme;

fn rgb(r: u8, g: u8, b: u8) -> Hsla {
    gpui::Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

const DIAGRAMS: &[(&str, &str)] = &[
    (
        "flowchart",
        "flowchart TD\n    A[Hello] --> B[World]\n    B --> C{Decision}\n    C -->|Yes| D[OK]\n    C -->|No| E[Fail]",
    ),
    (
        "sequence",
        "sequenceDiagram\n    Alice->>Bob: Hello\n    Bob-->>Alice: Hi\n    Note over Alice,Bob: A note",
    ),
    (
        "state",
        "stateDiagram-v2\n    [*] --> Active\n    Active --> [*]",
    ),
    (
        "er",
        "erDiagram\n    A { int id PK }\n    B { int id PK }\n    A ||--o{ B : has",
    ),
    (
        "class",
        "classDiagram\n    class Foo {\n        +bar() void\n    }",
    ),
    ("pie", "pie title Test\n    \"A\" : 42\n    \"B\" : 58"),
    (
        "gantt",
        "gantt\n    title Test\n    dateFormat YYYY-MM-DD\n    section S\n        Task :a1, 2025-01-01, 7d",
    ),
    ("mindmap", "mindmap\n  root((Root))\n    Child1\n    Child2"),
    (
        "journey",
        "journey\n    title Test\n    section S\n        Task: 5: Actor",
    ),
    (
        "gitgraph",
        "gitGraph\n    commit id: \"init\"\n    branch dev\n    commit id: \"feat\"\n    checkout main\n    merge dev",
    ),
    (
        "quadrant",
        "quadrantChart\n    title Test\n    x-axis Low --> High\n    y-axis Low --> High\n    A: [0.3, 0.8]\n    B: [0.7, 0.4]",
    ),
    (
        "timeline",
        "timeline\n    title Test\n    section 2020s\n        2020 : Event A\n        2022 : Event B",
    ),
    (
        "xychart",
        "xychart-beta\n    title Test\n    x-axis [\"A\", \"B\", \"C\"]\n    y-axis \"Val\" 0 --> 10\n    bar [3, 7, 5]",
    ),
];

/// The kinds the preview used to refuse for want of readable colors. Most are
/// the insert menu's templates (`MermaidDiagramKind::template` in the editor
/// crate), since those are what a user sees first.
const NEWER_DIAGRAMS: &[(&str, &str)] = &[
    (
        "radar",
        r#"radar-beta
    %% axis で軸、curve で系列。値は軸の順に並べる
    title スキルの比較
    axis design["設計"], build["実装"], test["テスト"], ops["運用"]
    curve a["Aさん"]{4, 3, 5, 2}
    curve b["Bさん"]{2, 5, 3, 4}
    max 5"#,
    ),
    (
        "requirement",
        r#"requirementDiagram
    %% requirement は要求、element は満たす側。- satisfies -> で結ぶ
    requirement login {
        id: 1
        text: "ログインできること"
        risk: high
        verifymethod: test
    }
    element login_page {
        type: "画面"
    }
    login_page - satisfies -> login"#,
    ),
    (
        "c4-context",
        r#"C4Context
    %% Person は利用者、System は自システム、System_Ext は外部、Rel は関係
    title 受注システムの全体像
    Person(customer, "顧客", "商品を注文する")
    System(shop, "受注システム", "注文を受け付ける")
    System_Ext(mail, "メール配信", "通知を送る")
    Rel(customer, shop, "注文する")
    Rel(shop, mail, "通知を依頼する")"#,
    ),
    (
        "c4-container",
        r#"C4Container
    title 受注システムのコンテナ
    Person(customer, "顧客")
    Container_Boundary(shop, "受注システム") {
        Container(web, "Web", "Rust", "画面を出す")
        ContainerDb(db, "DB", "SQLite", "注文を保存する")
    }
    Rel(customer, web, "注文する")
    Rel(web, db, "読み書きする")"#,
    ),
    (
        "c4-component",
        r#"C4Component
    title Web のコンポーネント
    Container_Boundary(web, "Web") {
        Component(form, "注文フォーム", "HTML")
        Component(api, "注文 API", "Rust")
    }
    Rel(form, api, "送信する")"#,
    ),
    (
        "c4-dynamic",
        r#"C4Dynamic
    title 注文の流れ
    Person(customer, "顧客")
    Container(web, "Web", "Rust")
    ContainerDb(db, "DB", "SQLite")
    Rel(customer, web, "注文する")
    Rel(web, db, "保存する")"#,
    ),
    (
        "c4-deployment",
        r#"C4Deployment
    title 配置
    Deployment_Node(server, "サーバー", "Linux") {
        Container(web, "Web", "Rust")
    }
    Deployment_Node(storage, "ストレージ", "SSD") {
        ContainerDb(db, "DB", "SQLite")
    }
    Rel(web, db, "読み書きする")"#,
    ),
    (
        "block",
        r#"block-beta
    %% columns で列の数を決め、space は空きマス、--> で矢印
    columns 3
    input["入力"] process["処理"] output["出力"]
    space log["記録"] space
    input --> process
    process --> output
    process --> log"#,
    ),
    (
        "sankey",
        r#"sankey-beta
    %% 1 行が 流れ元,流れ先,量
    電力,家庭,30
    電力,工場,50
    ガス,家庭,20
    ガス,工場,10"#,
    ),
    (
        "packet",
        r#"packet-beta
    %% 開始ビット-終了ビット: "項目名"
    title UDP ヘッダー
    0-15: "送信元ポート"
    16-31: "宛先ポート"
    32-47: "長さ"
    48-63: "チェックサム"
    64-95: "データ (可変長)""#,
    ),
    (
        "venn",
        r#"venn-beta
    %% set で集合、union で重なりに名前を付ける
    title 担当の重なり
    set Front["フロントエンド"]
    set Back["バックエンド"]
    union Front,Back["API"]"#,
    ),
    (
        "kanban",
        r#"kanban
    %% 字下げの浅い行が列、深い行がカード
    todo[未着手]
        spec[仕様を書く]
        screen[画面を作る]
    doing[作業中]
        api[API を作る]
    done[完了]
        setup[環境を整える]"#,
    ),
    (
        "treemap",
        r#"treemap-beta
    %% 字下げで入れ子。末端の項目に : で値を付ける
    "開発費"
        "設計": 30
        "実装": 50
    "運用費"
        "保守": 15
        "監視": 5"#,
    ),
    (
        "architecture",
        r#"architecture-beta
    %% group はまとまり、service は部品。:L :R :T :B で線をつなぐ辺を決める
    group app(cloud)[アプリ]
    service web(server)[Web サーバー] in app
    service db(database)[データベース] in app
    service disk(disk)[ストレージ] in app
    web:R -- L:db
    web:B -- T:disk"#,
    ),
    (
        "treeView",
        r#"treeView-beta
    %% 字下げで階層を表す
    "プロジェクト"
        "src"
            "main.rs"
            "lib.rs"
        "docs"
            "README.md""#,
    ),
    (
        "ishikawa",
        r#"ishikawa-beta
    %% 最初の行が結果、字下げした行が要因
    不良品が多い
        人
            経験が浅い
        方法
            手順書が古い
        材料
            仕入れ先のばらつき"#,
    ),
    (
        "eventmodeling",
        r#"eventmodeling
    %% tf 番号 種類 名前。ui は画面、cmd は操作、evt は出来事、rmo は読み取りモデル
    tf 01 ui かご画面
    tf 02 cmd 商品を追加
    tf 03 evt 商品が追加された
    tf 04 rmo かごの中身 ->> 03"#,
    ),
];

fn rgb_theme() -> MermaidTheme {
    MermaidTheme {
        dark_mode: true,
        font_family: "system-ui".to_string(),
        background: rgb(40, 44, 51),
        primary_color: rgb(47, 52, 62),
        primary_text_color: rgb(220, 224, 229),
        primary_border_color: rgb(70, 75, 87),
        secondary_color: rgb(46, 52, 62),
        tertiary_color: rgb(54, 60, 70),
        line_color: rgb(70, 75, 87),
        text_color: rgb(220, 224, 229),
        edge_label_background: rgb(40, 44, 51),
        cluster_background: rgb(47, 52, 62),
        cluster_border: rgb(54, 60, 70),
        note_background: rgb(47, 52, 62),
        note_border: rgb(54, 60, 70),
        actor_background: rgb(46, 52, 62),
        actor_border: rgb(70, 75, 87),
        activation_background: rgb(54, 60, 70),
        activation_border: rgb(70, 75, 87),
        git_branch_colors: [
            rgb(116, 173, 232),
            rgb(190, 80, 70),
            rgb(191, 149, 106),
            rgb(180, 119, 207),
            rgb(110, 180, 191),
            rgb(208, 114, 119),
            rgb(222, 193, 132),
            rgb(161, 193, 129),
        ],
        git_branch_label_colors: [
            rgb(116, 173, 232),
            rgb(190, 80, 70),
            rgb(191, 149, 106),
            rgb(180, 119, 207),
            rgb(110, 180, 191),
            rgb(208, 114, 119),
            rgb(222, 193, 132),
            rgb(161, 193, 129),
        ]
        .map(mermaid_render::text_color_for_background),
        er_attr_bg_odd: rgb(47, 52, 62),
        er_attr_bg_even: rgb(46, 52, 62),
        error_color: rgb(220, 38, 38),
        warning_color: rgb(217, 119, 6),
        accent_colors: vec![
            mermaid_render::AccentColor {
                foreground: rgb(116, 173, 232),
                background: rgb(116, 173, 232),
            },
            mermaid_render::AccentColor {
                foreground: rgb(190, 80, 70),
                background: rgb(190, 80, 70),
            },
            mermaid_render::AccentColor {
                foreground: rgb(191, 149, 106),
                background: rgb(191, 149, 106),
            },
            mermaid_render::AccentColor {
                foreground: rgb(180, 119, 207),
                background: rgb(180, 119, 207),
            },
            mermaid_render::AccentColor {
                foreground: rgb(110, 180, 191),
                background: rgb(110, 180, 191),
            },
            mermaid_render::AccentColor {
                foreground: rgb(208, 114, 119),
                background: rgb(208, 114, 119),
            },
            mermaid_render::AccentColor {
                foreground: rgb(222, 193, 132),
                background: rgb(222, 193, 132),
            },
            mermaid_render::AccentColor {
                foreground: rgb(161, 193, 129),
                background: rgb(161, 193, 129),
            },
        ],
    }
}

fn check_svg_issues(name: &str, svg: &str) -> Vec<String> {
    let bad_patterns = [
        "fill=\"\"",
        "stroke=\"\"",
        "width=\"\"",
        "height=\"\"",
        "NaN",
        // Also check for empty values in style attributes
        "fill: ;",
        "fill:;",
        "stroke: ;",
        "stroke:;",
        // Check for attributes with just whitespace
        "fill=\" \"",
    ];
    let mut issues = Vec::new();
    for pattern in &bad_patterns {
        let mut start = 0;
        while let Some(pos) = svg[start..].find(pattern) {
            let abs = start + pos;
            let ctx_start = abs.saturating_sub(100);
            let ctx_end = (abs + pattern.len() + 60).min(svg.len());
            issues.push(format!(
                "{name}: found `{pattern}` at byte {abs}:\n  ...{}...\n",
                &svg[ctx_start..ctx_end]
            ));
            start = abs + pattern.len();
        }
    }

    // Parse with quick-xml to find ANY empty attribute values on visual elements
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(svg);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let tag = String::from_utf8_lossy(e.name().local_name().as_ref()).to_string();
                for attr in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(attr.key.local_name().as_ref()).to_string();
                    let val = attr.unescape_value().unwrap_or_default();
                    let visual_attr = matches!(
                        key.as_str(),
                        "fill"
                            | "stroke"
                            | "width"
                            | "height"
                            | "x"
                            | "y"
                            | "r"
                            | "cx"
                            | "cy"
                            | "rx"
                            | "ry"
                            | "stroke-width"
                    );
                    if visual_attr && val.is_empty() {
                        issues.push(format!("{name}: <{tag}> has empty {key}=\"\"\n"));
                    }
                    // Check for CSS length units that usvg can't parse
                    if visual_attr
                        && matches!(key.as_str(), "width" | "height")
                        && val.ends_with("px")
                    {
                        issues.push(format!("{name}: <{tag}> has {key}=\"{val}\" (px suffix)\n"));
                    }
                }
            }
            Err(e) => {
                issues.push(format!("{name}: XML parse error: {e}\n"));
                break;
            }
            _ => {}
        }
    }

    issues
}

#[test]
fn accent_colors_auto_applied_to_nodes() {
    let theme = rgb_theme();

    // A plain state diagram with no :::accent syntax should get
    // automatic accent colors applied to its node groups.
    let source = "stateDiagram-v2\n    [*] --> Idle\n    Idle --> Processing\n    Processing --> Done\n    Done --> [*]";

    let svg = mermaid_render::render_to_svg(source, &theme).expect("render failed");

    // accent_fill_and_text darkens the background color for dark mode.
    // The stroke colors are direct hex conversions of the accent rgb values.
    // With 3 states (Idle, Processing, Done), we expect at least accent0 and
    // accent1 stroke colors to appear.
    let accent0_stroke = "#74ade8"; // rgb(116, 173, 232) -> hex
    let accent1_stroke = "#be5046"; // rgb(190, 80, 70) -> hex

    assert!(
        svg.contains(accent0_stroke),
        "Expected accent0 stroke color ({accent0_stroke}) in auto-colored state diagram SVG.\n\
         This means auto-coloring did not apply accent colors to node groups.\n\
         SVG snippet: {}...",
        &svg[..svg.len().min(2000)]
    );
    assert!(
        svg.contains(accent1_stroke),
        "Expected accent1 stroke color ({accent1_stroke}) in auto-colored state diagram SVG."
    );
}

#[test]
fn generics_not_double_escaped() {
    let theme = rgb_theme();
    let source = "classDiagram\n    class Shelter {\n        -List~Animal~ animals\n        +adopt(Animal a) bool\n    }";
    let svg = mermaid_render::render_to_svg(source, &theme).expect("render failed");
    assert!(
        !svg.contains("&amp;lt;"),
        "Double-escaped &amp;lt; found in SVG"
    );
    assert!(
        !svg.contains("&amp;gt;"),
        "Double-escaped &amp;gt; found in SVG"
    );
}

#[test]
fn class_diagram_label_text_uses_accent_classes() {
    let theme = rgb_theme();
    let source = r#"classDiagram
    class Animal {
        +String name
        +makeSound() void
    }
    class Dog {
        +String breed
        +bark() void
    }
    Dog --|> Animal"#;

    let svg = mermaid_render::render_to_svg(source, &theme).expect("render failed");

    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(&svg);
    let mut accent_classes: Vec<String> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) if e.name().as_ref() == b"text" => {
                if let Ok(Some(class_attr)) = e.try_get_attribute("class") {
                    let class = class_attr.unescape_value().unwrap_or_default().to_string();
                    for token in class.split_whitespace() {
                        if token.starts_with("zed-accent-") {
                            accent_classes.push(token.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    assert!(
        !accent_classes.is_empty(),
        "expected zed-accent-N classes on label text elements",
    );
}

#[test]
fn sequence_diagram_tspan_uses_accent_classes() {
    let theme = rgb_theme();
    let source = "sequenceDiagram\n    participant Database";
    let svg = mermaid_render::render_to_svg(source, &theme).expect("render failed");

    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(&svg);
    let mut accent_classes: Vec<String> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) if e.name().as_ref() == b"tspan" => {
                if let Ok(Some(class_attr)) = e.try_get_attribute("class") {
                    let class = class_attr.unescape_value().unwrap_or_default().to_string();
                    for token in class.split_whitespace() {
                        if token.starts_with("zed-accent-") {
                            accent_classes.push(token.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    assert!(
        !accent_classes.is_empty(),
        "expected zed-accent-N classes on tspan elements in sequence diagram",
    );
}

#[test]
fn no_empty_attributes_or_nan_with_rgb_theme() {
    let theme = rgb_theme();
    let mut all_issues = Vec::new();

    for (name, source) in DIAGRAMS {
        match mermaid_render::render_to_svg(source, &theme) {
            Ok(svg) => all_issues.extend(check_svg_issues(name, &svg)),
            Err(e) => eprintln!("{name}: render failed (skipped): {e}"),
        }
    }

    if !all_issues.is_empty() {
        panic!(
            "Found {} issues in merman SVG output (rgb theme):\n\n{}",
            all_issues.len(),
            all_issues.join("\n")
        );
    }
}

/// A light palette with accent colors, which the default test theme leaves
/// out. The branch colors are the ones the print export uses, since the default
/// test theme's pure yellow is not a color any light Zed theme gives its players.
fn light_theme() -> MermaidTheme {
    let mut theme = MermaidTheme::default();
    theme.git_branch_colors = [
        rgb(0x09, 0x69, 0xda),
        rgb(0x1a, 0x7f, 0x37),
        rgb(0x82, 0x50, 0xdf),
        rgb(0xbc, 0x4c, 0x00),
        rgb(0x0f, 0x6c, 0xbd),
        rgb(0x6e, 0x77, 0x81),
        rgb(0xbf, 0x39, 0x89),
        rgb(0xcf, 0x22, 0x2e),
    ];
    theme.git_branch_label_colors = theme
        .git_branch_colors
        .map(mermaid_render::text_color_for_background);
    theme.accent_colors = theme
        .git_branch_colors
        .iter()
        .map(|color| mermaid_render::AccentColor {
            foreground: *color,
            background: *color,
        })
        .collect();
    theme
}

fn both_themes() -> [(&'static str, MermaidTheme); 2] {
    // A family the test font database can resolve, so usvg keeps the text.
    let dark = MermaidTheme {
        font_family: "IBM Plex Sans, sans-serif".to_string(),
        ..rgb_theme()
    };
    [("light", light_theme()), ("dark", dark)]
}

fn render_newer(name: &str, theme_name: &str, theme: &MermaidTheme) -> String {
    let source = NEWER_DIAGRAMS
        .iter()
        .find(|(sample_name, _)| *sample_name == name)
        .map(|(_, source)| *source)
        .unwrap_or_else(|| panic!("no sample named {name}"));
    mermaid_render::render_to_svg(source, theme).unwrap_or_else(|error| {
        panic!("{name} should render with the {theme_name} theme: {error:#}")
    })
}

#[test]
fn newer_diagrams_render_without_invalid_attributes() {
    let mut all_issues = Vec::new();
    for (theme_name, theme) in both_themes() {
        for (name, _) in NEWER_DIAGRAMS {
            let svg = render_newer(name, theme_name, &theme);
            all_issues.extend(check_svg_issues(&format!("{name} ({theme_name})"), &svg));
        }
    }
    assert!(
        all_issues.is_empty(),
        "Found {} issues:\n\n{}",
        all_issues.len(),
        all_issues.join("\n")
    );
}

fn parse_with_fonts(svg: &str) -> usvg::Tree {
    const IBM_PLEX_REGULAR: &[u8] =
        include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");
    const IBM_PLEX_SEMIBOLD: &[u8] =
        include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf");
    let mut database = usvg::fontdb::Database::new();
    database.load_font_data(IBM_PLEX_REGULAR.to_vec());
    database.load_font_data(IBM_PLEX_SEMIBOLD.to_vec());
    database.set_sans_serif_family("IBM Plex Sans");
    let options = usvg::Options {
        fontdb: std::sync::Arc::new(database),
        ..Default::default()
    };
    usvg::Tree::from_data(svg.as_bytes(), &options).expect("the SVG should parse")
}

#[derive(Default)]
struct PaintedColors {
    /// `(text, fill)` for every text span, as resolved after the stylesheet.
    text_fills: Vec<(String, [u8; 3])>,
    strokes: Vec<[u8; 3]>,
}

fn solid(paint: &usvg::Paint) -> Option<[u8; 3]> {
    match paint {
        usvg::Paint::Color(color) => Some([color.red, color.green, color.blue]),
        _ => None,
    }
}

fn collect_colors(group: &usvg::Group, colors: &mut PaintedColors) {
    for node in group.children() {
        match node {
            usvg::Node::Group(group) => collect_colors(group, colors),
            usvg::Node::Path(path) => {
                if let Some(stroke) = path.stroke()
                    && stroke.opacity().get() > 0.0
                    && let Some(color) = solid(stroke.paint())
                {
                    colors.strokes.push(color);
                }
            }
            usvg::Node::Text(text) => {
                for chunk in text.chunks() {
                    for span in chunk.spans() {
                        if let Some(fill) = span.fill()
                            && fill.opacity().get() > 0.0
                            && let Some(color) = solid(fill.paint())
                        {
                            let content = chunk
                                .text()
                                .get(span.start()..span.end())
                                .unwrap_or_default()
                                .to_string();
                            colors.text_fills.push((content, color));
                        }
                    }
                }
            }
            usvg::Node::Image(_) => {}
        }
    }
}

fn relative_luminance([red, green, blue]: [u8; 3]) -> f32 {
    let linear = |channel: u8| {
        let channel = channel as f32 / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
}

fn contrast(first: [u8; 3], second: [u8; 3]) -> f32 {
    let (first, second) = (relative_luminance(first), relative_luminance(second));
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

fn background_of(theme: &MermaidTheme) -> [u8; 3] {
    let background = gpui::Rgba::from(theme.background);
    [background.r, background.g, background.b].map(|channel| (channel * 255.0).round() as u8)
}

/// Text drawn straight onto the preview background must stand out from it,
/// and on a dark theme no line may be darker than the background — merman's
/// light-mode defaults (black markers, black tree lines, `#444444` C4
/// relations) are exactly what disappeared before these kinds were themed.
#[test]
fn newer_diagrams_stay_readable_on_light_and_dark_themes() {
    // C4 writes its box captions in white onto its own dark blue and gray
    // boxes, whatever the page background is.
    const C4_BOX_CAPTION: [u8; 3] = [0xff, 0xff, 0xff];

    let mut problems = Vec::new();
    for (theme_name, theme) in both_themes() {
        let background = background_of(&theme);
        for (name, _) in NEWER_DIAGRAMS {
            let svg = render_newer(name, theme_name, &theme);
            let mut colors = PaintedColors::default();
            collect_colors(parse_with_fonts(&svg).root(), &mut colors);
            assert!(
                !colors.text_fills.is_empty(),
                "{name} ({theme_name}) should draw some text"
            );

            for (text, fill) in &colors.text_fills {
                if name.starts_with("c4") && *fill == C4_BOX_CAPTION {
                    continue;
                }
                let ratio = contrast(*fill, background);
                if ratio < 3.0 {
                    problems.push(format!(
                        "{name} ({theme_name}): text {text:?} in {fill:02x?} has contrast {ratio:.2}"
                    ));
                }
            }
            if theme.dark_mode {
                for stroke in &colors.strokes {
                    if relative_luminance(*stroke) < relative_luminance(background) {
                        problems.push(format!(
                            "{name} ({theme_name}): a line in {stroke:02x?} is darker than the background"
                        ));
                    }
                }
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn c4_relations_and_boundaries_use_the_theme_instead_of_fixed_gray() {
    for (theme_name, theme) in both_themes() {
        for name in [
            "c4-context",
            "c4-container",
            "c4-component",
            "c4-dynamic",
            "c4-deployment",
        ] {
            let svg = render_newer(name, theme_name, &theme);
            assert!(
                !svg.contains("#444444"),
                "{name} ({theme_name}) still draws in C4's fixed gray"
            );
        }
    }
}

#[test]
fn c4_keeps_colors_the_diagram_sets_itself() {
    let source = "C4Context\n    Person(a, \"A\")\n    System(b, \"B\")\n    Rel(a, b, \"uses\")\n    UpdateRelStyle(a, b, $textColor=\"#ff0000\", $lineColor=\"#00ff00\")";
    let svg = mermaid_render::render_to_svg(source, &rgb_theme()).expect("render failed");
    assert!(svg.contains("#ff0000"), "{svg}");
    assert!(svg.contains("#00ff00"), "{svg}");
}

#[test]
fn sankey_nodes_keep_their_colors_and_links_stay_visible_on_dark() {
    for (theme_name, theme) in both_themes() {
        let svg = render_newer("sankey", theme_name, &theme);
        assert!(
            svg.contains(r##"fill="#4e79a7" style="fill: #4e79a7;""##),
            "{theme_name}: a sankey node should keep its own color over the generic node rule: {svg}"
        );
        // Links are drawn over the node labels, so on a dark theme they must
        // lighten what is below them: a normal blend dims the label text.
        assert_eq!(
            svg.contains("mix-blend-mode: screen !important"),
            theme.dark_mode,
            "{theme_name}: multiplied links vanish only on a dark background"
        );
        assert!(
            !svg.contains("mix-blend-mode: normal"),
            "{theme_name}: links drawn normally over the labels veil them"
        );
    }
}

#[test]
fn treemap_labels_follow_the_theme_but_keep_class_def_colors() {
    let source = "treemap-beta\n\"Products\"\n    \"Electronics\":::whiteText\n        \"Phones\": 40\n    \"Furniture\"\n        \"Chairs\": 25\n\nclassDef whiteText fill:#2c3e50,color:#ffffff;";
    for (theme_name, theme) in both_themes() {
        let svg = mermaid_render::render_to_svg(source, &theme).expect("render failed");
        let text = format!("fill:{};", css_hex(theme.text_color));
        let furniture_label = svg
            .split("<text")
            .find(|element| element.contains("Furniture"))
            .expect("the Furniture section label should be drawn");
        assert!(
            furniture_label.contains(&text),
            "{theme_name}: an unstyled label should use the theme text color: {furniture_label}"
        );
        let electronics_label = svg
            .split("<text")
            .find(|element| element.contains("Electronics"))
            .expect("the Electronics section label should be drawn");
        let last_fill = electronics_label
            .rsplit("fill:")
            .next()
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        assert_eq!(
            last_fill,
            Some("#ffffff"),
            "{theme_name}: the classDef color must still win: {electronics_label}"
        );
    }
}

#[test]
fn event_modeling_boxes_are_tinted_for_the_theme() {
    let svg = render_newer("eventmodeling", "dark", &rgb_theme());
    for pastel in ["#bcd6fe", "#ffb778", "#d3f1a2", "rgb(250,250,250)"] {
        assert!(
            !svg.contains(pastel),
            "the light-mode default {pastel} should not be used on a dark theme"
        );
    }
}

fn css_hex(color: Hsla) -> String {
    let [red, green, blue] = {
        let rgba = gpui::Rgba::from(color);
        [rgba.r, rgba.g, rgba.b].map(|channel| (channel * 255.0).round() as u8)
    };
    format!("#{red:02x}{green:02x}{blue:02x}")
}
