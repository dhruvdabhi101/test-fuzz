use assert_cmd::Command;
use bitflags::bitflags;
use cargo_metadata::MetadataCommand;
use lazy_static::lazy_static;
use option_set::option_set;
use predicates::prelude::*;
use regex::Regex;
use rustc_version::{version_meta, Channel};
use serde::Deserialize;
use std::{
    fs::{read_to_string, OpenOptions},
    io::{stderr, Write},
    path::Path,
};
use tempfile::tempdir_in;

option_set! {
    #[derive(Copy, Clone, Eq, PartialEq)]
    struct Flags: UpperSnake + u8 {
        const EXPENSIVE = 1 << 0;
        const SKIP = 1 << 1;
        const SKIP_NIGHTLY = 1 << 2;
        const REQUIRES_ISOLATION = 1 << 3;
    }
}

#[derive(Deserialize)]
struct Test {
    flags: Flags,
    url: String,
    rev: String,
    patch: String,
    subdir: String,
    package: String,
    targets: Vec<String>,
}

lazy_static! {
    static ref TESTS: Vec<Test> = {
        let content =
            read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party.json")).unwrap();
        serde_json::from_str(&content).unwrap()
    };
}

// smoelius: This should match `scripts/update_patches.sh`.
const LINES_OF_CONTEXT: u32 = 2;

mod cheap_tests {
    use super::*;
    #[test]
    fn test() {
        let version_meta = version_meta().unwrap();
        for test in TESTS.iter() {
            run_test(
                module_path!(),
                test,
                test.flags.contains(Flags::EXPENSIVE)
                    || test.flags.contains(Flags::SKIP)
                    || (test.flags.contains(Flags::SKIP_NIGHTLY)
                        && version_meta.channel == Channel::Nightly),
            );
        }
    }
}

