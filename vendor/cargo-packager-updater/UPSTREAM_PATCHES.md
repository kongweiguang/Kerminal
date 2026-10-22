<!-- @author kongweiguang -->
# Upstream source and local patch

This directory vendors the crates.io package `cargo-packager-updater` version `0.2.3`.

- Upstream repository: <https://github.com/crabnebula-dev/cargo-packager>
- Upstream tag: `cargo-packager-updater-v0.2.3`
- Upstream commit: `2e71af6d882157e706062b99f978c1723c9c5c64`
- Upstream crate path: `crates/updater`
- License: `Apache-2.0 OR MIT`; the original `LICENSE_APACHE-2.0` and `LICENSE_MIT` files are retained here.

The local source is copied from the exact 0.2.3 Cargo registry package. The Windows NSIS launcher in `src/lib.rs` is narrowly patched to pass one complete PowerShell script after `-Command`. The executable path and each installer argument are encoded as PowerShell single-quoted literals, with apostrophes doubled and Windows UTF-16 path units preserved. Because PowerShell 5.1 `Start-Process -ArgumentList` flattens arrays, ordinary arguments also receive Windows command-line quoting so spaces, embedded quotes, empty values, and trailing backslashes retain their argument boundaries. The NSIS `/D=` directory switch is validated as the final argument and passed without native double quotes, as required by NSIS. This retains the SDK's download, signature verification, temporary artifact, `Start-Process`, and process-exit behavior.

`tests/support/windows_install_script_unit.rs` contains private regression tests for the command builder. It is included only by the crate's Windows unit-test build and does not add public API.

The macOS elevated installation branch also uses a fixed AppleScript with installation paths passed as separate `osascript --` arguments. The script applies AppleScript `quoted form of` before passing these paths to the shell. This prevents quotes and shell metacharacters in application or temporary directory names from becoming script syntax. The existing authorization trigger, removal/move sequence, errors and cleanup are retained. Download, signature verification and update formats are unchanged. The private constructor test in `tests/support/macos_install_script_unit.rs` runs on every unit-test platform and checks that dynamic paths remain outside the script. It does not execute `osascript` or prove macOS installation behavior.

Apple documents argument passing in its [osascript example](https://developer.apple.com/documentation/security/customizing-the-xcode-archive-process?language=objc) and shell path quoting in its [Mac Automation Scripting Guide](https://developer.apple.com/library/archive/documentation/LanguagesUtilities/Conceptual/MacAutomationScriptingGuide/CallCommandLineUtilities.html).
