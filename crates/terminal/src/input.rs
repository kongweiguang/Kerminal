//! 将 UI 无关的键盘、鼠标、焦点和粘贴事件编码为终端协议字节。
//! 键盘序列遵循 Kitty keyboard protocol 的 progressive enhancement 规则，并参考 Zed 冻结版
//! 的传统键位映射；这里只保留由当前平台事件模型能够准确表达的信息。
//! 规范：https://sw.kovidgoyal.net/kitty/keyboard-protocol/
//! Zed 参考：https://github.com/zed-industries/zed/blob/80f1bb45fa5b8f278a98a6e38a016c2f89c7eb12/crates/terminal/src/mappings/keys.rs
//! 粘贴边界参考 Alacritty 与同一冻结版 Zed 的实现，按本项目事件模型独立编码：
//! https://github.com/alacritty/alacritty/blob/master/alacritty/src/event.rs
//! https://github.com/zed-industries/zed/blob/80f1bb45fa5b8f278a98a6e38a016c2f89c7eb12/crates/terminal/src/terminal.rs
//!
//! @author kongweiguang

use crate::snapshot::{MouseTrackingMode, TerminalModes};

/// 修饰键状态；不依赖 GPUI 或操作系统事件类型。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
    pub super_key: bool,
}

impl InputModifiers {
    /// 计算 xterm/Kitty 协议共用的 1-based 修饰键掩码。
    ///
    /// @author kongweiguang
    const fn protocol_value(self) -> u8 {
        1 + self.shift as u8
            + (self.alt as u8) * 2
            + (self.control as u8) * 4
            + (self.super_key as u8) * 8
    }
}

/// Kitty keyboard 和普通键盘都可表示的键标识。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalKey {
    Character(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    BackTab,
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    Function(u8),
}

/// 按键阶段；只在 Kitty 协议启用时发送 release/repeat 事件。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyPhase {
    Press,
    Repeat,
    Release,
}

/// 与平台无关的键盘输入。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalKeyEvent {
    pub key: TerminalKey,
    pub phase: KeyPhase,
    pub modifiers: InputModifiers,
}

/// 鼠标按钮和滚轮方向。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalMouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

/// 鼠标动作；坐标从零开始，编码时统一转换成终端的一基坐标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalMouseAction {
    Press,
    Release,
    Move,
    Scroll,
}

/// 与终端窗口大小无关的鼠标事件。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalMouseEvent {
    pub button: TerminalMouseButton,
    pub action: TerminalMouseAction,
    /// Motion 是否发生在按键持续按下期间；1002 只报告这种移动。
    pub button_down: bool,
    pub column: u16,
    pub row: u16,
    pub modifiers: InputModifiers,
}

/// Kitty 协议可报告的备用键码；来源未知时留空，避免伪造键盘布局信息。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalKeyAlternates {
    pub shifted: Option<char>,
    pub base_layout: Option<char>,
}

/// UI 到终端输入层的事件；仅显式关联的按键与文本合并，其余 IME commit 保持独立。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalInputEvent {
    Key(TerminalKeyEvent),
    /// GPUI 同一次 keystroke 明确提供的文本；调用方随后不得再发相同的 Text 事件。
    KeyWithText {
        event: TerminalKeyEvent,
        text: String,
        alternates: TerminalKeyAlternates,
    },
    Text(String),
    Paste(String),
    Mouse(TerminalMouseEvent),
    Focus(bool),
}

/// 根据当前仿真器模式编码输入；普通字符由文本/IME commit 通道单独送入。
///
/// @author kongweiguang
pub fn encode_input(event: &TerminalInputEvent, modes: TerminalModes) -> Option<Vec<u8>> {
    match event {
        TerminalInputEvent::Text(text) => encode_text(text, modes),
        TerminalInputEvent::Paste(text) => Some(encode_paste(text, modes.bracketed_paste)),
        TerminalInputEvent::Key(key) => encode_key(*key, modes),
        TerminalInputEvent::KeyWithText {
            event,
            text,
            alternates,
        } => encode_key_with_text(*event, text, *alternates, modes),
        TerminalInputEvent::Mouse(mouse) => encode_mouse(*mouse, modes),
        TerminalInputEvent::Focus(focused) if modes.focus_reporting => Some(if *focused {
            b"\x1b[I".to_vec()
        } else {
            b"\x1b[O".to_vec()
        }),
        TerminalInputEvent::Focus(_) => None,
    }
}

