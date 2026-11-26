/**
 * Development-only mock backend.
 *
 * The client talks to Quark's private web API through a browser cookie, so a
 * checkout without credentials cannot exercise the UI. Build with
 * `VITE_MOCK=1 npx vite build` to render the app against this in-memory double
 * instead: it mirrors `lib/api.ts` exactly and keeps state in memory.
 */
import type {
  NetRecord,
  AccountInfo,
  AppInfo,
  AuthStatus,
  LogEntry,
  DirListing,
  EntryDto,
  PathNode,
  ProfileInfo,
  ProviderAuthState,
  ProviderId,
  QuotaInfo,
  RecycleEntry,
  SearchHit,
  Settings,
  TrafficInfo,
  ShareEntry,
  ShareListing,
  ShareSession,
  ShareCreated,
  ShareRecord,
  TransferTask,
} from "@/lib/types";

const now = Date.now();
const day = 86_400_000;

let seq = 0;
const nextFid = () => `fid_${++seq}`;

interface MockNode extends EntryDto {
  parent: string;
}

const nodes = new Map<string, MockNode>();

/** The UI sends qualified ids (`"quark:fid_3"`); this mock keys by native id. */
function stripPrefix(id: string): string {
  const i = id.indexOf(":");
  return i > 0 ? id.slice(i + 1) : id;
}

/** The drive part of a qualified id, defaulting to the fixture's own drive. */
function providerOf(id: string): string {
  const i = id.indexOf(":");
  return i > 0 ? id.slice(0, i) : "quark";
}

/**
 * Native id -> qualified. Every drive gets its own fixture tree, so the prefix
 * has to survive: a per-drive mock is what makes "did the list actually switch
 * drives?" testable at all.
 */
function withPrefix(id: string, provider = "quark"): string {
  return id.includes(":") ? id : `${provider}:${id}`;
}

function add(
  parent: string,
  name: string,
  opts: { dir?: boolean; size?: number; age?: number; provider?: ProviderId } = {},
): MockNode {
  const ext = opts.dir ? "" : (name.split(".").pop() ?? "").toLowerCase();
  const node: MockNode = {
    fid: nextFid(),
    name,
    pdirFid: parent,
    size: opts.dir ? 0 : (opts.size ?? 1_048_576),
    isDir: !!opts.dir,
    formatType: opts.dir ? "folder" : ext,
    updatedAt: now - (opts.age ?? 1) * day,
    createdAt: now - (opts.age ?? 1) * day - 3 * day,
    ext,
    parent,
    provider: opts.provider ?? "quark",
  };
  nodes.set(node.fid, node);
  return node;
}

// ------------------------------------------------------------------ fixtures
const root = add("", "root", { dir: true });
const projects = add(root.fid, "项目资料", { dir: true, age: 2 });
const photos = add(root.fid, "照片", { dir: true, age: 5 });
const music = add(root.fid, "音乐", { dir: true, age: 9 });
const archive = add(root.fid, "归档", { dir: true, age: 26 });
add(root.fid, "设计规范 V3.2.sketch", { size: 24_500_000, age: 0 });
add(root.fid, "季度复盘 2026 Q1.pdf", { size: 3_820_000, age: 1 });
add(root.fid, "产品需求文档.docx", { size: 892_000, age: 3 });
add(root.fid, "用户调研原始录音.m4a", { size: 68_300_000, age: 4 });
add(root.fid, "接口联调记录.md", { size: 24_600, age: 6 });
add(root.fid, "品牌视觉提案.pptx", { size: 41_200_000, age: 11 });
add(root.fid, "发布会主视觉.psd", { size: 512_000_000, age: 14 });
add(root.fid, "组件库源码.zip", { size: 8_940_000, age: 18 });
// Pictures and a video, so grid mode exercises the preview branch (the mock
// answers `inlineImage` with a grey square).
add(root.fid, "发布会现场.jpg", { size: 4_120_000, age: 7 });
add(root.fid, "主视觉导出.png", { size: 2_240_000, age: 8 });
add(root.fid, "产品演示.mp4", { size: 96_400_000, age: 9 });

add(projects.fid, "前后端联调", { dir: true, age: 1 });
add(projects.fid, "需求终稿.docx", { size: 642_000, age: 0 });
add(projects.fid, "排期表.xlsx", { size: 128_000, age: 2 });
add(projects.fid, "验收清单.md", { size: 12_400, age: 3 });
add(projects.fid, "线上问题记录.txt", { size: 3_100, age: 7 });

add(photos.fid, "2026 春日", { dir: true, age: 4 });
add(photos.fid, "IMG_2049.HEIC", { size: 6_240_000, age: 4 });
add(photos.fid, "IMG_2050.HEIC", { size: 5_980_000, age: 4 });
add(photos.fid, "DSC_8817.NEF", { size: 32_400_000, age: 12 });

