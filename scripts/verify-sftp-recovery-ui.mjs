#!/usr/bin/env node
/**
 * SFTP 传输恢复队列的真实浏览器视觉与交互冒烟验证。
 *
 * 该脚本通过 Vite 挂载生产 SftpTransferQueueRow，不连接真实主机或 vault；
 * 通过 CDP 驱动窄面板、三种主题、键盘操作和 DOM 几何检查，避免静态截图掩盖
 * 状态标签、恢复动作或长文件名溢出问题。
 *
 * @author kongweiguang
 */

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import http from "node:http";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

const repoRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
const args = parseArgs(process.argv.slice(2));

if (args.help) {
  printHelp();
  process.exit(0);
}

const outputPath = path.resolve(
  repoRoot,
  args.output ?? ".updeng/tmp/sftp-recovery-ui.json",
);
const screenshotBase = outputPath.replace(/\.json$/i, "");
const chromePath = findChromePath();
const chromePort = 9_280 + Math.floor(Math.random() * 300);
const vitePort = 10_380 + Math.floor(Math.random() * 300);
const userDataDir = path.join(
  tmpdir(),
  `kerminal-sftp-recovery-ui-${Date.now()}`,
);

if (!chromePath) {
  console.error(
    "Chrome executable not found. Set CHROME_PATH to run this visual smoke.",
  );
  process.exit(1);
}

/**
 * 启动隔离 Vite 和 headless Chrome，并汇总主题、布局、无障碍与交互证据。
 *
 * Vite 使用真实生产组件，Chrome 使用固定窄视口；这样失败报告能区分组件状态
 * 投影问题和静态源代码检查问题，同时不会触碰真实传输或凭据。
 */
