//! Command-line contract tests.
//!
//! These drive the real binary but never reach a Cargo build: every case either
//! asks a question Zirild answers itself (`--help`, `--version`) or is rejected
//! during argument parsing. That keeps the contract testable on a machine with
//! no Zig, no C/C++ toolchain, and no cross targets installed.

use std::process::{Command, Output};

fn cargo_zirild(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-zirild"))
        .args(arguments)
        .output()
        .expect("cargo-zirild should be runnable")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn version_flags_report_the_package_version() {
    let expected = format!("cargo-zirild {}", env!("CARGO_PKG_VERSION"));
    let cases: [&[&str]; 3] = [&["--version"], &["-V"], &["zirild", "--version"]];
    for arguments in cases {
        let output = cargo_zirild(arguments);
        assert!(
            output.status.success(),
            "{arguments:?} should succeed: {}",
            stderr(&output)
        );
        assert_eq!(stdout(&output).trim(), expected);
    }
}

#[test]
fn help_lists_every_supported_cargo_command_and_option() {
    for arguments in [["--help"], ["-h"], ["help"], ["?"]] {
        let output = cargo_zirild(&arguments);
        assert!(
            output.status.success(),
            "{arguments:?} should succeed: {}",
            stderr(&output)
        );
        let help = stdout(&output);

        let commands = help
            .lines()
            .find(|line| line.trim_start().starts_with("build (default)"))
            .expect("help should list the Cargo commands");
        for command in [
            "build", "check", "run", "test", "bench", "rustc", "clippy", "doc", "asm",
        ] {
            assert!(
                commands.contains(command),
                "{arguments:?} omits {command}: {commands}"
            );
        }

        for option in ["-target=", "--zig-path=", "--version", "--windows-runtime="] {
            assert!(
                help.contains(option),
                "{arguments:?} does not mention {option}"
            );
        }
    }
}

#[test]
fn a_missing_target_is_rejected() {
    let output = cargo_zirild(&["zirild"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("missing required -target="));
}

#[test]
fn a_separate_target_argument_is_rejected() {
    for arguments in [
        vec!["zirild", "-target", "x86_64-linux-musl"],
        vec!["zirild", "--target", "x86_64-linux-musl"],
        vec![
            "zirild",
            "-target=x86_64-linux-musl",
            "build",
            "--target",
            "x86_64-linux-musl",
        ],
    ] {
        let output = cargo_zirild(&arguments);
        assert!(!output.status.success(), "{arguments:?} should be rejected");
    }
}

#[test]
fn scope_limited_options_are_rejected_outside_their_target() {
    let windows_runtime = cargo_zirild(&[
        "zirild",
        "-target=x86_64-unknown-linux-gnu",
        "--windows-runtime=zig",
    ]);
    assert!(!windows_runtime.status.success());
    assert!(stderr(&windows_runtime).contains("only valid for a Windows GNU -target"));

    let android = cargo_zirild(&[
        "zirild",
        "-target=x86_64-unknown-linux-gnu",
        "-android-api=24",
    ]);
    assert!(!android.status.success());
    assert!(stderr(&android).contains("Android-only options require an Android"));
}

#[test]
fn an_invalid_optimization_mode_is_rejected() {
    let output = cargo_zirild(&["zirild", "-target=x86_64-linux-musl", "--zig-opt=turbo"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("invalid --zig-opt 'turbo'"));
}
