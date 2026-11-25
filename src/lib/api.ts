import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export * from "@/lib/types";
import type {
  AccountInfo,
  AppInfo,
  AuthStatus,
  DirListing,
  PathNode,
  ProfileInfo,
  ProviderAuthState,
  RecycleEntry,
  QuotaInfo,
  SearchHit,
  Settings,
  ShareListing,
  ShareSession,
  ShareCreated,
  ShareRecord,
  TransferTask,
} from "@/lib/types";

export const api = {
  authStatus: () => invoke<AuthStatus>("auth_status"),
  /** Login state of every drive, for the sidebar switcher. */
  authStates: () => invoke<ProviderAuthState[]>("auth_states"),
  /**
   * Nickname + avatar of every signed-in drive.
   *
   * Separate from `authStates` so the first paint does not wait for four avatar
   * downloads; each drive answers at most once per process.
   */
  providerProfiles: () => invoke<ProfileInfo[]>("provider_profiles"),
  /** Starts an 阿里云盘 QR login; returns the code to render. */
  aliyunQrStart: () =>
    invoke<{ content: string; t: number; ck: string }>("aliyun_qr_start"),
  /** Polls the QR login; `authenticated` flips once the phone confirmed. */
  aliyunQrPoll: (qr: { content: string; t: number; ck: string }) =>
    invoke<{ state: string; authenticated: boolean; message: string | null }>(
      "aliyun_qr_poll_with",
      { t: qr.t, ck: qr.ck, content: qr.content },
    ),
  /** Starts a 天翼云盘 QR login; the code is the `uuid` the app scans. */
  cloud189QrStart: () =>
    invoke<{ content: string; t: number; ck: string }>("cloud189_qr_start"),
  /** Polls the 天翼云盘 QR login; the session lives in the backend. */
  cloud189QrPoll: () =>
    invoke<{ state: string; authenticated: boolean; message: string | null }>(
      "cloud189_qr_poll",
    ),
  /** Logs in a drive that takes a pasted credential (139 today). */
  providerLogin: (provider: string, credential: string) =>
    invoke<ProviderAuthState>("provider_login", { provider, credential }),
  providerLogout: (provider: string) => invoke<void>("provider_logout", { provider }),
  /** Free/total space, when the drive exposes it. */
  providerQuota: (provider: string) =>
    invoke<QuotaInfo | null>("provider_quota", { provider }),
  recycleList: (provider: string, offset = 0, limit = 100) =>
    invoke<RecycleEntry[]>("recycle_list", { provider, offset, limit }),
  recycleRestore: (provider: string, entries: RecycleEntry[]) =>
    invoke<number>("recycle_restore", { provider, entries }),
  recyclePurge: (provider: string, entries: RecycleEntry[]) =>
    invoke<number>("recycle_purge", { provider, entries }),
  recycleClear: (provider: string) => invoke<void>("recycle_clear", { provider }),
  /**
   * Downloads a pre-signed preview and returns it as a `data:` url. The CSP
   * blocks remote hosts, so the thumbnail has to travel through the backend.
   */
  inlineImage: (url: string) => invoke<string | null>("inline_image", { url }),
  setCookie: (cookie: string) => invoke<AuthStatus>("set_cookie", { cookie }),
  clearCookie: () => invoke<AuthStatus>("clear_cookie"),
  validateCookie: () => invoke<number>("validate_cookie"),
  /** Opens a real webview on pan.quark.cn; the cookie is harvested for us. */
  openWebLogin: () => invoke<void>("open_web_login"),
  /** 139 web login: same flow as quark, harvested from `yun.139.com`. */
  openWebLogin139: () => invoke<void>("open_web_login_139"),
  /** 百度网盘 web login: harvested from `pan.baidu.com` (`BDUSS`/`STOKEN`). */
  openWebLoginBaidu: () => invoke<void>("open_web_login_baidu"),
  openWebLogin115: () => invoke<void>("open_web_login_115"),
  closeWebLogin: () => invoke<void>("close_web_login"),

  listDir: (pdirFid: string) => invoke<DirListing>("list_dir", { pdirFid }),
  resolvePath: (pdirFid: string) => invoke<PathNode[]>("resolve_path", { pdirFid }),
  search: (pdirFid: string, keyword: string, limit?: number) =>
    invoke<SearchHit[]>("search", { pdirFid, keyword, limit }),

  makeDir: (pdirFid: string, name: string) =>
    invoke<string>("make_dir", { pdirFid, name }),
  renameEntry: (fid: string, name: string) =>
    invoke<void>("rename_entry", { fid, name }),
  deleteEntries: (fids: string[]) => invoke<void>("delete_entries", { fids }),
  /**
   * Moves (`copy: false`) or copies entries into `toPdirFid`.
   *
   * Returns the number of entries handled. A target on another drive turns the
   * call into a transfer task (download → upload) and returns the file count.
   */
  transferEntries: (fids: string[], toPdirFid: string, copy: boolean) =>
    invoke<number>("transfer_entries", { fids, toPdirFid, copy }),

  downloadEntries: (
    items: {
      fid: string;
      name: string;
      size: number;
      isDir?: boolean;
      token?: string;
    }[],
    dir?: string,
  ) => invoke<TransferTask[]>("download_entries", { items, dir }),
  uploadFiles: (paths: string[], pdirFid: string) =>
    invoke<TransferTask[]>("upload_files", { paths, pdirFid }),

  transfersList: () => invoke<TransferTask[]>("transfers_list"),
  transferCancel: (id: string) => invoke<void>("transfer_cancel", { id }),
  /** Stops a transfer but keeps it resumable. */
  transferPause: (id: string) => invoke<void>("transfer_pause", { id }),
  /**
   * Drops a task. `purgeLocal` also deletes what it left on disk: the partial
   * placeholder of an unfinished download, or the finished file itself.
   */
  transferRemove: (id: string, purgeLocal = false) =>
    invoke<void>("transfer_remove", { id, purgeLocal }),
  /** The files a folder task is downloading right now. */
  transferChildren: (id: string) => invoke<TransferTask[]>("transfer_children", { id }),
  transferClear: () => invoke<void>("transfer_clear"),
  transferRetry: (id: string) => invoke<TransferTask>("transfer_retry", { id }),
  /**
   * One call for a whole selection.
   *
   * Looping `transferPause` over 3000 ids meant 3000 IPC round trips and 3000
   * events; the backend does it in one pass and announces the batch once.
   */
  transferBulk: (
    ids: string[],
    action: "pause" | "cancel" | "remove" | "resume",
    purgeLocal = false,
  ) => invoke<number>("transfers_bulk", { ids, action, purgeLocal }),

  settingsGet: () => invoke<Settings>("settings_get"),
  settingsSet: (settings: Settings) =>
    invoke<Settings>("settings_set", { settings }),
  appInfo: () => invoke<AppInfo>("app_info"),
  /** Quits even when "close to tray" is on. */
  quitApp: () => invoke<void>("quit_app"),
  /** Desktop colour scheme; WebKitGTK does not always expose prefers-color-scheme. */
  systemTheme: () => invoke<"light" | "dark">("system_theme"),

  accountInfo: (force = false) =>
    invoke<AccountInfo | null>("account_info", { force }),

  shareOpen: (link: string) => invoke<ShareSession>("share_open", { link }),
  shareList: (
    pwdId: string,
    stoken: string,
    pdirFid = "0",
    provider?: string,
    passcode?: string,
  ) =>
    invoke<ShareListing>("share_list", { pwdId, stoken, pdirFid, provider, passcode }),
  shareSave: (
    pwdId: string,
    stoken: string,
    entries: { fid: string; token: string }[],
    toPdirFid?: string,
    provider?: string,
    passcode?: string,
  ) =>
    invoke<number>("share_save", {
      pwdId,
      stoken,
      fids: entries.map((e) => e.fid),
      tokens: entries.map((e) => e.token),
      toPdirFid,
      provider,
      passcode,
    }),
  shareDownload: (
    pwdId: string,
    stoken: string,
    items: {
      fid: string;
      name: string;
      size: number;
      isDir?: boolean;
      token?: string;
    }[],
    dir?: string,
    provider?: string,
    passcode?: string,
  ) =>
    invoke<TransferTask[]>("share_download", {
      pwdId,
      stoken,
      items,
      dir,
      provider,
      passcode,
    }),
  /** A link that launched the app before the UI was listening. */
  deeplinkTake: () => invoke<string | null>("deeplink_take"),

  /**
   * Creates a share of the given files.
   *
   * `days` 0 means permanent; `passcode` may be empty, in which case 百度
   * invents one and the returned `passcode` carries it.
   */
  shareCreate: (
    provider: string,
    fids: string[],
    title?: string,
    days = 7,
    passcode?: string,
  ) =>
    invoke<ShareCreated>("share_create", {
      provider,
      fids,
      title,
      days,
      passcode,
    }),
  /** Revokes a share this app created. */
  shareRevoke: (provider: string, shareId: string, fids: string[]) =>
    invoke<void>("share_revoke", { provider, shareId, fids }),

  /** Lists the shares the account has already created on a drive. */
  shareRecords: (provider: string) => invoke<ShareRecord[]>("share_records", { provider }),
};

