// @author kongweiguang

import { ui } from "../../../ts/src/builders.ts";

// 静态树只绑定数据，交互状态和草稿由宿主持有并按实例隔离。
export const views = {
  "panel.json": ui.view({
    data: { saved: "", showPreview: true },
    root: ui.stack({ gap: "md", children: [
      ui.text({ text: "%panel.saved%", tone: "muted" }),
      ui.text({ text: "$saved", visible: "$showPreview" }),
    ] }),
  }),
  "editor.json": ui.view({
    data: { text: "", busy: false },
    root: ui.stack({ gap: "md", children: [
      ui.textarea({ bind: "text", label: "%editor.text%", placeholder: "%editor.placeholder%" }),
      ui.toolbar({ children: [
        ui.button({ label: "%editor.save%", action: "save", variant: "primary" }),
        ui.button({ label: "%editor.check%", action: "check", busy: "$busy" }),
      ] }),
    ] }),
  }),
  "settings.json": ui.view({
    root: ui.section({ title: "%settings.title%", children: [
      ui.form({ schema: "schemas/settings.json" }),
    ] }),
  }),
};

// CLI 需要包内完整路径；独立示例脚本仍可使用按文件名索引的 views。
export default Object.fromEntries(Object.entries(views).map(([name, view]) => [`views/${name}`, view]));