async function main() {
  const vite = await createServer({
    configFile: path.join(repoRoot, "vite.config.ts"),
    plugins: [sftpRecoverySmokePlugin()],
    root: repoRoot,
    server: {
      host: "127.0.0.1",
      port: vitePort,
      strictPort: true,
    },
  });
  await vite.listen();

  const chrome = spawn(
    chromePath,
    [
      "--headless=new",
      "--disable-background-networking",
      "--disable-default-apps",
      "--disable-extensions",
      "--disable-sync",
      "--hide-scrollbars",
      "--mute-audio",
      "--no-first-run",
      "--no-default-browser-check",
      `--remote-debugging-port=${chromePort}`,
      `--user-data-dir=${userDataDir}`,
      "about:blank",
    ],
    {
      stdio: ["ignore", "ignore", "pipe"],
      windowsHide: true,
    },
  );

  let stderr = "";
  chrome.stderr.on("data", (chunk) => {
    stderr += String(chunk);
  });

  let client;
  try {
    await waitForHttpOk(
      vitePort,
      "/__sftp_recovery_ui_smoke",
      20_000,
    );
    await waitForChrome(chromePort, chrome);
    const target = await requestJson(
      chromePort,
      "/json/new?about:blank",
      "PUT",
    );
    client = await CdpClient.connect(target.webSocketDebuggerUrl);
    await client.send("Runtime.enable");
    await client.send("Page.enable");
    const viewportWidths = [320, 440];
    await client.send("Emulation.setDeviceMetricsOverride", {
      deviceScaleFactor: 1,
      height: 900,
      mobile: false,
      width: viewportWidths[0],
    });
    await client.send("Page.navigate", {
      url: `http://127.0.0.1:${vitePort}/__sftp_recovery_ui_smoke`,
    });
    await waitForBrowserExpression(
      client,
      "window.__sftpRecoveryUiSmokeReady === true",
      30_000,
    );

    const screenshots = {};
    const themes = [];
    const failures = [];
    for (const viewportWidth of viewportWidths) {
      await client.send("Emulation.setDeviceMetricsOverride", {
        deviceScaleFactor: 1,
        height: 900,
        mobile: false,
        width: viewportWidth,
      });
      for (const [theme, mediaTheme] of [["light", "light"], ["dark", "dark"], ["system", "light"], ["system", "dark"]]) {
        await client.send("Emulation.setEmulatedMedia", {
          features: [{ name: "prefers-color-scheme", value: mediaTheme }],
        });
        const themeResult = await evaluate(
          client,
          `window.__sftpRecoveryUiSmoke.setTheme(${JSON.stringify(theme)})`,
        );
        await waitForBrowserExpression(
          client,
          `document.documentElement.dataset.theme === ${JSON.stringify(mediaTheme)}`,
          10_000,
        );
        const validation = await evaluate(
          client,
          "window.__sftpRecoveryUiSmoke.validate()",
        );
        const themeValidation = validation.result?.value ?? {
          failures: ["missing-validation-result"],
        };
        failures.push(
          ...themeValidation.failures.map(
            (failure) => `${viewportWidth}:${theme}:${failure}`,
          ),
        );
        themes.push({
          mode: theme,
          resolvedTheme: themeResult.result?.value?.resolvedTheme,
          validation: themeValidation,
          viewportWidth,
        });

        const screenshot = await client.send("Page.captureScreenshot", {
          captureBeyondViewport: true,
          format: "png",
          fromSurface: true,
        });
        const screenshotPath = `${screenshotBase}-${viewportWidth}-${theme}-${mediaTheme}.png`;
        mkdirSync(path.dirname(screenshotPath), { recursive: true });
        writeFileSync(screenshotPath, Buffer.from(screenshot.data, "base64"));
        screenshots[`${viewportWidth}-${theme}-${mediaTheme}`] = path
          .relative(repoRoot, screenshotPath)
          .replaceAll("\\", "/");
      }
    }

    const interactions = await runInteractions(client);
    failures.push(...interactions.failures);
    const report = {
      schemaVersion: 1,
      generatedAt: new Date().toISOString(),
      appUrl: `http://127.0.0.1:${vitePort}/__sftp_recovery_ui_smoke`,
      artifacts: {
        json: path.relative(repoRoot, outputPath).replaceAll("\\", "/"),
        screenshots,
      },
      environment: {
        chromePath,
        node: process.version,
        viewportWidths,
      },
      failures,
      interactions,
      pass: failures.length === 0,
      themes,
    };

    mkdirSync(path.dirname(outputPath), { recursive: true });
    writeFileSync(outputPath, `${JSON.stringify(report, null, 2)}\n`, "utf8");
    console.log(
      `SFTP recovery UI smoke: ${report.pass ? "passed" : "failed"}, themes ${themes.length}, screenshots ${Object.keys(screenshots).length}.`,
    );
    console.log(`Report: ${report.artifacts.json}`);
    if (!report.pass) {
      process.exitCode = 1;
    }
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    if (client) {
      try {
        console.error(
          JSON.stringify(await collectFailureDiagnostics(client), null, 2),
        );
      } catch (diagnosticError) {
        console.error(
          diagnosticError instanceof Error
            ? diagnosticError.message
            : String(diagnosticError),
        );
      }
    }
    if (stderr.trim()) {
      console.error(stderr.trim());
    }
    process.exitCode = 1;
  } finally {
    client?.close();
    await terminateChrome(chrome);
    await vite.close();
    try {
      rmSync(userDataDir, {
        force: true,
        maxRetries: 5,
        recursive: true,
        retryDelay: 100,
      });
    } catch (cleanupError) {
      console.error(
        `Chrome profile cleanup deferred: ${cleanupError instanceof Error ? cleanupError.code ?? cleanupError.message : String(cleanupError)}`,
      );
    }
  }
}

/**
 * 注入一个只依赖真实队列行的 Vite 路由，保持冒烟页不会成为生产入口。
 */
function sftpRecoverySmokePlugin() {
  const htmlPath = "/__sftp_recovery_ui_smoke";
  const entryPath = "/__sftp_recovery_ui_smoke_entry.jsx";
  return {
    name: "kerminal-sftp-recovery-ui-smoke",
    configureServer(server) {
      server.middlewares.use(htmlPath, async (request, response) => {
        const html = await server.transformIndexHtml(
          request.originalUrl ?? htmlPath,
          sftpRecoverySmokeHtml(entryPath),
        );
        response.writeHead(200, {
          "content-type": "text/html; charset=utf-8",
        });
        response.end(html);
      });
    },
    load(id) {
      return id.endsWith(entryPath) ? sftpRecoverySmokeEntry() : undefined;
    },
    resolveId(id) {
      return id === entryPath ? entryPath : undefined;
    },
  };
}

