use std::collections::HashMap;

use super::{Context, Result, bail, named_children, row_to_line};

pub(super) fn declarations(source: &str) -> Result<Vec<(String, u32)>> {
    let identifiers = c_identifier_ranges(source);
    let markers: Vec<_> = identifiers
        .iter()
        .filter(|range| &source[(*range).clone()] == "SUPERTEST")
        .cloned()
        .collect();
    let mut parsed_source = source.as_bytes().to_vec();
    for marker in &markers {
        parsed_source[marker.clone()].fill(b' ');
    }
    let mut parsed_source = String::from_utf8(parsed_source).expect("replacing ASCII C markers preserves UTF-8");
    let language = tree_sitter_c::LANGUAGE.into();
    // C collection libraries commonly expose iteration as macros that expand to a
    // control-flow statement. Before preprocessing, tree-sitter represents forms
    // such as `LL_FOREACH(head, node) { ... }` as an error inside the function body.
    // Discovery only needs the file-scoped function declaration; semantic checking
    // handles the body later, so tolerate body-local macro errors while retaining
    // syntax validation for top-level declarations and function headers.
    let tree = parse_c_tree(&language, &mut parsed_source, &identifiers)?;
    let root = tree.root_node();
    let mut declarations = Vec::new();
    for function in file_functions(root) {
        let Some(declarator) = function.child_by_field_name("declarator") else {
            continue;
        };
        let Some(name) = c_declarator_name(declarator) else {
            continue;
        };
        let Some(marker) = markers.iter().rev().find(|marker| {
            marker.end <= function.start_byte()
                && parsed_source[marker.end..function.start_byte()]
                    .bytes()
                    .all(|byte| byte.is_ascii_whitespace())
        }) else {
            continue;
        };
        declarations.push((
            source[name.byte_range()].to_owned(),
            row_to_line(source[..marker.start].bytes().filter(|byte| *byte == b'\n').count()),
        ));
    }
    Ok(declarations)
}

fn file_functions(root: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    let mut functions = Vec::new();
    for child in named_children(root) {
        match child.kind() {
            "function_definition" => functions.push(child),
            "preproc_if" | "preproc_ifdef" | "preproc_else" | "preproc_elif" | "preproc_elifdef" => {
                functions.extend(file_functions(child));
            }
            _ => {}
        }
    }
    functions
}

fn parse_c_tree(
    language: &tree_sitter::Language,
    source: &mut String,
    identifiers: &[std::ops::Range<usize>],
) -> Result<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(language)
        .context("could not load the source-language grammar")?;
    let mut tree = parser
        .parse(source.as_bytes(), None)
        .context("the source parser stopped before producing a syntax tree")?;
    // Object-like annotation macros are not C type names. Recognize only local
    // definitions that expand to an attribute or nothing in every branch. Keep
    // directives, comments, strings and byte offsets intact; the original source
    // is still submitted for semantic checking.
    let annotations = annotation_macros(tree.root_node(), source, &mut parser);
    let mut bytes = source.as_bytes().to_vec();
    let mut changed = false;
    for range in identifiers {
        if annotations.get(&source[range.clone()]) == Some(&true) {
            bytes[range.clone()].fill(b' ');
            changed = true;
        }
    }
    if changed {
        *source = String::from_utf8(bytes).expect("replacing identifiers preserves UTF-8");
        tree = parser
            .parse(source.as_bytes(), None)
            .context("the source parser stopped before producing a syntax tree")?;
    }
    if !c_has_declaration_error(tree.root_node()) {
        return Ok(tree);
    }
    bail!("the selected source contains syntax errors")
}

fn annotation_macros(
    root: tree_sitter::Node<'_>,
    source: &str,
    parser: &mut tree_sitter::Parser,
) -> HashMap<String, bool> {
    let mut macros = HashMap::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "preproc_def" | "preproc_function_def") {
            if let Some(name) = node.child_by_field_name("name") {
                let value = node
                    .child_by_field_name("value")
                    .map_or("", |value| source[value.byte_range()].trim());
                let annotation = node.kind() == "preproc_def" && is_annotation(value, parser);
                macros
                    .entry(source[name.byte_range()].to_owned())
                    .and_modify(|valid| *valid &= annotation)
                    .or_insert(annotation);
            }
        } else {
            pending.extend(named_children(node));
        }
    }
    macros
}

