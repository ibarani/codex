use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::path::PathBuf;

pub use runfiles;

/// Bazel sets this when runfiles directories are disabled, which we do on all platforms for consistency.
const RUNFILES_MANIFEST_ONLY_ENV: &str = "RUNFILES_MANIFEST_ONLY";

#[derive(Debug, thiserror::Error)]
pub enum CargoBinError {
    #[error("failed to read current exe")]
    CurrentExe {
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read current directory")]
    CurrentDir {
        #[source]
        source: std::io::Error,
    },
    #[error("binary path from {key} resolved to {path:?}, but it is not a file")]
    ResolvedPathDoesNotExist { key: String, path: PathBuf },
    #[error("could not locate binary {name:?}; tried env vars {env_keys:?}; {fallback}")]
    NotFound {
        name: String,
        env_keys: Vec<String>,
        fallback: String,
    },
}

/// Returns an absolute path to a binary target built for the current test run.
///
/// Bazel's `CARGO_BIN_EXE_*` values are resolved through runfiles. Otherwise,
/// nextest's remapped `NEXTEST_BIN_EXE_*` paths precede Cargo's absolute paths.
/// These variables cover the current package, not every workspace binary.
///
/// Cross-package lookup retains the existing Cargo layout fallback. With a
/// separate build directory, both `CARGO_BUILD_BUILD_DIR` and `CARGO_TARGET_DIR`
/// must be absolute: the profile and optional target suffix are carried from
/// the former to the latter. Relative roots and unknown layouts are rejected.
/// This function never builds a binary or searches `PATH`.
#[allow(deprecated)]
pub fn cargo_bin(name: &str) -> Result<PathBuf, CargoBinError> {
    let prefixes = if runfiles_available() {
        &["CARGO_BIN_EXE"][..]
    } else {
        &["NEXTEST_BIN_EXE", "CARGO_BIN_EXE"][..]
    };
    let env_keys = cargo_bin_env_keys(name, prefixes);
    for key in &env_keys {
        if let Some(value) = std::env::var_os(key) {
            return resolve_bin_from_env(key, value);
        }
    }
    // The existing fallback knows the running test's profile and target suffix,
    // but assert_cmd assumes final binaries also live in that build directory.
    let candidate = assert_cmd::cargo::cargo_bin(name);
    let build_dir = std::env::var_os("CARGO_BUILD_BUILD_DIR").map(PathBuf::from);
    let target_dir = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
    let path = relocate_cargo_bin(&candidate, build_dir.as_deref(), target_dir.as_deref())
        .map_err(|message| CargoBinError::NotFound {
            name: name.to_owned(),
            env_keys: env_keys.clone(),
            fallback: message.to_owned(),
        })?;
    if path.is_file() {
        Ok(path)
    } else {
        Err(CargoBinError::ResolvedPathDoesNotExist {
            key: "Cargo layout fallback".to_owned(),
            path,
        })
    }
}

fn cargo_bin_env_keys(name: &str, prefixes: &[&str]) -> Vec<String> {
    let mut keys = Vec::with_capacity(prefixes.len() * 2);
    let underscore_name = name.replace('-', "_");
    for prefix in prefixes {
        keys.push(format!("{prefix}_{name}"));
        if underscore_name != name {
            keys.push(format!("{prefix}_{underscore_name}"));
        }
    }
    keys
}

fn relocate_cargo_bin(
    candidate: &Path,
    build_dir: Option<&Path>,
    target_dir: Option<&Path>,
) -> Result<PathBuf, &'static str> {
    let Some(build_dir) = build_dir else {
        return Ok(candidate.to_path_buf());
    };
    let Some(target_dir) = target_dir else {
        return Err("separate Cargo build-dir lookup requires CARGO_TARGET_DIR");
    };
    if !build_dir.is_absolute() || !target_dir.is_absolute() {
        return Err("Cargo build and target roots must be absolute for binary lookup");
    }
    if build_dir
        .components()
        .chain(target_dir.components())
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("Cargo build and target roots must not contain parent traversal");
    }
    let suffix = candidate.strip_prefix(build_dir).map_err(
        |_| "Cargo fallback binary is outside the declared build root; check remapped roots",
    )?;
    let parts: Vec<_> = suffix.components().collect();
    if !matches!(parts.len(), 2 | 3)
        || parts
            .iter()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(
            "unsupported Cargo binary layout; expected profile/name or target/profile/name",
        );
    }
    Ok(target_dir.join(suffix))
}