/**
 * 创建最小 HTML 外壳；固定 viewport 由 CDP 设置以复现窄侧栏布局。
 */
function sftpRecoverySmokeHtml(entryPath) {
  return `<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Kerminal SFTP recovery UI smoke</title>
</head>
<body>
  <div id="root"></div>
  <script type="module" src="${entryPath}"></script>
</body>
</html>`;
}

/**
 * 返回真实队列组件的 React harness；同时区分字节到齐、提交失败与成功，防止满格误报完成。
 */
function sftpRecoverySmokeEntry() {
  return `
import React, { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import "/src/App.css";
import { useDocumentTheme } from "/src/lib/useDocumentTheme";
import { SftpTransferQueueRow } from "/src/features/sftp/SftpTransferQueueRow";

const longName = "release-artifact-with-a-deliberately-long-name-for-narrow-panels-2026-09-22.tar.zst";

/** 分别覆盖字节到齐后的提交、拒绝与未知结果，避免成功状态掩盖真实失败。 */
function fixtureTransfers() {
  return [
    transfer({ id: "waiting", phase: "waiting", status: "running", bytesTransferred: 32768 }),
    transfer({ id: "recovering", phase: "recovering", status: "running", recoveryAttempt: 1, bytesTransferred: 196608 }),
    transfer({ id: "canceling", phase: "canceling", status: "running", cancelRequested: true, bytesTransferred: 65536 }),
    transfer({ id: "idle-failed", failureKind: "idleTimeout", idleTimeoutSeconds: 180, retryable: true, resumable: true, status: "failed", bytesTransferred: 524288 }),
    transfer({ id: "committing", phase: "committing", cancelable: false, status: "running", bytesTransferred: 1048576 }),
    transfer({ id: "commit-failed", retryable: true, resumable: true, status: "failed", error: "替换目标文件失败，原文件已恢复，可继续传输。", bytesTransferred: 1048576 }),
    transfer({ id: "commit-unknown", failureKind: "commitUnknown", retryable: false, status: "failed", bytesTransferred: 1048576 }),
    transfer({ id: "canceled", status: "canceled", cancelRequested: true, bytesTransferred: 131072 }),
    transfer({ id: "succeeded", status: "succeeded", bytesTransferred: 1048576, totalBytes: 1048576 }),
  ];
}

function transfer(overrides = {}) {
  const localPath = "C:/Users/example/Downloads/" + longName;
  const remotePath = "/srv/releases/" + longName;
  return {
    bytesTransferred: 0,
    cancelRequested: false,
    conflictPolicy: "overwrite",
    createdAt: 1,
    currentItem: null,
    direction: "upload",
    error: null,
    hostId: "visual-smoke-host",
    id: "transfer",
    kind: "file",
    localPath,
    operation: "upload",
    phase: null,
    remotePath,
    source: { kind: "local", path: localPath },
    status: "queued",
    target: { hostId: "visual-smoke-host", hostLabel: "视觉验收主机", kind: "remote", path: remotePath },
    totalBytes: 1048576,
    transportMode: "singleHostSftp",
    updatedAt: 1,
    viewScope: null,
    ...overrides,
  };
}

function SmokeApp() {
  const [themeMode, setThemeMode] = useState("light");
  const [systemTheme, setSystemTheme] = useState(() => window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  const [transfers, setTransfers] = useState(() => fixtureTransfers());
  const [actions, setActions] = useState([]);
  const resolvedTheme = themeMode === "system" ? systemTheme : themeMode;

  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const update = () => setSystemTheme(media.matches ? "dark" : "light");
    media.addEventListener?.("change", update);
    return () => media.removeEventListener?.("change", update);
  }, []);

  useDocumentTheme({ density: "comfortable", language: "zh-CN", lang: "zh-CN", theme: resolvedTheme });

  useEffect(() => {
    window.__sftpRecoveryUiSmoke = {
      actionCount: (kind) => actions.filter((action) => action.kind === kind).length,
      getActions: () => actions,
      reset: () => { setTransfers(fixtureTransfers()); setActions([]); },
      setTheme: async (nextTheme) => { setThemeMode(nextTheme); await nextFrame(); return { mode: nextTheme, resolvedTheme: nextTheme === "system" ? (window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light") : nextTheme }; },
      snapshot: () => ({ actions, dataTheme: document.documentElement.dataset.theme, resolvedTheme, themeMode }),
      validate: () => validateQueue(themeMode, resolvedTheme),
    };
    window.__sftpRecoveryUiSmokeReady = true;
  }, [actions, resolvedTheme, themeMode, transfers]);

  const onCancel = (transferId) => {
    setActions((current) => [...current, { id: transferId, kind: "cancel" }]);
    setTransfers((current) => current.map((item) => item.id === transferId ? { ...item, cancelRequested: true, phase: "canceling" } : item));
  };
  const onRetry = (item) => {
    setActions((current) => [...current, { id: item.id, kind: "retry" }]);
    setTransfers((current) => current.map((currentItem) => currentItem.id === item.id ? { ...currentItem, phase: "recovering", recoveryAttempt: 1, status: "running", failureKind: null } : currentItem));
  };

  return React.createElement("main", { className: "min-h-screen bg-[var(--app-bg)] p-3 text-zinc-950 dark:text-zinc-50", id: "sftp-recovery-smoke-shell", style: { maxWidth: "440px", overflowX: "hidden" } },
    React.createElement("section", { "aria-label": "SFTP 传输恢复队列视觉验收", className: "min-w-0 space-y-1.5", id: "sftp-recovery-queue" },
      transfers.map((item) => React.createElement("div", { "data-sftp-transfer-state": item.id, className: "min-w-0", key: item.id }, React.createElement(SftpTransferQueueRow, { onCancel, onRetry, transfer: item }))),
    ),
  );
}

/** 同时检查布局、主题及提交语义；100% 与取消入口必须服从后端终态和提交门禁。 */
function validateQueue(themeMode, resolvedTheme) {
  const failures = [];
  const shell = document.querySelector("#sftp-recovery-smoke-shell");
  const queue = document.querySelector("#sftp-recovery-queue");
  if (!shell || !queue) failures.push("missing-queue-shell");
  if (document.documentElement.dataset.theme !== resolvedTheme) failures.push("wrong-theme-attribute");
  if ((document.documentElement.scrollWidth || 0) > (document.documentElement.clientWidth || 0) + 1) failures.push("document-horizontal-overflow");
  for (const row of document.querySelectorAll("[data-sftp-transfer-state]")) {
    const id = row.getAttribute("data-sftp-transfer-state");
    if (row.scrollWidth > row.clientWidth + 1) failures.push(id + ":row-horizontal-overflow");
    const group = row.querySelector('[role="group"]');
    if (group && group.scrollWidth > group.clientWidth + 1) failures.push(id + ":group-horizontal-overflow");
    const title = row.querySelector('[data-sftp-transfer-title="true"]');
    if (title && title.clientWidth < 32) failures.push(id + ":filename-not-readable");
    if (!group?.getAttribute("aria-label")?.trim()) failures.push(id + ":missing-group-name");
    const progress = row.querySelector('[role="progressbar"]');
    if (!progress?.getAttribute("aria-label") || !Number.isFinite(Number(progress?.getAttribute("aria-valuenow")))) failures.push(id + ":invalid-progress-accessibility");
    for (const button of row.querySelectorAll("button")) {
      if (!(button.getAttribute("aria-label") || button.textContent || "").trim()) failures.push(id + ":unnamed-button");
      const controls = button.getAttribute("aria-controls");
      if (controls !== null && !controls.trim()) failures.push(id + ":empty-aria-controls");
    }
  }
  const expected = {
    waiting: ["等待响应"],
    recovering: ["正在恢复连接", "1/1"],
    canceling: ["正在取消"],
    "idle-failed": ["失败", "网络连续 3 分钟无响应"],
    committing: ["提交文件", "99%"],
    "commit-failed": ["失败", "99%", "原文件已恢复"],
    "commit-unknown": ["失败", "99%", "请刷新目录核对目标文件后"],
    canceled: ["已取消"],
    succeeded: ["完成", "100%"],
  };
  for (const [id, labels] of Object.entries(expected)) {
    const text = document.querySelector('[data-sftp-transfer-state="' + id + '"]')?.textContent || "";
    for (const label of labels) if (!text.includes(label)) failures.push(id + ":missing-label-" + label);
  }
  const idleRow = document.querySelector('[data-sftp-transfer-state="idle-failed"]');
  const recoveryButtons = idleRow?.querySelectorAll('button[aria-label^="继续传输"]') || [];
  if (recoveryButtons.length !== 1) failures.push("idle-failed:recovery-action-count");
  if (idleRow?.querySelector('button[aria-label^="重试传输"]')) failures.push("idle-failed:duplicate-retry-action");
  if (!idleRow?.querySelector('[role="alert"]')) failures.push("idle-failed:missing-alert");
  const commitRow = document.querySelector('[data-sftp-transfer-state="commit-unknown"]');
  if (!commitRow?.querySelector('[role="alert"]')) failures.push("commit-unknown:missing-alert");
  if (commitRow?.querySelector('button[aria-label^="继续传输"], button[aria-label^="重新传输"]')) failures.push("commit-unknown:unsafe-retry-action");
  const committingRow = document.querySelector('[data-sftp-transfer-state="committing"]');
  if (committingRow?.querySelector('button[aria-label^="取消传输"]')) failures.push("committing:unsafe-cancel-action");
  return { dataTheme: document.documentElement.dataset.theme, failures, queueWidth: queue?.getBoundingClientRect().width ?? 0, themeMode, resolvedTheme, rows: document.querySelectorAll("[data-sftp-transfer-state]").length };
}

function nextFrame() { return new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))); }

createRoot(document.getElementById("root")).render(React.createElement(SmokeApp));
`;
}

