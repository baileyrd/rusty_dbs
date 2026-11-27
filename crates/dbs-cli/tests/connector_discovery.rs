//! Integration tests for issue #160: real `dbs-connector-*` candidate
//! discovery. `sources_connectors.rs`'s tests cover the honest
//! always-empty-registry behavior that predates this issue (`PATH`
//! never has any `dbs-connector-*` binaries in these tests' sandboxed
//! environment, so those assertions still hold); these tests instead
//! point `[dbs] connectors_dir` at a directory holding a real,
//! compiled `dbs-connector-raindrop` binary and confirm the CLI
//! actually finds and handshakes with it.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn dbs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dbs"))
}

/// `CARGO_BIN_EXE_<name>` is only set for the current *package's own*
/// binary targets, not a dev-dependency's — so this finds the real,
/// already-compiled `dbs-connector-raindrop` binary in the profile
/// directory (`target/<profile>/`) by walking up from this test binary.
///
/// Walking up rather than assuming "two directories up" matters:
/// older Cargo runs tests from `target/<profile>/deps/`, but newer Cargo
/// runs them from a separate build dir such as
/// `target/<profile>/build/<pkg>/<hash>/out/`, while finished binaries
/// still land in `target/<profile>/`. CI builds the workspace first so
/// the connector is there; a local `cargo build --workspace` does too.
fn raindrop_connector_bin() -> PathBuf {
    let name = format!("dbs-connector-raindrop{}", std::env::consts::EXE_SUFFIX);
    let test_exe = std::env::current_exe().unwrap();
    test_exe
        .ancestors()
        .skip(1)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| {
            panic!(
                "no {name} found above {} — run `cargo build --workspace` first",
                test_exe.display()
            )
        })
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dbs-cli-connector-discovery-test-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// A directory containing nothing but a copy of the real, compiled
/// `dbs-connector-raindrop` binary — what `connectors_dir` points at.
fn connectors_dir_with_raindrop(label: &str) -> PathBuf {
    let dir = temp_dir(label).join("connectors");
    std::fs::create_dir_all(&dir).unwrap();
    // `is_executable_file` (`dbs-core::registry`) requires a `.exe`
    // extension on Windows — a `dbs-connector-raindrop` copy without
    // one is silently invisible to `scan_connector_candidates`, same
    // suffix `raindrop_connector_bin()` already appends for the source.
    let dest = dir.join(format!(
        "dbs-connector-raindrop{}",
        std::env::consts::EXE_SUFFIX
    ));
    std::fs::copy(raindrop_connector_bin(), &dest).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn write_config(
    dir: &std::path::Path,
    connectors_dir: &std::path::Path,
    sources_block: &str,
) -> PathBuf {
    let config_path = dir.join("dbs.toml");
    let mut file = std::fs::File::create(&config_path).unwrap();
    writeln!(file, "[dbs]").unwrap();
    writeln!(file, "database = \"dbs.sqlite3\"").unwrap();
    writeln!(file, "export_dir = \"exports\"").unwrap();
    writeln!(file, "download_root = \"downloads\"").unwrap();
    writeln!(
        file,
        "connectors_dir = {:?}",
        connectors_dir.to_string_lossy()
    )
    .unwrap();
    writeln!(file, "{sources_block}").unwrap();
    config_path
}

/// On Windows, `cargo test` prepends this test binary's own
/// build-output directory (`target/debug` — exactly `dbs_bin()`'s
/// parent, since `CARGO_BIN_EXE_dbs` points straight at it) to *this
/// test process's* `PATH`, for DLL resolution. After `cargo build
/// --workspace`, that same directory holds every real
/// `dbs-connector-*` binary this repo ships (including the very
/// `dbs-connector-raindrop` this file copies around) — genuinely
/// discoverable via bare `PATH`, which is exactly what
/// `discovery_still_finds_nothing_without_a_configured_connectors_dir`
/// exists to prove *doesn't* happen. Strip that one directory so
/// `PATH`-only discovery in the spawned `dbs` child means what this
/// file's tests need it to mean; `connectors_dir`-based discovery
/// (the other three tests) is unaffected since it never depends on
/// `PATH` at all.
fn sanitized_path() -> std::ffi::OsString {
    let own_dir = dbs_bin().parent().map(std::path::Path::to_path_buf);
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(
        std::env::split_paths(&path_var).filter(|p| Some(p.as_path()) != own_dir.as_deref()),
    )
    .unwrap_or(path_var)
}

fn run(
    dir: &std::path::Path,
    config_path: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    Command::new(dbs_bin())
        .current_dir(dir)
        .env("PATH", sanitized_path())
        .arg("--config")
        .arg(config_path)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn connectors_describe_finds_a_real_connector_via_connectors_dir() {
    let dir = temp_dir("describe-found");
    let connectors_dir = connectors_dir_with_raindrop("describe-found");
    let config_path = write_config(&dir, &connectors_dir, "");

    let output = run(&dir, &config_path, &["connectors", "describe", "raindrop"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Raindrop.io"), "{stdout}");
    assert!(stdout.contains("RAINDROP_TOKEN"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn connectors_list_reports_a_connector_found_via_connectors_dir() {
    let dir = temp_dir("list-found");
    let connectors_dir = connectors_dir_with_raindrop("list-found");
    let config_path = write_config(&dir, &connectors_dir, "");

    let output = run(&dir, &config_path, &["connectors", "list"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("raindrop"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sources_check_succeeds_once_the_connector_is_actually_discoverable() {
    let dir = temp_dir("check-found");
    let connectors_dir = connectors_dir_with_raindrop("check-found");
    let config_path = write_config(
        &dir,
        &connectors_dir,
        "[sources.a]\ntype = \"raindrop\"\nenabled = true\n",
    );

    let output = run(&dir, &config_path, &["sources", "check"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("a:"), "{stdout}");
    assert!(!stdout.contains("not found"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn discovery_still_finds_nothing_without_a_configured_connectors_dir() {
    // Same source config as the "found" tests above, minus
    // connectors_dir — confirms PATH-only discovery (this sandboxed
    // test process's PATH has no dbs-connector-* binaries on it)
    // still behaves exactly like the pre-#160 always-empty registry.
    let dir = temp_dir("check-not-found");
    let config_path = dir.join("dbs.toml");
    let mut file = std::fs::File::create(&config_path).unwrap();
    writeln!(file, "[dbs]").unwrap();
    writeln!(file, "database = \"dbs.sqlite3\"").unwrap();
    writeln!(file, "[sources.a]\ntype = \"raindrop\"\nenabled = true").unwrap();
    drop(file);

    let output = run(&dir, &config_path, &["sources", "check"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("not found"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}