/// 依终端当前粘贴模式规范化换行，并隔离 bracketed-paste 的结束控制序列。
///
/// 普通模式把 CRLF/LF 归一为 Enter 的 CR，保留已有 CR；bracketed 模式保留行尾，
/// 但过滤 ESC 与 ETX，避免粘贴内容伪造结束标记或被部分 shell 提前解释为中断。
/// 空内容仍返回空字节或完整空 bracketed 帧，以保持 `Paste` 事件现有的 `Some` 语义。
///
/// @author kongweiguang
fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 12);
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend(text.bytes().filter(|byte| *byte != 0x1b && *byte != 0x03));
        bytes.extend_from_slice(b"\x1b[201~");
        return bytes;
    }

    let mut input = text.bytes().peekable();
    while let Some(byte) = input.next() {
        match byte {
            b'\r' => {
                bytes.push(b'\r');
                if input.peek() == Some(&b'\n') {
                    input.next();
                }
            }
            b'\n' => bytes.push(b'\r'),
            other => bytes.push(other),
        }
    }
    bytes
}

/// 按 Kitty 的全键模式处理未与物理按键关联的 IME 文本，并保留普通 UTF-8 路径。
///
/// @author kongweiguang
fn encode_text(text: &str, modes: TerminalModes) -> Option<Vec<u8>> {
    const REPORT_ALL_KEYS_AS_ESC: u8 = 0b01000;
    const REPORT_ASSOCIATED_TEXT: u8 = 0b10000;
    let flags = modes.kitty_keyboard_flags;
    if flags & REPORT_ALL_KEYS_AS_ESC == 0 {
        return Some(text.as_bytes().to_vec());
    }

    if flags & REPORT_ASSOCIATED_TEXT == 0 {
        return None;
    }
    encode_text_only_kitty_event(text)
}

/// 纯 IME 文本没有可证明的物理键来源，故使用 Kitty 协议约定的 key code 0。
///
/// @author kongweiguang
fn encode_text_only_kitty_event(text: &str) -> Option<Vec<u8>> {
    let text_codes = associated_text_codes(text)?;
    Some(format!("\x1b[0;;{text_codes}u").into_bytes())
}

/// 编码一个没有关联文本的按键，按模式只回退到规范 legacy 序列。
///
/// @author kongweiguang
fn encode_key(event: TerminalKeyEvent, modes: TerminalModes) -> Option<Vec<u8>> {
    let flags = modes.kitty_keyboard_flags;
    if let Some(encoded) = encode_kitty_key(event, flags, TerminalKeyAlternates::default(), None) {
        return Some(encoded);
    }
    encode_legacy_key(event, modes, None)
}

/// 编码 host 明确关联到同一按键的文本；release 只报告键状态，避免重复提交字符。
///
/// @author kongweiguang
fn encode_key_with_text(
    event: TerminalKeyEvent,
    text: &str,
    alternates: TerminalKeyAlternates,
    modes: TerminalModes,
) -> Option<Vec<u8>> {
    let flags = modes.kitty_keyboard_flags;
    let report_all = flags & 0b01000 != 0;
    let associated_text =
        (report_all && flags & 0b10000 != 0 && event.phase != KeyPhase::Release).then_some(text);
    if let Some(encoded) = encode_kitty_key(event, flags, alternates, associated_text) {
        return Some(encoded);
    }
    encode_legacy_key(event, modes, Some(text))
}

