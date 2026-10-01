use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

use anyhow::{Context, Result, bail};
use ignore::gitignore::GitignoreBuilder;
use regex::Regex;
use super_types::{SourceLanguage, Supertest};

use crate::git::GitRepository;

mod csharp;
mod java;
mod javascript;
mod vhdl;

#[derive(Debug)]
pub struct Discovery {
    pub canonical_selector: String,
    pub supertests: Vec<Supertest>,
    pub excluded: Vec<String>,
}

const MAX_SOURCE_FILE_SIZE: u64 = 10 * 1024 * 1024;

pub fn selector_target(current_directory: &Path, selector: &str) -> Result<PathBuf> {
    let (path_selector, _) = split_selector(selector)?;
    let current = current_directory
        .canonicalize()
        .with_context(|| format!("could not resolve {}", current_directory.display()))?;
    let selected = Path::new(path_selector);
    let joined = if selected.is_absolute() {
        selected.to_owned()
    } else {
        current.join(selected)
    };
    canonicalize_with_missing_tail(&joined)
}

pub fn canonicalize_selector(repository_root: &Path, current_directory: &Path, selector: &str) -> Result<String> {
    let (path_selector, name_selector) = split_selector(selector)?;
    let path = repository_relative(repository_root, current_directory, Path::new(path_selector))?;
    let path = path_to_git(&path);
    Ok(name_selector.map_or(path.clone(), |name| format!("{path}::{name}")))
}

/// Match a repository-relative selector against a previously checked declaration.
pub fn selector_matches(supertest: &Supertest, selector: &str) -> bool {
    selector == "."
        || selector == supertest.path
        || selector == supertest.selector()
        || supertest.path.starts_with(&format!("{selector}/"))
}

pub fn discover(
    repository: &GitRepository,
    commit_oid: &str,
    current_directory: &Path,
    selector: &str,
    explicit_commit: bool,
) -> Result<Discovery> {
    let (path_selector, name_selector) = split_selector(selector)?;
    let path = repository_relative(&repository.root, current_directory, Path::new(path_selector))?;
    let path_text = path_to_git(&path);
    let files = repository.files_at_commit(commit_oid)?;
    let is_file = files.iter().any(|file| file == &path_text);
    let prefix = if path_text == "." {
        String::new()
    } else {
        format!("{}/", path_text.trim_end_matches('/'))
    };

    let matching: Vec<_> = files
        .into_iter()
        .filter(|file| supported_language(file).is_some())
        .filter(|file| {
            if is_file {
                file == &path_text
            } else {
                file.starts_with(&prefix)
            }
        })
        .collect();
    if matching.is_empty() {
        if explicit_commit
            && supported_language(&path_text).is_some()
            && repository.root.join(&path).is_file()
            && !repository.path_exists_at_commit(commit_oid, &path_text)?
        {
            bail!(
                "`{path_selector}` exists in your working copy but is not present in selected commit {}.\n\
                 To check your working copy instead, use `--dirty` instead of `--commit`.",
                &commit_oid[..commit_oid.len().min(7)]
            )
        }
        bail!(
            "path `{path_selector}` does not match a supported Python, Rust, C, C#, JavaScript, Java, or VHDL source file"
        )
    }
    let ignores = superignore(repository, commit_oid)?;
    let candidates: Vec<_> = matching
        .into_iter()
        .filter(|file| {
            !ignores
                .matched_path_or_any_parents(repository.root.join(file), false)
                .is_ignore()
        })
        .collect();
    if candidates.is_empty() {
        bail!("all supported source files matching path `{path_selector}` are excluded by `.superignore`")
    }

    let mut supertests = Vec::new();
    let mut excluded = Vec::new();
    for file in candidates {
        let size = repository.file_size_at_commit(commit_oid, &file)?;
        if size > MAX_SOURCE_FILE_SIZE {
            excluded.push(format!("Excluded `{file}` because it is larger than 10 MiB."));
            continue;
        }
        let language = supported_language(&file).expect("candidate language");
        let source = repository.file_at_commit(commit_oid, &file)?;
        let declarations = parse_declarations(language, &source)
            .with_context(|| format!("could not parse `{file}` for supertests"))?;
        let mut names = HashSet::new();
        for (name, line) in declarations {
            if !names.insert(name.clone()) {
                bail!("`{file}` declares supertest `{name}` more than once")
            }
            if name_selector.is_none_or(|selected| selected == name) {
                supertests.push(Supertest {
                    path: file.clone(),
                    name,
                    language,
                    line: Some(line),
                });
            }
        }
    }
    supertests.sort_by(|left, right| left.path.cmp(&right.path).then_with(|| left.line.cmp(&right.line)));
    if supertests.is_empty() {
        if !excluded.is_empty() {
            bail!(excluded.join("\n"))
        }
        if let Some(name) = name_selector {
            bail!("no supertest named `{name}` was found in `{path_text}`")
        }
        bail!("no supertests were discovered at `{path_selector}`")
    }
    let canonical_selector = name_selector.map_or(path_text.clone(), |name| format!("{path_text}::{name}"));
    Ok(Discovery {
        canonical_selector,
        supertests,
        excluded,
    })
}

