//! POSIX mode 的输入和显示规则；权限元数据缺失时不得构造默认值。
//!
//! @author kongweiguang

/// 解析用户显式输入的三到四位八进制 mode，接受常见的零前缀和 `0o` 前缀。
///
/// @author kongweiguang
pub(super) fn parse_octal_mode(value: &str) -> Option<u32> {
    let value = value.trim();
    let digits = value
        .strip_prefix("0o")
        .or_else(|| value.strip_prefix("0O"))
        .unwrap_or(value);
    let valid_width =
        (3..=4).contains(&digits.len()) || (digits.len() == 5 && digits.starts_with('0'));
    if !valid_width || !digits.bytes().all(|digit| matches!(digit, b'0'..=b'7')) {
        return None;
    }

    u32::from_str_radix(digits, 8)
        .ok()
        .filter(|mode| *mode <= 0o7777)
}

/// 固定以四位八进制展示所有特殊位，避免只显示 rwx 时丢失 chmod 信息。
///
/// @author kongweiguang
pub(super) fn format_octal_mode(mode: u32) -> String {
    format!("{:04o}", mode & 0o7777)
}

/// 按 POSIX ls 规则显示 setuid、setgid 和 sticky 位的大小写语义。
///
/// @author kongweiguang
pub(super) fn format_symbolic_mode(mode: u32) -> String {
    let bits = [
        0o400, 0o200, 0o100, 0o040, 0o020, 0o010, 0o004, 0o002, 0o001,
    ];
    let mut output = String::with_capacity(9);
    for (index, bit) in bits.into_iter().enumerate() {
        output.push(if mode & bit == 0 {
            '-'
        } else {
            ['r', 'w', 'x'][index % 3]
        });
    }

    let special_bits = [
        (0o4000, 2, 's', 'S'),
        (0o2000, 5, 's', 'S'),
        (0o1000, 8, 't', 'T'),
    ];
    for (bit, index, with_execute, without_execute) in special_bits {
        if mode & bit != 0 {
            let execute_bit = bits[index];
            output.replace_range(
                index..index + 1,
                &if mode & execute_bit == 0 {
                    without_execute.to_string()
                } else {
                    with_execute.to_string()
                },
            );
        }
    }
    output
}

#[cfg(test)]
#[path = "../../../../tests/support/files_permission_mode.rs"]
mod tests;