add(music.fid, "夜航.flac", { size: 68_400_000, age: 8 });
add(music.fid, "潮汐.mp3", { size: 9_240_000, age: 8 });

// Every drive needs its own tree: with only Quark fixtures, switching drives
// showed the same rows (or none), so "did the list follow the drive?" could not
// be checked in the UI at all.
const roots = new Map<string, MockNode>([["quark", root]]);
function seed(provider: ProviderId, folders: string[], files: string[]) {
  const top = add("", "root", { dir: true, provider });
  roots.set(provider, top);
  for (const [index, name] of folders.entries()) {
    const folder = add(top.fid, name, { dir: true, age: index + 1, provider });
    add(folder.fid, `${name}-子文件.txt`, { size: 4_200 + index, age: index, provider });
  }
  for (const [index, name] of files.entries()) {
    add(top.fid, name, { size: 1_500_000 * (index + 1), age: index + 2, provider });
  }
}
seed("yun139", ["我的应用收藏", "手机图片", "临时文件"], ["家庭相册备份.zip"]);
seed("aliyun", ["临时文件", "Umi"], ["资源包.tar.gz", "备份-2026.iso"]);
seed("pan123", ["Umi", "尤米软件中心--资源包"], ["说明.txt"]);
seed("baidu", ["9科全景图", "动态壁纸"], ["课程录像.mp4", "笔记.md"]);

/** The fixture root of one drive. */
const rootOf = (provider: string) => roots.get(provider) ?? root;

let settings: Settings = {
  downloadDir: "/home/henvei/下载",
  uploadConflict: "rename",
  concurrentTransfers: 3,
  speedLimitKbps: 0,
  confirmDelete: true,
  apiBaseUrl: "https://drive.quark.cn",
  showHidden: false,
  listDensity: "comfortable",
  lastDirFid: "0",
  closeToTray: true,
  notifications: true,
  transferDensity: "card",
  notifyMode: "system",
};

const children = (fid: string) =>
  [...nodes.values()]
    .filter((n) => n.parent === fid)
    .sort((a, b) =>
      a.isDir === b.isDir ? a.name.localeCompare(b.name, "zh") : a.isDir ? -1 : 1,
    );

const dto = (n: MockNode): EntryDto => {
  const { parent: _parent, ...rest } = n;
  return {
    ...rest,
    fid: withPrefix(rest.fid, rest.provider as ProviderId),
    pdirFid: rest.pdirFid ? withPrefix(rest.pdirFid, rest.provider as ProviderId) : rest.pdirFid,
    // Pictures get a fake signed url so the thumbnail branch renders in mock
    // builds; `inlineImage` below turns it into a grey square.
    thumb:
      !rest.isDir && ["jpg", "jpeg", "png", "webp", "mp4"].includes(rest.ext)
        ? `https://mock.invalid/thumb/${rest.fid}.jpg`
        : null,
  };
};

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

// ------------------------------------------------------------------ transfers
let transfers: TransferTask[] = [];
const transferListeners = new Set<(t: TransferTask) => void>();
// Batch listeners live in their own set: mixing them into `transferListeners`
// fed a single task to a callback that expected an array, which threw inside
// emit() and silently stopped every later listener (pause/retry looked dead).
const batchListeners = new Set<(tasks: TransferTask[]) => void>();
const removedListeners = new Set<(id: string) => void>();
const removedManyListeners = new Set<(ids: string[]) => void>();

function emit(task: TransferTask) {
  const index = transfers.findIndex((t) => t.id === task.id);
  if (index >= 0) transfers[index] = task;
  else transfers.unshift(task);
  for (const cb of transferListeners) cb({ ...task });
  const batch = [{ ...task }];
  for (const cb of batchListeners) cb(batch);
}

/**
 * Writes a change back into the fixture array *and* broadcasts it.
 *
 * The progress ticker below re-reads `transfers`, so a pause that only emitted
 * an update would be overwritten by the next tick — which made the pause button
 * look broken in mock builds.
 */
function patchTask(id: string, patch: Partial<TransferTask>): TransferTask | undefined {
  const index = transfers.findIndex((t) => t.id === id);
  if (index < 0) return undefined;
  const next = { ...transfers[index], ...patch, updatedAt: Date.now() };
  transfers[index] = next;
  emit(next);
  return next;
}

