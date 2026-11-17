//! Refresh only skills already installed and still owned by the Super installer.
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Result, ensure};
use super_types::source_sha256;

const BASE: &str = "https://docs.schematic.tech/.well-known/skills/schematic-supertests";
const MANIFEST: &str = ".super-installer-manifest";
const MAX_FILE: usize = 512 * 1024;
const MAX_TOTAL: usize = 8 * 1024 * 1024;

type Manifest = BTreeMap<String, String>;

fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    ensure!(bytes.len() <= 64 * 1024, "skill manifest is too large");
    let mut files = BTreeMap::new();
    for line in std::str::from_utf8(bytes)?.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(fields.len() == 2, "invalid skill manifest entry");
        let (hash, path) = (fields[0], fields[1]);
        ensure!(
            hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "invalid skill hash"
        );
        ensure!(
            Path::new(path).extension().is_some_and(|extension| extension == "md")
                && path.bytes().all(|c| c.is_ascii_alphanumeric() || b"_./-".contains(&c))
                && path.split('/').all(|part| !part.is_empty() && !part.starts_with('.'))
                && !path.contains(".."),
            "invalid skill path"
        );
        ensure!(files.insert(path.into(), hash.into()).is_none(), "duplicate skill file");
    }
    ensure!(
        !files.is_empty() && files.len() <= 100 && files.contains_key("SKILL.md"),
        "invalid skill file set"
    );
    Ok(files)
}

fn regular_files(root: &Path, directory: &Path, files: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(!kind.is_symlink(), "custom skill symlink");
        if kind.is_dir() {
            regular_files(root, &entry.path(), files)?;
        } else {
            ensure!(kind.is_file(), "custom skill file");
            files.push(entry.path().strip_prefix(root)?.to_string_lossy().replace('\\', "/"));
        }
        ensure!(files.len() <= 101, "custom skill files");
    }
    Ok(())
}

fn managed(target: &Path) -> Result<Vec<u8>> {
    ensure!(
        !fs::symlink_metadata(target)?.file_type().is_symlink(),
        "custom skill symlink"
    );
    let manifest = fs::read(target.join(MANIFEST))?;
    let entries = parse_manifest(&manifest)?;
    let mut files = Vec::new();
    regular_files(target, target, &mut files)?;
    ensure!(files.len() == entries.len() + 1, "skill has local additions");
    for (path, hash) in entries {
        let bytes = fs::read(target.join(path))?;
        ensure!(source_sha256(&bytes) == hash, "skill has local edits");
    }
    Ok(manifest)
}

fn targets() -> Vec<PathBuf> {
    let Some(base) = directories::BaseDirs::new() else {
        return Vec::new();
    };
    let home = base.home_dir();
    let configured = |name: &str, fallback: PathBuf| std::env::var_os(name).map_or(fallback, PathBuf::from);
    let mut parents = vec![
        home.join(".agents/skills"),
        configured("CODEX_HOME", home.join(".codex")).join("skills"),
        configured("CLAUDE_CONFIG_DIR", home.join(".claude")).join("skills"),
        configured("XDG_CONFIG_HOME", home.join(".config")).join("opencode/skills"),
    ];
    for relative in [
        ".cursor/skills",
        ".copilot/skills",
        ".gemini/skills",
        ".codeium/windsurf/skills",
    ] {
        parents.push(home.join(relative));
    }
    let mut targets: Vec<_> = parents
        .into_iter()
        .map(|parent| parent.join("schematic-supertests"))
        .collect();
    targets.sort();
    targets.dedup();
    targets
}

async fn download(client: &reqwest::Client, url: &str, max: usize) -> Result<Vec<u8>> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(bytes.len() + chunk.len() <= max, "skill download is too large");
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

struct InstallLock(PathBuf);
impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

fn replace(target: &Path, previous: &[u8], manifest: &[u8], files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let parent = target.parent().ok_or_else(|| anyhow::anyhow!("missing skill parent"))?;
    let lock = parent.join(".super-supertests.install-lock");
    fs::create_dir(&lock)?;
    let _lock = InstallLock(lock);
    // Recheck ownership under the installer lock, after all network work.
    ensure!(managed(target)? == previous, "skill changed during refresh");
    let stage = tempfile::Builder::new().prefix(".super-skill.").tempdir_in(parent)?;
    for (path, bytes) in files {
        let destination = stage.path().join(path);
        fs::create_dir_all(destination.parent().expect("skill file has a parent"))?;
        fs::write(destination, bytes)?;
    }
    fs::write(stage.path().join(MANIFEST), manifest)?;
    let backup = tempfile::Builder::new()
        .prefix(".schematic-supertests.backup.")
        .tempdir_in(parent)?;
    fs::rename(target, backup.path().join("skill"))?;
    if let Err(error) = fs::rename(stage.path(), target) {
        // Keep the recoverable backup if restoring the old directory also fails.
        if fs::rename(backup.path().join("skill"), target).is_err() {
            let _ = backup.keep();
        }
        return Err(error.into());
    }
    Ok(())
}