/// 按 Kitty progressive-enhancement flags 选取 CSI-u 或规范功能键序列。
///
/// @author kongweiguang
fn encode_kitty_key(
    event: TerminalKeyEvent,
    flags: u8,
    alternates: TerminalKeyAlternates,
    associated_text: Option<&str>,
) -> Option<Vec<u8>> {
    const DISAMBIGUATE_ESCAPE_CODES: u8 = 0b0001;
    const REPORT_EVENT_TYPES: u8 = 0b0010;
    const REPORT_ALTERNATE_KEYS: u8 = 0b00100;
    const REPORT_ALL_KEYS_AS_ESC: u8 = 0b1000;
    const REPORT_ASSOCIATED_TEXT: u8 = 0b1_0000;
    if flags & 0b1_1111 == 0 {
        return None;
    }
    let report_all = flags & REPORT_ALL_KEYS_AS_ESC != 0;
    let report_types = flags & REPORT_EVENT_TYPES != 0;
    let modifiers = effective_modifiers(event);
    let modifier = modifiers.protocol_value();
    let disambiguate = flags & DISAMBIGUATE_ESCAPE_CODES != 0;

    if event.phase == KeyPhase::Release && !report_types {
        return None;
    }

    if !report_all {
        if matches!(event.key, TerminalKey::Escape) && !disambiguate && !report_types {
            return None;
        }
        if matches!(event.key, TerminalKey::Enter | TerminalKey::Backspace) {
            return None;
        }
        if matches!(event.key, TerminalKey::Tab | TerminalKey::BackTab)
            && !modifiers.control
            && !modifiers.super_key
        {
            return None;
        }
        if matches!(event.key, TerminalKey::Character(_))
            && !(disambiguate && character_needs_disambiguation(event, modifiers))
        {
            return None;
        }
        if is_functional_key(event.key)
            && !report_types
            && event.phase == KeyPhase::Press
            && !has_non_legacy_function_encoding(event.key)
        {
            return None;
        }
        let modified_tab = matches!(event.key, TerminalKey::Tab | TerminalKey::BackTab)
            && (modifiers.control || modifiers.super_key);
        if !is_functional_key(event.key)
            && !matches!(event.key, TerminalKey::Character(_) | TerminalKey::Escape)
            && !modified_tab
        {
            return None;
        }
    }

    if matches!(event.key, TerminalKey::Enter | TerminalKey::Backspace) && !report_all {
        return None;
    }
    if matches!(event.key, TerminalKey::Tab | TerminalKey::BackTab)
        && !report_all
        && !modifiers.control
        && !modifiers.super_key
    {
        return None;
    }

    if is_functional_key(event.key) && !report_all {
        if report_types {
            return kitty_functional_sequence(event, modifiers, flags);
        }
        return None;
    }

    let key_code = kitty_key_code(event.key)?;
    let event_type = report_types.then(|| phase_code(event.phase));
    let include_text = report_all && flags & REPORT_ASSOCIATED_TEXT != 0;
    let text_codes = if include_text {
        associated_text.and_then(associated_text_codes)
    } else {
        None
    };
    let alternate_flag = flags & REPORT_ALTERNATE_KEYS != 0;
    let mut key_field = key_code.to_string();
    if alternate_flag && matches!(event.key, TerminalKey::Character(_)) {
        append_alternate_keys(&mut key_field, alternates, modifiers);
    }
    Some(kitty_csi_u(
        &key_field,
        modifier,
        event_type,
        text_codes.as_deref(),
    ))
}

