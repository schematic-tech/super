use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    rc::Rc,
};

/// A source link and the reason it cannot be resolved.
#[derive(Debug)]
pub struct SourceLinkIssue {
    pub path: String,
    pub target: String,
    pub reason: &'static str,
}

impl fmt::Display for SourceLinkIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?} -> {:?}: {}", self.path, self.target, self.reason)
    }
}

/// Validate portable Git link targets against a complete, filtered tree without accessing the host.
/// Missing targets and cycles are retained, as they are in Git. A manifest entry may never be
/// another entry's parent: materialization must not write through a symlink.
///
/// # Errors
/// Returns an error for an escaping, nonportable, or overlong target, or overlapping entries.
pub fn validate_source_links<'a>(entries: impl IntoIterator<Item = (&'a str, Option<&'a str>)>) -> Result<(), String> {
    let issues = source_link_issues(entries)?;
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"))
    }
}

/// Report each link causing a validation failure once, without listing dependent aliases.
///
/// # Errors
/// Returns an error when a source entry is also the parent of another entry.
pub fn source_link_issues<'a>(
    entries: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
) -> Result<Vec<SourceLinkIssue>, String> {
    let entries = entries.into_iter().collect::<BTreeMap<_, _>>();
    let mut links = BTreeMap::new();
    let mut cache = BTreeMap::new();
    for (&path, &target) in &entries {
        let mut parent = path;
        while let Some((ancestor, _)) = parent.rsplit_once('/') {
            if entries.contains_key(ancestor) {
                return Err(format!("source entry `{ancestor}` is also a parent of `{path}`"));
            }
            parent = ancestor;
        }
        if let Some(target) = target {
            if let Some(reason) = invalid_target(target) {
                cache.insert(
                    path.to_owned(),
                    Err(Rc::new(LinkFailure {
                        path: path.to_owned(),
                        reason,
                    })),
                );
            }
            links.insert(path, target);
        }
    }
    let mut issues = BTreeMap::new();
    for &path in links.keys() {
        if let Err(failure) = resolve(path, &links, &mut cache, None) {
            issues.entry(failure.path.clone()).or_insert_with(|| SourceLinkIssue {
                path: failure.path.clone(),
                target: links[failure.path.as_str()].to_owned(),
                reason: failure.reason,
            });
        }
    }
    Ok(issues.into_values().collect())
}

/// Find unresolved links in a validated source manifest, including missing targets and cycles.
/// Uses only submitted paths; excluded and locally generated files do not count as targets.
pub fn source_link_warnings<'a>(entries: impl IntoIterator<Item = (&'a str, Option<&'a str>)>) -> Vec<SourceLinkIssue> {
    let entries = entries.into_iter().collect::<Vec<_>>();
    let links = entries
        .iter()
        .filter_map(|&(path, target)| target.map(|target| (path, target)))
        .collect::<BTreeMap<_, _>>();
    if links.is_empty() {
        return Vec::new();
    }
    let paths = SourcePaths::new(entries.iter().map(|&(path, _)| path));
    let mut cache = BTreeMap::new();
    links
        .iter()
        .filter_map(|(&path, &target)| {
            let reason = match resolve(path, &links, &mut cache, Some(&paths)) {
                Ok(Some(_)) => return None,
                Ok(None) => "target contains a symlink cycle",
                Err(failure) => failure.reason,
            };
            Some(SourceLinkIssue {
                path: path.to_owned(),
                target: target.to_owned(),
                reason,
            })
        })
        .collect()
}

struct SourcePaths<'a> {
    files: BTreeSet<&'a str>,
    directories: BTreeSet<&'a str>,
}

impl<'a> SourcePaths<'a> {
    fn new(files: impl Iterator<Item = &'a str>) -> Self {
        let files = files.collect::<BTreeSet<_>>();
        let mut directories = BTreeSet::from([""]);
        for &file in &files {
            let mut path = file;
            while let Some((parent, _)) = path.rsplit_once('/') {
                directories.insert(parent);
                path = parent;
            }
        }
        Self { files, directories }
    }

    fn issue(&self, path: &[&str], traversing: bool) -> Option<&'static str> {
        let path = path.join("/");
        if self.directories.contains(path.as_str()) {
            None
        } else if self.files.contains(path.as_str()) {
            traversing.then_some("target traverses a file as a directory")
        } else {
            Some("target is missing from the submitted source")
        }
    }
}

