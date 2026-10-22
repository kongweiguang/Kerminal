// @author kongweiguang
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

/** 通过冻结版前端 decoder 检查旧版实际恢复出的标签分组、递归布局与焦点。 */
async function main() {
  const root = process.env.KERMINAL_LEGACY_READBACK_ROOT;
  const expectedPath = process.env.KERMINAL_LEGACY_READBACK_EXPECTED;
  const bundlePath = process.env.KERMINAL_LEGACY_WORKSPACE_BUNDLE;
  assert.ok(root, '缺少隔离的旧版配置根目录');
  assert.ok(expectedPath, '缺少 fixture expected.json');
  assert.ok(bundlePath, '缺少归档源码 bundle');

  const expected = JSON.parse(readFileSync(expectedPath, 'utf8'));
  const source = JSON.parse(readFileSync(join(root, 'workspace', 'session.json'), 'utf8'));
  assert.equal(expected.contractVersion, 1);
  assert.equal(source.version, 3, '候选必须是 workspace session v3');

  const { decodeWorkspaceSessionSnapshot } = await import(pathToFileURL(bundlePath).href);
  const decoded = decodeWorkspaceSessionSnapshot(source);
  assert.equal(decoded.kind, 'loaded', decoded.message ?? '旧 decoder 未加载 session');
  assert.equal(decoded.session.activeTabId, expected.workspace.activeTabId);
  assert.equal(decoded.session.focusedPaneId, expected.workspace.focusedPaneId);
  assert.equal(decoded.session.terminalTabs.length, 2, '旧 decoder 应恢复两个 terminal tabs');
  const activeTab = decoded.session.terminalTabs.find(
    (tab) => tab.id === expected.workspace.activeTabId,
  );
  const secondTab = decoded.session.terminalTabs.find(
    (tab) => tab.id === expected.workspace.secondTabId,
  );
  assert.ok(activeTab, '旧 decoder 应保留活动 tab');
  assert.ok(secondTab, '旧 decoder 应保留第二个 tab');
  assert.equal(activeTab.tabGroupId, expected.workspace.groupId);
  assert.equal(secondTab.tabGroupId, expected.workspace.groupId);
  const group = decoded.session.terminalTabGroups[expected.workspace.groupId];
  assert.ok(group, '旧 decoder 应保留显式标签组');
  assert.equal(group.title, expected.workspace.groupTitle);
  assert.equal(group.color, 'teal');
  assert.equal(group.collapsed, false);

  const paneIds = decoded.session.terminalPanes.map((pane) => pane.id).sort();
  assert.deepEqual(paneIds, [...expected.workspace.paneIds].sort());
  assert.equal(activeTab.layout.type, 'split');
  assert.equal(activeTab.layout.id, expected.workspace.rootSplitId);
  assert.equal(activeTab.layout.direction, 'horizontal');
  assert.equal(activeTab.layout.children.length, 2);
  assert.equal(activeTab.layout.children[0].type, 'pane');
  assert.equal(activeTab.layout.children[0].paneId, expected.workspace.paneIds[0]);
  const nestedSplit = activeTab.layout.children[1];
  assert.equal(nestedSplit.type, 'split');
  assert.equal(nestedSplit.id, expected.workspace.nestedSplitId);
  assert.equal(nestedSplit.direction, 'vertical');
  assert.deepEqual(
    nestedSplit.children.map((child) => child.paneId),
    expected.workspace.paneIds.slice(1, 3),
  );
  assert.equal(
    nestedSplit.children.some((child) => child.paneId === decoded.session.focusedPaneId),
    true,
    '旧 decoder 应把焦点保留在活动 tab 的递归布局中',
  );
  assert.equal(secondTab.layout.type, 'pane');
  assert.equal(secondTab.layout.paneId, expected.workspace.paneIds[3]);
  process.stdout.write('旧版 workspace v3 decoder 回读通过。\n');
}

await main();
