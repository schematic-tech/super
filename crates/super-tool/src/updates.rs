use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

use anyhow::{Result, ensure};
use fs2::FileExt;
use semver::Version;
use serde::Deserialize;

use crate::{config::StateStore, output::Ui};

const RELEASES_URL: &str = "https://get.schematic.tech/releases.json";
const REFRESH_INTERVAL: Duration = Duration::from_hours(1);
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const CACHE_FILE: &str = "releases.json";
const ATTEMPT_FILE: &str = "last-attempt";
const LOCK_FILE: &str = "refresh.lock";

#[derive(Deserialize)]
struct Release {
    version: Version,
    #[serde(default, deserialize_with = "deserialize_alert")]
    alert: Option<String>,
}

fn deserialize_alert<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

impl Release {
    fn is_newer(&self, current: &str) -> bool {
        // Build metadata does not change precedence; stable clients stay off prereleases.
        self.version.pre.is_empty()
            && Version::parse(current).is_ok_and(|current| self.version.cmp_precedence(&current).is_gt())
    }
}

pub fn on_startup(ui: &mut Ui) {
    let Ok(store) = StateStore::discover() else {
        return;
    };
    let directory = store.profile_directory().join("updates");
    // The foreground only reads the last complete cache. All network work belongs
    // to a short-lived child, so a slow or unavailable server cannot delay the CLI.
    if let Some(release) = cached_release(&directory) {
        // An operator notice may replace the installation instructions themselves.
        let alerted = release
            .alert
            .as_deref()
            .is_some_and(|message| ui.release_alert(message));
        if !alerted
            && std::env::var_os("SUPER_NO_UPDATE_CHECK").is_none()
            && release.is_newer(env!("CARGO_PKG_VERSION"))
        {
            ui.update_available(env!("CARGO_PKG_VERSION"), &release.version);
        }
    }
    let _ = spawn_refresh(&directory);
}

fn parse_manifest(bytes: &[u8]) -> Result<BTreeMap<String, Release>> {
    Ok(serde_json::from_slice(bytes)?)
}

fn cached_release(directory: &Path) -> Option<Release> {
    let mut bytes = Vec::new();
    File::open(directory.join(CACHE_FILE))
        .ok()?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return None;
    }
    parse_manifest(&bytes).ok()?.remove("super-tool")
}

fn fresh(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < REFRESH_INTERVAL)
}

fn refresh_due(directory: &Path) -> bool {
    !fresh(&directory.join(CACHE_FILE)) && !fresh(&directory.join(ATTEMPT_FILE))
}

fn refresh_lock(directory: &Path) -> Result<Option<File>> {
    fs::create_dir_all(directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(LOCK_FILE))?;
    Ok(lock.try_lock_exclusive().is_ok().then_some(lock))
}

fn spawn_refresh(directory: &Path) -> Result<()> {
    spawn_refresh_with(directory, &std::env::current_exe()?)
}