/// 在无全键模式时遵守终端常用字节、Alt 前缀与 Kitty 兼容键组合。
///
/// @author kongweiguang
fn encode_legacy_key(
    event: TerminalKeyEvent,
    modes: TerminalModes,
    associated_text: Option<&str>,
) -> Option<Vec<u8>> {
    if event.phase == KeyPhase::Release {
        return None;
    }

    let modifiers = effective_modifiers(event);
    let modifier = modifiers.protocol_value();
    let has_alt = modifiers.alt;
    let has_control = modifiers.control;
    let has_shift = modifiers.shift;
    match event.key {
        TerminalKey::Character(character) => {
            if has_control && (has_shift || modifiers.super_key) {
                return Some(kitty_csi_u(
                    &(character as u32).to_string(),
                    modifier,
                    None,
                    None,
                ));
            }
            if has_control {
                let control = control_character(character)?;
                return Some(if has_alt {
                    vec![0x1b, control]
                } else {
                    vec![control]
                });
            }
            if modifiers.super_key {
                return Some(kitty_csi_u(
                    &(character as u32).to_string(),
                    modifier,
                    None,
                    None,
                ));
            }
            if has_alt {
                let fallback = associated_text.map(str::to_owned).unwrap_or_else(|| {
                    // 缺少平台 text 时只对 ASCII 字母使用确定的 Shift 变换。
                    if has_shift && character.is_ascii_lowercase() {
                        character.to_ascii_uppercase().to_string()
                    } else {
                        character.to_string()
                    }
                });
                let mut bytes = vec![0x1b];
                bytes.extend_from_slice(fallback.as_bytes());
                return Some(bytes);
            }
            associated_text.map(|text| text.as_bytes().to_vec())
        }
        TerminalKey::Enter => {
            let mut bytes = Vec::with_capacity(2);
            if has_alt {
                bytes.push(0x1b);
            }
            bytes.push(b'\r');
            Some(bytes)
        }
        TerminalKey::Tab | TerminalKey::BackTab => {
            if has_control || modifiers.super_key {
                return Some(kitty_csi_u("9", modifier, None, None));
            }
            if has_shift {
                let mut bytes = Vec::with_capacity(4);
                if has_alt {
                    bytes.push(0x1b);
                }
                bytes.extend_from_slice(b"\x1b[Z");
                return Some(bytes);
            }
            if has_alt {
                return Some(vec![0x1b, b'\t']);
            }
            Some(vec![b'\t'])
        }
        TerminalKey::Backspace => {
            let byte = if has_control { 0x08 } else { 0x7f };
            Some(if has_alt {
                vec![0x1b, byte]
            } else {
                vec![byte]
            })
        }
        TerminalKey::Escape => {
            if modifier == 1 {
                Some(vec![0x1b])
            } else if has_alt && !has_control && !modifiers.super_key {
                Some(vec![0x1b, 0x1b])
            } else {
                Some(kitty_csi_u("27", modifier, None, None))
            }
        }
        TerminalKey::Up => Some(cursor_sequence(b'A', modes.application_cursor, modifier)),
        TerminalKey::Down => Some(cursor_sequence(b'B', modes.application_cursor, modifier)),
        TerminalKey::Right => Some(cursor_sequence(b'C', modes.application_cursor, modifier)),
        TerminalKey::Left => Some(cursor_sequence(b'D', modes.application_cursor, modifier)),
        TerminalKey::Home => Some(tilde_or_cursor_sequence(b'H', None, modifier)),
        TerminalKey::End => Some(tilde_or_cursor_sequence(b'F', None, modifier)),
        TerminalKey::Insert => Some(tilde_or_cursor_sequence(b'~', Some(2), modifier)),
        TerminalKey::Delete => Some(tilde_or_cursor_sequence(b'~', Some(3), modifier)),
        TerminalKey::PageUp => Some(tilde_or_cursor_sequence(b'~', Some(5), modifier)),
        TerminalKey::PageDown => Some(tilde_or_cursor_sequence(b'~', Some(6), modifier)),
        TerminalKey::Function(number) => function_key_sequence(number, modifier),
    }
}

/// 只在功能键编码路径中把 BackTab 的固有 Shift 纳入 Kitty modifier 位。
///
/// @author kongweiguang
fn effective_modifiers(event: TerminalKeyEvent) -> InputModifiers {
    let mut modifiers = event.modifiers;
    if event.key == TerminalKey::BackTab {
        modifiers.shift = true;
    }
    modifiers
}

/// 判断字符按键是否会与 legacy 控制字节或终端快捷键产生歧义。
///
/// @author kongweiguang
fn character_needs_disambiguation(event: TerminalKeyEvent, modifiers: InputModifiers) -> bool {
    modifiers.alt
        || modifiers.control
        || modifiers.super_key
        || matches!(
            event.key,
            TerminalKey::Escape | TerminalKey::Character('\u{1b}' | '\u{7f}')
        )
}

/// 标记已经有传统控制序列的非文本键，避免 Kitty CSI-u 错把它们当 Unicode PUA。
///
/// @author kongweiguang
fn is_functional_key(key: TerminalKey) -> bool {
    matches!(
        key,
        TerminalKey::Up
            | TerminalKey::Down
            | TerminalKey::Right
            | TerminalKey::Left
            | TerminalKey::Home
            | TerminalKey::End
            | TerminalKey::Insert
            | TerminalKey::Delete
            | TerminalKey::PageUp
            | TerminalKey::PageDown
            | TerminalKey::Function(_)
    )
}

/// Kitty legacy 模式没有 F13 以上键序列时，使用协议定义的 PUA 码点继续报告。
///
/// @author kongweiguang
fn has_non_legacy_function_encoding(key: TerminalKey) -> bool {
    matches!(key, TerminalKey::Function(21..=35))
}

