//! 随 Linux 桌面程序分发并注册的 CJK 字体。
//!
//! @author kongweiguang

use std::borrow::Cow;

use gpui_pre::{Result, TextSystem};

const NOTO_SANS_CJK_SC_REGULAR: &[u8] =
    include_bytes!("../../../resources/fonts/NotoSansCJKsc-Regular.otf");
const NOTO_SANS_CJK_SC_FAMILY: &str = "Noto Sans CJK SC";

/// 首窗之前把字体加入 GPUI 的同一 TextSystem，并确认 Cosmic Text 能看到 fallback 家族。
///
/// @author kongweiguang
pub(crate) fn register_noto_sans_cjk_sc(text_system: &TextSystem) -> Result<()> {
    text_system.add_fonts(vec![Cow::Borrowed(NOTO_SANS_CJK_SC_REGULAR)])?;

    if !text_system
        .all_font_names()
        .iter()
        .any(|family| family == NOTO_SANS_CJK_SC_FAMILY)
    {
        return Err(
            std::io::Error::other("注册字体后 GPUI TextSystem 中未发现 Noto Sans CJK SC").into(),
        );
    }

    Ok(())
}