async fn refresh_from(base: &str, targets: &[PathBuf]) -> Result<()> {
    let installed: Vec<_> = targets
        .iter()
        .filter_map(|target| managed(target).ok().map(|manifest| (target, manifest)))
        .collect();
    if installed.is_empty() {
        return Ok(());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .https_only(base.starts_with("https:"))
        .build()?;
    let index = download(&client, &format!("{base}/install.txt"), 64 * 1024).await?;
    let split = index
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| anyhow::anyhow!("missing skill version"))?;
    let manifest = &index[split + 1..];
    ensure!(
        std::str::from_utf8(&index[..split])? == format!("version {}", source_sha256(manifest)),
        "skill version mismatch"
    );
    let entries = parse_manifest(manifest)?;
    if installed.iter().all(|(_, previous)| previous == manifest) {
        return Ok(());
    }
    let version = source_sha256(manifest);
    let mut files = BTreeMap::new();
    let mut total = 0;
    for (path, hash) in entries {
        let bytes = download(&client, &format!("{base}/versions/{version}/{path}"), MAX_FILE).await?;
        total += bytes.len();
        ensure!(
            total <= MAX_TOTAL && source_sha256(&bytes) == hash,
            "skill integrity mismatch"
        );
        files.insert(path, bytes);
    }
    for (target, previous) in installed {
        if previous != manifest {
            let _ = replace(target, &previous, manifest, &files);
        }
    }
    Ok(())
}

pub async fn refresh_installed() {
    if std::env::var_os("SUPER_NO_UPDATE_CHECK").is_some() {
        return;
    }
    // Independent of CLI versions; skill-only releases are refreshed too.
    let _ = tokio::time::timeout(Duration::from_mins(1), refresh_from(BASE, &targets())).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    fn fixture(root: &Path, text: &str) -> (PathBuf, Vec<u8>) {
        let target = root.join("schematic-supertests");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("SKILL.md"), text).unwrap();
        let manifest = format!("{}  SKILL.md\n", source_sha256(text.as_bytes())).into_bytes();
        fs::write(target.join(MANIFEST), &manifest).unwrap();
        (target, manifest)
    }
    #[test]
    fn validates_manifest_paths_hashes_duplicates_and_required_entry() {
        let hash = "a".repeat(64);
        for path in [
            "../SKILL.md",
            "/SKILL.md",
            "a/../SKILL.md",
            "a\\SKILL.md",
            ".hidden.md",
            "a//b.md",
        ] {
            assert!(parse_manifest(format!("{hash} {path}\n").as_bytes()).is_err());
        }
        assert!(parse_manifest(format!("{hash} SKILL.md\n{hash} SKILL.md\n").as_bytes()).is_err());
        assert!(parse_manifest(format!("{hash} references/example.md\n").as_bytes()).is_err());
    }
    #[test]
    fn updates_stale_installer_owned_skill_without_touching_customized_skills() {
        let root = tempfile::tempdir().unwrap();
        let (target, previous) = fixture(root.path(), "old");
        let files = BTreeMap::from([
            ("SKILL.md".into(), b"new".to_vec()),
            ("references/mcp.md".into(), b"MCP".to_vec()),
        ]);
        let mut manifest = String::new();
        for (path, bytes) in &files {
            writeln!(manifest, "{}  {path}", source_sha256(bytes)).unwrap();
        }
        let manifest = manifest.into_bytes();
        replace(&target, &previous, &manifest, &files).unwrap();
        assert_eq!(managed(&target).unwrap(), manifest);
        assert_eq!(fs::read(target.join("references/mcp.md")).unwrap(), b"MCP");
        fs::write(target.join("SKILL.md"), "local edits").unwrap();
        assert!(replace(&target, &manifest, &previous, &files).is_err());
        assert_eq!(fs::read_to_string(target.join("SKILL.md")).unwrap(), "local edits");
        assert!(!root.path().join(".super-supertests.install-lock").exists());
    }
    #[test]
    fn installer_lock_and_local_additions_prevent_background_replacement() {
        let root = tempfile::tempdir().unwrap();
        let (target, previous) = fixture(root.path(), "old");
        fs::create_dir(root.path().join(".super-supertests.install-lock")).unwrap();
        assert!(replace(&target, &previous, b"", &BTreeMap::new()).is_err());
        assert_eq!(fs::read_to_string(target.join("SKILL.md")).unwrap(), "old");
        fs::write(target.join("notes.md"), "custom").unwrap();
        assert!(managed(&target).is_err());
    }
    #[tokio::test]
    async fn skips_network_when_no_managed_skill_is_installed() {
        let root = tempfile::tempdir().unwrap();
        refresh_from("http://127.0.0.1:1", &[root.path().join("missing")])
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn downloads_verified_updates_and_preserves_previous_skill_on_corruption() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for corrupt in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (target, _) = fixture(root.path(), "old");
            let manifest = format!("{}  SKILL.md\n", source_sha256(b"new"));
            let version = source_sha256(manifest.as_bytes());
            let index = format!("version {version}\n{manifest}");
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                for (path, body) in [
                    ("/install.txt".to_owned(), index),
                    (
                        format!("/versions/{version}/SKILL.md"),
                        if corrupt { "bad" } else { "new" }.into(),
                    ),
                ] {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(socket.read_u8().await.unwrap());
                    }
                    let request = String::from_utf8(request).unwrap();
                    assert!(request.starts_with(&format!("GET {path} ")));
                    assert!(!request.to_lowercase().contains("authorization:"));
                    socket
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                }
            });
            let result = refresh_from(&base, std::slice::from_ref(&target)).await;
            assert_eq!(result.is_err(), corrupt);
            assert_eq!(
                fs::read_to_string(target.join("SKILL.md")).unwrap(),
                if corrupt { "old" } else { "new" }
            );
            assert!(managed(&target).is_ok());
            server.await.unwrap();
        }
    }
}
