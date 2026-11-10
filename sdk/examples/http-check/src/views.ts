// @author kongweiguang
import { ui } from "../../../ts/src/builders.ts";

// 结果结构固定，重复检查只替换数据，不重建标签内容。
export const views = {
  "result.json": ui.view({
    data: { url: "", message: "", busy: false },
    root: ui.stack({ gap: "md", children: [
      ui.section({ title: "%result.address%", children: [ui.text({ text: "$url" })] }),
      ui.section({ title: "%result.status%", children: [ui.text({ text: "$message" })] }),
      ui.button({ label: "%result.refresh%", action: "refresh", busy: "$busy" }),
    ] }),
  }),
};

// CLI 需要包内完整路径，保留命名导出供 Node 构建脚本复用。
export default Object.fromEntries(Object.entries(views).map(([name, view]) => [`views/${name}`, view]));