fn invalid_target(target: &str) -> Option<&'static str> {
    if target.is_empty() {
        Some("target is empty")
    } else if target.len() > 4096 {
        Some("target exceeds 4096 bytes")
    } else if target.starts_with('/') || target.as_bytes().get(1) == Some(&b':') {
        Some("absolute or drive-qualified targets are not supported")
    } else if target.contains(['\\', '\0']) {
        Some("target contains a backslash or NUL byte")
    } else if target.split('/').any(|part| part.eq_ignore_ascii_case(".git")) {
        Some("target enters reserved .git or .hydra metadata")
    } else {
        None
    }
}

/// Resolve a selector in the immutable source tree. Cycles have no resolved selector.
///
/// # Errors
/// Returns an error for an escaping, overlong, or broken link target.
pub fn resolve_source_path<'a>(
    entries: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    path: &'a str,
) -> Result<Option<String>, String> {
    let entries = entries.into_iter().collect::<BTreeMap<_, _>>();
    let links = entries
        .iter()
        .filter_map(|(&path, &target)| target.map(|target| (path, target)))
        .collect::<BTreeMap<_, _>>();
    // Ordinary missing selectors retain their discovery diagnostics. Once a selector enters
    // a link, every traversed component must exist in the selected source tree.
    let follows_link = std::iter::successors(Some(path), |path| path.rsplit_once('/').map(|(parent, _)| parent))
        .any(|prefix| links.contains_key(prefix));
    let paths = follows_link.then(|| SourcePaths::new(entries.keys().copied()));
    resolve(path, &links, &mut BTreeMap::new(), paths.as_ref())
        .map_err(|error| format!("symlink {:?}: {}", error.path, error.reason))
        .map(|resolved| {
            resolved.map(|parts| {
                if parts.is_empty() {
                    ".".to_owned()
                } else {
                    parts.join("/")
                }
            })
        })
}

#[derive(Debug)]
struct LinkFailure {
    path: String,
    reason: &'static str,
}

type ResolvedPath<'a> = Result<Option<Vec<&'a str>>, Rc<LinkFailure>>;

fn cache_failure<'a>(
    stack: &[Resolution<'a>],
    cache: &mut BTreeMap<String, ResolvedPath<'a>>,
    failure: Rc<LinkFailure>,
) -> Rc<LinkFailure> {
    for frame in stack {
        if let Some(link) = &frame.link {
            cache.insert(link.clone(), Err(Rc::clone(&failure)));
        }
    }
    failure
}

struct Resolution<'a> {
    path: Vec<&'a str>,
    remaining: std::str::Split<'a, char>,
    link: Option<String>,
}

impl Resolution<'_> {
    fn failure(&self, path: &str, reason: &'static str) -> Rc<LinkFailure> {
        Rc::new(LinkFailure {
            path: self.link.clone().unwrap_or_else(|| path.to_owned()),
            reason,
        })
    }
}

