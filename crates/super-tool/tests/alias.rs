use std::process::Command;

#[test]
fn both_executables_share_the_canonical_help_version_and_local_profile() {
    let profile = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(profile.path().join("updates")).unwrap();
    std::fs::write(profile.path().join("updates/releases.json"), b"{}").unwrap();
    for args in [["--help"], ["--version"], ["logout"]] {
        let outputs: Vec<_> = [env!("CARGO_BIN_EXE_super"), env!("CARGO_BIN_EXE_sup")]
            .into_iter()
            .map(|binary| {
                Command::new(binary)
                    .args(args)
                    .env("SUPER_CONFIG_DIR", profile.path())
                    .env("SUPER_NO_UPDATE_CHECK", "1")
                    .env("NO_COLOR", "1")
                    .output()
                    .unwrap()
            })
            .collect();
        for output in &outputs {
            assert!(output.status.success(), "{output:?}");
        }
        assert_eq!(outputs[0].stdout, outputs[1].stdout);
        assert_eq!(outputs[0].stderr, outputs[1].stderr);
        if args == ["--version"] {
            assert_eq!(
                String::from_utf8_lossy(&outputs[0].stdout),
                concat!("super ", env!("CARGO_PKG_VERSION"), "\n")
            );
        }
    }
}