function makeTransfer(partial: Partial<TransferTask> & { name: string; kind: TransferTask["kind"] }): TransferTask {
  const task: TransferTask = {
    id: `t_${++seq}`,
    fid: null,
    localPath: `/home/henvei/下载/${partial.name}`,
    remoteDirFid: null,
    size: 100_000_000,
    transferred: 0,
    speed: 0,
    status: "running",
    error: null,
    createdAt: now,
    updatedAt: now,
    rapid: false,
    connections: 0,
    segments: 0,
    segmentsDone: 0,
    partPath: null,
    share: null,
    cross: null,
    parent: null,
    hidden: false,
    listing: false,
    childrenTotal: 0,
    childrenDone: 0,
    childrenFailed: 0,
    provider: "quark",
    ...partial,
  };
  return task;
}

function seedTransfers() {
  transfers = [];
  const running = makeTransfer({
    kind: "download",
    name: "发布会主视觉.psd",
    size: 512_000_000,
    transferred: 188_400_000,
    speed: 6_840_000,
    // A segmented download, so the list has a real pool to show.
    connections: 256,
    segments: 1953,
    segmentsDone: 719,
  });
  transfers.push(running);
  // A folder download: one row for a whole tree, files counted underneath.
  transfers.push(
    makeTransfer({
      kind: "folder",
      name: "2026 年度素材",
      size: 12_400_000_000,
      transferred: 4_100_000_000,
      speed: 12_400_000,
      listing: true,
      childrenTotal: 1268,
      childrenDone: 412,
    }),
  );
  transfers.push(
    makeTransfer({
      kind: "upload",
      name: "季度复盘 2026 Q1.pdf",
      size: 3_820_000,
      transferred: 1_240_000,
      speed: 820_000,
      status: "running",
    }),
    makeTransfer({
      kind: "upload",
      name: "组件库源码.zip",
      size: 8_940_000,
      status: "hashing",
      transferred: 5_200_000,
      speed: 0,
      createdAt: now - 60_000,
    }),
    makeTransfer({
      kind: "download",
      name: "夜航.flac",
      size: 68_400_000,
      transferred: 68_400_000,
      status: "done",
      createdAt: now - 900_000,
    }),
    makeTransfer({
      kind: "download",
      name: "接口联调记录.md",
      size: 24_600,
      transferred: 24_600,
      status: "done",
      rapid: true,
      createdAt: now - 1_800_000,
    }),
    makeTransfer({
      kind: "upload",
      name: "旧版草案.pptx",
      size: 22_000_000,
      transferred: 4_100_000,
      status: "failed",
      error: "网络错误：连接被重置（已重试 3 次）",
      createdAt: now - 3_600_000,
    }),
  );

  // Keep one task animating so progress transitions are visible.
  setInterval(() => {
    const live = transfers.find((t) => t.status === "running" && t.kind === "download");
    if (!live) return;
    const transferred = Math.min(live.size, live.transferred + live.speed / 4);
    emit({ ...live, transferred, updatedAt: Date.now() });
  }, 250);
}

seedTransfers();

// ------------------------------------------------------------------ api double
/** Drives logged in during this session; the fixtures start with quark only. */
const loggedIn = new Set<string>(["quark"]);

function mockStates(): ProviderAuthState[] {
  const quarkOn = loggedIn.has("quark");
  // A 1×1 grey pixel, so the avatar branch of the sidebar is exercised in mock
  // builds without shipping a binary asset.
  const AVATAR =
    "data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='64' height='64'><rect width='64' height='64' fill='%234a5568'/></svg>";
  return [
    {
      provider: "quark",
      authenticated: quarkOn,
      source: quarkOn ? "file" : "none",
      masked: quarkOn ? "__puus=…a91f" : null,
      credentialPath: "/home/henvei/.config/yunyu-drive/cookie.txt",
      nickname: quarkOn ? "云语匀雨" : null,
      avatar: quarkOn ? AVATAR : null,
      capabilities: {
        shareOpen: true,
        shareCreate: true,
        move: true,
        copy: true,
        search: true,
        rename: true,
        folders: true,
        recycle: true,
      },
    },
    // The other drives exist but are not logged in yet, so the switcher shows a
    // login hint for them — same as a real build without their credentials.
    ...(["yun139", "aliyun", "pan123", "baidu", "cloud189"] as ProviderId[]).map((provider) => ({
      provider,
      authenticated: loggedIn.has(provider),
      source: "file" as const,
      masked: null,
      credentialPath: `/home/henvei/.config/yunyu-drive/${provider}`,
      nickname: loggedIn.has(provider) ? "测试用户" : null,
      // Every drive now reports a profile; 123 deliberately has none here so
      // the initial-letter fallback stays visible too.
      avatar: loggedIn.has(provider) && provider !== "pan123" ? AVATAR : null,
      capabilities: {
        shareOpen: true,
        shareCreate: true,
        // 123 can move (`mod_pid`); its copy goes through copy/async + a move.
        move: true,
        copy: provider !== "pan123",
        search: true,
        rename: true,
        folders: true,
        // 123 and 阿里 expose a bin; 百度 and 139 reject every restore call.
        recycle: provider === "pan123" || provider === "aliyun",
      },
    })),
  ];
}

