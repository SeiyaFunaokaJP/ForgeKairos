use std::{
    collections::BTreeMap,
    io,
    sync::atomic::AtomicBool,
    time::{Duration, Instant, UNIX_EPOCH},
};

use parking_lot::Mutex;

use crate::*;

fn parse_c(source: &str, name: &str, tag: Option<TagKind>) -> TypeLayout {
    parse_type_definition(source, TypeLocator::Name { name, tag }, SourceLanguage::C)
        .unwrap_or_else(|| panic!("no definition of {name}"))
}

fn member<'a>(layout: &'a TypeLayout, name: &str) -> &'a Member {
    layout
        .members
        .iter()
        .find(|member| member.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no member {name} in {:#?}", layout.members))
}

fn names(layout: &TypeLayout) -> Vec<Option<&str>> {
    layout
        .members
        .iter()
        .map(|member| member.name.as_deref())
        .collect()
}

/// The position of the `occurrence`-th match of `needle`, computed without the lexer.
fn position_of(source: &str, needle: &str, occurrence: usize) -> Position {
    let byte = source
        .match_indices(needle)
        .nth(occurrence)
        .map(|(index, _)| index)
        .expect("needle present");
    let before = &source[..byte];
    let row = before.matches('\n').count();
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    Position {
        byte: byte as u32,
        row: row as u32,
        column: (byte - line_start) as u32,
    }
}

fn offset_of(source: &str, needle: &str, occurrence: usize) -> usize {
    position_of(source, needle, occurrence).byte as usize
}

#[test]
fn plain_struct_members_types_and_positions() {
    let source = "struct point {\n\
                  \tint x;          /* horizontal */\n\
                  \tconst char *label;\n\
                  \tstruct point *next;\n\
                  \tunsigned long flags;\n\
                  };\n";
    let layout = parse_c(source, "point", Some(TagKind::Struct));
    assert_eq!(layout.kind, TypeKind::Struct);
    assert_eq!(layout.display_name(), "struct point");
    assert!(!layout.is_forward_declaration);
    assert_eq!(
        names(&layout),
        vec![Some("x"), Some("label"), Some("next"), Some("flags")]
    );
    let x = member(&layout, "x");
    assert_eq!(x.comment.as_deref(), Some("horizontal"));
    assert_eq!(x.ty.core_name.as_deref(), Some("int"));
    assert!(x.ty.is_builtin);
    assert_eq!(
        x.name_range.map(|range| range.start),
        Some(position_of(source, "x;", 0))
    );
    let label = member(&layout, "label");
    assert_eq!(label.ty.text, "const char *");
    assert_eq!(label.text, "const char *label");
    assert_eq!(label.ty.core_name.as_deref(), Some("char"));
    assert_eq!(label.ty.pointer_depth, 1);
    assert!(
        label
            .ty
            .qualifiers
            .iter()
            .any(|qualifier| qualifier == "const")
    );
    let next = member(&layout, "next");
    assert_eq!(next.ty.text, "struct point *");
    assert_eq!(next.ty.tag, Some(TagKind::Struct));
    assert_eq!(next.ty.core_name.as_deref(), Some("point"));
    assert!(next.ty.is_expandable());
    assert_eq!(
        member(&layout, "flags").ty.core_name.as_deref(),
        Some("unsigned long")
    );
    assert_eq!(layout.range.start, position_of(source, "struct point", 0));
    assert_eq!(layout.range.end.byte as usize, source.len() - 1);
}

#[test]
fn anonymous_and_named_nested_aggregates() {
    let source = "struct outer {\n\
                  \tint kind;\n\
                  \tunion {\n\
                  \t\tint i;\n\
                  \t\tfloat f;\n\
                  \t};\n\
                  \tstruct {\n\
                  \t\tchar a;\n\
                  \t\tstruct { int deep; } inner;\n\
                  \t} named;\n\
                  \tstruct tagged { int t; };\n\
                  };\n";
    let layout = parse_c(source, "outer", Some(TagKind::Struct));
    assert_eq!(layout.members.len(), 4, "{:#?}", layout.members);
    let anonymous = &layout.members[1];
    assert_eq!(anonymous.kind, MemberKind::AnonymousAggregate);
    assert_eq!(anonymous.name, None);
    let union_layout = anonymous.nested.as_deref().expect("nested union");
    assert_eq!(union_layout.kind, TypeKind::Union);
    assert_eq!(names(union_layout), vec![Some("i"), Some("f")]);
    // C reaches members of anonymous aggregates directly.
    assert_eq!(
        layout
            .find_member("f")
            .map(|member| member.ty.text.as_str()),
        Some("float")
    );
    let named = member(&layout, "named");
    assert_eq!(named.kind, MemberKind::Field);
    assert_eq!(named.ty.text, "struct {...}");
    assert_eq!(named.text, "struct {...} named");
    let named_layout = named.nested.as_deref().expect("nested struct");
    let inner = member(named_layout, "inner");
    assert_eq!(
        names(inner.nested.as_deref().expect("deeper")),
        vec![Some("deep")]
    );
    // Members of a named member are not reachable from the outer type.
    assert!(layout.find_member("deep").is_none());
    let tagged = &layout.members[3];
    assert_eq!(tagged.kind, MemberKind::NestedType);
    assert_eq!(tagged.ty.core_name.as_deref(), Some("tagged"));
}

#[test]
fn bit_fields_arrays_and_multiple_declarators() {
    let source = "struct bits {\n\
                  \tunsigned int ready : 1, error:2;\n\
                  \tint : 5;\n\
                  \tchar name[16];\n\
                  \tint matrix[2][3];\n\
                  \tchar buffer[MAX_LEN + 1];\n\
                  \tint flexible[];\n\
                  \tint a, *b, c[4];\n\
                  };\n";
    let layout = parse_c(source, "bits", None);
    assert_eq!(
        names(&layout),
        vec![
            Some("ready"),
            Some("error"),
            None,
            Some("name"),
            Some("matrix"),
            Some("buffer"),
            Some("flexible"),
            Some("a"),
            Some("b"),
            Some("c"),
        ]
    );
    let ready = member(&layout, "ready");
    assert_eq!(ready.bit_width.as_deref(), Some("1"));
    assert_eq!(ready.text, "unsigned int ready : 1");
    assert_eq!(member(&layout, "error").bit_width.as_deref(), Some("2"));
    let padding = &layout.members[2];
    assert_eq!(padding.kind, MemberKind::Field);
    assert_eq!(padding.bit_width.as_deref(), Some("5"));
    let name = member(&layout, "name");
    assert_eq!(name.ty.array_dimensions, vec!["16"]);
    assert_eq!(name.ty.text, "char [16]");
    assert_eq!(name.text, "char name[16]");
    assert_eq!(
        member(&layout, "matrix").ty.array_dimensions,
        vec!["2", "3"]
    );
    assert_eq!(
        member(&layout, "buffer").ty.array_dimensions,
        vec!["MAX_LEN + 1"]
    );
    assert_eq!(member(&layout, "flexible").ty.array_dimensions, vec![""]);
    assert_eq!(member(&layout, "a").ty.text, "int");
    let b = member(&layout, "b");
    assert_eq!((b.ty.text.as_str(), b.ty.pointer_depth), ("int *", 1));
    assert_eq!(b.text, "int *b");
    assert_eq!(member(&layout, "c").ty.array_dimensions, vec!["4"]);
    // Each declarator keeps its own position.
    assert_eq!(
        b.name_range.map(|range| range.start),
        Some(position_of(source, "b,", 0))
    );
}

#[test]
fn function_pointer_members() {
    let source = "struct file_operations {\n\
                  \tstruct module *owner;\n\
                  \tint (*open)(struct inode *, struct file *);\n\
                  \tssize_t (*read)(struct file *, char __user *, size_t, loff_t *);\n\
                  \tvoid (*handlers[4])(int);\n\
                  \tvoid (WINAPI *callback)(void);\n\
                  };\n";
    let layout = parse_c(source, "file_operations", Some(TagKind::Struct));
    let open = member(&layout, "open");
    assert_eq!(open.kind, MemberKind::FunctionPointer);
    assert!(open.ty.is_function_pointer);
    assert!(!open.ty.is_expandable());
    assert_eq!(open.ty.text, "int (*)(struct inode *, struct file *)");
    assert_eq!(open.text, "int (*open)(struct inode *, struct file *)");
    assert_eq!(open.ty.core_name, None);
    let read = member(&layout, "read");
    assert_eq!(
        read.text,
        "ssize_t (*read)(struct file *, char __user *, size_t, loff_t *)"
    );
    let handlers = member(&layout, "handlers");
    assert_eq!(handlers.kind, MemberKind::FunctionPointer);
    assert_eq!(handlers.ty.array_dimensions, vec!["4"]);
    assert_eq!(
        member(&layout, "callback").kind,
        MemberKind::FunctionPointer
    );
    assert_eq!(member(&layout, "owner").kind, MemberKind::Field);
}

#[test]
fn typedef_names_and_tags() {
    let source = "typedef struct { int a; } plain_t;\n\
                  typedef struct foo_s { int b; } foo_t, *foo_p;\n\
                  typedef struct foo_s other_t;\n\
                  typedef unsigned long size_type;\n\
                  typedef int (*callback_t)(int, void *);\n";
    let plain = parse_c(source, "plain_t", None);
    assert_eq!(plain.kind, TypeKind::Struct);
    assert_eq!(plain.tag_name, None);
    assert_eq!(plain.display_name(), "plain_t");
    assert_eq!(names(&plain), vec![Some("a")]);

    let foo = parse_c(source, "foo_t", None);
    assert_eq!(foo.tag_name.as_deref(), Some("foo_s"));
    assert_eq!(
        foo.typedef_names
            .iter()
            .map(|typedef| (typedef.name.as_str(), typedef.pointer_depth))
            .collect::<Vec<_>>(),
        vec![("foo_t", 0), ("foo_p", 1)]
    );
    assert_eq!(
        foo.range.start,
        position_of(source, "typedef struct foo_s", 0)
    );
    let by_tag = parse_c(source, "foo_s", Some(TagKind::Struct));
    assert_eq!(by_tag, foo);

    let other = parse_c(source, "other_t", None);
    assert_eq!(other.kind, TypeKind::Typedef);
    let alias = other.alias.as_ref().expect("alias");
    assert_eq!(
        (alias.core_name.as_deref(), alias.tag),
        (Some("foo_s"), Some(TagKind::Struct))
    );
    let size = parse_c(source, "size_type", None);
    assert_eq!(
        size.alias.as_ref().map(|alias| alias.text.as_str()),
        Some("unsigned long")
    );
    let callback = parse_c(source, "callback_t", None);
    let alias = callback.alias.as_ref().expect("alias");
    assert!(alias.is_function_pointer);
    assert_eq!(alias.text, "int (*)(int, void *)");
}

#[test]
fn a_body_is_preferred_over_a_forward_declaration() {
    let source = "struct later;\n\
                  typedef struct later later_t;\n\
                  struct later { int value; };\n";
    let later = parse_c(source, "later", Some(TagKind::Struct));
    assert!(!later.is_forward_declaration);
    assert_eq!(names(&later), vec![Some("value")]);
    let typedef = parse_c(source, "later_t", None);
    assert_eq!(typedef.kind, TypeKind::Typedef);
    let lonely = parse_c("struct lonely;\n", "lonely", Some(TagKind::Struct));
    assert!(lonely.is_forward_declaration);
    assert!(!lonely.has_body());
    // A tag of another kind does not match.
    assert!(
        parse_type_definition(
            source,
            TypeLocator::Name {
                name: "later",
                tag: Some(TagKind::Union)
            },
            SourceLanguage::C
        )
        .is_none()
    );
}

#[test]
fn enumerator_values() {
    let source = "enum color {\n\
                  \tRED,              /* first */\n\
                  \tGREEN = 5,\n\
                  \tBLUE,\n\
                  \tMASK = 0x10,\n\
                  \tNEG = -2,\n\
                  \tNEXT,\n\
                  \tSHIFTED = 1 << 3,\n\
                  \tAFTER,\n\
                  \tOCTAL = 010,\n\
                  };\n";
    let layout = parse_c(source, "color", Some(TagKind::Enum));
    let values: Vec<(&str, Option<&str>, Option<i128>, bool)> = layout
        .enumerators
        .iter()
        .map(|enumerator| {
            (
                enumerator.name.as_str(),
                enumerator.value_text.as_deref(),
                enumerator.value,
                enumerator.value_is_inferred,
            )
        })
        .collect();
    assert_eq!(
        values,
        vec![
            ("RED", None, Some(0), true),
            ("GREEN", Some("5"), Some(5), false),
            ("BLUE", None, Some(6), true),
            ("MASK", Some("0x10"), Some(16), false),
            ("NEG", Some("-2"), Some(-2), false),
            ("NEXT", None, Some(-1), true),
            ("SHIFTED", Some("1 << 3"), None, false),
            ("AFTER", None, None, false),
            ("OCTAL", Some("010"), Some(8), false),
        ]
    );
    assert_eq!(layout.enumerators[0].comment.as_deref(), Some("first"));
    assert_eq!(
        layout.enumerators[1].name_range.start,
        position_of(source, "GREEN", 0)
    );
}

#[test]
fn enumerators_under_conditions() {
    let source = "enum mode {\n\
                  \tA,\n\
                  #ifdef CONFIG_X\n\
                  \tB,\n\
                  \tC,\n\
                  #else\n\
                  \tD,\n\
                  #endif\n\
                  \tE,\n\
                  #ifdef CONFIG_Y\n\
                  \tF,\n\
                  #else\n\
                  \tG,\n\
                  #endif\n\
                  \tH\n\
                  };\n";
    let layout = parse_c(source, "mode", None);
    let values: Vec<(&str, Option<i128>, Option<&str>)> = layout
        .enumerators
        .iter()
        .map(|enumerator| {
            (
                enumerator.name.as_str(),
                enumerator.value,
                enumerator.condition.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        values,
        vec![
            ("A", Some(0), None),
            ("B", Some(1), Some("CONFIG_X")),
            ("C", Some(2), Some("CONFIG_X")),
            ("D", Some(1), Some("!CONFIG_X")),
            // The branches end at different values, so E cannot be counted.
            ("E", None, None),
            ("F", None, Some("CONFIG_Y")),
            ("G", None, Some("!CONFIG_Y")),
            ("H", None, None),
        ]
    );
    let agreeing = "enum agree {\n\
                    \tP = 3,\n\
                    #ifdef CONFIG_Y\n\
                    \tQ,\n\
                    #else\n\
                    \tR,\n\
                    #endif\n\
                    \tS\n\
                    };\n";
    let layout = parse_c(agreeing, "agree", None);
    assert_eq!(
        layout
            .enumerators
            .iter()
            .map(|enumerator| enumerator.value)
            .collect::<Vec<_>>(),
        vec![Some(3), Some(4), Some(4), Some(5)]
    );
}

#[test]
fn members_under_preprocessor_conditions() {
    let source = "struct task {\n\
                  \tint pid;\n\
                  #ifdef CONFIG_SMP\n\
                  \tint cpu;\n\
                  #if defined(CONFIG_NUMA) && NR_NODES > 1 /* numa */\n\
                  \tint node;\n\
                  #endif\n\
                  #else\n\
                  \tint only_up;\n\
                  #endif\n\
                  #ifndef CONFIG_NO_HZ\n\
                  \tlong ticks;\n\
                  #elif defined(CONFIG_HZ_ONLY)\n\
                  \tlong hz;\n\
                  #else\n\
                  \tlong other;\n\
                  #endif\n\
                  \tint last;\n\
                  };\n";
    let layout = parse_c(source, "task", None);
    let conditions: Vec<(&str, Option<&str>)> = layout
        .members
        .iter()
        .map(|member| {
            (
                member.name.as_deref().unwrap_or_default(),
                member.condition.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        conditions,
        vec![
            ("pid", None),
            ("cpu", Some("CONFIG_SMP")),
            (
                "node",
                Some("CONFIG_SMP && defined(CONFIG_NUMA) && NR_NODES > 1")
            ),
            ("only_up", Some("!CONFIG_SMP")),
            ("ticks", Some("!CONFIG_NO_HZ")),
            ("hz", Some("defined(CONFIG_HZ_ONLY)")),
            ("other", Some("!(!CONFIG_NO_HZ || defined(CONFIG_HZ_ONLY))")),
            ("last", None),
        ]
    );
}

#[test]
fn annotation_macros_do_not_change_the_core_name() {
    let source = "struct kernel_thing {\n\
                  \tstruct task_struct __rcu *owner;\n\
                  \tconst char __user *name;\n\
                  \tvoid __iomem *regs;\n\
                  \t__u32 value __attribute__((packed));\n\
                  \tu8 data[8] __aligned(8);\n\
                  \tstruct list_head list;\n\
                  \tunsigned long state ____cacheline_aligned;\n\
                  \t__u32 __reserved[2];\n\
                  \t_Field_size_(count) PVOID items;\n\
                  \tunknown_annotation foo_t tagged;\n\
                  } __randomize_layout;\n";
    let layout = parse_c(source, "kernel_thing", Some(TagKind::Struct));
    assert!(
        layout
            .members
            .iter()
            .all(|member| member.kind == MemberKind::Field),
        "{:#?}",
        layout.members
    );
    let owner = member(&layout, "owner");
    assert_eq!(owner.ty.core_name.as_deref(), Some("task_struct"));
    assert_eq!(owner.ty.tag, Some(TagKind::Struct));
    assert_eq!(owner.ty.pointer_depth, 1);
    assert_eq!(owner.text, "struct task_struct __rcu *owner");
    assert!(
        owner
            .ty
            .qualifiers
            .iter()
            .any(|qualifier| qualifier == "__rcu")
    );
    let name = member(&layout, "name");
    assert_eq!(name.ty.core_name.as_deref(), Some("char"));
    assert!(
        name.ty
            .qualifiers
            .iter()
            .any(|qualifier| qualifier == "__user")
    );
    assert_eq!(
        member(&layout, "regs").ty.core_name.as_deref(),
        Some("void")
    );
    let value = member(&layout, "value");
    assert_eq!(value.ty.core_name.as_deref(), Some("__u32"));
    assert_eq!(value.text, "__u32 value");
    let data = member(&layout, "data");
    assert_eq!(data.ty.core_name.as_deref(), Some("u8"));
    assert_eq!(data.ty.array_dimensions, vec!["8"]);
    assert_eq!(
        member(&layout, "list").ty.core_name.as_deref(),
        Some("list_head")
    );
    assert_eq!(
        member(&layout, "state").ty.core_name.as_deref(),
        Some("unsigned long")
    );
    assert_eq!(
        member(&layout, "__reserved").ty.core_name.as_deref(),
        Some("__u32")
    );
    assert_eq!(
        member(&layout, "items").ty.core_name.as_deref(),
        Some("PVOID")
    );
    // The type is the identifier just before the name, not an unknown annotation in front.
    assert_eq!(
        member(&layout, "tagged").ty.core_name.as_deref(),
        Some("foo_t")
    );
    assert!(layout.typedef_names.is_empty());
}

#[test]
fn members_declared_through_macros_are_kept_as_text() {
    let source = "struct holder {\n\
                  \tDECLARE_BITMAP(bits, 64);\n\
                  \tint after;\n\
                  \tstruct_group(stats, int a; int b;);\n\
                  \tLIST_ENTRY(link);\n\
                  \tu32 FIELD(x);\n\
                  \tArray(char) characters;\n\
                  \tSLIST_ENTRY(node) link;\n\
                  \tint last;\n\
                  };\n";
    let layout = parse_c(source, "holder", None);
    let summary: Vec<(MemberKind, &str)> = layout
        .members
        .iter()
        .map(|member| (member.kind, member.text.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (MemberKind::Unparsed, "DECLARE_BITMAP(bits, 64)"),
            (MemberKind::Field, "int after"),
            (MemberKind::Unparsed, "struct_group(stats, int a; int b;)"),
            (MemberKind::Unparsed, "LIST_ENTRY(link)"),
            (MemberKind::Unparsed, "u32 FIELD(x)"),
            (MemberKind::Field, "Array(char) characters"),
            (MemberKind::Field, "SLIST_ENTRY(node) link"),
            (MemberKind::Field, "int last"),
        ]
    );
    let characters = member(&layout, "characters");
    assert_eq!(characters.ty.text, "Array(char)");
    assert!(!characters.ty.is_expandable());
}

#[test]
fn com_vtables_midl_attributes_and_macro_prefixes() {
    let source = "typedef struct IFooVtbl {\n\
                  \tBEGIN_INTERFACE\n\
                  \tDECLSPEC_XFGVIRT(IUnknown, QueryInterface)\n\
                  \tHRESULT ( STDMETHODCALLTYPE *QueryInterface )(\n\
                  \t\t__RPC__in IFoo * This,\n\
                  \t\t/* [in] */ __RPC__in REFIID riid);\n\
                  \tDECLSPEC_XFGVIRT(IUnknown, AddRef)\n\
                  \tULONG ( STDMETHODCALLTYPE *AddRef )(\n\
                  \t\t__RPC__in IFoo * This);\n\
                  \tEND_INTERFACE\n\
                  } IFooVtbl;\n\
                  typedef struct _TRUSTEE {\n\
                  \t[switch_is(TrusteeForm)] union {\n\
                  \t\t[case(TRUSTEE_IS_NAME)] LPSTR ptstrName;\n\
                  \t\t[case(TRUSTEE_IS_SID)] SID *pSid;\n\
                  \t};\n\
                  \tKABI_RESERVE(1) int last;\n\
                  } TRUSTEE;\n";
    let vtable = parse_c(source, "IFooVtbl", None);
    let summary: Vec<(Option<&str>, MemberKind, Option<&str>)> = vtable
        .members
        .iter()
        .map(|member| {
            (
                member.name.as_deref(),
                member.kind,
                member.ty.core_name.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (Some("QueryInterface"), MemberKind::FunctionPointer, None),
            (Some("AddRef"), MemberKind::FunctionPointer, None),
        ]
    );
    let trustee = parse_c(source, "TRUSTEE", None);
    assert_eq!(trustee.members.len(), 2, "{:#?}", trustee.members);
    assert_eq!(trustee.members[0].kind, MemberKind::AnonymousAggregate);
    assert_eq!(
        trustee
            .find_member("pSid")
            .map(|member| member.ty.text.as_str()),
        Some("SID *")
    );
    assert_eq!(
        member(&trustee, "last").ty.core_name.as_deref(),
        Some("int")
    );
}

#[test]
fn comments_in_front_of_and_after_members() {
    let source = "struct commented {\n\
                  \t/* leading one\n\
                  \t * second line */\n\
                  \tint a;\n\
                  \tint b; // trailing b\n\
                  \t// own line for c\n\
                  \tint c; /* not used: c has a leading comment */\n\
                  \tint d; /**< doc for d */\n\
                  \tint e;\n\
                  \t/**\n\
                  \t *\n\
                  \t * after an empty line\n\
                  \t */\n\
                  \tint f;\n\
                  };\n";
    let layout = parse_c(source, "commented", None);
    let comments: Vec<(&str, Option<&str>)> = layout
        .members
        .iter()
        .map(|member| {
            (
                member.name.as_deref().unwrap_or_default(),
                member.comment.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        comments,
        vec![
            ("a", Some("leading one")),
            ("b", Some("trailing b")),
            ("c", Some("own line for c")),
            ("d", Some("doc for d")),
            ("e", None),
            ("f", Some("after an empty line")),
        ]
    );
    let long = format!(
        "struct long_comment {{\n\t/* {} */\n\tint a;\n}};\n",
        "x".repeat(500)
    );
    let layout = parse_c(&long, "long_comment", None);
    let comment = layout.members[0].comment.as_deref().expect("comment");
    assert_eq!(comment.chars().count(), MAX_COMMENT_CHARS + 1);
    assert!(comment.ends_with('…'));
}

#[test]
fn crlf_and_japanese_comments_keep_positions() {
    let source = "#define LONG_MACRO(x) \\\r\n\
                  \tdo { x; } while (0)\r\n\
                  /* 日本語の説明 */\r\n\
                  struct jp {\r\n\
                  \t/* 名前の長さ */\r\n\
                  \tint length;\r\n\
                  \tchar *label; // 値 ラベル\r\n\
                  };\r\n";
    let layout = parse_c(source, "jp", None);
    let length = member(&layout, "length");
    assert_eq!(length.comment.as_deref(), Some("名前の長さ"));
    assert_eq!(
        length.name_range.map(|range| range.start),
        Some(position_of(source, "length", 0))
    );
    let label = member(&layout, "label");
    assert_eq!(label.comment.as_deref(), Some("値 ラベル"));
    assert_eq!(
        label.name_range.map(|range| range.end),
        Some(position_of(source, "; // 値", 0))
    );
    assert_eq!(layout.range.start, position_of(source, "struct jp", 0));
}

#[test]
fn broken_sources_do_not_panic() {
    let source = "struct broken {\n\
                  \tint ok;\n\
                  \tint (*bad)(;\n\
                  \tint also_ok;\n\
                  \tstruct { int x;\n\
                  };\n";
    let layout = parse_c(source, "broken", None);
    assert_eq!(member(&layout, "ok").ty.text, "int");

    let sample = "typedef struct DECLSPEC_ALIGN(16) _SAMPLE {\n\
                  \tunion { struct { DWORD Low; LONG High; } DUMMYSTRUCTNAME; ULONGLONG Quad; } DUMMYUNIONNAME;\n\
                  #ifdef _WIN64\n\
                  \tint (WINAPI *callback)(_In_ PVOID context, _Out_writes_(n) char *buffer);\n\
                  #endif\n\
                  \tchar name[MAX_PATH + 1]; // name\n\
                  \tenum { A = 1, B, } kind : 3;\n\
                  } SAMPLE, *PSAMPLE;\n\
                  int use(struct _SAMPLE *s) { return s->DUMMYUNIONNAME.Quad + s->kind; }\n";
    let full = parse_c(sample, "SAMPLE", None);
    assert_eq!(full.tag_name.as_deref(), Some("_SAMPLE"));
    assert_eq!(full.members.len(), 4, "{:#?}", full.members);
    for end in 0..sample.len() {
        if !sample.is_char_boundary(end) {
            continue;
        }
        let prefix = &sample[..end];
        for name in ["SAMPLE", "_SAMPLE"] {
            let layout = parse_type_definition(
                prefix,
                TypeLocator::Name { name, tag: None },
                SourceLanguage::C,
            );
            assert!(layout.is_none_or(|layout| layout.members.len() <= 4));
        }
        for offset in [0, end / 2, end] {
            type_at(prefix, offset, SourceLanguage::C);
        }
        parse_type_text(prefix, SourceLanguage::C);
    }
    for garbage in [
        "struct",
        "struct {",
        "typedef",
        "enum x {",
        "struct a { int b",
        "{{{{",
        "))((",
    ] {
        parse_type_definition(
            garbage,
            TypeLocator::Name {
                name: "a",
                tag: None,
            },
            SourceLanguage::C,
        );
    }
}

#[test]
fn large_inputs_are_cut_at_the_limits() {
    let mut source = String::from("struct many {\n");
    for index in 0..(MAX_MEMBERS + 100) {
        source.push_str(&format!("\tint member_{index};\n"));
    }
    source.push_str("};\n");
    let layout = parse_c(&source, "many", None);
    assert_eq!(layout.members.len(), MAX_MEMBERS);
    assert!(layout.truncated);

    let mut deep = String::from("struct deep {\n");
    for _ in 0..200 {
        deep.push_str("struct {\n");
    }
    deep.push_str("int leaf;\n");
    for _ in 0..200 {
        deep.push_str("} ;\n");
    }
    deep.push_str("};\n");
    let deep_layout = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || parse_c(&deep, "deep", None))
        .expect("spawn")
        .join()
        .expect("no stack overflow");
    let mut level = &deep_layout;
    let mut depth = 0;
    while let Some(nested) = level
        .members
        .first()
        .and_then(|member| member.nested.as_deref())
    {
        level = nested;
        depth += 1;
    }
    assert_eq!(depth, MAX_NESTING_DEPTH);
    assert_eq!(
        level.members.first().map(|member| member.kind),
        Some(MemberKind::Unparsed)
    );

    // A definition longer than the byte limit, read from an index range.
    let long_name = "n".repeat(300);
    let mut huge = String::from("struct huge {\n");
    let mut index = 0;
    while huge.len() < MAX_DEFINITION_BYTES + 10_000 {
        huge.push_str(&format!("\tint {long_name}_{index};\n"));
        index += 1;
    }
    huge.push_str("};\n");
    let range = Range {
        start: Position::default(),
        end: position_of(&huge, "};", 0),
    };
    let layout =
        parse_type_definition(&huge, TypeLocator::Range(range), SourceLanguage::C).expect("layout");
    assert!(layout.truncated);
    assert!(layout.members.len() < index);
}

#[test]
fn index_ranges_include_a_preceding_typedef() {
    let source = "#include <x.h>\n\
                  typedef struct foo_s {\n\
                  \tint b;\n\
                  } foo_t;\n";
    let start = position_of(source, "struct foo_s", 0);
    let end = position_of(source, "} foo_t", 0);
    let layout = parse_type_definition(
        source,
        TypeLocator::Range(Range {
            start,
            end: Position {
                byte: end.byte + 1,
                row: end.row,
                column: end.column + 1,
            },
        }),
        SourceLanguage::C,
    )
    .expect("layout");
    assert_eq!(layout.range.start, position_of(source, "typedef", 0));
    assert_eq!(
        layout
            .typedef_names
            .iter()
            .map(|typedef| typedef.name.as_str())
            .collect::<Vec<_>>(),
        vec!["foo_t"]
    );
    assert_eq!(
        member(&layout, "b").name_range.map(|range| range.start),
        Some(position_of(source, "b;", 0))
    );
}

#[test]
fn cpp_classes() {
    let source = "namespace app {\n\
                  class Widget : public Base, private ns::Mixin<int> {\n\
                  public:\n\
                  \tWidget();\n\
                  \tvirtual ~Widget();\n\
                  \tint size() const { return count; }\n\
                  \tstd::vector<Item> items;\n\
                  \tstatic const int limit = 4;\n\
                  \tWidget &operator=(const Widget &other);\n\
                  private:\n\
                  \tint count;\n\
                  };\n\
                  }\n";
    let layout = parse_type_definition(
        source,
        TypeLocator::Name {
            name: "Widget",
            tag: None,
        },
        SourceLanguage::Cpp,
    )
    .expect("layout");
    assert_eq!(layout.kind, TypeKind::Class);
    assert_eq!(layout.base_classes, vec!["Base", "ns::Mixin<int>"]);
    let summary: Vec<(Option<&str>, MemberKind)> = layout
        .members
        .iter()
        .map(|member| (member.name.as_deref(), member.kind))
        .collect();
    assert_eq!(
        summary,
        vec![
            (Some("Widget"), MemberKind::Method),
            (Some("~Widget"), MemberKind::Method),
            (Some("size"), MemberKind::Method),
            (Some("items"), MemberKind::Field),
            (Some("limit"), MemberKind::Field),
            (Some("operator="), MemberKind::Method),
            (Some("count"), MemberKind::Field),
        ]
    );
    assert_eq!(
        member(&layout, "items").ty.core_name.as_deref(),
        Some("vector")
    );
    assert_eq!(member(&layout, "items").text, "std::vector<Item> items");
}

#[test]
fn type_text_from_a_language_server() {
    let text = parse_type_text("const struct file_operations *", SourceLanguage::C).expect("type");
    assert_eq!(text.core_name.as_deref(), Some("file_operations"));
    assert_eq!(text.tag, Some(TagKind::Struct));
    assert_eq!(text.pointer_depth, 1);
    let text = parse_type_text("foo_t", SourceLanguage::C).expect("type");
    assert_eq!((text.core_name.as_deref(), text.tag), (Some("foo_t"), None));
    let text = parse_type_text("int (*)(int)", SourceLanguage::C).expect("type");
    assert!(text.is_function_pointer);
    let text = parse_type_text("unsigned long long", SourceLanguage::C).expect("type");
    assert!(text.is_builtin);
    assert_eq!(text.core_name.as_deref(), Some("unsigned long long"));
}

#[test]
fn enclosing_conditions_and_includes() {
    let source = "#include \"a/b.h\"\n\
                  # include <linux/fs.h>\n\
                  #ifdef CONFIG_A /* comment */\n\
                  /*\n\
                  #endif inside a comment\n\
                  */\n\
                  #if X > 1 \\\n\
                  \t&& Y\n\
                  int here;\n\
                  #endif\n\
                  #endif\n\
                  int outside;\n";
    assert_eq!(
        enclosing_condition(source.as_bytes(), offset_of(source, "here", 0)).as_deref(),
        Some("CONFIG_A && X > 1 && Y")
    );
    assert_eq!(
        enclosing_condition(source.as_bytes(), offset_of(source, "outside", 0)),
        None
    );
    assert_eq!(
        include_paths(source.as_bytes()),
        vec!["a/b.h", "linux/fs.h"]
    );

    // Include guards are not conditions worth showing; a plain `#ifndef` is.
    let guarded = "#ifndef FOO_H\n\
                   #define FOO_H\n\
                   #if !defined(BAR_H)\n\
                   #define BAR_H 1\n\
                   #ifndef CONFIG_B\n\
                   #define OTHER 1\n\
                   int here;\n\
                   #endif\n\
                   #endif\n\
                   #endif\n";
    assert_eq!(
        enclosing_condition(guarded.as_bytes(), offset_of(guarded, "here", 0)).as_deref(),
        Some("!CONFIG_B")
    );
    let standard = parse_type_text("uint32_t", SourceLanguage::C).expect("type");
    assert!(standard.is_builtin && !standard.is_expandable());
}

// ---------------------------------------------------------------------------------------------
// The name under the cursor

const USES: &str = "struct foo { int x; struct bar *b; };\n\
                    struct bar { struct baz inner; int y; };\n\
                    static struct foo global_foo;\n\
                    struct foo *make_foo(void);\n\
                    #define WRAP(x) (x)\n\
                    int use(struct foo *param, int count)\n\
                    {\n\
                    \tstruct foo local;\n\
                    \tint value = count;\n\
                    \t{\n\
                    \t\tstruct bar count;\n\
                    \t\tcount.y = 1;\n\
                    \t}\n\
                    \tparam->b->inner.z = 2;\n\
                    \tlocal.x = value;\n\
                    \tmake_foo()->x = 3;\n\
                    \tglobal_foo.x = 4;\n\
                    \tWRAP(value);\n\
                    \tunknown_thing->field = 5;\n\
                    \t((struct bar *)param)->y = 6;\n\
                    \tfor (struct foo *it = param; it; it = 0) it->x = 7;\n\
                    \treturn count;\n\
                    }\n\
                    static const struct foo initialized = { .b = 0, .x = 1 };\n";

fn at(needle: &str, occurrence: usize) -> TypeAt {
    type_at(USES, offset_of(USES, needle, occurrence), SourceLanguage::C)
        .unwrap_or_else(|| panic!("nothing at {needle}"))
}

fn variable(result: TypeAt) -> VariableType {
    match result {
        TypeAt::Variable { variable, .. } => variable,
        other => panic!("not a variable: {other:#?}"),
    }
}

fn access(result: TypeAt) -> MemberAccess {
    match result {
        TypeAt::MemberAccess { access, .. } => access,
        other => panic!("not a member access: {other:#?}"),
    }
}

fn member_names(access: &MemberAccess) -> Vec<&str> {
    access
        .members
        .iter()
        .map(|step| step.name.as_str())
        .collect()
}

#[test]
fn type_at_type_names() {
    match at("foo {", 0) {
        TypeAt::TypeName { name, tag, range } => {
            assert_eq!((name.as_str(), tag), ("foo", Some(TagKind::Struct)));
            assert_eq!(range.start, position_of(USES, "foo {", 0));
        }
        other => panic!("{other:#?}"),
    }
    // The cursor just after the name still counts.
    let end = offset_of(USES, "foo {", 0) + 3;
    assert!(matches!(
        type_at(USES, end, SourceLanguage::C),
        Some(TypeAt::TypeName { .. })
    ));
    let source = "typedef int handle_t;\nhandle_t h;\n";
    match type_at(
        source,
        offset_of(source, "handle_t h", 0),
        SourceLanguage::C,
    ) {
        Some(TypeAt::TypeName {
            name, tag: None, ..
        }) => assert_eq!(name, "handle_t"),
        other => panic!("{other:#?}"),
    }
}

#[test]
fn type_at_variables_by_scope() {
    let parameter = variable(at("param->b", 0));
    assert_eq!(parameter.confidence, Confidence::Parameter);
    assert_eq!(parameter.ty.core_name.as_deref(), Some("foo"));
    assert_eq!(parameter.ty.pointer_depth, 1);
    assert_eq!(
        parameter.declaration.map(|range| range.start),
        Some(position_of(USES, "param,", 0))
    );

    let shadowing = variable(at("count.y", 0));
    assert_eq!(shadowing.confidence, Confidence::Local);
    assert_eq!(shadowing.ty.core_name.as_deref(), Some("bar"));

    let returned = variable(at("count;\n}", 0));
    assert_eq!(returned.confidence, Confidence::Parameter);
    assert_eq!(returned.ty.core_name.as_deref(), Some("int"));

    let local = variable(at("local.x", 0));
    assert_eq!(local.confidence, Confidence::Local);
    assert_eq!(local.ty.text, "struct foo");

    let global = variable(at("global_foo.x", 0));
    assert_eq!(global.confidence, Confidence::FileScope);
    assert_eq!(global.ty.core_name.as_deref(), Some("foo"));

    let loop_variable = variable(at("it->x", 0));
    assert_eq!(loop_variable.confidence, Confidence::Local);
    assert_eq!(loop_variable.ty.core_name.as_deref(), Some("foo"));

    let declared = variable(at("local;", 0));
    assert_eq!(declared.confidence, Confidence::Declaration);
    let field = variable(at("b; }", 0));
    assert_eq!(field.confidence, Confidence::Declaration);
    assert_eq!(field.ty.text, "struct bar *");
}

#[test]
fn type_at_member_chains() {
    let chain = access(at("z = 2", 0));
    assert_eq!(member_names(&chain), vec!["b", "inner", "z"]);
    match &chain.root {
        ChainRoot::Variable(root) => {
            assert_eq!(root.name, "param");
            assert_eq!(root.confidence, Confidence::Parameter);
        }
        other => panic!("{other:#?}"),
    }
    let middle = access(at("inner.z", 0));
    assert_eq!(member_names(&middle), vec!["b", "inner"]);

    let call = access(at("x = 3", 0));
    assert!(matches!(&call.root, ChainRoot::Call { function, .. } if function == "make_foo"));
    let unknown = access(at("field", 0));
    assert!(matches!(&unknown.root, ChainRoot::Unknown { name, .. } if name == "unknown_thing"));
    let cast = access(at("y = 6", 0));
    match &cast.root {
        ChainRoot::Type(ty) => {
            assert_eq!(ty.core_name.as_deref(), Some("bar"));
            assert_eq!(ty.pointer_depth, 1);
        }
        other => panic!("{other:#?}"),
    }
    let designated = access(at("x = 1", 0));
    assert_eq!(member_names(&designated), vec!["x"]);
    assert!(
        matches!(&designated.root, ChainRoot::Type(ty) if ty.core_name.as_deref() == Some("foo"))
    );
    match at("WRAP(value)", 0) {
        TypeAt::Identifier { name, .. } => assert_eq!(name, "WRAP"),
        other => panic!("{other:#?}"),
    }
}

#[test]
fn type_at_in_a_window() {
    let window = offset_of(USES, "int use(", 0)..offset_of(USES, "static const struct foo i", 0);
    let parameter = type_at_in_window(
        USES,
        offset_of(USES, "param->b", 0),
        SourceLanguage::C,
        window.clone(),
    );
    match parameter {
        Some(TypeAt::Variable { variable, .. }) => {
            assert_eq!(variable.confidence, Confidence::Parameter)
        }
        other => panic!("{other:#?}"),
    }
    // The file-scope declaration is outside the window.
    match type_at_in_window(
        USES,
        offset_of(USES, "global_foo.x", 0),
        SourceLanguage::C,
        window.clone(),
    ) {
        Some(TypeAt::Identifier { name, .. }) => assert_eq!(name, "global_foo"),
        other => panic!("{other:#?}"),
    }
    // A cursor outside the window parses the whole file.
    assert!(matches!(
        type_at_in_window(USES, offset_of(USES, "foo {", 0), SourceLanguage::C, window),
        Some(TypeAt::TypeName { .. })
    ));
}

#[test]
fn type_at_in_crlf_files_with_continued_macros() {
    let source = "#define M(x) \\\r\n\
                  \tfoo(x)\r\n\
                  struct jp { int length; };\r\n\
                  void f(void)\r\n\
                  {\r\n\
                  \tstruct jp *p = 0;\r\n\
                  \tp->length = 1;\r\n\
                  }\r\n";
    let chain = match type_at(
        source,
        offset_of(source, "length = 1", 0),
        SourceLanguage::C,
    ) {
        Some(TypeAt::MemberAccess { access, range }) => {
            assert_eq!(range.start, position_of(source, "length = 1", 0));
            access
        }
        other => panic!("{other:#?}"),
    };
    match &chain.root {
        ChainRoot::Variable(root) => {
            assert_eq!(root.confidence, Confidence::Local);
            assert_eq!(root.ty.core_name.as_deref(), Some("jp"));
        }
        other => panic!("{other:#?}"),
    }

    // A comment before a line continuation ends the macro early for tree-sitter-c, in CRLF
    // and LF files alike; the rest would be read as a block that swallows the global.
    for line_end in ["\r\n", "\n"] {
        let source = format!(
            "#define TABLE(x) x /* c */ \\{line_end}\t{{ y{line_end}static struct jp *global;{line_end}void g(void){line_end}{{{line_end}\tglobal->length = 1;{line_end}}}{line_end}"
        );
        match type_at(
            &source,
            offset_of(&source, "global->", 0),
            SourceLanguage::C,
        ) {
            Some(TypeAt::Variable { variable, .. }) => {
                assert_eq!(variable.confidence, Confidence::FileScope);
            }
            other => panic!("{line_end:?}: {other:#?}"),
        }
    }

    // Read as code, the continuation line would open an initializer that swallows the global.
    let source = "#define TABLE(x) \\\r\n\
                  \tstatic const char *x[] = {\r\n\
                  static struct jp *global;\r\n\
                  void g(void)\r\n\
                  {\r\n\
                  \tglobal->length = 1;\r\n\
                  }\r\n";
    match type_at(source, offset_of(source, "global->", 0), SourceLanguage::C) {
        Some(TypeAt::Variable { variable, .. }) => {
            assert_eq!(variable.confidence, Confidence::FileScope);
            assert_eq!(variable.ty.core_name.as_deref(), Some("jp"));
        }
        other => panic!("{other:#?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Resolution through the index

#[derive(Default)]
struct MemorySource {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemorySource {
    fn with(files: &[(&str, &str)]) -> Self {
        let source = Self::default();
        for (path, contents) in files {
            source
                .files
                .lock()
                .insert((*path).to_owned(), contents.as_bytes().to_vec());
        }
        source
    }

    fn contents(&self, path: &str) -> Vec<u8> {
        self.files.lock().get(path).cloned().unwrap_or_default()
    }
}

impl FileSource for MemorySource {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.files
            .lock()
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    index: SymbolIndex,
    source: MemorySource,
}

fn fixture(files: &[(&str, &str)]) -> Fixture {
    let directory = tempfile::tempdir().expect("tempdir");
    let index = SymbolIndex::open(
        directory.path(),
        IndexConfig {
            parallelism: Some(2),
            ..IndexConfig::default()
        },
    )
    .expect("open");
    let source = MemorySource::with(files);
    let entries: Vec<FileEntry> = source
        .files
        .lock()
        .iter()
        .map(|(path, contents)| FileEntry {
            path: path.as_str().into(),
            mtime: Some(UNIX_EPOCH + Duration::from_secs(1000)),
            size: contents.len() as u64,
        })
        .collect();
    index
        .update(&entries, &source, &|_| {}, &AtomicBool::new(false))
        .expect("update");
    Fixture {
        _directory: directory,
        index,
        source,
    }
}

impl Fixture {
    fn resolver(&self) -> TypeResolver<'_> {
        TypeResolver::new(&self.index, &self.source)
    }

    fn near(&self, path: &str) -> NearFile {
        NearFile::new(path, &self.source.contents(path))
    }
}

fn paths(resolved: &[ResolvedType]) -> Vec<&str> {
    resolved
        .iter()
        .map(|candidate| candidate.path.as_ref())
        .collect()
}

#[test]
fn same_name_candidates_are_ranked_by_proximity() {
    let fixture = fixture(&[
        ("include/a/types.h", "struct foo { int from_a; };\n"),
        ("include/b/types.h", "struct foo { int from_b; };\n"),
        (
            "src/main.c",
            "#include \"b/types.h\"\nstruct baz { int in_main; };\nstruct foo *p;\n",
        ),
        ("src/local.h", "struct bar { int local; };\n"),
        ("lib/bar.h", "struct bar { int other; };\n"),
        ("lib/baz.h", "struct baz { int other; };\n"),
    ]);
    let resolver = fixture.resolver();
    let near = fixture.near("src/main.c");
    let foo = resolver.resolve_type("foo", Some(TagKind::Struct), &near);
    assert_eq!(paths(&foo), vec!["include/b/types.h", "include/a/types.h"]);
    assert_eq!(foo[0].proximity, Proximity::IncludedHeader);
    assert_eq!(foo[1].proximity, Proximity::Other);
    assert_eq!(foo[0].row(), 0);
    let bar = resolver.resolve_type("bar", Some(TagKind::Struct), &near);
    assert_eq!(paths(&bar), vec!["src/local.h", "lib/bar.h"]);
    assert_eq!(bar[0].proximity, Proximity::SameDirectory);
    let baz = resolver.resolve_type("baz", Some(TagKind::Struct), &near);
    assert_eq!(paths(&baz), vec!["src/main.c", "lib/baz.h"]);
    assert_eq!(baz[0].proximity, Proximity::SameFile);
    // Reading only the best candidate reads the closest one first.
    let (best, candidates) = resolver.resolve_best("foo", Some(TagKind::Struct), &near);
    assert_eq!(
        best.map(|candidate| candidate.path.to_string()),
        Some("include/b/types.h".to_owned())
    );
    assert_eq!(candidates, 2);
    assert!(resolver.resolve_type("nothing", None, &near).is_empty());
    // A union of the same name is not a struct.
    assert!(
        resolver
            .resolve_type("foo", Some(TagKind::Union), &near)
            .is_empty()
    );
}

#[test]
fn typedef_chains_cycles_and_missing_targets() {
    let fixture = fixture(&[
        (
            "types.h",
            "struct foo { int value; };\n\
             typedef struct foo foo_t;\n\
             typedef foo_t alias_t;\n\
             typedef struct missing missing_t;\n\
             typedef loop_b loop_a;\n\
             typedef loop_a loop_b;\n\
             typedef unsigned int u32;\n\
             struct thing;\n\
             typedef struct thing thing_t;\n",
        ),
        ("thing.c", "struct thing { int x; };\n"),
        (
            "cfg.h",
            "#ifdef CONFIG_A\nstruct dual { int a; };\n#else\nstruct dual { int b; };\n#endif\n",
        ),
    ]);
    let resolver = fixture.resolver();
    let near = fixture.near("types.h");
    let alias = resolver.resolve_type("alias_t", None, &near);
    assert_eq!(alias.len(), 1, "{alias:#?}");
    assert_eq!(
        alias[0]
            .typedef_chain
            .iter()
            .map(|step| step.name.as_str())
            .collect::<Vec<_>>(),
        vec!["alias_t", "foo_t"]
    );
    assert_eq!(alias[0].typedef_chain[1].target.text, "struct foo");
    assert_eq!(alias[0].layout.display_name(), "struct foo");
    assert_eq!(alias[0].proximity, Proximity::SameFile);

    let missing = resolver.resolve_type("missing_t", None, &near);
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].layout.kind, TypeKind::Typedef);
    assert!(missing[0].typedef_chain.is_empty());

    let looped = resolver.resolve_type("loop_a", None, &near);
    assert!(!looped.is_empty());
    assert!(
        looped
            .iter()
            .all(|candidate| candidate.layout.kind == TypeKind::Typedef)
    );

    let u32_type = resolver.resolve_type("u32", None, &near);
    assert_eq!(
        u32_type[0]
            .layout
            .alias
            .as_ref()
            .map(|alias| alias.text.as_str()),
        Some("unsigned int")
    );

    // The forward declaration is not a definition; the body in another file is found.
    let thing = resolver.resolve_type("thing_t", None, &near);
    assert_eq!(paths(&thing), vec!["thing.c"]);
    assert_eq!(names(&thing[0].layout), vec![Some("x")]);

    let dual = resolver.resolve_type("dual", Some(TagKind::Struct), &near);
    assert_eq!(
        dual.iter()
            .map(|candidate| candidate.condition.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("CONFIG_A"), Some("!CONFIG_A")]
    );
}

const CHAIN_HEADER: &str = "struct baz { union { int z; }; };\n\
                            struct bar { struct baz inner; int y; };\n\
                            struct foo { int x; struct bar *b; };\n\
                            struct foo *make_foo(void);\n\
                            extern struct foo *global_ptr;\n\
                            #define WRAP(x) \\\r\n\
                            \t(x)\r\n";

const CHAIN_SOURCE: &str = "#include \"chain.h\"\n\
                            int use(struct foo *param)\n\
                            {\n\
                            \tparam->b->inner.z = 1;\n\
                            \tparam->b->missing.q = 2;\n\
                            \tmake_foo()->x = 3;\n\
                            \tglobal_ptr->b = 0;\n\
                            \treturn WRAP(param->x);\n\
                            }\n";

fn chain_fixture() -> Fixture {
    fixture(&[("chain.h", CHAIN_HEADER), ("use.c", CHAIN_SOURCE)])
}

fn hover(fixture: &Fixture, needle: &str) -> TypeHover {
    fixture
        .resolver()
        .describe_at(
            "use.c",
            CHAIN_SOURCE.as_bytes(),
            offset_of(CHAIN_SOURCE, needle, 0),
            SourceLanguage::C,
        )
        .unwrap_or_else(|| panic!("nothing at {needle}"))
}

fn chain_of(hover: &TypeHover) -> &MemberChain {
    match &hover.subject {
        HoverSubject::Member(chain) => chain,
        other => panic!("{other:#?}"),
    }
}

#[test]
fn member_chains_follow_the_index() {
    let fixture = chain_fixture();
    let result = hover(&fixture, "z = 1");
    let chain = chain_of(&result);
    assert!(chain.complete);
    assert_eq!(
        (chain.root_origin, chain.root_name.as_deref()),
        (ChainRootOrigin::InFile, Some("param"))
    );
    assert_eq!(
        chain
            .steps
            .iter()
            .map(|step| step.owner.as_ref().map(|owner| owner.display_name.as_str()))
            .collect::<Vec<_>>(),
        vec![Some("struct foo"), Some("struct bar"), Some("struct baz")]
    );
    assert_eq!(
        chain.final_member().map(|member| member.text.as_str()),
        Some("int z")
    );
    assert!(result.types.is_empty(), "int has no definition");

    let result = hover(&fixture, "q = 2");
    let chain = chain_of(&result);
    assert!(!chain.complete);
    assert_eq!(chain.steps.len(), 2);
    assert_eq!(chain.steps[1].name, "missing");
    assert!(chain.steps[1].member.is_none());
    assert!(chain.final_member().is_none());

    let result = hover(&fixture, "b->inner");
    assert_eq!(
        result
            .types
            .first()
            .map(|candidate| candidate.layout.display_name()),
        Some("struct bar".to_owned())
    );

    let result = hover(&fixture, "x = 3");
    let chain = chain_of(&result);
    assert_eq!(
        chain.root_type.as_ref().map(|ty| ty.text.as_str()),
        Some("struct foo *")
    );
    assert!(chain.complete);
    assert_eq!(
        (chain.root_origin, chain.root_name.as_deref()),
        (ChainRootOrigin::Call, Some("make_foo"))
    );

    let result = hover(&fixture, "b = 0");
    let chain = chain_of(&result);
    assert!(chain.complete);
    assert_eq!(
        (chain.root_origin, chain.root_name.as_deref()),
        (ChainRootOrigin::Global, Some("global_ptr"))
    );
    assert_eq!(
        chain.final_member().map(|member| member.text.as_str()),
        Some("struct bar *b")
    );
}

#[test]
fn large_files_are_parsed_around_the_cursor() {
    let padding = format!("/* {} */\n", "x".repeat(WINDOWED_PARSE_BYTES));
    let source = format!("{padding}{CHAIN_SOURCE}");
    let fixture = fixture(&[("chain.h", CHAIN_HEADER), ("big.c", source.as_str())]);
    let describe = |needle: &str| {
        fixture
            .resolver()
            .describe_at(
                "big.c",
                source.as_bytes(),
                offset_of(&source, needle, 0),
                SourceLanguage::C,
            )
            .unwrap_or_else(|| panic!("nothing at {needle}"))
    };
    let chain = describe("z = 1");
    assert!(chain_of(&chain).complete);
    // The global is declared in the header and found through the index.
    let global = describe("b = 0");
    assert_eq!(
        chain_of(&global)
            .final_member()
            .map(|member| member.text.as_str()),
        Some("struct bar *b")
    );
}

#[test]
fn macros_and_variables_through_the_index() {
    let chain = chain_fixture();
    let result = hover(&chain, "WRAP");
    match &result.subject {
        HoverSubject::Macro(macros) => {
            assert_eq!(macros.len(), 1);
            assert_eq!(macros[0].text, "#define WRAP(x) \\\n\t(x)");
            assert!(macros[0].is_function_like);
            assert!(!macros[0].truncated);
        }
        other => panic!("{other:#?}"),
    }
    let result = hover(&chain, "param)");
    match &result.subject {
        HoverSubject::Variable(variable) => {
            assert_eq!(variable.confidence, Confidence::Declaration)
        }
        other => panic!("{other:#?}"),
    }
    assert_eq!(paths(&result.types), vec!["chain.h"]);

    let long_macro = format!(
        "#define LONG \\\n{}\tlast\n",
        "\tline \\\n".repeat(MAX_MACRO_LINES + 3)
    );
    let macro_fixture = fixture(&[("macro.h", long_macro.as_str())]);
    let macros = macro_fixture.resolver().macro_definitions("LONG");
    assert_eq!(macros[0].text.lines().count(), MAX_MACRO_LINES);
    assert!(macros[0].truncated);
}

const TREE_HEADER: &str = "struct list_head { struct list_head *next, *prev; };\n\
                           typedef unsigned int u32;\n\
                           struct inner { u32 id; char name[8]; };\n\
                           struct outer {\n\
                           \tstruct inner first;    /* embedded */\n\
                           \tstruct inner *second;\n\
                           \tstruct list_head link;\n\
                           \tunion {\n\
                           \t\tint i;\n\
                           \t\tfloat f;\n\
                           \t} u;\n\
                           #ifdef CONFIG_DEBUG\n\
                           \tint debug;\n\
                           #endif\n\
                           \tstruct outer *parent;\n\
                           \tstruct missing *gone;\n\
                           \tDECLARE_BITMAP(bits, 8);\n\
                           };\n\
                           enum state { IDLE, BUSY = 4 };\n\
                           struct with_enum { enum state state; };\n";

fn tree_of(options: ExpandOptions, name: &str) -> TypeTree {
    let fixture = fixture(&[("tree.h", TREE_HEADER)]);
    let resolver = fixture.resolver();
    let root = resolver
        .resolve_type(name, Some(TagKind::Struct), &fixture.near("tree.h"))
        .into_iter()
        .next()
        .expect("root");
    resolver.expand(&root, options)
}

#[test]
fn expanded_tree_as_text() {
    let tree = tree_of(ExpandOptions::default(), "outer");
    assert!(!tree.truncated);
    assert_eq!(
        render_text(&tree, 10_000),
        "struct outer  (tree.h:4)\n\
         \x20 struct inner first  (tree.h:3)  // embedded\n\
         \x20   u32 id  (= unsigned int)\n\
         \x20   char name[8]\n\
         \x20 struct inner *second  (see above)\n\
         \x20 struct list_head link  (tree.h:1)\n\
         \x20   struct list_head *next  (recursive)\n\
         \x20   struct list_head *prev  (recursive)\n\
         \x20 union {...} u\n\
         \x20   int i\n\
         \x20   float f\n\
         \x20 int debug  [#if CONFIG_DEBUG]\n\
         \x20 struct outer *parent  (recursive)\n\
         \x20 struct missing *gone  (not found)\n\
         \x20 DECLARE_BITMAP(bits, 8)  (unparsed)\n"
    );
    let tree = tree_of(ExpandOptions::default(), "with_enum");
    assert_eq!(
        render_text(&tree, 10_000),
        "struct with_enum  (tree.h:20)\n\
         \x20 enum state state  (tree.h:19)\n\
         \x20   IDLE  (= 0)\n\
         \x20   BUSY = 4\n"
    );
}

#[test]
fn expansion_limits() {
    let shallow = tree_of(
        ExpandOptions {
            max_depth: 1,
            ..ExpandOptions::default()
        },
        "outer",
    );
    let text = render_text(&shallow, 10_000);
    assert!(text.contains("\n    u32 id\n"), "{text}");
    assert!(text.contains("\n    struct list_head *next\n"), "{text}");

    let no_pointers = tree_of(
        ExpandOptions {
            max_pointer_hops: 0,
            ..ExpandOptions::default()
        },
        "outer",
    );
    let text = render_text(&no_pointers, 10_000);
    assert!(text.contains("\n  struct inner *second\n"), "{text}");
    assert!(text.contains("\n  struct outer *parent\n"), "{text}");

    let limited = tree_of(
        ExpandOptions {
            max_nodes: 3,
            ..ExpandOptions::default()
        },
        "outer",
    );
    assert!(limited.truncated);
    let text = render_text(&limited, 10_000);
    assert!(text.ends_with("  ... (more members not shown)\n"), "{text}");

    let full = tree_of(ExpandOptions::default(), "outer");
    let cut = render_text(&full, 120);
    assert!(cut.chars().count() <= 120, "{cut}");
    assert!(cut.ends_with("... (truncated)\n"), "{cut}");
    assert!(cut.starts_with("struct outer  (tree.h:4)\n"));
}

#[test]
fn one_level_member_targets() {
    let fixture = fixture(&[("tree.h", TREE_HEADER)]);
    let resolver = fixture.resolver();
    let outer = resolver
        .resolve_type("outer", Some(TagKind::Struct), &fixture.near("tree.h"))
        .into_iter()
        .next()
        .expect("outer");
    let path = outer.path.clone();
    match resolver.member_target(member(&outer.layout, "second"), &path) {
        MemberTarget::Resolved(types) => {
            assert_eq!(types[0].layout.display_name(), "struct inner")
        }
        other => panic!("{other:#?}"),
    }
    assert!(matches!(
        resolver.member_target(member(&outer.layout, "u"), &path),
        MemberTarget::Inline(layout) if layout.kind == TypeKind::Union
    ));
    assert_eq!(
        resolver.member_target(member(&outer.layout, "debug"), &path),
        MemberTarget::NotExpandable
    );
}

/// Measures lookups on a copy of a real index. Run with
/// `TYPE_LAYOUT_INDEX=<index dir> TYPE_LAYOUT_TREE=<source root> TYPE_LAYOUT_NAMES=a,b
/// cargo test --release -p symbol_index measure_type_lookups -- --ignored --nocapture`.
#[test]
#[ignore]
fn measure_type_lookups() {
    // `TYPE_LAYOUT_TYPE_AT_FILE=<file>` times finding the name under the cursor, which parses
    // the whole file.
    if let Ok(file) = std::env::var("TYPE_LAYOUT_TYPE_AT_FILE")
        && let Ok(contents) = std::fs::read(&file)
    {
        let text = String::from_utf8_lossy(&contents).into_owned();
        for step in 1..=5 {
            let offset = text.len() * step / 6;
            let offset = (offset..text.len())
                .find(|&offset| text.is_char_boundary(offset))
                .unwrap_or(0);
            let started = Instant::now();
            let found = type_at(&text, offset, SourceLanguage::C);
            println!(
                "type_at {} bytes at {offset}: {:?} ({})",
                text.len(),
                started.elapsed(),
                found.is_some()
            );
            let started = Instant::now();
            let windowed = type_at_in_window(
                &text,
                offset,
                SourceLanguage::C,
                offset.saturating_sub(4000)..offset + 4000,
            );
            println!(
                "  in an 8000-byte window: {:?} ({})",
                started.elapsed(),
                windowed.is_some()
            );
        }
    }
    let (Ok(index_directory), Ok(tree)) = (
        std::env::var("TYPE_LAYOUT_INDEX"),
        std::env::var("TYPE_LAYOUT_TREE"),
    ) else {
        return;
    };
    let index = SymbolIndex::open(
        std::path::Path::new(&index_directory),
        IndexConfig {
            verify_checksums: false,
            ..IndexConfig::default()
        },
    )
    .expect("open");
    println!(
        "index: {:?}, reset: {:?}",
        index.stats(),
        index.reset_reason()
    );
    let files = DirectorySource::new(tree);
    let names = std::env::var("TYPE_LAYOUT_NAMES").unwrap_or_default();
    for name in names.split(',').filter(|name| !name.is_empty()) {
        for definition in index.definitions(name).iter().take(3) {
            let parsed = files.read(&definition.path).ok().and_then(|contents| {
                parse_type_definition_bytes(
                    &contents,
                    TypeLocator::Range(definition.item_range),
                    definition.language,
                )
            });
            println!(
                "  {:?} {}:{} -> {:?}",
                definition.kind,
                definition.path,
                definition.item_range.start.row + 1,
                parsed.map(|layout| (layout.display_name(), layout.members.len()))
            );
        }
        for round in 0..3 {
            let started = Instant::now();
            let resolver = TypeResolver::new(&index, &files);
            let resolved = resolver.resolve_type(name, None, &NearFile::default());
            let resolve_time = started.elapsed();
            let expand_started = Instant::now();
            let text = resolved
                .first()
                .map(|root| render_text(&resolver.expand(root, ExpandOptions::default()), 4000));
            println!(
                "{name} round {round}: {} definitions in the index, {} candidates, resolve {:?}, expand+render {:?}, first {:?} ({} members)",
                index.definitions(name).len(),
                resolved.len(),
                resolve_time,
                expand_started.elapsed(),
                resolved.first().map(|candidate| candidate.path.to_string()),
                resolved
                    .first()
                    .map_or(0, |candidate| candidate.layout.members.len()),
            );
            if round == 0
                && let Some(text) = text
            {
                println!("{}", text.lines().take(12).collect::<Vec<_>>().join("\n"));
            }
        }
    }
}

/// Parses every struct and typedef in a directory of headers (such as the Windows SDK) to check
/// that nothing panics, and reports the time. Run with
/// `TYPE_LAYOUT_HEADERS=<dir> cargo test --release -p symbol_index parse_every_header -- --ignored --nocapture`.
#[test]
#[ignore]
fn parse_every_header() {
    let Ok(root) = std::env::var("TYPE_LAYOUT_HEADERS") else {
        return;
    };
    let entries = scan_directory(std::path::Path::new(&root)).expect("scan");
    let started = Instant::now();
    let mut definitions = 0usize;
    let mut members = 0usize;
    let mut unparsed = 0usize;
    let mut slowest = (Duration::ZERO, String::new());
    for entry in &entries {
        let Ok(contents) = std::fs::read(std::path::Path::new(&root).join(entry.path.as_ref()))
        else {
            continue;
        };
        let offsets: Vec<usize> = contents
            .windows(8)
            .enumerate()
            .filter(|(_, window)| window.starts_with(b"struct ") || *window == b"typedef ")
            .map(|(offset, _)| offset)
            .collect();
        for offset in offsets {
            let Ok(byte) = u32::try_from(offset) else {
                continue;
            };
            let parse_started = Instant::now();
            let layout = parse_type_definition_bytes(
                &contents,
                TypeLocator::Range(Range {
                    start: Position {
                        byte,
                        row: 0,
                        column: u32::MAX,
                    },
                    end: Position {
                        byte,
                        row: 0,
                        column: 0,
                    },
                }),
                SourceLanguage::C,
            );
            let elapsed = parse_started.elapsed();
            if elapsed > slowest.0 {
                slowest = (elapsed, format!("{}@{offset}", entry.path));
            }
            if let Some(layout) = layout {
                definitions += 1;
                members += layout.members.len();
                for member in &layout.members {
                    if member.kind == MemberKind::Unparsed {
                        unparsed += 1;
                        if unparsed % 5000 == 1 {
                            println!(
                                "unparsed: {} ({}:{} in {} starting at row {})",
                                member.text,
                                entry.path,
                                member.range.start.row + 1,
                                layout.display_name(),
                                layout.range.start.row + 1
                            );
                        }
                    }
                }
            }
        }
    }
    println!(
        "{} files, {definitions} definitions, {members} members ({unparsed} unparsed) in {:?}; slowest {:?} at {}",
        entries.len(),
        started.elapsed(),
        slowest.0,
        slowest.1
    );
}

#[test]
fn a_range_that_no_longer_holds_the_type_falls_back_to_a_search_by_name() {
    let fixture = fixture(&[
        (
            "stale.h",
            "struct stale { int a; };\ntypedef struct stale stale_t;\n",
        ),
        ("other.c", "struct unrelated { int x; };\n"),
    ]);
    // The file changed after it was indexed: the recorded ranges now point elsewhere.
    let edited = "/* a comment added above\n * the definitions */\nstruct stale { int a; int b; };\ntypedef struct stale stale_t;\n";
    fixture
        .source
        .files
        .lock()
        .insert("stale.h".to_owned(), edited.as_bytes().to_vec());
    let resolver = fixture.resolver();
    let near = NearFile::path_only("other.c");
    let resolved = resolver.resolve_type("stale", Some(TagKind::Struct), &near);
    assert_eq!(resolved.len(), 1, "{resolved:#?}");
    assert_eq!(names(&resolved[0].layout), vec![Some("a"), Some("b")]);
    assert_eq!(
        resolved[0].layout.range.start,
        position_of(edited, "struct stale", 0)
    );
    let through_typedef = resolver.resolve_type("stale_t", None, &near);
    assert_eq!(
        through_typedef
            .first()
            .map(|candidate| candidate.layout.members.len()),
        Some(2)
    );

    // A type the index has never seen is still found in the requesting file.
    let with_new_type = format!("{edited}struct added {{ int z; }};\n");
    fixture
        .source
        .files
        .lock()
        .insert("stale.h".to_owned(), with_new_type.into_bytes());
    let resolver = fixture.resolver();
    assert!(
        resolver
            .resolve_type("added", Some(TagKind::Struct), &near)
            .is_empty()
    );
    let added = resolver.resolve_type(
        "added",
        Some(TagKind::Struct),
        &NearFile::path_only("stale.h"),
    );
    assert_eq!(
        added.first().map(|candidate| names(&candidate.layout)),
        Some(vec![Some("z")])
    );
}

// ---------------------------------------------------------------------------------------------
// Hostile inputs

/// Runs `work` on a thread with a small stack and fails if it does not finish in time, so that
/// a hang or a quadratic path shows up as a failure rather than a test that never ends.
fn finishes_in_time<T: Send + 'static>(
    label: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let started = Instant::now();
    std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            sender.send(work()).ok();
        })
        .expect("spawn");
    let result = receiver.recv_timeout(Duration::from_secs(10)).ok();
    eprintln!("{label}: {:?}", started.elapsed());
    result
}

fn hostile_sources() -> Vec<(&'static str, SourceLanguage, String)> {
    let mut sources = Vec::new();
    sources.push((
        "open brackets",
        SourceLanguage::C,
        format!("struct s {{\n\tint a;\n{}\n}};\n", "[".repeat(200_000)),
    ));
    sources.push((
        "open attributes",
        SourceLanguage::C,
        format!(
            "struct s {{\n\tint a;\n{}\n}};\n",
            "__attribute__(".repeat(60_000)
        ),
    ));
    sources.push((
        "unterminated typedefs",
        SourceLanguage::C,
        "typedef s ".repeat(100_000),
    ));
    sources.push((
        "unclosed typedef bodies",
        SourceLanguage::C,
        "typedef struct { int a; ".repeat(20_000),
    ));
    let mut nested = String::from("struct s {\n");
    for _ in 0..11 {
        nested.push_str("struct {\n");
    }
    nested.push_str("int leaf;\n");
    for _ in 0..11 {
        nested.push_str("} a0, a1, a2, a3, a4, a5, a6, a7;\n");
    }
    nested.push_str("};\n");
    sources.push((
        "declarators sharing a nested body",
        SourceLanguage::C,
        nested,
    ));
    sources.push((
        "deep conditions",
        SourceLanguage::C,
        format!(
            "struct s {{\n{}{}}};\n",
            "#if CONFIG_SOMETHING_LONG\n".repeat(20_000),
            "\tint member;\n".repeat(4_000)
        ),
    ));
    sources.push((
        "empty bodies",
        SourceLanguage::Cpp,
        format!("class s {{ int x {} }};\n", "{}".repeat(100_000)),
    ));
    sources.push((
        "open angle brackets",
        SourceLanguage::Cpp,
        format!("class s {{ a{} }};\n", "<b".repeat(100_000)),
    ));
    sources.push((
        "open parentheses",
        SourceLanguage::C,
        format!("struct s {{ int a{} }};\n", "(".repeat(200_000)),
    ));
    sources.push((
        "deep blocks",
        SourceLanguage::C,
        format!(
            "struct s {{ int x; }};\nint f(struct s *p) {{\n{}p->x;\n{}}}\n",
            "{".repeat(20_000),
            "}".repeat(20_000)
        ),
    ));
    sources
}

#[test]
fn hostile_inputs_finish_in_bounded_time() {
    let mut slow = Vec::new();
    for (label, language, source) in hostile_sources() {
        let source = std::sync::Arc::new(source);
        {
            let source = source.clone();
            let label = format!("{label}: by name");
            finishes_in_time(&label, move || {
                parse_type_definition(
                    &source,
                    TypeLocator::Name {
                        name: "s",
                        tag: None,
                    },
                    language,
                )
                .map(|layout| layout.members.len())
            })
            .unwrap_or_else(|| {
                slow.push(label.clone());
                None
            });
        }
        {
            let source = source.clone();
            let label = format!("{label}: by range");
            finishes_in_time(&label, move || {
                parse_type_definition(&source, TypeLocator::Range(Range::default()), language)
                    .map(|layout| layout.members.len())
            })
            .unwrap_or_else(|| {
                slow.push(label.clone());
                None
            });
        }
        {
            let source = source.clone();
            let label = format!("{label}: type_at");
            finishes_in_time(&label, move || {
                let offset = source.find("p->x").unwrap_or(source.len() / 2);
                type_at(&source, offset, language).is_some()
            })
            .unwrap_or_else(|| {
                slow.push(label.clone());
                false
            });
        }
    }
    assert!(slow.is_empty(), "too slow: {slow:#?}");
}

fn nodes_in(layout: &TypeLayout) -> usize {
    layout.enumerators.len()
        + layout
            .members
            .iter()
            .map(|member| 1 + member.nested.as_deref().map_or(0, nodes_in))
            .sum::<usize>()
}

#[test]
fn copies_of_a_shared_nested_body_count_against_the_member_limit() {
    let mut source = String::from("struct s {\n");
    for _ in 0..8 {
        source.push_str("struct {\n");
    }
    source.push_str("int leaf;\n");
    for _ in 0..8 {
        source.push_str("} a0, a1, a2, a3, a4, a5, a6, a7;\n");
    }
    source.push_str("};\n");
    let layout = parse_c(&source, "s", None);
    // Without the limit this would be 8^8 copies of `leaf`.
    assert!(
        nodes_in(&layout) <= 2 * MAX_MEMBERS,
        "{}",
        nodes_in(&layout)
    );
    assert!(layout.truncated);
    // Small shared bodies are still copied to every declarator.
    let layout = parse_c("struct t { struct { int x; int y; } a, b, c; };", "t", None);
    assert!(!layout.truncated);
    for name in ["a", "b", "c"] {
        let nested = member(&layout, name)
            .nested
            .as_deref()
            .expect("nested body");
        assert_eq!(names(nested), vec![Some("x"), Some("y")]);
    }
}

#[test]
fn long_and_deep_conditions_are_cut() {
    let source = format!(
        "struct s {{\n{}\tint member;\n}};\n",
        "#if CONFIG_SOMETHING_LONG\n".repeat(1_000)
    );
    let layout = parse_c(&source, "s", None);
    let condition = member(&layout, "member")
        .condition
        .clone()
        .expect("condition");
    assert!(
        condition.chars().count() <= MAX_CONDITION_CHARS + 1,
        "{condition}"
    );
    assert!(condition.ends_with('\u{2026}'));
    assert!(condition.starts_with("CONFIG_SOMETHING_LONG && CONFIG_SOMETHING_LONG"));
    // Multi-byte text is cut on a character boundary.
    let source = format!("#if {}\nint here;\n#endif\n", "\u{3042}".repeat(400));
    let condition =
        enclosing_condition(source.as_bytes(), offset_of(&source, "here", 0)).expect("condition");
    assert!(condition.ends_with('\u{2026}'));
    assert!(condition.len() <= MAX_CONDITION_CHARS + '\u{2026}'.len_utf8());
}

/// Random edits of real-looking C: truncation, flipped bytes, removed brackets and inserted
/// syntax. Nothing may panic, loop forever or overflow a small stack.
#[test]
fn mutated_sources_do_not_panic_or_hang() {
    let base = format!("{USES}{CHAIN_HEADER}{TREE_HEADER}{CHAIN_SOURCE}");
    let base = format!(
        "{base}typedef struct DECLSPEC_ALIGN(16) _SAMPLE {{\n\
         \tunion {{ struct {{ DWORD Low; LONG High; }} DUMMYSTRUCTNAME; ULONGLONG Quad; }} DUMMYUNIONNAME;\n\
         #ifdef _WIN64\n\
         \tint (WINAPI *callback)(_In_ PVOID context, _Out_writes_(n) char *buffer); // \u{65e5}\u{672c}\n\
         #elif defined(X) /* c */ \\\r\n && Y\n\
         \tenum {{ A = 1, B, }} kind : 3;\n\
         #endif\n\
         }} SAMPLE, *PSAMPLE;\n\
         class Widget : public Base<int> {{ public: int size() const {{ return 1; }} }};\n"
    );
    let names = [
        "foo",
        "bar",
        "outer",
        "SAMPLE",
        "_SAMPLE",
        "list_head",
        "Widget",
        "u32",
    ];
    finishes_in_time("mutated sources", move || {
        let mut random: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move || {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random
        };
        let inserted: [&[u8]; 16] = [
            b"{",
            b"}",
            b"(",
            b")",
            b"[",
            b"<",
            b"#if A\n",
            b"#endif\n",
            b"\\\r\n",
            b"\"",
            b"'",
            b"/*",
            b"*/",
            b"struct ",
            b"typedef ",
            b"\xe3\x81",
        ];
        for round in 0..400 {
            let mut source = base.as_bytes().to_vec();
            match round % 4 {
                0 => source.truncate((next() as usize) % (source.len() + 1)),
                1 => {
                    for _ in 0..(1 + next() % 8) {
                        let index = (next() as usize) % source.len();
                        source[index] ^= 1 << (next() % 8);
                    }
                }
                2 => source.retain(|&byte| {
                    !(matches!(byte, b'{' | b'}' | b'(' | b')' | b';') && next() % 3 == 0)
                }),
                _ => {
                    for _ in 0..(1 + next() % 8) {
                        let index = (next() as usize) % (source.len() + 1);
                        let piece = inserted[(next() as usize) % inserted.len()];
                        source.splice(index..index, piece.iter().copied());
                    }
                }
            }
            let language = if round % 5 == 0 {
                SourceLanguage::Cpp
            } else {
                SourceLanguage::C
            };
            for name in names {
                parse_type_definition_bytes(
                    &source,
                    TypeLocator::Name { name, tag: None },
                    language,
                );
            }
            for _ in 0..4 {
                let byte = (next() as usize % (source.len() + 1)) as u32;
                let range = Range {
                    start: Position {
                        byte,
                        row: next() as u32 % 50,
                        column: next() as u32 % 200,
                    },
                    end: Position {
                        byte: byte.saturating_add(next() as u32 % 400),
                        row: 0,
                        column: 0,
                    },
                };
                if let Some(layout) =
                    parse_type_definition_bytes(&source, TypeLocator::Range(range), language)
                {
                    assert!(layout.range.start.byte as usize <= source.len());
                    assert!(layout.range.end.byte as usize <= source.len());
                }
                let offset = next() as usize % (source.len() + 1);
                crate::type_layout::type_at_bytes(&source, offset, language, None);
                crate::type_layout::type_at_bytes(
                    &source,
                    offset,
                    language,
                    Some(offset.saturating_sub(300)..offset + 300),
                );
                enclosing_condition(&source, offset);
            }
            let text = String::from_utf8_lossy(&source);
            parse_type_text(&text, language);
            parse_declaration_text(&text, language);
            include_paths(&source);
        }
        // Uniform noise.
        for length in [0usize, 1, 2, 3, 17, 1000, 100_000] {
            let noise: Vec<u8> = (0..length).map(|_| next() as u8).collect();
            for language in [SourceLanguage::C, SourceLanguage::Cpp] {
                parse_type_definition_bytes(
                    &noise,
                    TypeLocator::Name {
                        name: "a",
                        tag: None,
                    },
                    language,
                );
                parse_type_definition_bytes(&noise, TypeLocator::Range(Range::default()), language);
                crate::type_layout::type_at_bytes(&noise, length / 2, language, None);
            }
        }
    });
}

#[test]
fn declarator_shapes_comments_and_conditions() {
    let source = "struct probe {\n\
        \tint a, *b, c[4];\n\
        \tstruct foo *(*get)(void);\n\
        \tint (*arr[4])(int);\n\
        \tconst char *const names[];\n\
        \tunsigned long long x : 3;\n\
        \tstruct { int x; } s1, s2;\n\
        \tenum { A, B } mode;\n\
        \tstruct list_head list __attribute__((aligned(8)));\n\
        \t__u8 data[] __counted_by(len);\n\
        \tunion { struct { int ua, ub; }; long raw; };\n\
        \t/* before */\n\
        \tint commented; // after\n\
        \tint plain;\n\
        \tint next_one; /* trailing */\n\
        #if A\n\
        \tint in_a;\n\
        #elif B\n\
        #if C\n\
        \tint in_b_c;\n\
        #endif\n\
        #else\n\
        \tint in_else;\n\
        #endif\n\
        };\n\
        typedef int (*handler_t)(int);\n\
        typedef struct a *a_ptr, a_val;\n";
    let layout = parse_c(source, "probe", None);
    let summary = |name: &str| {
        let member = member(&layout, name);
        (
            member.kind,
            member.text.clone(),
            member.ty.core_name.clone(),
            member.ty.pointer_depth,
            member.ty.array_dimensions.clone(),
        )
    };
    let text = |value: &str| value.to_owned();
    assert_eq!(
        summary("get"),
        (
            MemberKind::FunctionPointer,
            text("struct foo *(*get)(void)"),
            None,
            1,
            vec![]
        )
    );
    assert_eq!(
        summary("arr"),
        (
            MemberKind::FunctionPointer,
            text("int (*arr[4])(int)"),
            None,
            1,
            vec![text("4")]
        )
    );
    assert_eq!(
        summary("names"),
        (
            MemberKind::Field,
            text("const char *const names[]"),
            Some(text("char")),
            1,
            vec![text("")]
        )
    );
    assert_eq!(
        summary("list"),
        (
            MemberKind::Field,
            text("struct list_head list"),
            Some(text("list_head")),
            0,
            vec![]
        )
    );
    assert_eq!(
        summary("data"),
        (
            MemberKind::Field,
            text("__u8 data[]"),
            Some(text("__u8")),
            0,
            vec![text("")]
        )
    );
    assert_eq!(member(&layout, "x").bit_width.as_deref(), Some("3"));
    for name in ["s1", "s2"] {
        assert_eq!(
            member(&layout, name).nested.as_deref().map(names),
            Some(vec![Some("x")])
        );
    }
    let mode = member(&layout, "mode")
        .nested
        .as_deref()
        .expect("enum body");
    assert_eq!(mode.enumerators.len(), 2);
    let anonymous = layout
        .members
        .iter()
        .find(|member| member.kind == MemberKind::AnonymousAggregate)
        .expect("anonymous union");
    assert_eq!(
        layout.find_member("ub").map(|member| member.text.as_str()),
        Some("int ub")
    );
    assert_eq!(
        anonymous
            .nested
            .as_deref()
            .map(|nested| nested.members.len()),
        Some(2)
    );
    // A comment at the end of one member's line is not the next member's comment.
    assert_eq!(
        member(&layout, "commented").comment.as_deref(),
        Some("before")
    );
    assert_eq!(member(&layout, "plain").comment, None);
    assert_eq!(
        member(&layout, "next_one").comment.as_deref(),
        Some("trailing")
    );
    assert_eq!(member(&layout, "in_a").condition.as_deref(), Some("A"));
    assert_eq!(
        member(&layout, "in_b_c").condition.as_deref(),
        Some("B && C")
    );
    assert_eq!(
        member(&layout, "in_else").condition.as_deref(),
        Some("!(A || B)")
    );
    let handler = parse_c(source, "handler_t", None).alias.expect("alias");
    assert!(handler.is_function_pointer);
    assert_eq!(handler.text, "int (*)(int)");
}

/// A typedef that defines its type in place is reached through a step naming it, at its own
/// name, as a separate typedef would be.
#[test]
fn a_typedef_defining_its_type_in_place_is_a_step_of_the_chain() {
    let source = "typedef struct a { int x; } a_t;\n\
                  typedef union u { int z; } u_t, *u_p;\n\
                  typedef struct { int y; } anon_t;\n\
                  typedef struct same { int w; } same;\n\
                  typedef a_t b_t;\n";
    let fixture = fixture(&[("t.h", source)]);
    let resolver = fixture.resolver();
    let near = fixture.near("t.h");
    let chain = |name: &str, tag: Option<TagKind>| -> Vec<(String, String, u32, u32)> {
        let resolved = resolver.resolve_type(name, tag, &near);
        assert_eq!(resolved.len(), 1, "{name}: {resolved:#?}");
        resolved[0]
            .typedef_chain
            .iter()
            .map(|step| {
                (
                    step.name.clone(),
                    step.target.text.clone(),
                    step.range.start.row,
                    step.range.start.column,
                )
            })
            .collect()
    };
    assert_eq!(
        chain("a_t", None),
        vec![("a_t".to_string(), "struct a".to_string(), 0, 28)]
    );
    assert_eq!(
        chain("u_p", None),
        vec![("u_p".to_string(), "union u *".to_string(), 1, 33)]
    );
    assert_eq!(
        chain("b_t", None),
        vec![
            ("b_t".to_string(), "a_t".to_string(), 4, 12),
            ("a_t".to_string(), "struct a".to_string(), 0, 28)
        ]
    );
    // Asked for by its tag, or named by nothing else, the type is reached directly.
    assert!(chain("a", Some(TagKind::Struct)).is_empty());
    assert!(chain("anon_t", None).is_empty());
    assert!(chain("same", None).is_empty());
    let resolved = resolver.resolve_type("u_p", None, &near);
    assert_eq!(resolved[0].layout.display_name(), "union u");
    assert_eq!(resolved[0].typedef_chain[0].target.pointer_depth, 1);
}

#[test]
fn every_name_of_a_typedef_gets_its_own_target() {
    let source = "typedef struct a *a_ptr, a_val;\nstruct a { int x; };\n";
    let by_name = parse_c(source, "a_val", None);
    let alias = by_name.alias.expect("alias");
    assert_eq!((alias.text.as_str(), alias.pointer_depth), ("struct a", 0));
    let first = parse_c(source, "a_ptr", None).alias.expect("alias");
    assert_eq!(
        (first.text.as_str(), first.pointer_depth),
        ("struct a *", 1)
    );

    let fixture = fixture(&[("t.h", source)]);
    let resolver = fixture.resolver();
    let resolved = resolver.resolve_type("a_val", None, &fixture.near("t.h"));
    let step = resolved
        .first()
        .and_then(|candidate| candidate.typedef_chain.first())
        .expect("typedef step");
    assert_eq!(step.target.text, "struct a");
    assert_eq!(resolved[0].layout.display_name(), "struct a");
}

#[test]
fn the_hovered_file_is_read_from_the_given_contents() {
    // The buffer has a type that the file on disk (and so the index) does not.
    let fixture = fixture(&[("use.c", "int unrelated;\n")]);
    let buffer = "struct fresh { int x; };\nvoid f(struct fresh *p) { p->x = 1; }\n";
    let hover = fixture
        .resolver()
        .describe_at(
            "use.c",
            buffer.as_bytes(),
            offset_of(buffer, "x = 1", 0),
            SourceLanguage::C,
        )
        .expect("hover");
    let chain = chain_of(&hover);
    assert!(chain.complete, "{chain:#?}");
    assert_eq!(
        chain.final_member().map(|member| member.text.as_str()),
        Some("int x")
    );
}

#[test]
fn type_at_reads_the_same_rewritten_source_as_the_index() {
    for line_end in ["\r\n", "\n"] {
        let sources = [
            // A condition continued on the next line.
            format!(
                "#if defined(A) \\{line_end} && B{line_end}static struct jp *global;{line_end}#endif{line_end}"
            ),
            // A comment over two lines inside a macro, with more of the macro after it.
            format!(
                "#define TWO(x) x /* one{line_end} two */ {{ y{line_end}static struct jp *global;{line_end}"
            ),
            // A member macro without a semicolon before the closing brace.
            format!(
                "struct jp {{{line_end}\tint length;{line_end}\tJP_EXTRA{line_end}}};{line_end}static struct jp *global;{line_end}"
            ),
        ];
        for prefix in sources {
            let source = format!(
                "{prefix}void g(void){line_end}{{{line_end}\tglobal->length = 1;{line_end}}}{line_end}"
            );
            match type_at(
                &source,
                offset_of(&source, "global->", 0),
                SourceLanguage::C,
            ) {
                Some(TypeAt::Variable { variable, .. }) => {
                    assert_eq!(variable.confidence, Confidence::FileScope, "{source:?}");
                    assert_eq!(variable.ty.core_name.as_deref(), Some("jp"));
                }
                other => panic!("{source:?}: {other:#?}"),
            }
        }
    }
}

#[test]
fn large_headers_are_parsed_around_a_declaration_outside_functions() {
    let padding = format!("/* {} */\n", "x".repeat(WINDOWED_PARSE_BYTES));
    let source = format!(
        "{padding}struct inner {{ int id; }};\nstruct big {{\n\tstruct inner member;\n\tint other;\n}};\n"
    );
    let fixture = fixture(&[("big.h", source.as_str())]);
    let hover = fixture
        .resolver()
        .describe_at(
            "big.h",
            source.as_bytes(),
            offset_of(&source, "member", 0),
            SourceLanguage::C,
        )
        .expect("hover");
    match &hover.subject {
        HoverSubject::Variable(variable) => {
            assert_eq!(variable.confidence, Confidence::Declaration);
            assert_eq!(variable.ty.core_name.as_deref(), Some("inner"));
        }
        other => panic!("{other:#?}"),
    }
    assert_eq!(
        hover
            .types
            .first()
            .map(|candidate| candidate.layout.display_name()),
        Some("struct inner".to_owned())
    );
}

#[test]
fn a_path_filter_hides_definitions_from_every_lookup() {
    let fixture = fixture(&[
        (
            "pub/types.h",
            "struct foo { int shown; };\ntypedef struct hidden_target alias_t;\n",
        ),
        (
            "secret/types.h",
            "struct foo { int secret; };\nstruct hidden_target { int x; };\n#define SECRET_MACRO 1\nint secret_global;\n",
        ),
        (
            "src/main.c",
            "#include \"types.h\"\nint f(void) { return SECRET_MACRO + secret_global; }\n",
        ),
    ]);
    let filter = |path: &str| !path.starts_with("secret/");
    let near = fixture.near("src/main.c");

    let open = fixture.resolver();
    assert_eq!(
        paths(&open.resolve_type("foo", Some(TagKind::Struct), &near)).len(),
        2
    );
    assert_eq!(
        open.type_candidates("foo", Some(TagKind::Struct), &near)
            .len(),
        2
    );

    let filtered = fixture.resolver().with_path_filter(&filter);
    assert_eq!(
        paths(&filtered.resolve_type("foo", Some(TagKind::Struct), &near)),
        vec!["pub/types.h"]
    );
    assert_eq!(
        filtered
            .type_candidates("foo", Some(TagKind::Struct), &near)
            .iter()
            .map(|definition| definition.path.as_ref())
            .collect::<Vec<_>>(),
        vec!["pub/types.h"],
        "a hidden definition is not even counted"
    );
    assert_eq!(
        filtered.resolve_best("foo", Some(TagKind::Struct), &near).1,
        1
    );
    // The typedef is shown, what it names is not: the typedef stays unresolved.
    let alias = filtered.resolve_type("alias_t", None, &near);
    assert_eq!(paths(&alias), vec!["pub/types.h"]);
    assert!(!alias[0].layout.has_body());
    assert!(filtered.macro_definitions("SECRET_MACRO").is_empty());
    assert!(filtered.global_variable("secret_global", &near).is_none());
    assert!(
        filtered
            .locate_type("secret/types.h", 0, 0, "foo")
            .is_none()
    );
    assert!(open.locate_type("secret/types.h", 0, 0, "foo").is_some());
}

#[test]
fn limited_resolution_reads_only_the_best_candidates() {
    let fixture = fixture(&[
        ("a/types.h", "struct foo { int from_a; };\n"),
        ("b/types.h", "struct foo { int from_b; };\n"),
        ("c/types.h", "struct foo { int from_c; };\n"),
        ("b/main.c", "struct foo *p;\n"),
    ]);
    let near = fixture.near("b/main.c");
    let resolver = fixture.resolver();
    let one = resolver.resolve_type_limited("foo", Some(TagKind::Struct), &near, 1);
    assert_eq!(paths(&one), vec!["b/types.h"]);
    let candidates = resolver.type_candidates("foo", Some(TagKind::Struct), &near);
    assert_eq!(
        candidates
            .first()
            .map(|definition| definition.path.as_ref()),
        Some("b/types.h"),
        "the candidates are in the order they are read"
    );
    assert_eq!(candidates.len(), 3);
    let limited = fixture.resolver().with_candidate_limit(2);
    assert_eq!(
        limited
            .resolve_type("foo", Some(TagKind::Struct), &near)
            .len(),
        2
    );
}

#[test]
fn a_type_is_located_again_by_its_position() {
    let header = "\
/* padding */
typedef struct node {
    int value;
} node_t;
struct other { int x; };
";
    let fixture = fixture(&[("list.h", header)]);
    let resolver = fixture.resolver();
    let node = resolver.locate_type("list.h", 1, 0, "node").expect("node");
    assert_eq!(node.layout.display_name(), "struct node");
    assert_eq!(node.row(), 1);
    let by_typedef = resolver
        .locate_type("list.h", 1, 0, "node_t")
        .expect("node_t");
    assert_eq!(by_typedef.layout.tag_name.as_deref(), Some("node"));
    let other = resolver
        .locate_type("list.h", 4, 0, "other")
        .expect("other");
    assert_eq!(other.layout.display_name(), "struct other");
    // A stale position still finds the name in the same file.
    let moved = resolver
        .locate_type("list.h", 0, 0, "other")
        .expect("found by name");
    assert_eq!(moved.row(), 4);
    assert!(resolver.locate_type("list.h", 99, 0, "nothing").is_none());
    assert!(resolver.locate_type("missing.h", 0, 0, "node").is_none());
}

#[test]
fn a_window_is_described_with_the_file_scope_from_the_file() {
    let file = "\
struct config { int verbose; };
static struct config global_config;
int run(int argc)
{
    struct config *local = &global_config;
    return local->verbose + global_config.verbose;
}
";
    let fixture = fixture(&[("src/run.c", file)]);
    let resolver = fixture.resolver();
    let window_start = file.find("int run").expect("function");
    let window = &file[window_start..];
    let offset = offset_of(window, "local->", 0) + "local->".len();
    let hover = resolver
        .describe_in_window("src/run.c", window.as_bytes(), offset, SourceLanguage::C)
        .expect("hover");
    assert_eq!(hover.name, "verbose");
    assert_eq!(hover.range.start.row, 3, "a row within the window");
    match &hover.subject {
        HoverSubject::Member(chain) => {
            assert!(chain.complete, "{chain:#?}");
            let owner = chain.steps[0].owner.as_ref().expect("owner");
            assert_eq!(owner.display_name, "struct config");
            assert_eq!(
                owner.range.start.row, 0,
                "the owner's row in the file, not the window"
            );
        }
        other => panic!("{other:#?}"),
    }
    let global_offset = offset_of(window, "global_config.verbose", 0);
    let hover = resolver
        .describe_in_window(
            "src/run.c",
            window.as_bytes(),
            global_offset,
            SourceLanguage::C,
        )
        .expect("hover");
    match &hover.subject {
        HoverSubject::Variable(variable) => {
            assert_eq!(variable.ty.core_name.as_deref(), Some("config"));
            assert_eq!(variable.confidence, Confidence::NameOnly);
        }
        other => panic!("{other:#?}"),
    }
    assert_eq!(hover.types.len(), 1);
}

const OPS_HEADER: &str = "struct result { int value; struct result *next; };\n\
                          typedef struct result *(*lookup_fn)(int key);\n\
                          struct ops {\n\
                          \tstruct result *(*get)(void);\n\
                          \tlookup_fn lookup;\n\
                          \tint (*count)(void);\n\
                          };\n\
                          struct device { const struct ops *ops; };\n";

const OPS_SOURCE: &str = "#include \"ops.h\"\n\
                          int use(struct device *a)\n\
                          {\n\
                          \ta->ops->get()->value = 1;\n\
                          \t(*a->ops->get)()->next->value = 2;\n\
                          \ta->ops->lookup(3)->value = 4;\n\
                          \ta->ops->count()->value = 5;\n\
                          \treturn 0;\n\
                          }\n";

#[test]
fn member_chains_continue_through_function_pointer_calls() {
    let access = match type_at(
        OPS_SOURCE,
        offset_of(OPS_SOURCE, "value = 1", 0),
        SourceLanguage::C,
    ) {
        Some(TypeAt::MemberAccess { access, .. }) => access,
        other => panic!("{other:#?}"),
    };
    assert_eq!(
        access
            .members
            .iter()
            .map(|step| (step.name.as_str(), step.called))
            .collect::<Vec<_>>(),
        vec![("ops", false), ("get", true), ("value", false)]
    );
    assert!(matches!(&access.root, ChainRoot::Variable(root) if root.name == "a"));

    let fixture = fixture(&[("ops.h", OPS_HEADER), ("use.c", OPS_SOURCE)]);
    let describe = |needle: &str| {
        let hover = fixture
            .resolver()
            .describe_at(
                "use.c",
                OPS_SOURCE.as_bytes(),
                offset_of(OPS_SOURCE, needle, 0),
                SourceLanguage::C,
            )
            .unwrap_or_else(|| panic!("nothing at {needle}"));
        match hover.subject {
            HoverSubject::Member(chain) => chain,
            other => panic!("{other:#?}"),
        }
    };
    let owners = |chain: &MemberChain| {
        chain
            .steps
            .iter()
            .map(|step| step.owner.as_ref().map(|owner| owner.display_name.clone()))
            .collect::<Vec<_>>()
    };
    let owner = |name: &str| Some(name.to_owned());

    let chain = describe("value = 1");
    assert!(chain.complete, "{chain:#?}");
    assert_eq!(
        owners(&chain),
        vec![
            owner("struct device"),
            owner("struct ops"),
            owner("struct result")
        ]
    );
    assert_eq!(
        chain.final_member().map(|member| member.text.as_str()),
        Some("int value")
    );

    // Called through `*`, and a member after the returned one.
    let chain = describe("value = 2");
    assert!(chain.complete, "{chain:#?}");
    assert_eq!(chain.steps.len(), 4);

    // A member whose type is a typedef of a function pointer.
    let chain = describe("value = 4");
    assert!(chain.complete, "{chain:#?}");
    assert_eq!(owners(&chain).last(), Some(&owner("struct result")));

    // What `count` returns has no members.
    let chain = describe("value = 5");
    assert!(!chain.complete);
    assert_eq!(
        chain.steps.last().map(|step| step.name.as_str()),
        Some("value")
    );
    assert!(chain.steps.last().is_some_and(|step| step.owner.is_none()));

    // A long chain of calls is cut short, not followed into a deep recursion.
    let long = format!(
        "int f(struct device *a)\n{{\n\ta{}->value = 1;\n}}\n",
        "->ops->get()".repeat(5_000)
    );
    let offset = offset_of(&long, "value = 1", 0);
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || type_at(&long, offset, SourceLanguage::C).is_some())
        .expect("spawn")
        .join()
        .expect("no stack overflow");
}

#[test]
fn knr_parameters_and_typeof_declarations() {
    let source = "struct foo { int count; struct foo *next; };\n\
                  int knr(x, y)\n\
                  \tregister struct foo *x;\n\
                  \tint y;\n\
                  {\n\
                  \treturn x->count + y;\n\
                  }\n\
                  void use(struct foo *pos)\n\
                  {\n\
                  \ttypeof(*pos) *tmp = pos;\n\
                  \t__typeof__(pos) other = pos;\n\
                  \ttypeof(struct foo) value;\n\
                  \ttypeof(value) copy;\n\
                  \ttypeof(value) *copy_pointer;\n\
                  \ttmp->next = other->next;\n\
                  \tvalue.count = copy.count + copy_pointer->count;\n\
                  }\n";
    let root_type = |needle: &str| -> VariableType {
        match type_at(source, offset_of(source, needle, 0), SourceLanguage::C) {
            Some(TypeAt::MemberAccess { access, .. }) => match access.root {
                ChainRoot::Variable(variable) => variable,
                other => panic!("{needle}: {other:#?}"),
            },
            other => panic!("{needle}: {other:#?}"),
        }
    };
    for (needle, pointer_depth, confidence) in [
        ("count + y", 1, Confidence::Parameter),
        ("next = other", 1, Confidence::Local),
        ("next;\n\tvalue", 1, Confidence::Local),
        ("count = copy", 0, Confidence::Local),
        ("count + copy_pointer", 0, Confidence::Local),
        ("count;\n}", 1, Confidence::Local),
    ] {
        let variable = root_type(needle);
        assert_eq!(variable.confidence, confidence, "{needle}");
        assert_eq!(
            variable.ty.core_name.as_deref(),
            Some("foo"),
            "{needle}: {variable:#?}"
        );
        assert_eq!(variable.ty.tag, Some(TagKind::Struct), "{needle}");
        assert_eq!(variable.ty.pointer_depth, pointer_depth, "{needle}");
    }
    // The text stays as written.
    assert_eq!(root_type("next = other").ty.text, "typeof(*pos) *");
    match type_at(source, offset_of(source, "y;\n}", 0), SourceLanguage::C) {
        Some(TypeAt::Variable { variable, .. }) => {
            assert_eq!(variable.ty.core_name.as_deref(), Some("int"));
            assert_eq!(variable.confidence, Confidence::Parameter);
        }
        other => panic!("{other:#?}"),
    }
    // A member declared with `typeof` of a type.
    let layout = parse_c(
        "struct holder { typeof(struct foo *) first; __typeof__(unsigned long) flags; };\n",
        "holder",
        None,
    );
    let first = member(&layout, "first");
    assert_eq!(first.ty.core_name.as_deref(), Some("foo"));
    assert_eq!(first.ty.tag, Some(TagKind::Struct));
    assert_eq!(first.ty.pointer_depth, 1);
    assert_eq!(
        member(&layout, "flags").ty.core_name.as_deref(),
        Some("unsigned long")
    );
    // Nested deeply, it stays unread rather than recursing.
    let nested = format!(
        "struct deep {{ {}int{} member; }};\n",
        "typeof(".repeat(10_000),
        ")".repeat(10_000)
    );
    let layout = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || parse_c(&nested, "deep", None))
        .expect("spawn")
        .join()
        .expect("no stack overflow");
    assert_eq!(member(&layout, "member").ty.core_name, None);
}

#[test]
fn member_macros_without_semicolons_are_members_of_their_own() {
    let source = "struct task {\n\
                  \tint before;\n\
                  \trandomized_struct_fields_start\n\
                  \tint inside;\n\
                  \tRH_KABI_RESERVE(1)\n\
                  \tlong after_reserve;\n\
                  \tstruct_group(addrs,\n\
                  \t\tu8 saddr;\n\
                  \t\tu8 daddr;\n\
                  \t);\n\
                  \tDECLARE_BITMAP(bits, 64);\n\
                  \tkabi_reserve(2) // reserved\n\
                  \tint last;\n\
                  \tmy_type_t\n\
                  \tfield;\n\
                  \tSLIST_ENTRY(node)\n\
                  \tlink;\n\
                  \trandomized_struct_fields_end\n\
                  };\n";
    let layout = parse_c(source, "task", None);
    let summary: Vec<(MemberKind, &str)> = layout
        .members
        .iter()
        .map(|member| (member.kind, member.text.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (MemberKind::Field, "int before"),
            (MemberKind::Unparsed, "randomized_struct_fields_start"),
            (MemberKind::Field, "int inside"),
            (MemberKind::Unparsed, "RH_KABI_RESERVE(1)"),
            (MemberKind::Field, "long after_reserve"),
            (
                MemberKind::Unparsed,
                "struct_group(addrs, u8 saddr; u8 daddr; )"
            ),
            (MemberKind::Unparsed, "DECLARE_BITMAP(bits, 64)"),
            (MemberKind::Unparsed, "kabi_reserve(2)"),
            (MemberKind::Field, "int last"),
            (MemberKind::Field, "my_type_t field"),
            (MemberKind::Field, "SLIST_ENTRY(node) link"),
            (MemberKind::Unparsed, "randomized_struct_fields_end"),
        ]
    );
    let reserve = &layout.members[7];
    assert_eq!(reserve.comment.as_deref(), Some("reserved"));
    assert_eq!(
        reserve.range.start,
        position_of(source, "kabi_reserve(2)", 0)
    );
    assert_eq!(member(&layout, "last").comment, None);
    assert_eq!(
        member(&layout, "after_reserve").range.start,
        position_of(source, "long after_reserve", 0)
    );

    // An annotation after the name does not make the line a declaration of its own, but a
    // type such as `__u32` at its start does.
    let layout = parse_c(
        "struct counted {\n\
         \tatomic_t\n\
         \t\trefcount __aligned(8);\n\
         \tatomic_long_t\n\
         \t\tusers __project_marker;\n\
         \tfields_start\n\
         \t__u32 flags;\n\
         };\n",
        "counted",
        None,
    );
    let summary: Vec<(MemberKind, &str)> = layout
        .members
        .iter()
        .map(|member| (member.kind, member.text.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (MemberKind::Field, "atomic_t refcount"),
            (MemberKind::Field, "atomic_long_t users __project_marker"),
            (MemberKind::Unparsed, "fields_start"),
            (MemberKind::Field, "__u32 flags"),
        ]
    );
}

#[test]
fn comment_openers_in_literals_do_not_hide_code() {
    let source = "#define OPEN \"/*\"\n\
                  #define OPEN_CHAR '/' \"*\"\n\
                  struct after { int a; };\n\
                  /* a real comment */\n";
    assert_eq!(names(&parse_c(source, "after", None)), vec![Some("a")]);

    let source = "static const char *pattern = \"/*\";\n\
                  #ifdef CONFIG_X\n\
                  struct conditional { int a; };\n\
                  #endif\n";
    assert_eq!(
        enclosing_condition(
            source.as_bytes(),
            offset_of(source, "struct conditional", 0)
        )
        .as_deref(),
        Some("CONFIG_X")
    );
}
