use std::process::Command;

#[test]
fn managed_runtime_rejects_host_secret_overrides_without_logging_values() {
    for name in ["CELLD_VAR_PRIVATE", "CELLD_VARS_FILE"] {
        let output = Command::new(env!("CARGO_BIN_EXE_celld"))
            .env_clear()
            .env(name, "private-value-must-never-appear")
            .arg("--control-plane")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(
            error.contains("runtime Worker variable overrides require standalone mode"),
            "{error}"
        );
        assert!(!error.contains("private-value-must-never-appear"));
    }
}
