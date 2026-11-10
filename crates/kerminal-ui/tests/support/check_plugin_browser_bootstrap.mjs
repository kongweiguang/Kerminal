// 执行实际注入脚本，核对主题、代次、快捷键与关闭信号；不宣称原生浏览器验收。
// @author kongweiguang
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const source = readFileSync(new URL('../../src/plugins/external_webview.rs', import.meta.url), 'utf8');
const sdk = readFileSync(new URL('../../resources/plugin-browser-sdk.js', import.meta.url), 'utf8');
const template = source.match(/r#"\(\(\)=>\{\{([\s\S]*?)\}\}\)\(\);"#/);
assert.ok(template, 'Rust bootstrap template must be found');
const parameters = { identity: { view_instance: 'instance', generation: 4 }, environment: { locale: 'zh-CN', theme: 'dark' } };
const script = `(()=>{{${template[1]}}})();`.replaceAll('{sdk}', '\u0001').replaceAll('{values}', '\u0002')
  .replaceAll('{{', '{').replaceAll('}}', '}').replaceAll('\u0001', sdk).replaceAll('\u0002', JSON.stringify(parameters));
const listeners = new Map();
const sent = [];
const styles = new Map();
let heartbeat;
let intervalCleared = false;
const window = {
  ipc: { postMessage: value => sent.push(JSON.parse(value)) },
  addEventListener: (name, handler) => { const entries = listeners.get(name) ?? []; entries.push(handler); listeners.set(name, entries); },
  removeEventListener: (name, handler) => listeners.set(name, (listeners.get(name) ?? []).filter(entry => entry !== handler)),
  setInterval: handler => { heartbeat = handler; return 1; },
  clearInterval: id => { assert.equal(id, 1); intervalCleared = true; },
};
const root = { style: { setProperty: (name, value) => styles.set(name, value) } };
vm.runInNewContext(script, { window, document: { documentElement: root }, AbortController, console });
/** 事件使用真实注入监听器，避免测试复制分派实现。 */
function emit(name, event) { for (const listener of [...(listeners.get(name) ?? [])]) listener(event); }
emit('kerminal:message', { detail: { view_instance: 'old', generation: 4, message: { method: 'theme.changed', params: { tokens: { '--k-bg': 'red' } } } } });
assert.equal(styles.size, 0);
emit('kerminal:message', { detail: { ...parameters.identity, message: { jsonrpc: '2.0', method: 'theme.changed', params: { theme: 'dark', tokens: { '--k-bg': 'black', '--k-accent': 'blue', position: 'fixed' } } } } });
assert.equal(styles.get('--k-bg'), 'black');
assert.equal(styles.get('--k-accent'), 'blue');
assert.equal(styles.has('position'), false);
assert.equal(root.style.colorScheme, 'dark');
let prevented = false;
emit('keydown', { ctrlKey: true, shiftKey: true, code: 'KeyP', preventDefault: () => { prevented = true; } });
assert.equal(prevented, true);
assert.equal(sent.at(-1).message.method, 'webview.commandPalette');
const before = sent.length;
emit('keydown', { ctrlKey: true, shiftKey: true, code: 'KeyP', isComposing: true, preventDefault: () => assert.fail('IME must not trigger command palette') });
assert.equal(sent.length, before);
heartbeat();
assert.equal(sent.at(-1).message.method, 'webview.heartbeat');
const pending = window.kerminal.request('view.action', { action: 'search' });
emit('pagehide', {});
await assert.rejects(pending, error => error.code === -32004);
assert.equal(intervalCleared, true);
assert.equal(sent.at(-1).message.method, '$/cancel');
console.log('Browser bootstrap: theme isolation, shortcut/IME, heartbeat, pagehide cancellation passed.');
