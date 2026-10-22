use anyhow::{Context, Result, bail};
use tree_sitter::Node;

use super::{named_children, row_to_line};

pub(super) fn declarations(source: &str) -> Result<Vec<(String, u32)>> {
    let tree = declaration_tree(source)?;
    let mut declarations = Vec::new();
    visit(tree.root_node(), source, &[], &mut declarations)?;
    Ok(declarations)
}

fn declaration_tree(source: &str) -> Result<tree_sitter::Tree> {
    let language = tree_sitter_c_sharp::LANGUAGE.into();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language)
        .context("could not load the source-language grammar")?;
    let mut tree = parser
        .parse(source, None)
        .context("the source parser stopped before producing a syntax tree")?;
    // tree-sitter-c-sharp 0.23.5 loses scanner state for $"". Replace only the
    // recognized interpolation prefix with a space, then require a valid reparse.
    // Byte offsets and the original source used for declarations/uploads stay intact.
    let mut recovered_source: Option<Vec<u8>> = None;
    while tree.root_node().has_error() {
        let mut changed = false;
        let mut pending = vec![tree.root_node()];
        while let Some(node) = pending.pop() {
            if node.kind() == "interpolation_start"
                && &source[node.byte_range()] == "$"
                && let Some(tail) = source[node.end_byte()..].strip_prefix("\"\"")
                && !tail.starts_with('"')
                && !tail.starts_with("u8")
            {
                let bytes = recovered_source.get_or_insert_with(|| source.as_bytes().to_vec());
                changed |= bytes[node.start_byte()] == b'$';
                bytes[node.start_byte()] = b' ';
            }
            pending.extend(named_children(node));
        }
        if !changed {
            bail!("the selected source contains syntax errors");
        }
        // A reparse can expose another empty string obscured by the first error.
        tree = parser
            .parse(recovered_source.as_ref().expect("a changed source exists"), None)
            .context("the source parser stopped before producing a syntax tree")?;
    }
    Ok(tree)
}

fn visit(node: Node<'_>, source: &str, inherited: &[String], declarations: &mut Vec<(String, u32)>) -> Result<()> {
    let mut markers = inherited.to_vec();
    for using in named_children(node).filter(|child| child.kind() == "using_directive") {
        let Some(target) = named_children(using).last() else {
            continue;
        };
        let target = source[target.byte_range()].replace("global::", "");
        if let Some(alias) = using.child_by_field_name("name") {
            if matches!(target.as_str(), "Schematic.Supertest" | "Schematic.SupertestAttribute") {
                markers.push(source[alias.byte_range()].to_owned());
            } else if target == "Schematic" {
                let alias = &source[alias.byte_range()];
                markers.extend([format!("{alias}.Supertest"), format!("{alias}.SupertestAttribute")]);
            }
        } else if target == "Schematic" {
            markers.extend(["Supertest".into(), "SupertestAttribute".into()]);
        }
    }
    for child in named_children(node) {
        match child.kind() {
            "method_declaration" => {
                let mut marker = None;
                for attribute in named_children(child)
                    .filter(|node| node.kind() == "attribute_list")
                    .flat_map(named_children)
                    .filter(|node| node.kind() == "attribute")
                {
                    let Some(name) = attribute.child_by_field_name("name") else {
                        continue;
                    };
                    let name = source[name.byte_range()].replace("global::", "");
                    if !matches!(name.as_str(), "Schematic.Supertest" | "Schematic.SupertestAttribute")
                        && !markers.contains(&name)
                    {
                        continue;
                    }
                    if marker.replace(attribute.start_position().row).is_some() {
                        bail!("a C# method must not have more than one supertest marker");
                    }
                }
                if let (Some(row), Some(name)) = (marker, child.child_by_field_name("name")) {
                    if child.child_by_field_name("body").is_none() {
                        bail!("a C# supertest must have a method body");
                    }
                    declarations.push((
                        source[name.byte_range()].trim_start_matches('@').into(),
                        row_to_line(row),
                    ));
                }
            }
            "namespace_declaration"
            | "declaration_list"
            | "class_declaration"
            | "struct_declaration"
            | "record_declaration"
            | "interface_declaration" => visit(child, source, &markers, declarations)?,
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_interpolated_strings_do_not_block_discovery() {
        let source = r#"using Schematic;
public static class Checks {
    [Supertest]
    public static void EmptyInterpolatedString() {
        var value = $"";
        System.Diagnostics.Debug.Assert(value == "");
    }
}
"#;
        assert_eq!(declarations(source).unwrap(), [("EmptyInterpolatedString".into(), 3)]);
        assert!(
            declarations(r#"class Ordinary { void Log() { output.WriteLine($""); } }"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn empty_interpolation_recovery_preserves_declaration_names_and_lines() {
        let source = r#"using Schematic;
class Claims {
    // Empty strings: $"". Unicode before the marker: café.
    string ordinary = "$\"\"";
    string verbatim = @"$""""";
    string raw = """text $"" text""";
    [Supertest]
    void @first() { var values = new[] { $"", $"", $@"", @$"" }; }

    [Supertest]
    void café() { var nested = $"prefix {($"")} suffix"; }
}
"#;
        assert_eq!(
            declarations(source).unwrap(),
            [("first".into(), 7), ("café".into(), 10)]
        );
    }

    #[test]
    fn empty_interpolation_recovery_does_not_hide_invalid_syntax() {
        for expression in [
            r#"$$"""#,
            r#"$ """#,
            r#"$/* gap */"""#,
            r#"$""u8"#,
            r#"$""""#,
            r#"$"" +"#,
            r#"$"{""#,
        ] {
            let source = format!(
                "class C {{ [Schematic.Supertest] void law() {{ var valid = $\"\"; var invalid = {expression}; }} }}"
            );
            assert!(declarations(&source).is_err(), "{expression}");
        }
        assert!(declarations(r#"class C { [Schematic.Supertest] void law() { var s = $""; }"#).is_err());
        assert!(
            declarations(r#"class C { [Schematic.Supertest, Schematic.Supertest] void law() { var s = $""; } }"#)
                .is_err()
        );
    }

    #[test]
    fn finds_imported_qualified_and_aliased_methods_in_types_and_namespaces() {
        let source = "using Schematic;\nusing Law = Schematic.SupertestAttribute;\nnamespace TextTools;\nclass Claims {\n[Supertest]\npublic static void first(string text) {}\n[Law] void second() {}\nclass Nested { [global::Schematic.SupertestAttribute] void third() {} }\n}";
        assert_eq!(
            declarations(source).unwrap(),
            [("first".into(), 5), ("second".into(), 7), ("third".into(), 8)]
        );
        assert_eq!(
            declarations("namespace N { using Schematic; class C { [Supertest] void law() {} } }")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn ignores_unrelated_attributes_comments_strings_and_local_functions() {
        let source = r#"using Other;
class Claims {
    // [Schematic.Supertest] void fake() {}
    string text = "[Schematic.Supertest] void fake() {}";
    [Supertest] void unrelated() {}
    void ordinary() { [Schematic.Supertest] void nested() {} }
}"#;
        assert!(declarations(source).unwrap().is_empty());
        assert!(
            declarations("namespace A { using Schematic; } namespace B { class C { [Supertest] void law() {} } }")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_duplicate_markers_missing_bodies_and_invalid_syntax() {
        for source in [
            "class C { [Schematic.Supertest, Schematic.Supertest] void law() {} }",
            "interface C { [Schematic.Supertest] void law(); }",
            "class C { [Schematic.Supertest] void law( }",
        ] {
            assert!(declarations(source).is_err(), "{source}");
        }
    }
}
