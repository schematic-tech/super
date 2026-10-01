use anyhow::{Result, bail};
use tree_sitter::Node;

use super::{named_children, parse_tree, row_to_line};

pub(super) fn declarations(source: &str) -> Result<Vec<(String, u32)>> {
    let tree = parse_tree(&tree_sitter_vhdl::LANGUAGE.into(), source)?;
    let mut declarations = Vec::new();
    visit(tree.root_node(), source, &mut declarations)?;
    Ok(declarations)
}

fn visit(node: Node<'_>, source: &str, declarations: &mut Vec<(String, u32)>) -> Result<()> {
    for child in named_children(node) {
        if child.kind() == "line_comment" && source[child.byte_range()].trim() == "--% supertest" {
            let mut next = child.next_named_sibling();
            while let Some(comment) = next.filter(|node| matches!(node.kind(), "line_comment" | "block_comment")) {
                if source[comment.byte_range()].trim() == "--% supertest" {
                    bail!("a VHDL process must not have more than one supertest marker");
                }
                next = comment.next_named_sibling();
            }
            let Some(process) = next.filter(|node| node.kind() == "process_statement") else {
                bail!("`--% supertest` must precede a labeled VHDL process");
            };
            let label = named_children(process)
                .find(|node| node.kind() == "label_declaration")
                .and_then(|node| named_children(node).find(|node| node.kind() == "label"));
            let Some(label) = label else {
                bail!("a VHDL supertest process must have a label");
            };
            let name = &source[label.byte_range()];
            // Basic VHDL identifiers are case-insensitive; extended identifiers are not.
            let name = if name.starts_with('\\') {
                name.to_owned()
            } else {
                name.to_ascii_lowercase()
            };
            declarations.push((name, row_to_line(child.start_position().row)));
        } else if !matches!(child.kind(), "line_comment" | "block_comment") {
            visit(child, source, declarations)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn architecture(body: &str) -> String {
        format!(
            "entity claims is port (value : in natural range 0 to 255); end entity;\narchitecture tests of claims is begin\n{body}\nend architecture;"
        )
    }

    #[test]
    fn finds_labeled_processes_after_exact_markers_and_comments() {
        let source = architecture(
            "--% supertest\n-- a description\nIncrement_Never_Decreases : process begin assert value >= 0; wait; end process;",
        );
        assert_eq!(
            declarations(&source).unwrap(),
            [("increment_never_decreases".into(), 3)]
        );
    }

    #[test]
    fn ignores_strings_ordinary_comments_and_unmarked_processes() {
        let source = architecture(
            "-- example: --% supertest\nordinary : process begin report \"--% supertest\"; wait; end process;",
        );
        assert!(declarations(&source).unwrap().is_empty());
    }

    #[test]
    fn rejects_unlabeled_misplaced_and_duplicate_markers() {
        for body in [
            "--% supertest\nprocess begin wait; end process;",
            "--% supertest\nassert true;\nlaw : process begin wait; end process;",
            "--% supertest\n--% supertest\nlaw : process begin wait; end process;",
            "--% supertest",
        ] {
            assert!(declarations(&architecture(body)).is_err(), "{body}");
        }
    }

    #[test]
    fn rejects_invalid_syntax() {
        assert!(
            declarations(&architecture(
                "--% supertest\nlaw : process begin assert ; end process;"
            ))
            .is_err()
        );
    }
}
