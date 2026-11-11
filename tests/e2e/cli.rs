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
