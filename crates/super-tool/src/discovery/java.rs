use anyhow::{Result, bail};
use tree_sitter::Node;

use super::{named_children, parse_tree, row_to_line};

pub(super) fn declarations(source: &str) -> Result<Vec<(String, u32)>> {
    let tree = parse_tree(&tree_sitter_java::LANGUAGE.into(), source)?;
    let root = tree.root_node();
    let imported = named_children(root).any(|node| {
        matches!(node.kind(), "import_declaration" | "package_declaration")
            && matches!(
                source[node.byte_range()]
                    .split_whitespace()
                    .collect::<String>()
                    .as_str(),
                "importtech.schematic.Supertest;" | "importtech.schematic.*;" | "packagetech.schematic;"
            )
    });
    let mut declarations = Vec::new();
    visit(root, source, imported, &mut declarations)?;
    Ok(declarations)
}

fn visit(node: Node<'_>, source: &str, imported: bool, declarations: &mut Vec<(String, u32)>) -> Result<()> {
    for child in named_children(node) {
        match child.kind() {
            "method_declaration" => {
                let mut marker = None;
                for annotation in named_children(child)
                    .filter(|node| node.kind() == "modifiers")
                    .flat_map(named_children)
                    .filter(|node| matches!(node.kind(), "annotation" | "marker_annotation"))
                {
                    let Some(name) = annotation.child_by_field_name("name") else {
                        continue;
                    };
                    let name = &source[name.byte_range()];
                    if name != "tech.schematic.Supertest" && !(imported && name == "Supertest") {
                        continue;
                    }
                    if marker.replace(annotation.start_position().row).is_some() {
                        bail!("a Java method must not have more than one supertest marker");
                    }
                }
                if let (Some(row), Some(name)) = (marker, child.child_by_field_name("name")) {
                    if child.child_by_field_name("body").is_none() {
                        bail!("a Java supertest must have a method body");
                    }
                    declarations.push((source[name.byte_range()].into(), row_to_line(row)));
                }
            }
            "class_declaration"
            | "class_body"
            | "record_declaration"
            | "interface_declaration"
            | "interface_body"
            | "enum_declaration"
            | "enum_body"
            | "enum_body_declarations" => {
                visit(child, source, imported, declarations)?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_imported_and_qualified_methods_in_nested_classes() {
        let source = "package texttools;\nimport tech.schematic.Supertest;\nclass Claims {\n@Supertest\npublic static void first(String text) {}\nclass Nested { @tech.schematic.Supertest() void second() {} }\n}";
        assert_eq!(
            declarations(source).unwrap(),
            [("first".into(), 4), ("second".into(), 6)]
        );
        assert_eq!(
            declarations("import tech.schematic.*; class C { @Supertest void law() {} }")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn ignores_unrelated_annotations_comments_strings_and_local_classes() {
        let source = r#"import other.Supertest;
class Claims {
    // @tech.schematic.Supertest void fake() {}
    String text = "@tech.schematic.Supertest void fake() {}";
    @Supertest void unrelated() {}
    void ordinary() { class Nested { @tech.schematic.Supertest void nested() {} } }
}"#;
        assert!(declarations(source).unwrap().is_empty());
    }

    #[test]
    fn rejects_duplicate_markers_missing_bodies_and_invalid_syntax() {
        for source in [
            "class C { @tech.schematic.Supertest @tech.schematic.Supertest void law() {} }",
            "interface C { @tech.schematic.Supertest void law(); }",
            "class C { @tech.schematic.Supertest void law( }",
        ] {
            assert!(declarations(source).is_err(), "{source}");
        }
    }
}
