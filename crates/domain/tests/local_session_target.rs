//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString};

use kerminal_domain::sessions::{LocalSessionTarget, SessionTarget};

/// 同时覆盖直接和嵌套诊断，保证调用方无需记住额外脱敏步骤。
#[test]
fn debug_redacts_decrypted_environment_names_and_values() {
    let target = LocalSessionTarget {
        program: OsString::from("pwsh"),
        args: Vec::new(),
        working_directory: None,
        environment: BTreeMap::from([(
            OsString::from("PRIVATE_ENVIRONMENT_NAME"),
            OsString::from("decrypted-secret-value"),
        )]),
    };
    let nested = SessionTarget::Local(target.clone());
    for diagnostic in [format!("{target:?}"), format!("{nested:?}")] {
        assert!(diagnostic.contains("environment_entries: 1"));
        assert!(!diagnostic.contains("PRIVATE_ENVIRONMENT_NAME"));
        assert!(!diagnostic.contains("decrypted-secret-value"));
    }
}
