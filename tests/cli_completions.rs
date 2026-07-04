// SPDX-License-Identifier: MIT

use std::process::Command;

use assert_cmd::prelude::*;

#[test]
fn completions_are_generated_for_supported_shells() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = Command::cargo_bin("rgfile")
            .unwrap()
            .args(["--no-config", "completions", shell])
            .output()
            .unwrap();

        assert!(output.status.success(), "{shell}: {output:?}");
        assert!(!output.stdout.is_empty(), "{shell} produced empty output");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("rgfile"), "{shell}: {text}");
    }
}