/** Shares created during this mock session, newest first. */
const shareRecords: ShareRecord[] = [
  {
    shareId: "1-演示",
    url: "https://pan.mock/s/demo1234",
    title: "演示分享",
    passcode: "ab12",
    expiredAt: now + 7 * day,
    createdAt: now - 2 * day,
    fileNum: 3,
    size: 12_582_912,
    visitCount: 18,
    saveCount: 4,
    downloadCount: 9,
    expired: false,
    fids: [],
  },
  {
    shareId: "2-永久",
    url: "https://pan.mock/s/keep5678",
    title: "长期分享",
    passcode: "",
    expiredAt: 0,
    createdAt: now - 30 * day,
    fileNum: 1,
    size: 2_097_152,
    visitCount: 2,
    saveCount: 0,
    downloadCount: 1,
    expired: false,
    fids: [],
  },
  {
    shareId: "3-过期",
    url: "https://pan.mock/s/old9999",
    title: "过期的分享",
    passcode: "",
    expiredAt: now - 3 * day,
    createdAt: now - 20 * day,
    fileNum: 2,
    size: 1_048_576,
    visitCount: 5,
    saveCount: 1,
    downloadCount: 0,
    expired: true,
    fids: [],
  },
];

export const mockApi = {
  authStatus: async (): Promise<AuthStatus> => ({
    authenticated: loggedIn.has("quark"),
    source: loggedIn.has("quark") ? "file" : "none",
    masked: loggedIn.has("quark") ? "__puus=…a91f; __pus=…7c02; __kp=…" : null,
    cookiePath: "/home/henvei/.config/yunyu-drive/cookie.txt",
  }),
  authStates: async (): Promise<ProviderAuthState[]> => mockStates(),
  /** A few scripted lines so the 日志 view can be looked at without a backend. */
  logsTail: async (since: number, limit = 500): Promise<LogEntry[]> => {
    const lines: LogEntry[] = [
      { seq: 1, at: "10:24:01.812", level: "log", target: "app", message: "app start, version 0.4.1, mock build" },
      { seq: 2, at: "10:24:01.905", level: "invoke", target: "auth_states", message: "ok · 3 ms" },
      { seq: 3, at: "10:24:02.140", level: "invoke", target: "files_list", message: "ok · 128 ms" },
      { seq: 4, at: "10:24:03.001", level: "warn", target: "engine", message: "第 3 个分片超时，重试一次" },
      { seq: 5, at: "10:24:04.552", level: "error", target: "recycle_list", message: "天翼云盘网页会话没有返回登录凭据，请重新登录 · 41 ms" },
      { seq: 6, at: "10:24:05.010", level: "ok", target: "upload", message: "上传完成 · 52428800 字节 · 12.4 MB/s" },
    ];
    return lines.filter((line) => line.seq > since).slice(0, limit);
  },
  /** A failed upload plus a healthy call, so the network panel has substance. */
  netTail: async (since: number, limit = 200, failedOnly = false): Promise<NetRecord[]> => {
    const rows: NetRecord[] = [
      {
        seq: 1,
        at: "10:24:02.310",
        tag: "quark",
        method: "GET",
        url: "https://drive-m.quark.cn/1/clouddrive/file/sort?pr=ucpro&fr=pc&pdir_fid=0",
        status: 200,
        ms: 128,
        reqHeaders: [
          ["cookie", "…（已脱敏）"],
          ["user-agent", "Mozilla/5.0 (X11; Linux x86_64)"],
        ],
        reqBody: "",
        respHeaders: [["content-type", "application/json"]],
        respBody: '{\n  "status": 200,\n  "code": 0,\n  "data": { "list": [ … 128 项 ] }\n}',
        bytes: 18_422,
        error: null,
      },
      {
        seq: 2,
        at: "10:25:41.802",
        tag: "quark",
        method: "POST",
        url: "https://drive-pc.quark.cn/1/clouddrive/file/upload/finish?pr=ucpro&fr=pc",
        status: 200,
        ms: 341,
        reqHeaders: [
          ["content-type", "application/json"],
          ["cookie", "…（已脱敏）"],
        ],
        reqBody: '{\n  "fid": "a1b2c3",\n  "obj_key": "…",\n  "file_name": "install.sh"\n}',
        respHeaders: [["content-type", "application/json"]],
        respBody: '{\n  "status": 200,\n  "code": 23018,\n  "message": "该类型文件不允许分享和保存",\n  "data": null\n}',
        bytes: 118,
        error: null,
      },
    ];
    return rows.filter((row) => row.seq > since && (!failedOnly || row.status === null || row.status >= 400 || row.respBody.includes('"code":23018'))).slice(0, limit);
  },
  netClear: async (): Promise<void> => {},
  logsClear: async (): Promise<void> => {},
  logsFile: async (): Promise<string | null> => "/home/henvei/.local/share/yunyu-drive/logs/app.log",
  recycleList: async (provider: string): Promise<RecycleEntry[]> => {
    const stamp = Date.now() - 3600_000;
    return [
      { fid: `${provider}:1`, name: "旧版安装包.apk", isDir: false, size: 62_400_512, deletedAt: stamp, token: "rec-1", provider: provider as ProviderId },
      { fid: `${provider}:2`, name: "临时素材", isDir: true, size: 0, deletedAt: stamp - 86_400_000, token: "rec-2", provider: provider as ProviderId },
      { fid: `${provider}:3`, name: "会议录屏.mp4", isDir: false, size: 1_284_915_200, deletedAt: stamp - 3 * 86_400_000, token: "rec-3", provider: provider as ProviderId },
    ];
  },
  recycleRestore: async (_provider: string, entries: RecycleEntry[]): Promise<number> => entries.length,
  recyclePurge: async (_provider: string, entries: RecycleEntry[]): Promise<number> =>
    entries.length,
  recycleClear: async (): Promise<void> => {},
  // A 1×1 grey pixel stands in for every preview, so the grid's thumbnail
  // branch renders in mock builds without network access.
  inlineImage: async (_url: string): Promise<string | null> =>
    "data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'><rect width='8' height='8' fill='%234a5568'/></svg>",
  providerProfiles: async (): Promise<ProfileInfo[]> =>
    mockStates()
      .filter((s) => s.authenticated)
      .map((s) => ({ provider: s.provider, nickname: s.nickname, avatar: s.avatar ?? null })),

  aliyunQrStart: async () => ({ content: "mock-qr-content", t: 1, ck: "mock-ck" }),
  aliyunQrPoll: async () => ({ state: "new", authenticated: false, message: null }),

  cloud189QrStart: async () => ({ content: "mock-qr-content", t: 1, ck: "mock-ck" }),
  cloud189QrPoll: async () => ({ state: "new", authenticated: false, message: null }),

  providerLogin: async (
    provider: string,
    _credential: string,
  ): Promise<ProviderAuthState> => {
    loggedIn.add(provider);
    const existing = mockStates().find((p) => p.provider === provider);
    return { ...(existing as ProviderAuthState), authenticated: true };
  },
  // Every drive can sign out in the mock, Quark included: the sidebar bubble
  // and the Settings rows both call this, and leaving Quark pinned to "signed
  // in" made the sign-out path untestable.
  providerLogout: async (provider: string) => {
    loggedIn.delete(provider);
  },
  clearLoginWebData: async () => ({ windows: 0, files: 12, bytes: 4096 }),

  // All four drives report capacity, from very different endpoints; the
  // numbers differ per drive so a wrong-drive lookup is visible at a glance.
  providerTraffic: async (provider: string): Promise<TrafficInfo | null> =>
    provider === "pan123"
      ? { left: 9.81 * 1024 ** 3, share: 0, direct: 0 }
      : null,
  providerQuota: async (provider: string): Promise<QuotaInfo | null> => {
    const used = provider === "aliyun" ? 61.4 * 1024 ** 3 : 128 * 1024 ** 3;
    const total =
      provider === "yun139"
        ? 512 * 1024 ** 3
        : provider === "pan123"
          ? 2048 * 1024 ** 3
          : provider === "baidu"
            ? 4096 * 1024 ** 3
            : 1024 * 1024 ** 3;
    return { used: Math.round(used), total: Math.round(total) };
  },

  setCookie: async (_cookie?: string): Promise<AuthStatus> => mockApi.authStatus(),
  clearCookie: async (): Promise<AuthStatus> => {
    loggedIn.delete("quark");
    return {
      authenticated: false,
      source: "none",
      masked: null,
      cookiePath: "/home/henvei/.config/yunyu-drive/cookie.txt",
    };
  },
  validateCookie: async () => children(root.fid).length,
  openWebLogin: async () => {},
  openWebLogin139: async () => {},
  openWebLoginBaidu: async () => {},
  openWebLogin115: async () => {},
  closeWebLogin: async () => {},
  systemTheme: async (): Promise<"light" | "dark"> => "dark",

  listDir: async (pdirFid: string): Promise<DirListing> => {
    await sleep(240);
    const provider = providerOf(pdirFid);
    const native = stripPrefix(pdirFid);
    const fid = native === "0" || native === "" ? rootOf(provider).fid : native;
    const entries = children(fid).map(dto);
    return { pdirFid, entries, total: entries.length };
  },

  resolvePath: async (pdirFid: string): Promise<PathNode[]> => {
    const provider = providerOf(pdirFid);
    const chain: PathNode[] = [];
    let cursor = nodes.get(stripPrefix(pdirFid));
    const top = rootOf(provider);
    while (cursor && cursor.fid !== top.fid) {
      chain.unshift({ fid: withPrefix(cursor.fid, provider), name: cursor.name });
      cursor = nodes.get(cursor.parent);
    }
    return chain;
  },

  search: async (pdirFid: string, keyword: string, _limit?: number): Promise<SearchHit[]> => {
    await sleep(180);
    const needle = keyword.trim().toLowerCase();
    if (!needle) return [];
    const provider = providerOf(pdirFid);
    const native = stripPrefix(pdirFid);
    const start = native === "0" || native === "" ? rootOf(provider).fid : native;
    const hits: SearchHit[] = [];
    const walk = (fid: string, path: PathNode[]) => {
      for (const node of children(fid)) {
        const next = [...path, { fid: withPrefix(node.fid, provider), name: node.name }];
        if (node.name.toLowerCase().includes(needle)) {
          hits.push({ entry: dto(node), path });
        }
        if (node.isDir && path.length < 6) walk(node.fid, next);
      }
    };
    walk(start, []);
    return hits.slice(0, 120);
  },

  makeDir: async (pdirFid: string, name: string) => {
    await sleep(260);
    const provider = providerOf(pdirFid);
    const native = stripPrefix(pdirFid);
    const parent = native === "0" || native === "" ? rootOf(provider).fid : native;
    const created = add(parent, name.trim(), { dir: true, age: 0, provider: provider as ProviderId });
    return withPrefix(created.fid, provider);
  },

  renameEntry: async (fid: string, name: string) => {
    await sleep(220);
    const node = nodes.get(stripPrefix(fid));
    if (node) node.name = name.trim();
  },

  transferEntries: async (fids: string[], toPdirFid: string, copy: boolean) => {
    await sleep(320);
    const target = providerOf(toPdirFid);
    const sources = new Set(fids.map(providerOf));
    // Crossing drives is a transfer job in the real app; mirror that here so the
    // list shows a "跨盘" row instead of pretending the move happened.
    if (sources.size > 0 && (sources.size > 1 || !sources.has(target))) {
      const from = [...sources][0];
      const picked = fids
        .map((fid) => nodes.get(stripPrefix(fid)))
        .filter((node): node is NonNullable<typeof node> => !!node);
      // Folders carry no size in the fixture; give the row something to show.
      const total = picked.reduce((sum, node) => sum + node.size, 0) || 24_000_000;
      const task = makeTransfer({
        kind: copy ? "copy" : "move",
        name: picked.length === 1 ? picked[0].name : `${picked.length} 个文件`,
        size: total,
        transferred: 0,
        status: "running",
        provider: from as ProviderId,
        cross: {
          fromProvider: from as ProviderId,
          toProvider: target as ProviderId,
          toPdirFid,
          copy,
          files: picked.map((node) => ({
            fid: `x:${node.name}`,
            name: node.name,
            size: node.size,
            toParent: stripPrefix(toPdirFid),
          })),
        },
      });
      emit(task);
      return picked.length;
    }
    for (const fid of fids) {
      const node = nodes.get(stripPrefix(fid));
      if (!node) continue;
      if (copy) {
        // A copy is a new entry; the original has to stay where it was.
        const created = add(stripPrefix(toPdirFid), node.name, {
          dir: node.isDir,
          size: node.size,
          age: 0,
          provider: node.provider as ProviderId,
        });
        created.ext = node.ext;
        created.formatType = node.formatType;
      } else {
        node.parent = stripPrefix(toPdirFid);
      }
    }
    return fids.length;
  },

  deleteEntries: async (fids: string[]) => {
    await sleep(240);
    for (const raw of fids) {
      const fid = stripPrefix(raw);
      nodes.delete(fid);
      for (const [key, node] of [...nodes]) if (node.parent === fid) nodes.delete(key);
    }
  },

  accountInfo: async (_force = false): Promise<AccountInfo> => ({
    nickname: "云语匀雨",
    avatar: null,
  }),

  shareOpen: async (link: string): Promise<ShareSession> => {
    await sleep(320);
    const id = /\/s\/([0-9a-zA-Z]+)/.exec(link)?.[1] ?? "mock";
    return {
      provider: "quark",
      pwdId: id,
      passcode: /pwd=([^&]+)/.exec(link)?.[1] ?? "",
      stoken: "mock-stoken",
      title: "跨界对决（示例分享）",
      owner: "Da*zle_HM",
      ownerAvatar: null,
      fileNum: 3,
      totalSize: 1_184_600_261,
      expiredAt: Date.now() + 30 * day,
      firstFid: null,
      canSave: true,
      webUrl: `https://pan.quark.cn/s/${id}`,
    };
  },

  shareList: async (pwdId: string, _stoken: string, pdirFid = "0"): Promise<ShareListing> => {
    await sleep(220);
    const entries: ShareEntry[] =
      pdirFid === "0"
        ? [
            {
              fid: "share_dir_1",
              token: "tok_dir_1",
              name: "跨界对决",
              size: 0,
              isDir: true,
              updatedAt: now - 2 * day,
            },
          ]
        : [
            {
              fid: "share_f_1",
              token: "tok_f_1",
              name: "BNO跨界对决1.101R1手机端.apk",
              size: 620_097_405,
              isDir: false,
              updatedAt: now - 3 * day,
            },
            {
              fid: "share_f_2",
              token: "tok_f_2",
              name: "BNO跨界对决1.101R1电脑端.zip",
              size: 552_835_266,
              isDir: false,
              updatedAt: now - 3 * day,
            },
            {
              fid: "share_f_3",
              token: "tok_f_3",
              name: "像素联机.apk",
              size: 11_667_590,
              isDir: false,
              updatedAt: now - 4 * day,
            },
          ];
    return { pdirFid, entries, total: entries.length };
  },

  shareSave: async (
    _pwdId: string,
    _stoken: string,
    entries: { fid: string; token: string }[],
    _toPdirFid?: string,
  ): Promise<number> => {
    await sleep(420);
    return entries.length;
  },

  shareDownload: async (
    _pwdId: string,
    _stoken: string,
    items: { fid: string; name: string; size: number; isDir?: boolean; token?: string }[],
    dir?: string,
  ): Promise<TransferTask[]> => mockApi.downloadEntries(items, dir),

  shareCreate: async (
    provider: string,
    fids: string[],
    title?: string,
    days = 7,
    passcode?: string,
  ): Promise<ShareCreated> => {
    await sleep(500);
    // 百度 always invents a code when none is given; the others accept an open
    // link. Mirroring that keeps the UI branch visible in mock builds.
    const code = passcode?.trim() || (provider === "baidu" ? "p4y6" : "");
    const shareId = `${fids.length}-${title ?? "分享"}`;
    const created: ShareCreated = {
      url: `https://pan.mock/s/${Math.random().toString(36).slice(2, 10)}`,
      passcode: code,
      expiredAt: days === 0 ? 0 : Date.now() + days * 86_400_000,
      shareId,
    };
    // Keep it so "我的分享" shows the share that was just created.
    shareRecords.unshift({
      shareId,
      url: created.url,
      title: title?.trim() || "分享",
      passcode: code,
      expiredAt: created.expiredAt,
      createdAt: Date.now(),
      fileNum: fids.length,
      size: 0,
      visitCount: 0,
      saveCount: 0,
      downloadCount: 0,
      expired: false,
      fids,
    });
    return created;
  },

  shareRevoke: async (_provider: string, shareId: string, _fids: string[]): Promise<void> => {
    await sleep(300);
    const at = shareRecords.findIndex((row) => row.shareId === shareId);
    if (at >= 0) shareRecords.splice(at, 1);
  },

  shareRecords: async (_provider: string): Promise<ShareRecord[]> => {
    await sleep(300);
    return shareRecords.map((row) => ({ ...row }));
  },

  deeplinkTake: async (): Promise<string | null> => null,

  downloadEntries: async (
    items: { fid: string; name: string; size: number; isDir?: boolean }[],
    _dir?: string,
  ) => {
    await sleep(200);
    // A folder expands into one mock task per child file, mirroring the backend.
    const flat: { fid: string; name: string; size: number }[] = [];
    for (const item of items) {
      if (!item.isDir) {
        flat.push(item);
        continue;
      }
      const stack = [item.fid];
      while (stack.length) {
        const fid = stack.pop() as string;
        for (const child of children(fid)) {
          if (child.isDir) stack.push(child.fid);
          else flat.push({ fid: child.fid, name: child.name, size: child.size });
        }
      }
    }
    const created = flat.map((item) =>
      makeTransfer({ kind: "download", name: item.name, size: item.size, fid: item.fid }),
    );
    created.forEach(emit);
    return created;
  },

  uploadFiles: async (paths: string[], _pdirFid?: string) => {
    await sleep(200);
    const created = paths.map((path) =>
      makeTransfer({
        kind: "upload",
        name: path.split("/").pop() ?? "file",
        status: "hashing",
      }),
    );
    created.forEach(emit);
    return created;
  },

  transfersList: async () => transfers,

  transferCancel: async (id: string) => patchTask(id, { status: "canceled", speed: 0 }),

  transferPause: async (id: string) => patchTask(id, { status: "paused", speed: 0 }),

  transferChildren: async (id: string) => {
    const parent = transfers.find((t) => t.id === id);
    if (!parent || parent.kind !== "folder") return [];
    const base = parent.updatedAt ?? Date.now();
    return [
      { name: "开场动画.mov", size: 820_000_000, transferred: 640_000_000, status: "running" as const },
      { name: "logo 导出/logo@2x.png", size: 12_400_000, transferred: 12_400_000, status: "done" as const },
      { name: "分镜稿/第 03 版.psd", size: 410_000_000, transferred: 96_000_000, status: "running" as const },
    ].map((row, index) => ({
      id: `${id}-kid-${index}`,
      kind: "download" as const,
      name: row.name,
      fid: `kid-${index}`,
      localPath: parent.localPath,
      remoteDirFid: parent.remoteDirFid,
      size: row.size,
      transferred: row.transferred,
      speed: row.status === "running" ? 4_800_000 : 0,
      status: row.status,
      error: null,
      createdAt: base,
      updatedAt: base,
      rapid: false,
      prepared: true,
      connections: 1,
      segments: 0,
      segmentsDone: 0,
      partPath: null,
      share: null,
      provider: parent.provider,
      cross: null,
      parent: id,
      hidden: true,
    }));
  },

  transferRemove: async (id: string, _purgeLocal = false) => {
    transfers = transfers.filter((t) => t.id !== id);
    for (const cb of removedListeners) cb(id);
  },

  transferRetry: async (id: string) => {
    const task = transfers.find((t) => t.id === id);
    if (!task) throw new Error("任务不存在");
    return patchTask(id, {
      status: "running" as TransferTask["status"],
      error: null,
      speed: 1_200_000,
    })!;
  },

  transferBulk: async (
    ids: string[],
    action: "pause" | "cancel" | "remove" | "resume",
    _purgeLocal = false,
  ) => {
    let changed = 0;
    for (const id of ids) {
      const task = transfers.find((t) => t.id === id);
      if (!task) continue;
      changed += 1;
      if (action === "remove") continue;
      if (action === "resume") {
        emit({ ...task, status: "running", error: null, speed: 1_200_000 });
      } else if (action === "pause") {
        emit({ ...task, status: "paused", speed: 0 });
      } else {
        emit({ ...task, status: "canceled", speed: 0 });
      }
    }
    if (action === "remove") {
      transfers = transfers.filter((t) => !ids.includes(t.id));
      for (const id of ids) for (const cb of removedListeners) cb(id);
    }
    return changed;
  },

  transferClear: async () => {
    const removed = transfers.filter((t) => ["done", "failed", "canceled"].includes(t.status));
    transfers = transfers.filter((t) => !removed.includes(t));
    for (const task of removed) for (const cb of removedListeners) cb(task.id);
  },

  settingsGet: async () => settings,
  settingsSet: async (next: Settings) => {
    await sleep(180);
    settings = { ...next };
    return settings;
  },

  appInfo: async (): Promise<AppInfo> => ({
    version: "0.1.0",
    configDir: "/home/henvei/.config/yunyu-drive",
    cookiePath: "/home/henvei/.config/yunyu-drive/cookie.txt",
    downloadDir: settings.downloadDir,
    platform: "linux (x86_64)",
    logPath: "/home/henvei/.local/share/yunyu-drive/logs/app.log",
  }),
};

