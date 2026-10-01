use anyhow::{Result, bail};
use tree_sitter::Node;

use super::{named_children, parse_tree, row_to_line};

pub(super) fn declarations(source: &str) -> Result<Vec<(String, u32)>> {
    let tree = parse_tree(&tree_sitter_c_sharp::LANGUAGE.into(), source)?;
    let mut declarations = Vec::new();
    visit(tree.root_node(), source, &[], &mut declarations)?;
    Ok(declarations)
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
