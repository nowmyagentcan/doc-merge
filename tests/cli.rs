use std::process::Command;

#[test]
fn version_prints_name_and_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_doc-merge"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "doc-merge 0.1.0\n");
}