fn resolve<'a>(
    remaining: &'a str,
    links: &BTreeMap<&str, &'a str>,
    cache: &mut BTreeMap<String, ResolvedPath<'a>>,
    paths: Option<&SourcePaths<'_>>,
) -> ResolvedPath<'a> {
    // An explicit stack accepts Git-storable long chains without recursive stack growth.
    let mut stack = vec![Resolution {
        path: Vec::new(),
        remaining: remaining.split('/'),
        link: None,
    }];
    let mut active = BTreeSet::new();
    while let Some(frame) = stack.last_mut() {
        let Some(component) = frame.remaining.next() else {
            let resolved = stack.pop().expect("resolution frame exists");
            if let Some(link) = resolved.link {
                active.remove(&link);
                cache.insert(link, Ok(Some(resolved.path.clone())));
            }
            if let Some(parent) = stack.last_mut() {
                parent.path = resolved.path;
                continue;
            }
            return Ok(Some(resolved.path));
        };
        if let Some(reason) = paths.and_then(|paths| paths.issue(&frame.path, true)) {
            let failure = frame.failure(remaining, reason);
            return Err(cache_failure(&stack, cache, failure));
        }
        match component {
            "" | "." => {}
            ".." => {
                if frame.path.pop().is_none() {
                    let failure = frame.failure(remaining, "target escapes the repository");
                    return Err(cache_failure(&stack, cache, failure));
                }
            }
            component => {
                // Like source manifest paths, only the root .hydra namespace is reserved.
                // Resolve directory aliases and parent components before deciding the depth.
                if component == ".hydra" && frame.path.is_empty() {
                    let failure = frame.failure(remaining, "target enters reserved .git or .hydra metadata");
                    return Err(cache_failure(&stack, cache, failure));
                }
                frame.path.push(component);
                if frame.path.iter().map(|part| part.len() + 1).sum::<usize>() > 4096 {
                    let failure = frame.failure(remaining, "resolved target exceeds 4096 bytes");
                    return Err(cache_failure(&stack, cache, failure));
                }
                if let Some(reason) = paths.and_then(|paths| paths.issue(&frame.path, false)) {
                    let failure = frame.failure(remaining, reason);
                    return Err(cache_failure(&stack, cache, failure));
                }
                let name = frame.path.join("/");
                if let Some(&target) = links.get(name.as_str()) {
                    if let Some(cached) = cache.get(&name) {
                        match cached {
                            Ok(Some(resolved)) => {
                                frame.path.clone_from(resolved);
                                continue;
                            }
                            Err(failure) => {
                                return Err(cache_failure(&stack, cache, Rc::clone(failure)));
                            }
                            Ok(None) => {}
                        }
                    }
                    if cache.contains_key(&name) || !active.insert(name.clone()) {
                        // Cycles are preserved as links. They cannot resolve to an external referent.
                        for link in active {
                            cache.insert(link, Ok(None));
                        }
                        return Ok(None);
                    }
                    frame.path.pop();
                    let path = std::mem::take(&mut frame.path);
                    stack.push(Resolution {
                        path,
                        remaining: target.split('/'),
                        link: Some(name),
                    });
                }
            }
        }
    }
    Ok(Some(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_chains_are_preserved_and_checked_without_dereferencing() {
        let mut entries = (0..1000)
            .map(|index| (format!("link{index:04}"), format!("link{:04}", index + 1)))
            .collect::<Vec<_>>();
        validate_source_links(
            entries
                .iter()
                .map(|(path, target)| (path.as_str(), Some(target.as_str()))),
        )
        .unwrap();
        assert_eq!(
            source_link_warnings(
                entries
                    .iter()
                    .map(|(path, target)| (path.as_str(), Some(target.as_str())))
            )
            .len(),
            1000
        );
        entries.push(("link1000".to_owned(), "../outside".to_owned()));
        let issues = source_link_issues(
            entries
                .iter()
                .map(|(path, target)| (path.as_str(), Some(target.as_str()))),
        )
        .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "link1000");
        assert_eq!(issues[0].target, "../outside");
        assert!(!issues[0].to_string().contains("related link"));
    }

    #[test]
    fn git_links_preserve_missing_targets_cycles_and_parent_components() {
        validate_source_links([
            ("src/file", None),
            ("src/link", Some("../src/file")),
            ("missing", Some("generated/output")),
            ("cycle-a", Some("cycle-b")),
            ("cycle-b", Some("cycle-a")),
            ("dir", Some("src")),
            ("twice", Some("dir/../dir/file")),
        ])
        .unwrap();
    }

    #[test]
    fn warnings_distinguish_missing_targets_cycles_and_non_directory_paths() {
        let entries = [
            ("src/file", None),
            ("src/nested/file", None),
            ("file", Some("src/file")),
            ("directory", Some("src")),
            ("directory-file", Some("directory/nested/file")),
            ("root", Some(".")),
            ("root-file", Some("root/src/file")),
            ("missing", Some("generated/output")),
            ("missing-chain", Some("missing")),
            ("missing-parent", Some("absent/../src/file")),
            ("not-directory", Some("file/../file")),
            ("trailing-slash", Some("file/")),
            ("cycle-a", Some("cycle-b")),
            ("cycle-b", Some("cycle-a")),
        ];
        validate_source_links(entries).unwrap();
        let warnings = source_link_warnings(entries);
        let details = warnings
            .iter()
            .map(|warning| (warning.path.as_str(), warning.reason))
            .collect::<Vec<_>>();
        assert_eq!(
            details,
            [
                ("cycle-a", "target contains a symlink cycle"),
                ("cycle-b", "target contains a symlink cycle"),
                ("missing", "target is missing from the submitted source"),
                ("missing-chain", "target is missing from the submitted source"),
                ("missing-parent", "target is missing from the submitted source"),
                ("not-directory", "target traverses a file as a directory"),
                ("trailing-slash", "target traverses a file as a directory"),
            ]
        );
        assert_eq!(warnings[3].target, "missing");
        assert!(source_link_warnings([("src/file", None), ("alias", Some("src"))]).is_empty());
    }

    #[test]
    fn rejects_escapes_including_directory_links_and_missing_components() {
        for target in [
            "../outside",
            "/outside",
            "C:/outside",
            "C:outside",
            "\\\\host\\share",
            "missing/../../outside",
            ".git/config",
        ] {
            assert!(validate_source_links([("link", Some(target))]).is_err(), "{target}");
        }
        assert!(validate_source_links([("deep/dir", Some("..")), ("link", Some("deep/dir/../outside")),]).is_err());
        assert!(validate_source_links([("link", Some("dir")), ("link/file", None)]).is_err());
    }

    #[test]
    fn hydra_metadata_is_reserved_only_at_the_resolved_repository_root() {
        let entries = [
            ("nested/.hydra/state", None),
            ("nested/link", Some(".hydra/state")),
            ("dir", Some("nested")),
            ("alias", Some("dir/.hydra/state")),
            ("missing", Some("other/.hydra/state")),
        ];
        validate_source_links(entries).unwrap();
        for path in ["nested/link", "alias"] {
            assert_eq!(
                resolve_source_path(entries, path).unwrap().as_deref(),
                Some("nested/.hydra/state")
            );
        }

        for (path, target) in [
            ("link", ".hydra/state"),
            ("link", "./.hydra/state"),
            ("link", ".hydra/../source"),
            ("nested/link", "../.hydra/state"),
            ("link", "nested/../.hydra/state"),
            ("link", "dir/../.hydra/state"),
            ("link", "nested/up/.hydra/state"),
            ("link", "root/.hydra/state"),
            ("link", ".git/config"),
            ("link", "nested/.git/config"),
        ] {
            let entries = [
                ("dir", Some("nested")),
                ("nested/up", Some("..")),
                ("root", Some(".")),
                (path, Some(target)),
            ];
            let issues = source_link_issues(entries).unwrap();
            assert_eq!(issues.len(), 1, "{path} -> {target}");
            assert_eq!(issues[0].path, path);
            assert_eq!(issues[0].target, target);
            assert_eq!(issues[0].reason, "target enters reserved .git or .hydra metadata");
        }
    }

    #[test]
    fn reports_all_causes_and_retains_aliases_after_the_causes_are_excluded() {
        let entries = [
            ("alias", Some("outside")),
            ("another-alias", Some("alias")),
            ("absolute", Some("/external")),
            ("deep/dir", Some("..")),
            ("indirect", Some("deep/dir/../secret")),
            ("metadata", Some(".git/config")),
            ("outside", Some("../external")),
            ("cycle-a", Some("cycle-b")),
            ("cycle-b", Some("cycle-a")),
            ("missing", Some("generated/output")),
        ];
        let issues = source_link_issues(entries).unwrap();
        assert_eq!(
            issues.iter().map(|issue| issue.path.as_str()).collect::<Vec<_>>(),
            ["absolute", "indirect", "metadata", "outside"]
        );
        assert_eq!(
            issues[3].to_string(),
            "\"outside\" -> \"../external\": target escapes the repository"
        );
        let remaining = entries
            .into_iter()
            .filter(|(path, _)| !issues.iter().any(|issue| issue.path == *path))
            .collect::<Vec<_>>();
        validate_source_links(remaining.iter().copied()).unwrap();
        assert!(
            resolve_source_path(remaining, "another-alias")
                .unwrap_err()
                .contains("missing")
        );
    }

    #[test]
    fn invalid_targets_and_diagnostics_are_safe_and_deterministic() {
        let entries = [
            ("tab\tlink", Some("../outside\nname")),
            ("windows", Some("C:/outside")),
            ("reserved", Some(".hydra/state")),
        ];
        let forward = validate_source_links(entries).unwrap_err();
        let reversed = validate_source_links(entries.into_iter().rev()).unwrap_err();
        assert_eq!(forward, reversed);
        assert!(forward.contains("\"tab\\tlink\" -> \"../outside\\nname\""));
        assert!(forward.contains("\"windows\" -> \"C:/outside\""));
        assert!(forward.contains("\"reserved\" -> \".hydra/state\""));
        assert!(!forward.contains('\t'));
    }
}
