use super::*;
use std::fs;
use std::process::Command;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[test]
fn separate_build_roots_preserve_profile_target_and_executable_suffix() {
    let root = std::env::temp_dir();
    let build = root.join("build");
    let target = root.join("final");
    let binary = format!("workspace-helper{}", std::env::consts::EXE_SUFFIX);
    for profile in ["debug", "release", "dev-small"] {
        for triple in [
            None,
            Some("aarch64-unknown-linux-gnu"),
            Some("x86_64-pc-windows-msvc"),
        ] {
            let mut suffix = PathBuf::new();
            if let Some(triple) = triple {
                suffix.push(triple);
            }
            suffix.push(profile);
            suffix.push(&binary);
            assert_eq!(
                relocate_cargo_bin(&build.join(&suffix), Some(&build), Some(&target)),
                Ok(target.join(&suffix))
            );
        }
    }
}

#[test]
fn colocated_or_unspecified_build_root_preserves_existing_fallback() {
    let root = std::env::temp_dir().join("target");
    let candidate = root.join("debug/helper");
    assert_eq!(
        relocate_cargo_bin(&candidate, Some(&root), Some(&root)),
        Ok(candidate.clone())
    );
    assert_eq!(
        relocate_cargo_bin(&candidate, /*build_dir*/ None, Some(&root)),
        Ok(candidate)
    );
}

#[test]
fn separate_build_lookup_rejects_ambiguous_roots_and_layouts() {
    let root = std::env::temp_dir();
    let build = root.join("build");
    let target = root.join("target");
    let candidate = build.join("debug/helper");
    assert!(relocate_cargo_bin(&candidate, Some(&build), /*target_dir*/ None).is_err());
    assert!(relocate_cargo_bin(&candidate, Some(Path::new("build")), Some(&target)).is_err());
    assert!(relocate_cargo_bin(&candidate, Some(&build), Some(Path::new("target"))).is_err());
    assert!(relocate_cargo_bin(&candidate, Some(&build), Some(&target.join("../other"))).is_err());
    for suffix in ["helper", "debug/deps/package/helper", "../debug/helper"] {
        assert!(relocate_cargo_bin(&build.join(suffix), Some(&build), Some(&target)).is_err());
    }
    assert!(
        relocate_cargo_bin(
            &root.join("build-other/debug/helper"),
            Some(&build),
            Some(&target)
        )
        .is_err()
    );
}

#[test]
fn colocated_declared_roots_still_require_candidate_containment_and_layout() {
    let root = std::env::temp_dir();
    let target = root.join("target");
    let outside = root.join("stale-target/debug/helper");
    assert!(relocate_cargo_bin(&outside, Some(&target), Some(&target)).is_err());
    for suffix in ["helper", "debug/deps/package/helper", "../debug/helper"] {
        assert!(relocate_cargo_bin(&target.join(suffix), Some(&target), Some(&target)).is_err());
    }
}

#[test]
fn runtime_keys_preserve_bazel_and_nextest_remapping_precedence() {
    assert_eq!(
        cargo_bin_env_keys("test-helper", &["CARGO_BIN_EXE"]),
        ["CARGO_BIN_EXE_test-helper", "CARGO_BIN_EXE_test_helper"]
    );
    assert_eq!(
        cargo_bin_env_keys("test-helper", &["NEXTEST_BIN_EXE", "CARGO_BIN_EXE"]),
        [
            "NEXTEST_BIN_EXE_test-helper",
            "NEXTEST_BIN_EXE_test_helper",
            "CARGO_BIN_EXE_test-helper",
            "CARGO_BIN_EXE_test_helper",
        ]
    );
}

// Each process receives its own environment; these tests never mutate the
// environment of the shared test process or execute the synthetic target file.
#[test]
fn public_lookup_finds_cross_package_binary_in_separate_target_root() {
    const NAME: &str = "codex-resolver-cross-package-fixture";
    if let Some(expected) = std::env::var_os("CODEX_CARGO_BIN_EXPECTED") {
        assert_eq!(cargo_bin(NAME).unwrap(), PathBuf::from(expected));
        return;
    }
    let temp = Scratch::new();
    let executable = std::env::current_exe().unwrap();
    let mut profile_dir = executable.parent().unwrap();
    if profile_dir.ends_with("deps") {
        profile_dir = profile_dir.parent().unwrap();
    }
    let build = profile_dir.parent().unwrap();
    let profile = profile_dir.file_name().unwrap();
    let expected = temp
        .0
        .join(profile)
        .join(format!("{NAME}{}", std::env::consts::EXE_SUFFIX));
    fs::create_dir_all(expected.parent().unwrap()).unwrap();
    fs::write(&expected, b"synthetic lookup fixture; never executed").unwrap();
    let mut command =
        child_test("tests::public_lookup_finds_cross_package_binary_in_separate_target_root");
    command
        .env("CARGO_BUILD_BUILD_DIR", build)
        .env("CARGO_TARGET_DIR", &temp.0)
        .env("CODEX_CARGO_BIN_EXPECTED", &expected);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn public_lookup_prefers_remapped_nextest_path_and_rejects_missing_override() {
    const NAME: &str = "codex-resolver-remapped-fixture";
    if let Some(expected) = std::env::var_os("CODEX_CARGO_BIN_EXPECTED") {
        let expected = PathBuf::from(expected);
        if expected.is_file() {
            assert_eq!(cargo_bin(NAME).unwrap(), expected);
        } else {
            assert!(matches!(
                cargo_bin(NAME),
                Err(CargoBinError::ResolvedPathDoesNotExist { key, path })
                    if key == format!("NEXTEST_BIN_EXE_{NAME}") && path == expected
            ));
        }
        return;
    }
    let temp = Scratch::new();
    let executable = std::env::current_exe().unwrap();
    let remapped = temp.0.join("remapped-binary");
    fs::write(&remapped, b"remapped lookup fixture; never executed").unwrap();
    // An authoritative missing path or directory must not fall through to a
    // different, existing Cargo binary and silently run the wrong artifact.
    for expected in [&remapped, &temp.0.join("missing"), &temp.0] {
        let output = child_test(
            "tests::public_lookup_prefers_remapped_nextest_path_and_rejects_missing_override",
        )
        .env(format!("NEXTEST_BIN_EXE_{NAME}"), expected)
        .env(format!("CARGO_BIN_EXE_{NAME}"), &executable)
        .env("CODEX_CARGO_BIN_EXPECTED", expected)
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn child_test(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", name, "--nocapture"]);
    // Exercise Cargo resolution even when this test binary came from Bazel.
    command.env_remove(RUNFILES_MANIFEST_ONLY_ENV);
    command
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "codex-cargo-bin-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(/*val*/ 1, Ordering::Relaxed)
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove owned binary lookup fixture");
    }
}
