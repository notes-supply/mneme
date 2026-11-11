use std::process::Command;

#[test]
fn an_unknown_subcommand_is_refused() {
    let status = Command::new(env!("CARGO_BIN_EXE_mneme"))
        .arg("unknown")
        .output()
        .unwrap()
        .status;
    assert!(!status.success());
}

#[test]
fn autosave_refuses_a_number_of_seconds_that_is_not_one() {
    for value in ["-1", "NaN", "inf", "soon", "1e30"] {
        let output = Command::new(env!("CARGO_BIN_EXE_mneme"))
            .args([
                "autosave",
                "run",
                "--folder",
                ".",
                "--server",
                "http://127.0.0.1:9",
            ])
            .args(["--token-file", "/dev/null", "--name", "x"])
            .arg(format!("--tick-seconds={value}"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{value}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--tick-seconds"),
            "{value}"
        );
    }
}
