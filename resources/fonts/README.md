# Linux 简体中文字体

<!-- @author kongweiguang -->

此目录中的 `NotoSansCJKsc-Regular.otf` 是从 Noto CJK 官方仓库固定版本复制的完整简体中文 Regular OTF，随 Linux 桌面程序内嵌，以满足 GPUI/Cosmic Text 使用的 `Noto Sans CJK SC` Han fallback。它保留完整官方字体字形范围，不按当前静态 UI 文案裁剪；终端输出、主机名和文件名都可能在运行时出现其他中文字符。

- 上游项目与版本：[notofonts/noto-cjk，Sans2.004](https://github.com/notofonts/noto-cjk/releases/tag/Sans2.004)
- 固定 Git commit：`523d033d6cb47f4a80c58a35753646f5c3608a78`
- 固定源文件：[NotoSansCJKsc-Regular.otf](https://github.com/notofonts/noto-cjk/blob/523d033d6cb47f4a80c58a35753646f5c3608a78/Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Regular.otf)
- 固定 raw URL：<https://raw.githubusercontent.com/notofonts/noto-cjk/523d033d6cb47f4a80c58a35753646f5c3608a78/Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Regular.otf>
- 上游文件大小：16,437,364 bytes
- 上游 Git blob SHA-1：`dc15562470b4f842321894787a0d066879ccff8b`
- 本地 SHA-256：`2C76254F6FC379FDDFCE0A7E84FB5385BB135D3E399294F6EEB6680D0365B74B`
- 字体元数据：family `Noto Sans CJK SC`，style `Regular`；使用 `fc-query` 验证包含常用汉字、CJK Unified Ideographs 与 Extension A 字符。
- 许可证：SIL Open Font License 1.1，原文见 [`licenses/NOTO-CJK-SIL-OFL-1.1.txt`](../../licenses/NOTO-CJK-SIL-OFL-1.1.txt)。上游 license 源文件大小 4,301 bytes，Git blob SHA-1 `d952d62c065f3f35fb83a173496e90b21525aef3`，本地 SHA-256 `6A73F9541C2DE74158C0E7CF6B0A58EF774F5A780BF191F2D7EC9CC53EFE2BF2`；Linux 打包配置将原文复制到安装包的 `licenses/` 目录。

该字体只在 Linux 启动时注册到 GPUI TextSystem；Windows 和 macOS 继续使用平台字体 fallback。
