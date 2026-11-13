// @author kongweiguang

import type { JsonValue } from "./json.js";
import type {
  CommandRunParams, HostOpenResult, HostOperationParams, HostTestResult, ViewActionParams,
  ViewActionResult, ViewFetchParams, ViewFetchResult, ViewInstanceParams, ViewOpenParams,
  ViewOpenResult, ViewStateResult, ViewVisibilityParams, EventParams,
} from "./generated/rpc.js";

export type JsonObject = { [key: string]: JsonValue };
export type RuntimeHandler<Params, Result> = (params: Params) => Result | Promise<Result>;
/** 请求取消携带协作信号；宿主只保证基础 AbortSignal，不承诺中断正在进行的网络或磁盘调用。 */
export type CancellableRuntimeHandler<Params, Result> = (params: Params, signal: AbortSignal) => Result | Promise<Result>;
/** 可选信号只取消当前请求，不会隐式重试。 */
export interface RuntimeRequestOptions { readonly signal?: AbortSignal }
/** 宿主能力的字段契约尚未冻结，调用方只能把参数与结果视为 JSON。 */
export type RemoteCall = <Result extends JsonValue = JsonValue>(params?: JsonObject) => Promise<Result>;

export interface KerminalEnvironment {
  readonly locale: string;
  readonly theme: string;
}

export interface HostRequests {
  "view.open": { params: ViewOpenParams; result: ViewOpenResult };
  "view.action": { params: ViewActionParams; result: ViewActionResult };
  "view.fetch": { params: ViewFetchParams; result: ViewFetchResult };
  "view.snapshot": { params: ViewInstanceParams; result: ViewStateResult };
  "view.close": { params: ViewInstanceParams; result: ViewStateResult };
  "command.run": { params: CommandRunParams; result: JsonObject };
  "host.test": { params: HostOperationParams; result: HostTestResult };
  "host.open": { params: HostOperationParams; result: HostOpenResult };
}

export interface HostNotifications {
  "view.visibility": ViewVisibilityParams;
  event: EventParams;
  "settings.changed": { settings: JsonValue };
}

export interface Kerminal {
  readonly environment: KerminalEnvironment;
  onRequest<Method extends keyof HostRequests>(
    method: Method,
    handler: RuntimeHandler<HostRequests[Method]["params"], HostRequests[Method]["result"]>,
  ): void;
  onRequest<Method extends keyof HostNotifications>(
    method: Method,
    handler: RuntimeHandler<HostNotifications[Method], void>,
  ): void;
  onRequest<Method extends string>(
    method: Method & (Method extends keyof HostRequests | keyof HostNotifications ? never : unknown),
    handler: RuntimeHandler<JsonObject, unknown>,
  ): void;
  onCancellableRequest<Method extends keyof HostRequests>(
    method: Method,
    handler: CancellableRuntimeHandler<HostRequests[Method]["params"], HostRequests[Method]["result"]>,
  ): void;
  onCancellableRequest<Method extends keyof HostNotifications>(
    method: Method,
    handler: CancellableRuntimeHandler<HostNotifications[Method], void>,
  ): void;
  onCancellableRequest<Method extends string>(
    method: Method & (Method extends keyof HostRequests | keyof HostNotifications ? never : unknown),
    handler: CancellableRuntimeHandler<JsonObject, unknown>,
  ): void;
  request<Result extends JsonValue = JsonValue>(method: string, params?: JsonObject, options?: RuntimeRequestOptions): Promise<Result>;
  readonly view: {
    update<Result extends JsonValue = JsonValue>(instance: string, patch: JsonValue, options?: RuntimeRequestOptions): Promise<Result>;
  };
  readonly ui: {
    readonly notify: RemoteCall;
    readonly confirm: RemoteCall;
    readonly prompt: RemoteCall;
    readonly pickFile: RemoteCall;
  };
  readonly settings: { readonly get: RemoteCall };
  readonly state: { readonly get: RemoteCall; readonly set: RemoteCall };
  readonly secrets: { readonly get: RemoteCall; readonly set: RemoteCall; readonly delete: RemoteCall };
  readonly clipboard: { readonly read: RemoteCall; readonly write: RemoteCall };
  readonly shell: { readonly openExternal: RemoteCall };
  readonly hosts: { readonly list: RemoteCall };
  readonly terminal: { readonly paste: RemoteCall };
  readonly events: { readonly subscribe: RemoteCall };
}

declare global {
  interface Window {
    readonly kerminal: Kerminal;
  }
}
