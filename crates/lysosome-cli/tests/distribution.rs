//! `--version`, `completions` and the hidden `man` command used by releases.

use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;

fn lysosome(args: &[&str]) -> Output {
    let out = Command::cargo_bin("lysosome")
        .expect("binary")
        .args(args)
        .output()
        .expect("run lysosome");
    assert!(
        out.status.success(),
        "lysosome {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn stdout(args: &[&str]) -> String {
    String::from_utf8(lysosome(args).stdout).expect("utf-8")
}

#[test]
fn short_version_is_name_and_version() {
    let v = stdout(&["-V"]);
    assert_eq!(v.trim(), format!("lysosome {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn long_version_adds_commit_and_target() {
    let v = stdout(&["--version"]);
    let prefix = format!("lysosome {} (", env!("CARGO_PKG_VERSION"));
    assert!(v.starts_with(&prefix), "unexpected --version: {v}");
    assert!(v.trim_end().ends_with(')'), "unexpected --version: {v}");
}

#[test]
fn completions_for_every_shell() {
    for (shell, marker) in [
        ("bash", "_lysosome()"),
        ("zsh", "#compdef lysosome"),
        ("fish", "complete -c lysosome"),
        ("powershell", "Register-ArgumentCompleter"),
        ("elvish", "edit:completion:arg-completer[lysosome]"),
    ] {
        let script = stdout(&["completions", shell]);
        assert!(
            script.contains(marker),
            "{shell} completions missing {marker:?}"
        );
        assert!(
            script.contains("worktrees"),
            "{shell} completions missing subcommand"
        );
        assert!(
            !script.contains("generate-man"),
            "{shell} completions expose hidden `man`"
        );
    }
}

#[test]
fn completions_rejects_unknown_shell() {
    let out = Command::cargo_bin("lysosome")
        .unwrap()
        .args(["completions", "tcsh"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn generate_man_writes_a_page_per_subcommand() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("man");
    let out = lysosome(&["__generate-man", dir.to_str().unwrap()]);
    assert!(out.stdout.is_empty());
    for page in [
        "lysosome.1",
        "lysosome-worktrees.1",
        "lysosome-completions.1",
    ] {
        let text =
            std::fs::read_to_string(dir.join(page)).unwrap_or_else(|e| panic!("{page}: {e}"));
        assert!(text.contains(".TH"), "{page} is not a man page");
        assert!(
            text.contains(env!("CARGO_PKG_VERSION")),
            "{page} lacks version"
        );
    }
    assert!(!dir.join("lysosome-help.1").exists());
}

#[test]
fn generate_man_requires_exactly_one_dir() {
    let out = Command::cargo_bin("lysosome")
        .unwrap()
        .arg("__generate-man")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn man_generator_is_hidden_from_help() {
    let help = stdout(&["--help"]);
    assert!(help.contains("completions"));
    assert!(!help.contains("generate-man"), "--help leaks it:\n{help}");
}
