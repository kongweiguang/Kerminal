//! 独立识别同一系统帧中的网页与宿主浮层，不能由其他截图拼接补足。
//!
//! @author kongweiguang

/// 固定夹具的背景、细节和浮层分别计数，避免单色背景或遮罩误通过。
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub(super) struct FixturePixels {
    pub body: usize,
    pub proof: usize,
    pub overlay: usize,
}

impl FixturePixels {
    /// 就绪阶段不应残留浮层；菜单和对话框必须同时包含网页及宿主标记。
    pub(super) fn accepted(self, overlay: bool) -> bool {
        self.body >= 2000
            && self.proof >= 500
            && if overlay {
                self.overlay >= 100
            } else {
                self.overlay == 0
            }
    }
}

/// 只接受已知遮罩对应的离散衰减色，避免将黑屏或其他深色区域当成网页。
fn matches_page_pixel(pixel: &[u8], color: [u8; 3], modal: bool) -> bool {
    [100u16, 95, 80]
        .into_iter()
        .filter(|scale| modal || *scale == 100)
        .any(|scale| {
            pixel[..3].iter().zip(color).all(|(channel, expected)| {
                let expected = ((u16::from(expected) * scale + 50) / 100) as u8;
                channel.abs_diff(expected) <= 3
            })
        })
}

/// 输入为捕获帧原始 BGRA；尾部不完整像素不会参与验收。
pub(super) fn count_fixture_pixels(bgra: &[u8], modal: bool) -> FixturePixels {
    let mut result = FixturePixels::default();
    for pixel in bgra.chunks_exact(4) {
        let body = matches_page_pixel(pixel, [0x56, 0x34, 0x12], modal);
        result.body += usize::from(body);
        result.proof += usize::from(!body && matches_page_pixel(pixel, [0x78, 0xcf, 0x25], modal));
        result.overlay += usize::from(matches_page_pixel(pixel, [0x91, 0x3a, 0xe6], false));
    }
    result
}

#[cfg(test)]
#[path = "../../tests/support/webview_probe_pixels_unit.rs"]
mod tests;