export interface WebLoginEvent {
  ok: boolean;
  message: string;
}

export const events = {
  onTransfer: (cb: (task: TransferTask) => void): Promise<UnlistenFn> =>
    listen<TransferTask>("transfer://update", (e) => cb(e.payload)),
  onTransferRemoved: (cb: (id: string) => void): Promise<UnlistenFn> =>
    listen<string>("transfer://removed", (e) => cb(e.payload)),
  /** Many tasks at once — a folder download expanded into its files. */
  onTransfersBatch: (cb: (tasks: TransferTask[]) => void): Promise<UnlistenFn> =>
    listen<TransferTask[]>("transfer://batch", (e) => cb(e.payload)),
  /** Many removals at once. */
  onTransfersRemovedMany: (cb: (ids: string[]) => void): Promise<UnlistenFn> =>
    listen<string[]>("transfer://removed-many", (e) => cb(e.payload)),
  /** How many files a folder walk has found so far, before any task exists. */
  onWebLogin: (cb: (evt: WebLoginEvent) => void): Promise<UnlistenFn> =>
    listen<WebLoginEvent>("auth://web-login", (e) => cb(e.payload)),
  onWebLogin139: (cb: (evt: WebLoginEvent) => void): Promise<UnlistenFn> =>
    listen<WebLoginEvent>("auth://yun139-web-login", (e) => cb(e.payload)),
  onWebLogin115: (cb: (evt: WebLoginEvent) => void): Promise<UnlistenFn> =>
    listen<WebLoginEvent>("auth://pan115-web-login", (e) => cb(e.payload)),
  onWebLoginBaidu: (cb: (evt: WebLoginEvent) => void): Promise<UnlistenFn> =>
    listen<WebLoginEvent>("auth://baidu-web-login", (e) => cb(e.payload)),
  onDeeplink: (cb: (link: string) => void): Promise<UnlistenFn> =>
    listen<string>("deeplink://open", (e) => cb(e.payload)),
  /** Tray menu asking for a screen change (`"transfers"`). */
  onNavigate: (cb: (view: string) => void): Promise<UnlistenFn> =>
    listen<string>("app://navigate", (e) => cb(e.payload)),
};

