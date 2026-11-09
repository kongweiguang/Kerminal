// @author kongweiguang
import { send } from './bridge.js';
const boot = Math.random().toString(36).slice(2);
// 回执读取实际 DOM 和视口，不能从宿主发送值推定浏览器已更新。
function inspect(request_id) {
  const input = document.getElementById('draft');
  send({request_id, boot, draft: input.value, width: innerWidth, height: innerHeight,
    dpr: devicePixelRatio, background: getComputedStyle(document.body).backgroundColor});
}
// 等待浏览器两次绘制再回执，避免宿主将未提交的布局当成当前尺寸。
window.addEventListener('kerminal:message', event => {
  const message = event.detail;
  if (message.operation === 'draft') document.getElementById('draft').value = message.value;
  requestAnimationFrame(() => requestAnimationFrame(() => inspect(message.request_id)));
});
inspect('ready');