/// 将导航键和功能键按 Kitty event type 扩展为 CSI 序列，不误用 CSI-u PUA 键码。
///
/// @author kongweiguang
fn kitty_functional_sequence(
    event: TerminalKeyEvent,
    modifiers: InputModifiers,
    flags: u8,
) -> Option<Vec<u8>> {
    let modifier = modifiers.protocol_value();
    let event_type = (flags & 0b10 != 0).then(|| phase_code(event.phase));
    let suffix = event_type.map_or_else(String::new, |kind| format!(":{kind}"));
    let sequence = match event.key {
        TerminalKey::Up => format!("\x1b[1;{modifier}{suffix}A"),
        TerminalKey::Down => format!("\x1b[1;{modifier}{suffix}B"),
        TerminalKey::Right => format!("\x1b[1;{modifier}{suffix}C"),
        TerminalKey::Left => format!("\x1b[1;{modifier}{suffix}D"),
        TerminalKey::Home => format!("\x1b[1;{modifier}{suffix}H"),
        TerminalKey::End => format!("\x1b[1;{modifier}{suffix}F"),
        TerminalKey::Insert => format!("\x1b[2;{modifier}{suffix}~"),
        TerminalKey::Delete => format!("\x1b[3;{modifier}{suffix}~"),
        TerminalKey::PageUp => format!("\x1b[5;{modifier}{suffix}~"),
        TerminalKey::PageDown => format!("\x1b[6;{modifier}{suffix}~"),
        TerminalKey::Function(number @ 1..=4) => {
            format!("\x1b[1;{modifier}{suffix}{}", char::from(b'P' + number - 1))
        }
        TerminalKey::Function(number @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(number - 5)];
            format!("\x1b[{code};{modifier}{suffix}~")
        }
        TerminalKey::Function(number @ 13..=35) => {
            let code = 57363 + u32::from(number);
            return Some(kitty_csi_u(&code.to_string(), modifier, event_type, None));
        }
        _ => return None,
    };
    Some(sequence.into_bytes())
}

/// 根据当前 flag 组合构造规范 CSI-u；仅在需要后续字段时显式保留默认 modifier=1。
///
/// @author kongweiguang
fn kitty_csi_u(
    key_field: &str,
    modifier: u8,
    event_type: Option<u8>,
    text_codes: Option<&str>,
) -> Vec<u8> {
    let has_following_field = event_type.is_some() || text_codes.is_some();
    let mut sequence = if modifier == 1 && !has_following_field {
        format!("\x1b[{key_field}")
    } else {
        format!("\x1b[{key_field};{modifier}")
    };
    if let Some(event_type) = event_type {
        sequence.push_str(&format!(":{event_type}"));
    }
    if let Some(text_codes) = text_codes {
        sequence.push(';');
        sequence.push_str(text_codes);
    }
    sequence.push('u');
    sequence.into_bytes()
}

/// 只把 Kitty 允许的非控制 Unicode code point 放进 associated-text 字段。
///
/// @author kongweiguang
fn associated_text_codes(text: &str) -> Option<String> {
    let codes = text
        .chars()
        .filter(|character| !is_protocol_control(*character))
        .map(|character| (character as u32).to_string())
        .collect::<Vec<_>>();
    (!codes.is_empty()).then(|| codes.join(":"))
}

/// 排除 Kitty associated-text 明确禁止的 C0、DEL 与 C1 控制码。
///
/// @author kongweiguang
fn is_protocol_control(character: char) -> bool {
    matches!(character as u32, 0x00..=0x1f | 0x7f..=0x9f)
}

/// 生成 Kitty CSI-u 的 shifted/base-layout 子字段，仅使用 host 确认提供的数据。
///
/// @author kongweiguang
fn append_alternate_keys(
    key_field: &mut String,
    alternates: TerminalKeyAlternates,
    modifiers: InputModifiers,
) {
    let shifted = modifiers.shift.then_some(alternates.shifted).flatten();
    match (shifted, alternates.base_layout) {
        (None, None) => {}
        (Some(shifted), None) => key_field.push_str(&format!(":{}", shifted as u32)),
        (shifted, Some(base_layout)) => {
            key_field.push_str(&format!(
                ":{}:{}",
                shifted.map_or(String::new(), |key| (key as u32).to_string()),
                base_layout as u32
            ));
        }
    }
}

