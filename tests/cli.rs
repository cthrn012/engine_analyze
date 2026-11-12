//! Runs the built binary to check how it handles its command line.
use std::process::{Command, Output};

fn engine(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_engine"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn help_prints_usage_and_succeeds() {
    for args in [
        &[][..],
        &["help"],
        &["--help"],
        &["-h"],
        &["scan", "--help"],
    ] {
        let out = engine(args);
        assert!(out.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("USAGE:"),
            "{args:?}"
        );
    }
}

#[test]
fn an_unknown_command_fails() {
    let out = engine(&["anlyze", "INTC"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown command 'anlyze'"), "{err}");
}