/**
 * 用键盘 Enter 触发恢复、取消和详情展开，验证用户可用的实际事件闭环。
 */
async function runInteractions(client) {
  const failures = [];
  await evaluate(client, "window.__sftpRecoveryUiSmoke.reset()");
  await delay(100);
  const retryFocused = await evaluate(
    client,
    `(() => { const button = document.querySelector('[data-sftp-transfer-state="idle-failed"] button[aria-label^="继续传输"]'); button?.focus(); return document.activeElement === button; })()`,
  );
  if (retryFocused.result?.value !== true) {
    failures.push("retry-button-not-focusable");
  } else {
    await pressEnter(client);
    await waitForBrowserExpression(
      client,
      'window.__sftpRecoveryUiSmoke.actionCount("retry") === 1',
      5_000,
    );
    const retryState = await evaluate(
      client,
      `document.querySelector('[data-sftp-transfer-state="idle-failed"]')?.textContent ?? ""`,
    );
    if (!retryState.result?.value?.includes("正在恢复连接")) {
      failures.push("retry-event-did-not-render-recovering-state");
    }
  }

  await evaluate(client, "window.__sftpRecoveryUiSmoke.reset()");
  await delay(100);
  const cancelFocused = await evaluate(
    client,
    `(() => { const button = document.querySelector('[data-sftp-transfer-state="waiting"] button[aria-label^="取消传输"]'); button?.focus(); return document.activeElement === button; })()`,
  );
  if (cancelFocused.result?.value !== true) {
    failures.push("cancel-button-not-focusable");
  } else {
    await pressEnter(client);
    await waitForBrowserExpression(
      client,
      'window.__sftpRecoveryUiSmoke.actionCount("cancel") === 1',
      5_000,
    );
    const cancelActions = await evaluate(
      client,
      "window.__sftpRecoveryUiSmoke.getActions()",
    );
    if (!cancelActions.result?.value?.some((action) => action.kind === "cancel")) {
      failures.push("cancel-event-not-recorded");
    }
  }

  await evaluate(client, "window.__sftpRecoveryUiSmoke.reset()");
  await delay(100);
  await evaluate(client, `document.querySelector('[data-sftp-transfer-state="waiting"] button[aria-label^="查看传输详情"]')?.focus()`);
  await pressEnter(client);
  await delay(100);
  const detailResult = await evaluate(
    client,
    `(() => { const button = document.querySelector('[data-sftp-transfer-state="waiting"] button[aria-controls]'); return { expanded: button?.getAttribute("aria-expanded") === "true", target: Boolean(button?.getAttribute("aria-controls") && document.getElementById(button.getAttribute("aria-controls"))) }; })()`,
  );
  if (detailResult.result?.value?.expanded !== true || detailResult.result?.value?.target !== true) failures.push("details-click-did-not-expand");
  return { failures, retryViaKeyboard: !failures.includes("retry-button-not-focusable"), cancelViaKeyboard: !failures.includes("cancel-button-not-focusable"), detailsClick: detailResult.result?.value?.expanded === true, detailsTarget: detailResult.result?.value?.target === true };
}

