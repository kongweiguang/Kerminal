// @author kongweiguang

use kerminal_domain::commands::{WorkflowStepInput, WorkflowValidationError, validate_workflow};

/// 使用单步骤领域输入验证脚本边界，不通过 UI 或存储层掩盖领域规则。
fn validate_script(command: &str) -> Result<(), WorkflowValidationError> {
    validate_workflow(
        "脚本测试",
        None,
        &[],
        &[WorkflowStepInput {
            id: None,
            title: "多行脚本".to_owned(),
            command: command.to_owned(),
            description: None,
            scope: None,
            requires_confirmation: false,
        }],
    )
}

/// 多行脚本、单引号、heredoc 与中文都保留为合法工作流输入。
#[test]
fn workflow_accepts_multiline_shell_source_without_rewriting() {
    let script = "cat <<'EOF'\n单引号 ' 与双引号 \"\nEOF\nprintf '%s\\n' '重庆'";

    assert_eq!(validate_script(script), Ok(()));
    assert_eq!(script.lines().count(), 4);
}

/// 八千个四字节 Unicode scalar 正好达到 32,000 字节边界，不能截断有效脚本。
#[test]
fn workflow_accepts_exact_character_and_utf8_byte_limits() {
    let script = "😀".repeat(8_000);

    assert_eq!(script.len(), 32_000);
    assert_eq!(validate_script(&script), Ok(()));
}

/// 超过领域字符上限时拒绝整个脚本，防止终端包装时静默裁剪。
#[test]
fn workflow_rejects_scripts_over_character_limit() {
    assert_eq!(
        validate_script(&"x".repeat(8_001)),
        Err(WorkflowValidationError::StepCommandTooLong)
    );
}

/// 不支持的注入控制字符在保存前拒绝，避免运行时无法可靠关联 shell 结果。
#[test]
fn workflow_rejects_cr_nul_and_escape() {
    for script in ["echo one\recho two", "echo \0", "echo \u{1b}"] {
        assert_eq!(
            validate_script(script),
            Err(WorkflowValidationError::StepCommandControlCharacter)
        );
    }
}
