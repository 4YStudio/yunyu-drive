/** Shared IPC payload types (mirrors the Rust structs in src-tauri). */

/** Which cloud drive an entry belongs to. */
export type ProviderId = "quark" | "yun139" | "aliyun" | "pan123" | "baidu";

export const PROVIDERS: {
  id: ProviderId;
  name: string;
  short: string;
  /** Bundled logo under `public/`. */
  logo: string;
}[] = [
  { id: "quark", name: "夸克网盘", short: "夸克", logo: "/netdisk-quark.png" },
  { id: "yun139", name: "中国移动云盘", short: "移动", logo: "/netdisk-yun139.png" },
  { id: "aliyun", name: "阿里云盘", short: "阿里", logo: "/netdisk-aliyun.png" },
  { id: "pan123", name: "123 云盘", short: "123", logo: "/netdisk-pan123.png" },
  { id: "baidu", name: "百度网盘", short: "百度", logo: "/netdisk-baidu.png" },
];

export interface EntryDto {
  /** Qualified id: `"<provider>:<native>"`. */
  fid: string;
  name: string;
  pdirFid: string;
  size: number;
  isDir: boolean;
  formatType: string;
  updatedAt: number;
  createdAt: number;
  ext: string;
  provider: ProviderId;
}

export interface ProviderCapabilities {
  shareOpen: boolean;
  shareCreate: boolean;
  move: boolean;
  copy: boolean;
  search: boolean;
  rename: boolean;
  folders: boolean;
  /** Has a recycle bin we can list and restore from. */
  recycle: boolean;
}

/** One row of a drive's recycle bin (`recycle_list`). */
export interface RecycleEntry {
  /** Qualified id: `"<provider>:<native>"`. */
  fid: string;
  name: string;
  isDir: boolean;
  size: number;
  /** Unix ms of the deletion, when the drive reports it. */
  deletedAt: number;
  /** Drive-specific handle (Quark's recycle `record_id`). */
  token: string;
  provider: ProviderId;
}

export interface ProviderAuthState {
  provider: ProviderId;
  authenticated: boolean;
  source: string;
  masked: string | null;
  credentialPath: string;
  nickname: string | null;
  /** Inlined `data:` URL; the CSP blocks remote avatar hosts. */
  avatar?: string | null;
  capabilities: ProviderCapabilities;
}

export interface QuotaInfo {
  used: number;
  total: number;
}

/** Nickname + avatar of one signed-in drive, from `provider_profiles`. */
export interface ProfileInfo {
  provider: ProviderId;
  nickname: string | null;
  /** Inlined `data:` URL. */
  avatar: string | null;
}
export interface PathNode {
  fid: string;
  name: string;
}

export interface DirListing {
  pdirFid: string;
  entries: EntryDto[];
  total: number;
}

export interface SearchHit {
  entry: EntryDto;
  path: PathNode[];
}

export interface AuthStatus {
  authenticated: boolean;
  source: "env" | "file" | "none" | string;
  masked: string | null;
  cookiePath: string;
}

export interface TransferTask {
  id: string;
  kind: "download" | "upload";
  name: string;
  fid: string | null;
  localPath: string;
  remoteDirFid: string | null;
  size: number;
  transferred: number;
  speed: number;
  status:
    | "queued"
    | "hashing"
    | "running"
    | "paused"
    | "done"
    | "failed"
    | "canceled";
  error: string | null;
  createdAt: number;
  updatedAt: number;
  rapid: boolean;
  /** Partial file a failed download can resume from. */
  partPath?: string | null;
  /** Present for share downloads, so a retry can rebuild the request. */
  share?: { fidToken: string; pwdId: string; stoken: string } | null;
  /** Which drive this task belongs to, for the badge in the list. */
  provider?: ProviderId | "";
}

export interface Settings {
  downloadDir: string;
  uploadConflict: string;
  concurrentTransfers: number;
  confirmDelete: boolean;
  apiBaseUrl: string;
  showHidden: boolean;
  listDensity: string;
  lastDirFid: string;
  /** Keep running in the tray after the window is closed. */
  closeToTray: boolean;
  /** Desktop notifications for transfers and share links. */
  notifications: boolean;
}

export interface AppInfo {
  version: string;
  configDir: string;
  cookiePath: string;
  downloadDir: string;
  platform: string;
  /** Log file the app appends to; shown so a user can attach it to a bug report. */
  logPath: string | null;
}

export interface AccountInfo {
  nickname: string;
  /** Inlined `data:` URL — remote avatar hosts are outside the CSP. */
  avatar: string | null;
}

export interface ShareEntry {
  fid: string;
  token: string;
  name: string;
  size: number;
  isDir: boolean;
  updatedAt: number;
}

export interface ShareSession {
  /** `quark` / `yun139` / `aliyun` / `pan123` / `baidu`. */
  provider: ProviderId;
  pwdId: string;
  passcode: string;
  stoken: string;
  title: string;
  owner: string;
  ownerAvatar: string | null;
  fileNum: number;
  totalSize: number;
  expiredAt: number;
  firstFid: string | null;
  /** Whether this drive can save share entries into the user's own drive. */
  canSave: boolean;
  /** Browser address of the share, for "open in browser". */
  webUrl: string | null;
}

export interface ShareListing {
  pdirFid: string;
  entries: ShareEntry[];
  total: number;
}