/**
 * 发送浏览器级 Enter 事件，避免直接调用 React handler 让交互验证失去意义。
 */
async function pressEnter(client) {
  await client.send("Input.dispatchKeyEvent", {
    code: "Enter",
    key: "Enter",
    nativeVirtualKeyCode: 13,
    text: "\r",
    type: "keyDown",
    unmodifiedText: "\r",
    windowsVirtualKeyCode: 13,
  });
  await client.send("Input.dispatchKeyEvent", {
    code: "Enter",
    key: "Enter",
    type: "keyUp",
    windowsVirtualKeyCode: 13,
  });
}

/**
 * 收集失败现场但限制输出长度，避免报告泄露内部配置或产生不可读日志。
 */
async function collectFailureDiagnostics(client) {
  const result = await evaluate(
    client,
    `(() => ({ bodyText: document.body?.innerText?.slice(0, 3000) ?? "", dataTheme: document.documentElement.dataset.theme, html: document.querySelector("#root")?.innerHTML?.slice(0, 3000) ?? "", ready: window.__sftpRecoveryUiSmokeReady ?? false }))()`,
  );
  return result.result?.value;
}

/**
 * 发起 JSON HTTP 请求，供 Vite/Chrome DevTools 启动握手使用。
 */
function requestJson(portNumber, pathname, method = "GET") {
  return new Promise((resolve, reject) => {
    const request = http.request({ hostname: "127.0.0.1", method, path: pathname, port: portNumber }, (response) => {
      let body = "";
      response.setEncoding("utf8");
      response.on("data", (chunk) => { body += chunk; });
      response.on("end", () => {
        if ((response.statusCode ?? 500) >= 400) { reject(new Error(`HTTP ${response.statusCode}: ${body}`)); return; }
        try { resolve(JSON.parse(body)); } catch (error) { reject(error); }
      });
    });
    request.on("error", reject);
    request.end();
  });
}