fn split_selector(selector: &str) -> Result<(&str, Option<&str>)> {
    let (path, name) = selector
        .rsplit_once("::")
        .map_or((selector, None), |(path, name)| (path, Some(name)));
    if path.is_empty() || name.is_some_and(str::is_empty) {
        bail!("invalid path or supertest name `{selector}`; use a file or directory path, or file::supertest_name")
    }
    Ok((path, name))
}

fn canonicalize_with_missing_tail(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }

    let mut existing = path.to_owned();
    let mut missing = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut resolved = canonical;
            for component in missing.iter().rev() {
                resolved.push(component);
            }
            return Ok(normalize(&resolved));
        }
        let name = existing
            .file_name()
            .with_context(|| format!("could not resolve path `{}` from an existing directory", path.display()))?;
        missing.push(name.to_owned());
        existing.pop();
    }
}

fn repository_relative(root: &Path, current: &Path, selected: &Path) -> Result<PathBuf> {
    let current = current
        .canonicalize()
        .with_context(|| format!("could not resolve {}", current.display()))?;
    let joined = if selected.is_absolute() {
        selected.to_owned()
    } else {
        current.join(selected)
    };
    let normalized = normalize(&joined);
    let normalized = if normalized.starts_with(root) {
        normalized
    } else {
        canonicalize_with_missing_tail(&joined)?
    };
    let relative = normalized.strip_prefix(root).with_context(|| {
        format!(
            "path `{}` is outside linked repository {}",
            selected.display(),
            root.display()
        )
    })?;
    Ok(if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative.to_owned()
    })
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn path_to_git(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn superignore(repository: &GitRepository, commit_oid: &str) -> Result<ignore::gitignore::Gitignore> {
    let mut builder = GitignoreBuilder::new(&repository.root);
    if let Ok(source) = repository.file_at_commit(commit_oid, ".superignore") {
        for line in source.lines() {
            builder.add_line(Some(PathBuf::from(".superignore")), line)?;
        }
    }
    builder
        .build()
        .context("could not parse .superignore at the selected commit")
}

fn supported_language(path: &str) -> Option<SourceLanguage> {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| match extension {
            "c" | "h" => Some(SourceLanguage::C),
            "rs" => Some(SourceLanguage::Rust),
            "py" => Some(SourceLanguage::Python),
            "cs" => Some(SourceLanguage::CSharp),
            "js" | "mjs" | "cjs" => Some(SourceLanguage::JavaScript),
            "java" => Some(SourceLanguage::Java),
            "vhd" | "vhdl" => Some(SourceLanguage::Vhdl),
            _ => None,
        })
}

fn parse_declarations(language: SourceLanguage, source: &str) -> Result<Vec<(String, u32)>> {
    match language {
        SourceLanguage::C => c_declarations(source),
        SourceLanguage::Rust => rust_declarations(source),
        SourceLanguage::Python => python_declarations(source),
        SourceLanguage::CSharp => csharp::declarations(source),
        SourceLanguage::JavaScript => javascript::declarations(source),
        SourceLanguage::Java => java::declarations(source),
        SourceLanguage::Vhdl => vhdl::declarations(source),
    }
}