/// 把 press/repeat/release 映射为 Kitty 协议事件编号。
///
/// @author kongweiguang
const fn phase_code(phase: KeyPhase) -> u8 {
    match phase {
        KeyPhase::Press => 1,
        KeyPhase::Repeat => 2,
        KeyPhase::Release => 3,
    }
}

/// 将 Kitty 的全键 keycode 与协议要求的渐进式 CSI/SS3 功能键输入分开维护。
///
/// @author kongweiguang
fn kitty_key_code(key: TerminalKey) -> Option<u32> {
    Some(match key {
        TerminalKey::Character(character) => character as u32,
        TerminalKey::Enter => 13,
        TerminalKey::Tab => 9,
        TerminalKey::Backspace => 127,
        TerminalKey::Escape => 27,
        TerminalKey::BackTab => 9,
        TerminalKey::Up => 57352,
        TerminalKey::Down => 57353,
        TerminalKey::Right => 57354,
        TerminalKey::Left => 57355,
        TerminalKey::Insert => 57348,
        TerminalKey::Delete => 57349,
        TerminalKey::PageUp => 57350,
        TerminalKey::PageDown => 57351,
        TerminalKey::Home => 57356,
        TerminalKey::End => 57357,
        TerminalKey::Function(number @ 1..=35) => 57363 + u32::from(number),
        TerminalKey::Function(_) => return None,
    })
}

/// 生成方向键序列，并按 application-cursor 与 xterm 修饰键模式选择格式。
///
/// @author kongweiguang
fn cursor_sequence(final_byte: u8, application_cursor: bool, modifier: u8) -> Vec<u8> {
    if modifier != 1 {
        return format!("\x1b[1;{modifier}{}", char::from(final_byte)).into_bytes();
    }
    if application_cursor {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

/// 编码 Home/End 与 tilde 类编辑键。
///
/// @author kongweiguang
fn tilde_or_cursor_sequence(final_byte: u8, number: Option<u8>, modifier: u8) -> Vec<u8> {
    match number {
        Some(number) if modifier == 1 => format!("\x1b[{number}~").into_bytes(),
        Some(number) => format!("\x1b[{number};{modifier}~").into_bytes(),
        None if modifier == 1 => vec![0x1b, b'[', final_byte],
        None => format!("\x1b[1;{modifier}{}", char::from(final_byte)).into_bytes(),
    }
}

/// 编码 F1 至 F20 的常见 legacy 序列，并用 Kitty PUA 承载 F21 至 F35。
///
/// @author kongweiguang
fn function_key_sequence(number: u8, modifier: u8) -> Option<Vec<u8>> {
    let bytes = match number {
        1 if modifier == 1 => b"\x1bOP".to_vec(),
        2 if modifier == 1 => b"\x1bOQ".to_vec(),
        3 if modifier == 1 => b"\x1bOR".to_vec(),
        4 if modifier == 1 => b"\x1bOS".to_vec(),
        1..=4 => format!("\x1b[1;{modifier}{}", char::from(b'P' + number - 1)).into_bytes(),
        5..=12 => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(number - 5)];
            if modifier == 1 {
                format!("\x1b[{code}~").into_bytes()
            } else {
                format!("\x1b[{code};{modifier}~").into_bytes()
            }
        }
        13..=20 => {
            let code = [25, 26, 28, 29, 31, 32, 33, 34][usize::from(number - 13)];
            if modifier == 1 {
                format!("\x1b[{code}~").into_bytes()
            } else {
                format!("\x1b[{code};{modifier}~").into_bytes()
            }
        }
        21..=35 => kitty_csi_u(
            &(57363 + u32::from(number)).to_string(),
            modifier,
            None,
            None,
        ),
        _ => return None,
    };
    Some(bytes)
}

/// 把可被传统终端表达的 Ctrl+ASCII 组合映射到 VT 控制字节。
///
/// @author kongweiguang
fn control_character(character: char) -> Option<u8> {
    let value = character.to_ascii_lowercase();
    Some(match value {
        'a'..='z' => value as u8 - b'a' + 1,
        ' ' | '@' | '2' => 0,
        '3' => 0x1b,
        '4' => 0x1c,
        '5' => 0x1d,
        '6' | '^' | '~' => 0x1e,
        '7' | '/' | '_' => 0x1f,
        '8' | '?' => 0x7f,
        ascii if ascii.is_ascii() => ascii as u8,
        _ => return None,
    })
}

