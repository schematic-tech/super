use std::collections::HashSet;

use anyhow::{Result, bail};
use tree_sitter::Node;

use super::{named_children, parse_tree, row_to_line};

#[derive(Default)]
struct Imports<'a> {
    direct: HashSet<&'a str>,
    namespace: HashSet<&'a str>,
}

fn esm_imports<'a>(root: Node<'_>, source: &'a str) -> Imports<'a> {
    let mut imports = Imports::default();
    for import in named_children(root).filter(|node| node.kind() == "import_statement") {
        if !import
            .child_by_field_name("source")
            .is_some_and(|node| authoring_package(node, source))
        {
            continue;
        }
        for clause in named_children(import).filter(|node| node.kind() == "import_clause") {
            for binding in named_children(clause) {
                if binding.kind() == "namespace_import" {
                    if let Some(name) = named_children(binding).find(|node| node.kind() == "identifier") {
                        imports.namespace.insert(&source[name.byte_range()]);
                    }
                } else if binding.kind() == "named_imports" {
                    for specifier in named_children(binding).filter(|node| node.kind() == "import_specifier") {
                        if let Some(name) = specifier.child_by_field_name("name")
                            && &source[name.byte_range()] == "supertest"
                        {
                            let alias = specifier.child_by_field_name("alias").unwrap_or(name);
                            imports.direct.insert(&source[alias.byte_range()]);
                        }
                    }
                }
            }
        }
    }
    imports
}

fn commonjs_imports<'a>(bindings: &[Node<'_>], source: &'a str, imports: &mut Imports<'a>) {
    for binding in bindings {
        let (Some(name), Some(value)) = (
            binding.child_by_field_name("name"),
            binding.child_by_field_name("value"),
        ) else {
            continue;
        };
        if !is_require(value, source) {
            continue;
        }
        match name.kind() {
            "identifier" => {
                imports.namespace.insert(&source[name.byte_range()]);
            }
            "object_pattern" => {
                for property in named_children(name) {
                    if property.kind() == "shorthand_property_identifier_pattern"
                        && &source[property.byte_range()] == "supertest"
                    {
                        imports.direct.insert(&source[property.byte_range()]);
                    } else if property.kind() == "pair_pattern"
                        && let (Some(key), Some(value)) = (
                            property.child_by_field_name("key"),
                            property.child_by_field_name("value"),
                        )
                        && &source[key.byte_range()] == "supertest"
                        && value.kind() == "identifier"
                    {
                        imports.direct.insert(&source[value.byte_range()]);
                    }
                }
            }
            _ => {}
        }
    }
}

pub(super) fn declarations(source: &str) -> Result<Vec<(String, u32)>> {
    let tree = parse_tree(&tree_sitter_javascript::LANGUAGE.into(), source)?;
    let root = tree.root_node();
    let bindings: Vec<_> = named_children(root)
        .flat_map(|node| {
            if node.kind() == "export_statement" {
                named_children(node).collect::<Vec<_>>()
            } else {
                vec![node]
            }
        })
        .filter(|node| matches!(node.kind(), "lexical_declaration" | "variable_declaration"))
        .flat_map(named_children)
        .filter(|node| node.kind() == "variable_declarator")
        .collect();
    let mut imports = esm_imports(root, source);
    commonjs_imports(&bindings, source, &mut imports);
    let mut declarations = Vec::new();
    for binding in bindings {
        let (Some(name), Some(value)) = (
            binding.child_by_field_name("name"),
            binding.child_by_field_name("value"),
        ) else {
            continue;
        };
        if value.kind() != "call_expression" {
            continue;
        }
        let Some(function) = value.child_by_field_name("function") else {
            continue;
        };
        let marked = match function.kind() {
            "identifier" => imports.direct.contains(&source[function.byte_range()]),
            "member_expression" => {
                match (
                    function.child_by_field_name("object"),
                    function.child_by_field_name("property"),
                ) {
                    (Some(object), Some(property)) => {
                        object.kind() == "identifier"
                            && imports.namespace.contains(&source[object.byte_range()])
                            && &source[property.byte_range()] == "supertest"
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        if marked {
            if name.kind() != "identifier" {
                bail!("a JavaScript supertest must be assigned to a single named binding");
            }
            let arguments = value.child_by_field_name("arguments").expect("call arguments");
            if named_children(arguments)
                .filter(|node| node.kind() != "comment")
                .count()
                != 1
            {
                bail!("a JavaScript supertest requires exactly one function argument");
            }
            declarations.push((
                source[name.byte_range()].into(),
                row_to_line(binding.start_position().row),
            ));
        }
    }
    Ok(declarations)
}

fn authoring_package(node: Node<'_>, source: &str) -> bool {
    node.kind() == "string"
        && matches!(
            source[node.byte_range()].trim_matches(['\'', '"']),
            "schematic-supertest" | "schematic-supertest/browser"
        )
}

fn is_require(node: Node<'_>, source: &str) -> bool {
    node.kind() == "call_expression"
        && node
            .child_by_field_name("function")
            .is_some_and(|node| node.kind() == "identifier" && &source[node.byte_range()] == "require")
        && node.child_by_field_name("arguments").is_some_and(|arguments| {
            let arguments: Vec<_> = named_children(arguments)
                .filter(|node| node.kind() != "comment")
                .collect();
            arguments.len() == 1 && authoring_package(arguments[0], source)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_esm_import_aliases_namespace_imports_and_named_bindings() {
        let source = "import { supertest as law } from 'schematic-supertest';\nimport * as schematic from 'schematic-supertest/browser';\nexport const first = law((text) => {});\nconst second = schematic.supertest(function (text) {});";
        assert_eq!(
            declarations(source).unwrap(),
            [("first".into(), 3), ("second".into(), 4)]
        );
    }

    #[test]
    fn finds_commonjs_imports() {
        let source = "const { supertest, supertest: law } = require('schematic-supertest');\nconst schematic = require('schematic-supertest');\nconst first = supertest(x => x), second = law(function(x) {});\nconst third = schematic.supertest(x => x);";
        assert_eq!(
            declarations(source).unwrap(),
            [("first".into(), 3), ("second".into(), 3), ("third".into(), 4)]
        );
    }

    #[test]
    fn ignores_other_packages_strings_comments_and_nested_bindings() {
        let source = r#"import { supertest } from 'somewhere-else';
import { supertest as law } from 'schematic-supertest';
const ordinary = supertest(x => x);
const example = "const fake = law(x => x)";
const template = `const fake = law(x => x)`;
// const fake = law(x => x);
function helper() { const nested = law(x => x); }
"#;
        assert!(declarations(source).unwrap().is_empty());
    }

    #[test]
    fn rejects_destructured_supertests_missing_arguments_and_invalid_syntax() {
        for source in [
            "import { supertest } from 'schematic-supertest'; const { law } = supertest(x => x);",
            "import { supertest } from 'schematic-supertest'; const law = supertest();",
            "import { supertest } from 'schematic-supertest'; const law = supertest(};",
        ] {
            assert!(declarations(source).is_err(), "{source}");
        }
    }
}