fn c_declarations(source: &str) -> Result<Vec<(String, u32)>> {
    let markers = c_marker_ranges(source);
    let mut parsed_source = source.as_bytes().to_vec();
    for marker in &markers {
        parsed_source[marker.clone()].fill(b' ');
    }
    let parsed_source = String::from_utf8(parsed_source).expect("replacing ASCII C markers preserves UTF-8");
    let language = tree_sitter_c::LANGUAGE.into();
    let tree = parse_tree(&language, &parsed_source)?;
    let root = tree.root_node();
    let mut declarations = Vec::new();
    for function in named_children(root).filter(|node| node.kind() == "function_definition") {
        let Some(declarator) = function.child_by_field_name("declarator") else {
            continue;
        };
        let Some(name) = c_declarator_name(declarator) else {
            continue;
        };
        let Some(marker) = markers.iter().rev().find(|marker| {
            marker.end <= function.start_byte()
                && source[marker.end..function.start_byte()]
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

fn c_marker_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        LineComment,
        BlockComment,
        Quoted(u8),
    }

    const MARKER: &[u8] = b"SUPERTEST";
    let bytes = source.as_bytes();
    let mut markers = Vec::new();
    let mut state = State::Code;
    let mut preprocessor_line = false;
    let mut index = 0;
    while index < bytes.len() {
        match state {
            State::Code => match bytes[index] {
                b'\n' => {
                    let continued = index > 0 && bytes[index - 1] == b'\\';
                    preprocessor_line &= continued;
                    index += 1;
                }
                b'#' if source[..index]
                    .rsplit_once('\n')
                    .map_or(&source[..index], |(_, line)| line)
                    .trim()
                    .is_empty() =>
                {
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
                byte if byte == b'_' || byte.is_ascii_alphabetic() => {
                    let start = index;
                    index += 1;
                    while bytes
                        .get(index)
                        .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
                    {
                        index += 1;
                    }
                    if !preprocessor_line && &bytes[start..index] == MARKER {
                        markers.push(start..index);
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
    markers
}

fn c_declarator_name(mut declarator: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    loop {
        if declarator.kind() == "identifier" {
            return Some(declarator);
        }
        declarator = declarator.child_by_field_name("declarator")?;
    }
}

fn rust_declarations(source: &str) -> Result<Vec<(String, u32)>> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let attribute = RE.get_or_init(|| {
        Regex::new(r"(?m)#\s*\[\s*(?:schematic::)?supertest(?:\s*\([^]]*\))?\s*\]").expect("valid Rust attribute regex")
    });
    let language = tree_sitter_rust::LANGUAGE.into();
    let tree = parse_tree(&language, source)?;
    let root = tree.root_node();
    let mut declarations = Vec::new();
    for function in named_children(root).filter(|node| node.kind() == "function_item") {
        let Some(name) = function.child_by_field_name("name") else {
            continue;
        };
        let header = &source[function.start_byte()..name.start_byte()];
        let mut declaration_row = attribute.is_match(header).then_some(function.start_position().row);
        let mut previous = function.prev_named_sibling();
        while let Some(node) = previous.filter(|node| node.kind() == "attribute_item") {
            if attribute.is_match(&source[node.byte_range()]) {
                declaration_row = Some(node.start_position().row);
            }
            previous = node.prev_named_sibling();
        }
        if let Some(row) = declaration_row {
            declarations.push((source[name.byte_range()].into(), row_to_line(row)));
        }
    }
    Ok(declarations)
}

fn python_declarations(source: &str) -> Result<Vec<(String, u32)>> {
    static QUALIFIED_IMPORT: OnceLock<Regex> = OnceLock::new();
    static DIRECT_IMPORT: OnceLock<Regex> = OnceLock::new();
    static DIRECT_DECORATOR: OnceLock<Regex> = OnceLock::new();
    static QUALIFIED_DECORATOR: OnceLock<Regex> = OnceLock::new();
    let language = tree_sitter_python::LANGUAGE.into();
    let tree = parse_tree(&language, source)?;
    let root = tree.root_node();
    let children: Vec<_> = named_children(root).collect();
    let qualified_import = children.iter().any(|node| {
        node.kind() == "import_statement"
            && QUALIFIED_IMPORT
                .get_or_init(|| Regex::new(r"^\s*import\s+schematic(?:\s|$|,)").expect("valid Python import regex"))
                .is_match(&source[node.byte_range()])
    });
    let direct_import = children.iter().any(|node| {
        node.kind() == "import_from_statement"
            && DIRECT_IMPORT
                .get_or_init(|| {
                    Regex::new(r"^\s*from\s+schematic\s+import\s+(?:\*|(?s:.*\bsupertest\b))")
                        .expect("valid Python import regex")
                })
                .is_match(&source[node.byte_range()])
    });
    let direct_decorator = DIRECT_DECORATOR
        .get_or_init(|| Regex::new(r"^\s*@supertest(?:\s*\(|\s*$)").expect("valid Python decorator regex"));
    let qualified_decorator = QUALIFIED_DECORATOR
        .get_or_init(|| Regex::new(r"^\s*@schematic\.supertest(?:\s*\(|\s*$)").expect("valid Python decorator regex"));
    let mut declarations = Vec::new();
    for decorated in children
        .iter()
        .copied()
        .filter(|node| node.kind() == "decorated_definition")
    {
        let nested: Vec<_> = named_children(decorated).collect();
        let valid_decorator = nested.iter().filter(|node| node.kind() == "decorator").any(|node| {
            let text = &source[node.byte_range()];
            (qualified_import && qualified_decorator.is_match(text))
                || (direct_import && direct_decorator.is_match(text))
        });
        if !valid_decorator {
            continue;
        }
        let Some(function) = nested.iter().find(|node| node.kind() == "function_definition") else {
            continue;
        };
        let Some(name) = function.child_by_field_name("name") else {
            continue;
        };
        declarations.push((
            source[name.byte_range()].into(),
            row_to_line(decorated.start_position().row),
        ));
    }
    Ok(declarations)
}

fn parse_tree(language: &tree_sitter::Language, source: &str) -> Result<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(language)
        .context("could not load the source-language grammar")?;
    let tree = parser
        .parse(source, None)
        .context("the source parser stopped before producing a syntax tree")?;
    if tree.root_node().has_error() {
        bail!("the selected source contains syntax errors")
    }
    Ok(tree)
}

fn named_children(node: tree_sitter::Node<'_>) -> impl Iterator<Item = tree_sitter::Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect::<Vec<_>>().into_iter()
}

fn row_to_line(row: usize) -> u32 {
    u32::try_from(row.saturating_add(1)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_documented_c_forms() {
        let source = r"
#include <schematic.h>

SUPERTEST
static void accepts_positive_values(int value) {}

SUPERTEST void preserves_zero(void) {}
";
        assert_eq!(
            c_declarations(source)
                .unwrap()
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            ["accepts_positive_values", "preserves_zero"]
        );
    }

    #[test]
    fn does_not_accept_the_retired_c_marker_names() {
        let source = format!(
            "#include <schematic.h>\n#define {}\nstatic void old_marker(void) {{}}\n\nvoid old_assumption(int value) {{\n    {}(value > 0);\n}}\n",
            ["SCHEMATIC", "SUPERTEST"].concat(),
            ["SCHEMATIC", "ASSUME"].concat(),
        );
        assert!(c_declarations(&source).unwrap().is_empty());
    }

    #[test]
    fn finds_documented_rust_forms() {
        let source = r"
use schematic::supertest;
#[supertest]
fn identity(value: i32) {}

#[schematic::supertest]
pub async fn public_identity(value: i32) {}
";
        assert_eq!(
            rust_declarations(source)
                .unwrap()
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            ["identity", "public_identity"]
        );
    }

    #[test]
    fn python_requires_a_supported_import() {
        let direct = "from schematic import *\n\n@supertest\ndef identity(value):\n    pass\n";
        let qualified = "import schematic\n\n@schematic.supertest\ndef identity(value):\n    pass\n";
        let unrelated = "@supertest\ndef ordinary(value):\n    pass\n";
        assert_eq!(python_declarations(direct).unwrap().len(), 1);
        assert_eq!(python_declarations(qualified).unwrap().len(), 1);
        assert!(python_declarations(unrelated).unwrap().is_empty());
    }

    #[test]
    fn ignores_declarations_inside_strings_and_comments() {
        let c = r#"
/* SUPERTEST */ void ordinary(void) {}
const char *example = "SUPERTEST void not_real(void) {}";
"#;
        let rust = r##"const EXAMPLE: &str = "#[supertest] fn not_real() {}";"##;
        let python = "from schematic import *\ntext = \"@supertest\\ndef not_real(): pass\"\n";
        assert!(c_declarations(c).unwrap().is_empty());
        assert!(rust_declarations(rust).unwrap().is_empty());
        assert!(python_declarations(python).unwrap().is_empty());
    }

    #[test]
    fn only_file_scoped_declarations_are_discovered() {
        let c = "void helper(void) { SUPERTEST void nested(void) {} }\n";
        let rust = "fn helper() { #[supertest] fn nested() {} }\n";
        let python = "from schematic import *\nclass Claims:\n    @supertest\n    def nested(self): pass\n";
        assert!(c_declarations(c).unwrap().is_empty());
        assert!(rust_declarations(rust).unwrap().is_empty());
        assert!(python_declarations(python).unwrap().is_empty());
    }

    #[test]
    fn selector_target_resolves_an_external_linked_root() {
        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("pup");
        let examples = directory.path().join("supertest-examples-good");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::create_dir_all(examples.join("supertests")).unwrap();

        assert_eq!(
            selector_target(&current, "../supertest-examples-good").unwrap(),
            examples.canonicalize().unwrap()
        );
        assert_eq!(
            selector_target(&current, "../supertest-examples-good/supertests/missing.py::claim").unwrap(),
            examples.canonicalize().unwrap().join("supertests/missing.py")
        );
    }

    #[test]
    fn selectors_match_complete_paths_and_declaration_names() {
        let supertest = Supertest {
            path: "tests/nested/law.py".into(),
            name: "law".into(),
            language: SourceLanguage::Python,
            line: Some(4),
        };
        for selector in [
            ".",
            "tests",
            "tests/nested",
            "tests/nested/law.py",
            "tests/nested/law.py::law",
        ] {
            assert!(selector_matches(&supertest, selector), "{selector}");
        }
        for selector in ["test", "tests/nest", "tests/nested/law", "tests/nested/law.py::other"] {
            assert!(!selector_matches(&supertest, selector), "{selector}");
        }
    }

    #[test]
    fn dot_is_current_directory_scope_from_a_nested_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repository");
        let nested = root.join("supertests/nested");
        std::fs::create_dir_all(&nested).unwrap();
        let root = root.canonicalize().unwrap();

        assert_eq!(canonicalize_selector(&root, &nested, ".").unwrap(), "supertests/nested");
        assert_eq!(canonicalize_selector(&root, &root, ".").unwrap(), ".");
        assert_eq!(
            canonicalize_selector(&root, &nested, "../other").unwrap(),
            "supertests/other"
        );
    }

    #[cfg(unix)]
    #[test]
    fn repository_relative_resolves_symlinked_absolute_selectors() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repository");
        let alias = directory.path().join("repository-alias");
        let current = directory.path().join("current");
        std::fs::create_dir_all(root.join("supertests")).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        std::os::unix::fs::symlink(&root, &alias).unwrap();

        assert_eq!(
            repository_relative(
                &root.canonicalize().unwrap(),
                &current,
                &alias.join("supertests/missing.py")
            )
            .unwrap(),
            PathBuf::from("supertests/missing.py")
        );
    }
}