/// 根据应用鼠标模式选择 SGR 或传统 X10 编码；未启用鼠标报告时留给选择 UI 处理。
///
/// @author kongweiguang
fn encode_mouse(event: TerminalMouseEvent, modes: TerminalModes) -> Option<Vec<u8>> {
    if modes.mouse_tracking == MouseTrackingMode::None {
        return None;
    }
    if event.action == TerminalMouseAction::Move && modes.mouse_tracking == MouseTrackingMode::Click
    {
        return None;
    }
    if event.action == TerminalMouseAction::Move
        && modes.mouse_tracking == MouseTrackingMode::ButtonMotion
        && !event.button_down
    {
        return None;
    }
    if event.action == TerminalMouseAction::Move && event.button_is_wheel() {
        return None;
    }
    if event.action == TerminalMouseAction::Move
        && modes.mouse_tracking == MouseTrackingMode::AnyMotion
        && !event.button_down
    {
        // DEC 1003 reports motion without a button as button code 3.
        let mut code = 3;
        code += modifier_bits(event.modifiers);
        return encode_mouse_code(code, event, modes, 'M');
    }
    let mut code = match event.button {
        TerminalMouseButton::Left => 0,
        TerminalMouseButton::Middle => 1,
        TerminalMouseButton::Right => 2,
        TerminalMouseButton::WheelUp => 64,
        TerminalMouseButton::WheelDown => 65,
        TerminalMouseButton::WheelLeft => 66,
        TerminalMouseButton::WheelRight => 67,
    };
    code += modifier_bits(event.modifiers);
    match event.action {
        TerminalMouseAction::Move => code += 32,
        TerminalMouseAction::Scroll => {}
        TerminalMouseAction::Release if !modes.sgr_mouse => {
            code = 3 + modifier_bits(event.modifiers)
        }
        TerminalMouseAction::Release => {}
        TerminalMouseAction::Press => {}
    }
    let column = u32::from(event.column) + 1;
    let row = u32::from(event.row) + 1;
    if modes.sgr_mouse {
        let suffix = if event.action == TerminalMouseAction::Release {
            'm'
        } else {
            'M'
        };
        Some(format!("\x1b[<{code};{column};{row}{suffix}").into_bytes())
    } else if column < 224 && row < 224 && code < 224 {
        Some(vec![
            0x1b,
            b'[',
            b'M',
            (code + 32) as u8,
            (column + 32) as u8,
            (row + 32) as u8,
        ])
    } else {
        None
    }
}

/// 提取鼠标修饰键位掩码，保持 X10 与 SGR 对 modifiers 的相同定义。
///
/// @author kongweiguang
const fn modifier_bits(modifiers: InputModifiers) -> u32 {
    (modifiers.shift as u32) * 4 + (modifiers.alt as u32) * 8 + (modifiers.control as u32) * 16
}

impl TerminalMouseEvent {
    /// 将 wheel 事件从按键拖动路径中排除，避免伪造按钮移动序列。
    ///
    /// @author kongweiguang
    const fn button_is_wheel(self) -> bool {
        matches!(
            self.button,
            TerminalMouseButton::WheelUp
                | TerminalMouseButton::WheelDown
                | TerminalMouseButton::WheelLeft
                | TerminalMouseButton::WheelRight
        )
    }
}

/// 按指定终结符输出鼠标事件，保留 SGR 真实 button 编码与释放终结符。
///
/// @author kongweiguang
fn encode_mouse_code(
    code: u32,
    event: TerminalMouseEvent,
    modes: TerminalModes,
    suffix: char,
) -> Option<Vec<u8>> {
    let column = u32::from(event.column) + 1;
    let row = u32::from(event.row) + 1;
    if modes.sgr_mouse {
        Some(format!("\x1b[<{code};{column};{row}{suffix}").into_bytes())
    } else if column < 224 && row < 224 && code < 224 {
        Some(vec![
            0x1b,
            b'[',
            b'M',
            (code + 32) as u8,
            (column + 32) as u8,
            (row + 32) as u8,
        ])
    } else {
        None
    }
}