/**
 * 等待 Vite smoke 路由可访问；启动失败时给出确定的超时，而不是永久挂起。
 */
function waitForHttpOk(portNumber, pathname, timeoutMs) {
  const startedAt = Date.now();
  return new Promise((resolve, reject) => {
    const retry = () => {
      if (Date.now() - startedAt > timeoutMs) { reject(new Error("Timed out waiting for Vite dev server")); return; }
      setTimeout(attempt, 100);
    };
    const attempt = () => {
      const request = http.request({ hostname: "127.0.0.1", method: "GET", path: pathname, port: portNumber }, (response) => {
        response.resume();
        if ((response.statusCode ?? 500) < 500) { resolve(); return; }
        retry();
      });
      request.on("error", retry);
      request.end();
    };
    attempt();
  });
}

/**
 * 等待 Chrome DevTools 端口，进程提前退出时立即失败以保留真实原因。
 */
async function waitForChrome(portNumber, processHandle) {
  const startedAt = Date.now();
  while (Date.now() - startedAt < 10_000) {
    if (processHandle.exitCode !== null) throw new Error(`Chrome exited with code ${processHandle.exitCode}`);
    try { await requestJson(portNumber, "/json/version"); return; } catch { await delay(100); }
  }
  throw new Error("Timed out waiting for Chrome DevTools");
}

