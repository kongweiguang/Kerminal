// 两代脚本在隔离 VM 中通过宿主请求接口测试，不能将模拟结果当作原生窗口或真实插件进程证据。
// @author kongweiguang
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

/** 每个版本使用独立宿主替身，避免插件实例或持久状态跨代泄漏。 */
async function createFixture(version) {
  const source = await readFile(new URL(`./notes-${version}/dist/main.js`, import.meta.url), 'utf8');
  const handlers = new Map();
  const updates = [];
  const host = {
    environment: { locale: 'en-US' },
    onRequest: (name, handler) => handlers.set(name, handler),
    request: async name => {
      if (name === 'state.get') return { state: { text: 'persisted' } };
      throw new Error(`unexpected host request: ${name}`);
    },
    view: { update: async (instance, patch) => { updates.push({ instance, patch }); } },
  };
  vm.runInContext(source, vm.createContext({ kerminal: host }), { filename: `notes-${version}/dist/main.js` });
  return { handlers, updates };
}

for (const version of ['1.0', '1.1']) {
  const { handlers, updates } = await createFixture(version);
  const open = handlers.get('view.open');
  const action = handlers.get('view.action');
  const snapshot = handlers.get('view.snapshot');
  const close = handlers.get('view.close');
  const first = await open({ instance: 'first', view: 'notes.editor', state: { text: 'first draft' } });
  const second = await open({ instance: 'second', view: 'notes.editor', state: { text: 'second draft' } });
  assert.equal(first.data.marker, `${version}.0`);
  assert.equal(first.data.text, 'first draft');
  assert.equal(second.data.text, 'second draft');
  if (version === '1.0') {
    assert.equal('codePointCount' in first.data, false);
    await assert.rejects(action({ instance: 'first', action: 'count-code-points', data: { text: 'A猫🙂' } }), /unknown action/);
    assert.equal(updates.length, 0);
  } else {
    assert.equal(first.data.codePointCount, '');
    const inputs = ['A猫🙂', 'é🙂!'];
    for (const [index, input] of inputs.entries()) {
      const instance = index === 0 ? 'first' : 'second';
      await action({ instance, action: 'count-code-points', data: { text: input } });
      assert.equal(updates.length, index + 1);
      assert.equal(updates[index].instance, instance);
      assert.equal(updates[index].patch.codePointCount, Array.from(input).length);
      assert.notEqual(updates[index].patch.codePointCount, input.length);
    }
    assert.equal(snapshot({ instance: 'first', data: {} }).state.text, inputs[0]);
    assert.equal(snapshot({ instance: 'second', data: {} }).state.text, inputs[1]);
  }
  close({ instance: 'first', data: {} });
  await assert.rejects(action({ instance: 'first', action: 'count-code-points', data: {} }), /stale instance/);
  assert.equal(snapshot({ instance: 'second', data: {} }).state_version, 1);
}
console.log('restart fixture version, Unicode count and instance isolation passed');