export const mockEvents = {
  onTransfer: async (cb: (task: TransferTask) => void): Promise<() => void> => {
    transferListeners.add(cb);
    return () => {
      transferListeners.delete(cb);
    };
  },
  onTransferRemoved: async (cb: (id: string) => void): Promise<() => void> => {
    removedListeners.add(cb);
    return () => {
      removedListeners.delete(cb);
    };
  },
  onWebLogin: async (_cb: (evt: { ok: boolean; message: string }) => void) => () => {},
  onWebLogin139: async (_cb: (evt: { ok: boolean; message: string }) => void) => () => {},
  onWebLoginBaidu: async (_cb: (evt: { ok: boolean; message: string }) => void) => () => {},
  onTransfersBatch: async (cb: (tasks: TransferTask[]) => void): Promise<() => void> => {
    batchListeners.add(cb);
    return () => {
      batchListeners.delete(cb);
    };
  },
  onTransfersRemovedMany: async (cb: (ids: string[]) => void): Promise<() => void> => {
    removedManyListeners.add(cb);
    return () => {
      removedManyListeners.delete(cb);
    };
  },
  onInAppNotify: async (_cb: (evt: { title: string; body: string }) => void) => () => {},
  onDeeplink: async (_cb: (link: string) => void) => () => {},
  onNavigate: async (_cb: (view: string) => void) => () => {},
};
