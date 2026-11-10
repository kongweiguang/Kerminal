// 验证注入副本直接对应 SDK 源码，避免浏览器协议在两处演进时悄悄分叉。
// @author kongweiguang
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';

const root = new URL('../../../../', import.meta.url);
const require = createRequire(new URL('sdk/ts/package.json', root));
const ts = require('typescript');
const source = readFileSync(new URL('sdk/ts/src/browser.ts', root), 'utf8');
const actual = readFileSync(new URL('crates/kerminal-ui/resources/plugin-browser-sdk.js', root), 'utf8');
const compiled = ts.transpileModule(source, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
}).outputText.replace(/\r\n/g, '\n').replace(/^export /gm, '');
const embedded = actual.replace(/\r\n/g, '\n').split('\n').slice(1).join('\n');
if (compiled !== embedded) {
    console.error('Browser SDK injection differs from sdk/ts/src/browser.ts');
    process.exitCode = 1;
}
