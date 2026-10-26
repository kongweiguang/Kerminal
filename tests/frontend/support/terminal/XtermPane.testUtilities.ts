// @author kongweiguang

import { defaultAppSettings } from "../../../../src/features/settings/settingsModel";

/** @author kongweiguang 显式模拟布局指标，避免依赖 jsdom 的零尺寸。 */
export function mockElementBox(
  element: HTMLElement,
  metrics: {
    clientHeight?: number;
    clientWidth?: number;
    height?: number;
    offsetLeft?: number;
    offsetTop?: number;
    rectLeft?: number;
    rectTop?: number;
    width?: number;
  },
) {
  if (typeof metrics.clientHeight === "number") {
    Object.defineProperty(element, "clientHeight", {
      configurable: true,
      value: metrics.clientHeight,
    });
  }
  if (typeof metrics.clientWidth === "number") {
    Object.defineProperty(element, "clientWidth", {
      configurable: true,
      value: metrics.clientWidth,
    });
  }
  if (typeof metrics.offsetLeft === "number") {
    Object.defineProperty(element, "offsetLeft", {
      configurable: true,
      value: metrics.offsetLeft,
    });
  }
  if (typeof metrics.offsetTop === "number") {
    Object.defineProperty(element, "offsetTop", {
      configurable: true,
      value: metrics.offsetTop,
    });
  }

  const width = metrics.width ?? 0;
  const height = metrics.height ?? 0;
  const left = metrics.rectLeft ?? 0;
  const top = metrics.rectTop ?? 0;
  Object.defineProperty(element, "getBoundingClientRect", {
    configurable: true,
    value: () =>
      ({
        bottom: top + height,
        height,
        left,
        right: left + width,
        top,
        width,
        x: left,
        y: top,
        toJSON: () => ({}),
      }) as DOMRect,
  });
}

/** @author kongweiguang 深合并 provider 开关，保留未覆盖的默认设置。 */
export function terminalAppearanceWithInlineSuggestion(
  inlineSuggestion: Partial<
    typeof defaultAppSettings.terminal.inlineSuggestion
  >,
) {
  return {
    ...defaultAppSettings.terminal,
    inlineSuggestion: {
      ...defaultAppSettings.terminal.inlineSuggestion,
      ...inlineSuggestion,
      providers: {
        ...defaultAppSettings.terminal.inlineSuggestion.providers,
        ...(inlineSuggestion.providers ?? {}),
      },
    },
  };
}