mod all_tests {
    use super::*;
    #[test]
    #[ignore]
    fn test() {
        let version_meta = version_meta().unwrap();
        for test in TESTS.iter() {
            run_test(
                module_path!(),
                test,
                test.flags.contains(Flags::SKIP)
                    || (test.flags.contains(Flags::SKIP_NIGHTLY)
                        && version_meta.channel == Channel::Nightly),
            );
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run_test(module_path: &str, test: &Test, no_run: bool) {
    let (_, module) = module_path.split_once("::").unwrap();
    #[allow(clippy::explicit_write)]
    writeln!(
        stderr(),
        "{}: {}{}",
        module,
        test.url,
        if no_run { " (no-run)" } else { "" }
    )
    .unwrap();

    // smoelius: Each patch expects test-fuzz to be an ancestor of the directory in which the patch
    // is applied.
    #[allow(unknown_lints)]
    #[allow(env_cargo_path)]
    let tempdir = tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();

    Command::new("git")
        .current_dir(&tempdir)
        .args(["clone", &test.url, "."])
        .assert()
        .success();

    Command::new("git")
        .current_dir(&tempdir)
        .args(["checkout", &test.rev])
        .assert()
        .success();

    #[allow(unknown_lints)]
    #[allow(env_cargo_path)]
    let patch = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("patches")
        .join(&test.patch)
        .canonicalize()
        .unwrap();

    Command::new("git")
        .current_dir(&tempdir)
        .args(["apply", &patch.to_string_lossy()])
        .assert()
        .success();

    let subdir = tempdir.path().join(&test.subdir);

    if test.flags.contains(Flags::REQUIRES_ISOLATION) {
        let mut file = OpenOptions::new()
            .write(true)
            .append(true)
            .open(subdir.join("Cargo.toml"))
            .unwrap();

        writeln!(file)
            .and_then(|_| writeln!(file, "[workspace]"))
            .unwrap();
    }

    // smoelius: Right now, Substrate's lockfile refers to `pin-project:0.4.27`, which is
    // incompatible with `syn:1.0.84`.
    // smoelius: The `pin-project` issue has been resolved. But Substrate now chokes when
    // `libp2p-swarm-derive` is upgraded from 0.30.1 to 0.30.2.
    Command::new("cargo")
        .current_dir(&subdir)
        .args(["update"])
        .assert()
        .success();

    // smoelius: The `libp2p-swarm-derive` issue appears to have been resolved.
    #[cfg(any())]
    {
        let mut command = Command::new("cargo");
        command
            .current_dir(&subdir)
            .args(["update", "-p", "libp2p-swarm-derive"]);
        if command.assert().try_success().is_ok() {
            command.args(["--precise", "0.30.1"]).assert().success();
        }
    }

    check_test_fuzz_dependency(&subdir, &test.package);

    if no_run {
        return;
    }

    // smoelius: Use `std::process::Command` so that we can see the output of the command as it
    // runs. `assert_cmd::Command` would capture the output.
    assert!(std::process::Command::new("cargo")
        .current_dir(&subdir)
        .args(["test", "--package", &test.package, "--", "--nocapture"])
        .status()
        .unwrap()
        .success());

    for target in &test.targets {
        Command::cargo_bin("cargo-test-fuzz")
            .unwrap()
            .current_dir(&subdir)
            .args([
                "test-fuzz",
                "--package",
                &test.package,
                "--display=corpus",
                target,
            ])
            .assert()
            .success()
            .stdout(predicate::str::is_match(r#"(?m)^[[:xdigit:]]{40}:"#).unwrap());

        Command::cargo_bin("cargo-test-fuzz")
            .unwrap()
            .current_dir(&subdir)
            .args([
                "test-fuzz",
                "--package",
                &test.package,
                "--replay=corpus",
                target,
            ])
            .assert()
            .success()
            .stdout(
                predicate::str::is_match(r#"(?m)^[[:xdigit:]]{40}: Ret\((Ok|Err)\(.*\)\)$"#)
                    .unwrap(),
            );
    }
}

fn check_test_fuzz_dependency(subdir: &Path, test_package: &str) {
    let metadata = MetadataCommand::new()
        .current_dir(subdir)
        .no_deps()
        .exec()
        .unwrap();
    let package = metadata
        .packages
        .iter()
        .find(|package| package.name == test_package)
        .unwrap_or_else(|| panic!("Could not find package `{}`", test_package));
    let dep = package
        .dependencies
        .iter()
        .find(|dep| dep.name == "test-fuzz")
        .expect("Could not find dependency `test-fuzz`");
    assert!(dep.path.is_some());
}

#[test]
#[ignore]
fn patches_are_current() {
    let re = Regex::new(r#"^index [[:xdigit:]]{7}\.\.[[:xdigit:]]{7} [0-7]{6}$"#).unwrap();

    for test in TESTS.iter() {
        let tempdir = tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();

        Command::new("git")
            .current_dir(&tempdir)
            .args(["clone", "--depth=1", &test.url, "."])
            .assert()
            .success();

        let patch_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("patches")
            .join(&test.patch);
        let patch = read_to_string(patch_path).unwrap();

        Command::new("git")
            .current_dir(&tempdir)
            .args(["apply"])
            .write_stdin(patch.as_bytes())
            .assert()
            .success();

        // smoelius: The following checks are *not* redundant. They can fail even if the patch
        // applies.

        let assert = Command::new("git")
            .current_dir(&tempdir)
            .args(["diff", &format!("--unified={LINES_OF_CONTEXT}")])
            .assert()
            .success();

        let diff = std::str::from_utf8(&assert.get_output().stdout).unwrap();

        let patch_lines = patch.lines().collect::<Vec<_>>();
        let diff_lines = diff.lines().collect::<Vec<_>>();

        assert_eq!(patch_lines.len(), diff_lines.len());

        for (patch_line, diff_line) in patch_lines.into_iter().zip(diff_lines) {
            if !(re.is_match(patch_line) && re.is_match(diff_line)) {
                assert_eq!(patch_line, diff_line);
            }
        }
    }
}

#[test]
#[ignore]
fn revisions_are_current() {
    for test in TESTS.iter() {
        let tempdir = tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();

        Command::new("git")
            .current_dir(&tempdir)
            .args(["clone", "--depth=1", &test.url, "."])
            .assert()
            .success();

        let assert = Command::new("git")
            .current_dir(&tempdir)
            .args(["rev-parse", "HEAD"])
            .assert()
            .success();

        let stdout = std::str::from_utf8(&assert.get_output().stdout).unwrap();

        assert_eq!(stdout.trim_end(), test.rev);
    }
}