/**
 * 轮询浏览器表达式，覆盖 React 两帧异步提交和主题 effect 的更新边界。
 */
async function waitForBrowserExpression(client, expression, timeoutMs) {
  const startedAt = Date.now();
  while (Date.now() - startedAt < timeoutMs) {
    const result = await evaluate(client, expression);
    if (result.result?.value === true) return;
    await delay(100);
  }
  throw new Error(`Timed out waiting for browser expression: ${expression}`);
}

/**
 * 执行 CDP Runtime 表达式并按值返回，允许 harness 暴露结构化验证结果。
 */
function evaluate(client, expression) {
  return client.send("Runtime.evaluate", { awaitPromise: true, expression, returnByValue: true });
}

/**
 * 使用非阻塞定时器等待短暂的浏览器渲染状态，不引入额外依赖。
 */
function delay(ms) { return new Promise((resolve) => setTimeout(resolve, ms)); }

/**
 * 终止本次 smoke 启动的 Chrome，避免孤儿浏览器占用端口或用户目录。
 */
function terminateChrome(processHandle) {
  return new Promise((resolve) => {
    if (processHandle.exitCode !== null) { resolve(); return; }
    const timer = setTimeout(() => { processHandle.kill("SIGKILL"); resolve(); }, 2_000);
    processHandle.once("exit", () => { clearTimeout(timer); resolve(); });
    processHandle.kill();
  });
}

/**
 * 定位 Windows Chrome；允许 CI 或本地验证用 CHROME_PATH 指定隔离浏览器。
 */
function findChromePath() {
  const candidates = [
    process.env.CHROME_PATH,
    path.join(process.env.PROGRAMFILES ?? "C:\\Program Files", "Google", "Chrome", "Application", "chrome.exe"),
    path.join(process.env["PROGRAMFILES(X86)"] ?? "C:\\Program Files (x86)", "Google", "Chrome", "Application", "chrome.exe"),
  ].filter(Boolean);
  return candidates.find((candidate) => Boolean(candidate) && existsSync(candidate));
}

/**
 * 解析 --output 等无副作用命令行参数，并保持脚本可由测试检查 --help。
 */
function parseArgs(rawArgs) {
  const parsed = {};
  for (let index = 0; index < rawArgs.length; index += 1) {
    const arg = rawArgs[index];
    if (!arg.startsWith("--")) continue;
    const key = arg.slice(2);
    const value = rawArgs[index + 1];
    if (!value || value.startsWith("--")) { parsed[key] = true; continue; }
    parsed[key] = value;
    index += 1;
  }
  return parsed;
}

/**
 * 输出 CLI 用法，让没有 Chrome 的环境仍能发现报告和截图参数。
 */
function printHelp() {
  console.log("Usage: node scripts/verify-sftp-recovery-ui.mjs [--output <report.json>]");
  console.log("Runs a real Vite + headless Chrome SFTP queue recovery UI smoke.");
}

class CdpClient {
  /**
   * 建立 WebSocket CDP 客户端；只承载本脚本的单页面会话。
   */
  static connect(webSocketUrl) {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(webSocketUrl);
      const client = new CdpClient(ws);
      ws.addEventListener("open", () => resolve(client), { once: true });
      ws.addEventListener("error", reject, { once: true });
    });
  }

  /**
   * 初始化请求序列，按 CDP id 匹配并发命令，避免主题切换时串响应。
   */
  constructor(ws) {
    this.ws = ws;
    this.nextId = 1;
    this.pending = new Map();
    ws.addEventListener("message", (event) => {
      const message = JSON.parse(event.data);
      if (!message.id) return;
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      if (message.error) pending.reject(new Error(JSON.stringify(message.error)));
      else pending.resolve(message.result);
    });
  }

  /**
   * 发送单个 CDP 命令；失败原样转成 Promise rejection 供主流程收口。
   */
  send(method, params = {}) {
    const id = this.nextId;
    this.nextId += 1;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { reject, resolve });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  /**
   * 关闭页面 WebSocket，释放测试专用 DevTools 连接。
   */
  close() { this.ws.close(); }
}

await main();