fn is_annotation(value: &str, parser: &mut tree_sitter::Parser) -> bool {
    if value.is_empty() {
        return true;
    }
    if !["__attribute__", "__attribute", "__declspec"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
    {
        return false;
    }
    let probe = format!("{value} int super_annotation_probe;");
    let Some(tree) = parser.parse(&probe, None) else {
        return false;
    };
    let root = tree.root_node();
    if root.has_error() || root.named_child_count() != 1 {
        return false;
    }
    let Some(declaration) = root.named_child(0) else {
        return false;
    };
    declaration.kind() == "declaration"
        && declaration
            .child_by_field_name("type")
            .is_some_and(|ty| &probe[ty.byte_range()] == "int")
        && declaration
            .child_by_field_name("declarator")
            .is_some_and(|name| &probe[name.byte_range()] == "super_annotation_probe")
}

fn c_has_declaration_error(node: tree_sitter::Node<'_>) -> bool {
    if node.kind() == "function_definition" {
        return c_has_syntax_error_outside(node, node.child_by_field_name("body"));
    }
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(c_has_declaration_error)
}

fn c_has_syntax_error_outside(node: tree_sitter::Node<'_>, excluded: Option<tree_sitter::Node<'_>>) -> bool {
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| {
        if excluded.is_some_and(|excluded| child == excluded) {
            false
        } else {
            c_has_syntax_error_outside(child, excluded)
        }
    })
}

fn c_identifier_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        LineComment,
        BlockComment,
        Quoted(u8),
    }

    let bytes = source.as_bytes();
    let mut identifiers = Vec::new();
    let mut state = State::Code;
    let mut preprocessor_line = false;
    let mut index = 0;
    while index < bytes.len() {
        // C joins escaped physical lines before recognizing comments or
        // directives. In particular, a continued // comment stays a comment.
        if bytes[index] == b'\\' {
            if bytes.get(index + 1) == Some(&b'\n') {
                index += 2;
                continue;
            }
            if bytes.get(index + 1..index + 3) == Some(b"\r\n") {
                index += 3;
                continue;
            }
        }
        match state {
            State::Code => match bytes[index] {
                b'\n' => {
                    preprocessor_line = false;
                    index += 1;
                }
                b'#' => {
                    preprocessor_line = true;
                    index += 1;
                }
                b'/' if bytes.get(index + 1) == Some(&b'/') => {
                    state = State::LineComment;
                    index += 2;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    state = State::BlockComment;
                    index += 2;
                }
                quote @ (b'"' | b'\'') => {
                    state = State::Quoted(quote);
                    index += 1;
                }
                byte if byte == b'_' || byte.is_ascii_alphabetic() || byte >= 0x80 => {
                    let start = index;
                    index += 1;
                    while bytes
                        .get(index)
                        .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphanumeric() || *byte >= 0x80)
                    {
                        index += 1;
                    }
                    if !preprocessor_line {
                        identifiers.push(start..index);
                    }
                }
                _ => index += 1,
            },
            State::LineComment => {
                if bytes[index] == b'\n' {
                    state = State::Code;
                    preprocessor_line = false;
                }
                index += 1;
            }
            State::BlockComment => {
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    state = State::Code;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            State::Quoted(quote) => {
                if bytes[index] == b'\\' {
                    index = (index + 2).min(bytes.len());
                } else {
                    if bytes[index] == quote {
                        state = State::Code;
                    }
                    index += 1;
                }
            }
        }
    }
    identifiers
}

