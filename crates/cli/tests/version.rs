#[test]
fn version_flag_prints_cetra() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cetra"))
        .arg("--version")
        .output()
        .expect("run cetra --version");
    assert!(
        output.status.success(),
        "cetra --version failed: {output:?}"
    );
    let stdout = String::from_utf8(output.stdout).expect("version is utf-8");
    assert!(
        stdout.starts_with("cetra "),
        "expected version to start with \"cetra \", got {stdout:?}"
    );
}

#[test]
fn version_command_reports_the_embedded_ui() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cetra"))
        .arg("version")
        .output()
        .expect("run cetra version");
    assert!(output.status.success(), "cetra version failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("version is utf-8");
    let ui = if cetra_ui::embedded() {
        "ui: embedded"
    } else {
        "ui: not built"
    };
    assert_eq!(stdout.lines().nth(1), Some(ui), "{stdout:?}");
}