fn spawn_refresh_with(directory: &Path, executable: &Path) -> Result<()> {
    if !refresh_due(directory) {
        return Ok(());
    }
    let Some(lock) = refresh_lock(directory)? else {
        return Ok(());
    };
    if !refresh_due(directory) {
        return Ok(());
    }
    let directory = directory.canonicalize()?;
    // Reserve this hour before spawning. Concurrent startups and failed fetches
    // must not cause a process/request storm. The OS releases the lock on a crash.
    File::create(directory.join(ATTEMPT_FILE))?.set_modified(SystemTime::now())?;
    drop(lock);
    let mut command = Command::new(executable);
    command
        .arg("refresh-releases")
        .arg(&directory)
        .current_dir(&directory)
        .env_remove("SUPER_ACCESS_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let mut child = command.spawn()?;
    // Reap the child if the CLI remains running; this thread never delays shutdown.
    let _ = std::thread::Builder::new()
        .name("super-release-refresh".into())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(())
}

pub async fn refresh_detached(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    nix::unistd::setsid()?;
    refresh(directory, RELEASES_URL, FETCH_TIMEOUT).await
}

async fn refresh(directory: &Path, url: &str, timeout: Duration) -> Result<()> {
    let Some(_lock) = refresh_lock(directory)? else {
        return Ok(());
    };
    if fresh(&directory.join(CACHE_FILE)) {
        return Ok(());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .user_agent(concat!("super/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    if let Some(length) = response.content_length() {
        ensure!(length <= MAX_MANIFEST_BYTES, "release manifest is too large");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() as u64 + chunk.len() as u64 <= MAX_MANIFEST_BYTES,
            "release manifest is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    parse_manifest(&bytes)?;
    // A temporary file on the same filesystem makes the rename atomic. A failed
    // download or malformed manifest leaves the previous complete cache intact.
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(directory.join(CACHE_FILE))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn assert_lock_released(directory: &Path) {
        // Other parallel tests fork Git children. Until exec closes inherited FDs,
        // a child can briefly keep our file lock alive after the owning future drops.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while refresh_lock(directory).unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "refresh lock was not released");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn stale_cache(directory: &Path) -> Vec<u8> {
        let bytes = br#"{"super-tool":{"version":"0.4.2"},"another-tool":{"version":"2.0.0"}}"#.to_vec();
        fs::write(directory.join(CACHE_FILE), &bytes).unwrap();
        OpenOptions::new()
            .write(true)
            .open(directory.join(CACHE_FILE))
            .unwrap()
            .set_modified(SystemTime::now() - REFRESH_INTERVAL - Duration::from_secs(1))
            .unwrap();
        bytes
    }

    #[test]
    fn compares_semver_precedence_and_ignores_prereleases_and_other_tools() {
        let directory = tempfile::tempdir().unwrap();
        for (latest, current, expected) in [
            ("0.4.10", "0.4.9", true),
            ("0.5.0", "0.4.10", true),
            ("0.5.0", "0.5.0", false),
            ("0.4.2", "0.5.0", false),
            ("0.5.0+build.2", "0.5.0+build.1", false),
            ("0.5.0", "0.5.0-rc.1", true),
            ("0.6.0-rc.1", "0.5.0", false),
        ] {
            fs::write(
                directory.path().join(CACHE_FILE),
                serde_json::json!({"super-tool": {"version": latest}}).to_string(),
            )
            .unwrap();
            assert_eq!(
                cached_release(directory.path()).unwrap().is_newer(current),
                expected,
                "{latest} / {current}"
            );
        }
        for invalid in [
            "{",
            "[]",
            r#"{"super-tool":"0.5.0"}"#,
            r#"{"super-tool":"latest"}"#,
            r#"{"super-tool":7}"#,
            r#"{"super-tool":{"alert":"notice"}}"#,
            r#"{"super-tool":{"version":"latest"}}"#,
            r#"{"super-tool":{"version":"0.5.0","alert":null}}"#,
            r#"{"super-tool":{"version":"0.5.0","alert":7}}"#,
            r#"{"other-tool":{"version":"999.0.0"}}"#,
            r#"{"pup-tool":{"version":"999.0.0"}}"#,
            r#"{"super-tool":"9.0.0\nrun this"}"#,
        ] {
            fs::write(directory.path().join(CACHE_FILE), invalid).unwrap();
            assert!(cached_release(directory.path()).is_none());
        }
        fs::remove_file(directory.path().join(CACHE_FILE)).unwrap();
        assert!(cached_release(directory.path()).is_none());
    }

    #[test]
    fn alerts_are_independent_of_version_precedence() {
        for version in ["0.4.0", "0.5.0", "0.6.0-rc.1", "0.6.0"] {
            let manifest = serde_json::json!({
                "super-tool": {"version": version, "alert": "Installation is changing.", "future": true},
                "another-tool": {"version": "2.0.0"}
            });
            let mut releases = parse_manifest(manifest.to_string().as_bytes()).unwrap();
            let release = releases.remove("super-tool").unwrap();
            assert_eq!(release.version.to_string(), version);
            assert_eq!(release.alert.as_deref(), Some("Installation is changing."));
            assert!(releases["another-tool"].alert.is_none());
        }
    }

    #[test]
    fn refresh_is_hourly_including_failed_attempts_and_recovers_after_crashes() {
        let directory = tempfile::tempdir().unwrap();
        assert!(refresh_due(directory.path()));
        stale_cache(directory.path());
        assert!(refresh_due(directory.path()));
        let attempt = File::create(directory.path().join(ATTEMPT_FILE)).unwrap();
        assert!(!refresh_due(directory.path()));
        attempt
            .set_modified(SystemTime::now() - REFRESH_INTERVAL - Duration::from_secs(1))
            .unwrap();
        assert!(refresh_due(directory.path()));
        let lock = refresh_lock(directory.path()).unwrap().unwrap();
        assert!(refresh_lock(directory.path()).unwrap().is_none());
        drop(lock);
        assert_lock_released(directory.path());
        fs::write(directory.path().join(CACHE_FILE), b"{}").unwrap();
        assert!(!refresh_due(directory.path()));
    }

    async fn server(status: u16, body: Vec<u8>, delay: Duration) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/releases.json", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            assert!(request.starts_with(b"GET /releases.json "));
            assert!(
                !String::from_utf8_lossy(&request)
                    .to_lowercase()
                    .contains("authorization:")
            );
            tokio::time::sleep(delay).await;
            let header = format!(
                "HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(header.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        });
        (url, server)
    }

    #[tokio::test]
    async fn replaces_a_complete_manifest_and_preserves_cache_during_the_download() {
        let directory = tempfile::tempdir().unwrap();
        let before = stale_cache(directory.path());
        let after = br#"{"super-tool":{"version":"0.5.0","alert":"Installation is changing."},"another-tool":{"version":"3.0.0"}}"#.to_vec();
        let (url, server) = server(200, after.clone(), Duration::from_millis(100)).await;
        let refresh = refresh(directory.path(), &url, FETCH_TIMEOUT);
        let reader = async {
            while !server.is_finished() {
                let bytes = fs::read(directory.path().join(CACHE_FILE)).unwrap();
                assert!(bytes == before || bytes == after);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        let (result, ()) = tokio::join!(refresh, reader);
        result.unwrap();
        server.await.unwrap();
        assert_eq!(fs::read(directory.path().join(CACHE_FILE)).unwrap(), after);
        assert!(!refresh_due(directory.path()));
    }

    #[tokio::test]
    async fn refreshing_without_an_alert_clears_the_cached_notice() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join(CACHE_FILE),
            br#"{"super-tool":{"version":"0.5.0","alert":"Installation is changing."}}"#,
        )
        .unwrap();
        File::options()
            .write(true)
            .open(directory.path().join(CACHE_FILE))
            .unwrap()
            .set_modified(SystemTime::now() - REFRESH_INTERVAL - Duration::from_secs(1))
            .unwrap();
        assert!(cached_release(directory.path()).unwrap().alert.is_some());
        let (url, server) = server(200, br#"{"super-tool":{"version":"0.5.0"}}"#.to_vec(), Duration::ZERO).await;
        refresh(directory.path(), &url, FETCH_TIMEOUT).await.unwrap();
        server.await.unwrap();
        assert!(cached_release(directory.path()).unwrap().alert.is_none());
    }

    #[tokio::test]
    async fn errors_invalid_versions_and_oversized_responses_preserve_previous_cache() {
        for (status, body) in [
            (404, b"not found".to_vec()),
            (500, b"unavailable".to_vec()),
            (200, b"{".to_vec()),
            (200, br#"{"super-tool":"0.5.0"}"#.to_vec()),
            (200, br#"{"super-tool":"not-semver"}"#.to_vec()),
            (200, br#"{"super-tool":{"version":"0.5.0","alert":false}}"#.to_vec()),
            (200, vec![b' '; usize::try_from(MAX_MANIFEST_BYTES + 1).unwrap()]),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let before = stale_cache(directory.path());
            let (url, server) = server(status, body, Duration::ZERO).await;
            assert!(refresh(directory.path(), &url, FETCH_TIMEOUT).await.is_err());
            server.await.unwrap();
            assert_eq!(fs::read(directory.path().join(CACHE_FILE)).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn stalled_fetches_time_out_and_release_the_lock() {
        let directory = tempfile::tempdir().unwrap();
        let before = stale_cache(directory.path());
        let (url, server) = server(200, b"{}".to_vec(), Duration::from_secs(10)).await;
        assert!(
            refresh(directory.path(), &url, Duration::from_millis(100))
                .await
                .is_err()
        );
        server.abort();
        assert_eq!(fs::read(directory.path().join(CACHE_FILE)).unwrap(), before);
        assert_lock_released(directory.path());
    }

    #[cfg(unix)]
    #[test]
    fn simultaneous_startups_spawn_once_and_do_not_wait_for_the_worker() {
        use std::sync::{Arc, Barrier};

        let directory = tempfile::tempdir().unwrap();
        // Run the existing shell with our helper argument as its script filename.
        // Executing a freshly written file can race with other tests' forked processes
        // briefly inheriting its writable descriptor and fail with ETXTBSY on Linux.
        let executable = Path::new("/bin/sh");
        fs::write(
            directory.path().join("refresh-releases"),
            "printf 'started\\n' >> starts\nwhile [ ! -f finish ]; do sleep 0.01; done\nprintf 'done' > completed\n",
        )
        .unwrap();
        let barrier = Arc::new(Barrier::new(8));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let barrier = &barrier;
                let directory = directory.path();
                scope.spawn(move || {
                    barrier.wait();
                    spawn_refresh_with(directory, executable).unwrap();
                });
            }
        });
        // Every foreground call returned while the child still waits for its signal.
        assert!(!directory.path().join("completed").exists());
        fs::write(directory.path().join("finish"), b"").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !directory.path().join("completed").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(directory.path().join("starts")).unwrap(),
            "started\n"
        );
    }
}