pub fn runfiles_available() -> bool {
    std::env::var_os(RUNFILES_MANIFEST_ONLY_ENV).is_some()
}

fn resolve_bin_from_env(key: &str, value: OsString) -> Result<PathBuf, CargoBinError> {
    let raw = PathBuf::from(&value);
    if runfiles_available() {
        let runfiles = runfiles::Runfiles::create().map_err(|err| CargoBinError::CurrentExe {
            source: std::io::Error::other(err),
        })?;
        if let Some(mut resolved) = runfiles::rlocation!(runfiles, &raw) {
            if !resolved.is_absolute() {
                resolved = std::env::current_dir()
                    .map_err(|source| CargoBinError::CurrentDir { source })?
                    .join(resolved);
            }
            if resolved.is_file() {
                return Ok(resolved);
            }
        }
    } else if raw.is_absolute() && raw.is_file() {
        return Ok(raw);
    }

    Err(CargoBinError::ResolvedPathDoesNotExist {
        key: key.to_owned(),
        path: raw,
    })
}

/// Macro that derives the path to a test resource at runtime, the value of
/// which depends on whether Cargo or Bazel is being used to build and run a
/// test. Note the return value may be a relative or absolute path.
/// (Incidentally, this is a macro rather than a function because it reads
/// compile-time environment variables that need to be captured at the call
/// site.)
///
/// This is expected to be used exclusively in test code because Codex CLI is a
/// standalone binary with no packaged resources.
#[macro_export]
macro_rules! find_resource {
    ($resource:expr) => {{
        let resource = std::path::Path::new(&$resource);
        if $crate::runfiles_available() {
            // When this code is built and run with Bazel:
            // - we inject `BAZEL_PACKAGE` as a compile-time environment variable
            //   that points to native.package_name()
            // - at runtime, Bazel will set runfiles-related env vars
            $crate::resolve_bazel_runfile(option_env!("BAZEL_PACKAGE"), resource)
        } else {
            let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            Ok(manifest_dir.join(resource))
        }
    }};
}

pub fn resolve_bazel_runfile(
    bazel_package: Option<&str>,
    resource: &Path,
) -> std::io::Result<PathBuf> {
    let runfiles = runfiles::Runfiles::create()
        .map_err(|err| std::io::Error::other(format!("failed to create runfiles: {err}")))?;
    let runfile_path = match bazel_package {
        Some(bazel_package) => PathBuf::from("_main").join(bazel_package).join(resource),
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "BAZEL_PACKAGE was not set at compile time",
            ));
        }
    };
    let runfile_path = normalize_runfile_path(&runfile_path);
    if let Some(resolved) = runfiles::rlocation!(runfiles, &runfile_path)
        && resolved.exists()
    {
        return Ok(resolved);
    }
    let runfile_path_display = runfile_path.display();
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("runfile does not exist at: {runfile_path_display}"),
    ))
}

pub fn resolve_cargo_runfile(resource: &Path) -> std::io::Result<PathBuf> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest_dir.join(resource))
}

pub fn repo_root() -> io::Result<PathBuf> {
    let marker = if runfiles_available() {
        let runfiles = runfiles::Runfiles::create()
            .map_err(|err| io::Error::other(format!("failed to create runfiles: {err}")))?;
        let marker_path = option_env!("CODEX_REPO_ROOT_MARKER")
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "CODEX_REPO_ROOT_MARKER was not set at compile time",
                )
            })?;
        runfiles::rlocation!(runfiles, &marker_path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "repo_root.marker not available in runfiles",
            )
        })?
    } else {
        resolve_cargo_runfile(Path::new("repo_root.marker"))?
    };
    let mut root = marker;
    for _ in 0..4 {
        root = root
            .parent()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "repo_root.marker did not have expected parent depth",
                )
            })?
            .to_path_buf();
    }
    Ok(root)
}

fn normalize_runfile_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if matches!(components.last(), Some(std::path::Component::Normal(_))) {
                    components.pop();
                } else {
                    components.push(component);
                }
            }
            _ => components.push(component),
        }
    }

    components
        .into_iter()
        .fold(PathBuf::new(), |mut acc, component| {
            acc.push(component.as_os_str());
            acc
        })
}

#[cfg(test)]
#[path = "cargo_bin_tests.rs"]
mod tests;