fn c_declarator_name(mut declarator: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    loop {
        if declarator.kind() == "identifier" {
            return Some(declarator);
        }
        declarator = declarator.child_by_field_name("declarator")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_respect_continued_directives_comments_and_unicode_boundaries() {
        let source = "/* prefix */ #define EXAMPLE \\\nSUPERTEST UNUSED\n// continued \\\nSUPERTEST UNUSED\nαUNUSED SUPERTEST void real(void) {}\n";
        for source in [source.to_owned(), source.replace('\n', "\r\n")] {
            let identifiers: Vec<_> = c_identifier_ranges(&source)
                .into_iter()
                .map(|range| &source[range])
                .collect();
            assert_eq!(identifiers, ["αUNUSED", "SUPERTEST", "void", "real", "void"]);
        }
    }

    #[test]
    fn annotations_preserve_supertest_names_and_source_lines() {
        let source = "#ifdef __GNUC__\n#define UNUSED __attribute__((__unused__))\n#else\n#define UNUSED\n#endif\nstatic const int value UNUSED = 1;\nUNUSED static void helper(void) {}\nSUPERTEST UNUSED static void claim(void) {}\n";
        assert_eq!(declarations(source).unwrap(), [("claim".into(), 8)]);
        assert_eq!(
            declarations(&source.replace('\n', "\r\n")).unwrap(),
            [("claim".into(), 8)]
        );
    }

    #[test]
    fn continued_macros_with_comments_do_not_consume_later_declarations() {
        let source = r"
#define HASH(key) do { \
    switch (key) { \
        case 1: key++; /* FALLTHROUGH */ \
        default: break; \
    } \
    /* Mix the value. */ \
    key ^= key << 3; \
} while (0)
SUPERTEST void after_macro(void) {}
";
        assert_eq!(declarations(source).unwrap(), [("after_macro".into(), 10)]);
        assert_eq!(
            declarations(&source.replace('\n', "\r\n")).unwrap(),
            [("after_macro".into(), 10)]
        );
    }

    #[test]
    fn conditional_if_headers_preserve_the_enclosing_and_following_functions() {
        let source = r"
SUPERTEST void read_peer(void) {
#ifdef __FreeBSD__
    if (read_mem(ptr, pid, start, len)) {
#else
    if (read_mem(ptr, fd, start, len)) {
#endif
        report_error();
    }
}
SUPERTEST void after_conditionals(void) {}
";
        assert_eq!(
            declarations(source).unwrap(),
            [("read_peer".into(), 2), ("after_conditionals".into(), 11)]
        );
    }

    #[test]
    fn file_scope_inside_preprocessor_branches_keeps_macro_body_tolerance() {
        let source = r"
#ifndef CLAIMS_H
#define CLAIMS_H
#if MODE == 1
SUPERTEST void first(void) { LL_FOREACH(head, node) {} }
#else
SUPERTEST void second(void) { HASH_ITER(hh, head, node, tmp) {} }
#endif
#endif
";
        assert_eq!(
            declarations(source).unwrap(),
            [("first".into(), 5), ("second".into(), 7)]
        );
    }

    #[test]
    fn annotation_recovery_does_not_hide_invalid_declarations() {
        for source in [
            "#define UNUSED __attribute__((unused))\nSUPERTEST UNUSED void broken( {}",
            "#define UNUSED __attribute__((unused))\nint x UNUSED = ;\nSUPERTEST void claim(void) {}",
            "#define UNUSED __attribute__((unused))\nint x UNUSED\nSUPERTEST void claim(void) {}",
            "#define UNUSED __attribute__((unused))\n#ifdef MODE\n#define UNUSED int\n#endif\nSUPERTEST UNUSED static void claim(void) {}",
        ] {
            assert!(declarations(source).is_err(), "{source}");
        }
    }

    #[test]
    fn annotations_and_markers_inside_literals_comments_and_definitions_are_ignored() {
        let source = r#"
#define UNUSED __attribute__((unused))
#define EXAMPLE SUPERTEST void fake(void) {}
const char *example = "UNUSED SUPERTEST void fake(void) {}";
/* SUPERTEST UNUSED void also_fake(void) {} */
SUPERTEST void actual(void) { const char *s = "UNUSED"; }
"#;
        assert_eq!(declarations(source).unwrap(), [("actual".into(), 6)]);
    }
}
