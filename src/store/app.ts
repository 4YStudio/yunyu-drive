import { computed, reactive, watch } from "vue";
import { hostSetTheme, hostSystemTheme, onSystemThemeChange } from "@/lib/host";
import { api, events } from "@/lib/backend";
import { errText } from "@/lib/format";
import { storageKey } from "@/lib/brand";
import type {
  AccountInfo,
  ProviderAuthState,
  ProviderId,
  QuotaInfo,
  Settings,
  TransferTask,
} from "@/lib/types";

export type View =
  | "files"
  | "recycle"
  | "myShares"
  | "transfers"
  | "settings"
  | "login"
  | "share";
export type ThemeMode = "light" | "dark" | "system";

export interface Toast {
  id: number;
  message: string;
  kind: "info" | "success" | "error";
}

let toastSeq = 0;

interface AppStore {
  ready: boolean;
  view: View;
  themeMode: ThemeMode;
  systemDark: boolean;
  authenticated: boolean;
  authSource: string;
  maskedCookie: string | null;
  cookiePath: string;
  settings: Settings | null;
  transfers: TransferTask[];
  toasts: Toast[];
  bootError: string | null;
  sidebarOpen: boolean;
  /** Nickname + avatar of the signed-in account. */
  account: AccountInfo | null;
  /** Bumped whenever the file list should refetch (login, share saved, …). */
  filesToken: number;
  /** A deep-linked share queued for the share view. */
  shareLink: string | null;
  shareNonce: number;
  /** Drive currently shown in the file browser. */
  activeProvider: ProviderId;
  /**
   * Drive the login screen should open directly, or `null` for the chooser.
   *
   * Clicking "登录" on a drive's empty state must land on *that* drive's login
   * page — asking again which cloud to log into is a redundant second step.
   */
  loginTarget: ProviderId | null;
  /** Login state of every drive, refreshed after a login or logout. */
  providers: ProviderAuthState[];
  /**
   * Capacity per drive, filled in lazily.
   *
   * `null` means "asked and the drive does not report one" (123 has no such
   * endpoint), `undefined` means "not asked yet".
   */
  quotas: Partial<Record<ProviderId, QuotaInfo | null>>;
}

export const app = reactive<AppStore>({
  ready: false,
  view: "files",
  themeMode: "system",
  systemDark: window.matchMedia("(prefers-color-scheme: dark)").matches,
  authenticated: false,
  authSource: "none",
  maskedCookie: null,
  cookiePath: "",
  settings: null,
  transfers: [],
  toasts: [],
  bootError: null,
  sidebarOpen: true,
  account: null,
  filesToken: 0,
  shareLink: null,
  shareNonce: 0,
  activeProvider: "quark",
  loginTarget: null,
  providers: [],
  quotas: {},
});

/** Qualified root id of the active drive, e.g. `"quark:0"`. */
export const rootFid = computed(() =>
  app.activeProvider ? `${app.activeProvider}:0` : "0",
);

/** Login state + capabilities of the active drive. */
export const activeProviderState = computed(
  () => app.providers.find((p) => p.provider === app.activeProvider) ?? null,
);

/** Switches the browser to another drive and reloads its file list. */
/**
 * Opens the login screen for one drive, skipping the chooser.
 *
 * The login screen reads `loginTarget` once on mount; it is cleared right away
 * so a later visit through the sidebar shows the picker again.
 */
export function goLoginFor(id?: ProviderId) {
  app.loginTarget = id ?? app.activeProvider;
  app.view = "login";
}

export function setActiveProvider(id: ProviderId) {
  if (app.activeProvider === id) return;
  app.activeProvider = id;
  // The share screen belongs to the previous drive's capability set.
  if (app.view === "share") app.view = "files";
  localStorage.setItem(storageKey("provider"), id);
  void loadQuota(id);
  // The remembered directory belonged to the previous drive.
  void saveSettings({ ...(app.settings as Settings), lastDirFid: `${id}:0` });
  requestFilesRefresh();
}

export const isDark = computed(() =>
  app.themeMode === "system" ? app.systemDark : app.themeMode === "dark",
);

export const activeTransfers = computed(
  () =>
    app.transfers.filter(
      (t) => t.status === "running" || t.status === "hashing" || t.status === "queued",
    ).length,
);

export function toast(message: string, kind: Toast["kind"] = "info") {
  const item: Toast = { id: ++toastSeq, message, kind };
  app.toasts.push(item);
  const ttl = kind === "error" ? 6000 : 3200;
  window.setTimeout(() => dismissToast(item.id), ttl);
}

export function dismissToast(id: number) {
  const index = app.toasts.findIndex((t) => t.id === id);
  if (index >= 0) app.toasts.splice(index, 1);
}

let lastTheme: "light" | "dark" | null = null;
let themeTimer: number | undefined;

function applyTheme() {
  const root = document.documentElement;
  const next = isDark.value ? "dark" : "light";
  // Cross-fade every themed surface for one beat so switching modes reads as a
  // soft dissolve instead of a hard repaint. Skipped on first paint.
  if (lastTheme && lastTheme !== next) {
    root.classList.add("theme-anim");
    window.clearTimeout(themeTimer);
    themeTimer = window.setTimeout(() => root.classList.remove("theme-anim"), 420);
  }
  lastTheme = next;
  root.dataset.theme = next;
  root.style.colorScheme = next;
  // Follow-system mode leaves the window chrome to the desktop environment so
  // KDE keeps drawing it with the user's own decoration; explicit modes force it.
  void hostSetTheme(app.themeMode === "system" ? null : next);
}

/** Seeds `systemDark` from the desktop, then keeps it in sync. */
async function syncSystemTheme() {
  const system = await hostSystemTheme();
  if (system) {
    app.systemDark = system === "dark";
    if (app.themeMode === "system") applyTheme();
  }
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  media.addEventListener("change", (event) => {
    app.systemDark = event.matches;
    if (app.themeMode === "system") applyTheme();
  });
  await onSystemThemeChange((theme) => {
    app.systemDark = theme === "dark";
    if (app.themeMode === "system") applyTheme();
  });
}

export function setThemeMode(mode: ThemeMode) {
  app.themeMode = mode;
  localStorage.setItem(storageKey("theme"), mode);
  applyTheme();
}

export function cycleTheme() {
  const order: ThemeMode[] = ["light", "dark", "system"];
  const next = order[(order.indexOf(app.themeMode) + 1) % order.length];
  setThemeMode(next);
}

/** Asks the file view to refetch (login, share save, deeplink restore). */
export function requestFilesRefresh() {
  app.filesToken += 1;
}

/** Queues a share link and switches to the share screen. */
export function openShareLink(link: string) {
  app.shareLink = link;
  app.shareNonce += 1;
  app.view = "share";
}

export async function loadAccount(force = false) {
  if (!app.authenticated) {
    app.account = null;
    return;
  }
  try {
    const info = await api.accountInfo(force);
    if (info) app.account = info;
  } catch {
    /* profile is cosmetic; never block the UI on it */
  }
}

export function applyTransfer(task: TransferTask) {
  const index = app.transfers.findIndex((t) => t.id === task.id);
  const before = index >= 0 ? app.transfers[index]?.status : undefined;
  if (index >= 0) app.transfers[index] = task;
  else app.transfers.unshift(task);
  // Bytes only hit the drive when a job finishes, and the file list was loaded
  // before the new entry existed — so both the capacity bar and the listing are
  // stale the moment this fires. Progress events arrive every few hundred ms,
  // hence the transition check instead of "status === done".
  if (task.status === "done" && before !== "done") {
    // Downloads do not touch the remote side; the other three do.
    if (task.kind !== "download") refreshQuota(app.activeProvider);
    requestFilesRefresh();
  }
}

export async function refreshTransfers() {
  app.transfers = await api.transfersList();
}

/** Refreshes the per-drive login state shown in the sidebar. */
export async function loadProviderStates() {
  try {
    app.providers = await api.authStates();
  } catch {
    /* the switcher simply stays empty */
  }
  // Fire-and-forget: after a login the new drive's nickname/avatar should fill
  // in, but nothing should wait for it.
  void loadProviderProfiles();
}

/**
 * Fills in nicknames and avatars for the signed-in drives.
 *
 * Kept apart from [`loadProviderStates`] because each drive has to answer with a
 * network call the first time (avatar downloads included), and the file list
 * must not wait for that. The results are merged into `app.providers` so every
 * consumer of the switcher state sees them.
 */
export async function loadProviderProfiles() {
  let profiles;
  try {
    profiles = await api.providerProfiles();
  } catch {
    return;
  }
  for (const profile of profiles) {
    const state = app.providers.find((p) => p.provider === profile.provider);
    if (!state) continue;
    if (profile.nickname) state.nickname = profile.nickname;
    if (profile.avatar) state.avatar = profile.avatar;
  }
}

/**
 * Signs out of one drive.
 *
 * Lives here rather than in a view because both the sidebar account bubble and
 * the settings rows need it. Quark keeps its session in the single-drive
 * `authenticated`/`account` fields, so it is cleared separately.
 */
export async function logoutProvider(id: ProviderId) {
  if (id === "quark") {
    await api.clearCookie();
    app.authenticated = false;
    app.maskedCookie = null;
    app.account = null;
  } else {
    await api.providerLogout(id);
  }
  forgetQuota(id);
  await loadProviderStates();
}

/** Drives whose quota request is in flight, so a re-render cannot pile them up. */
const quotaInFlight = new Set<ProviderId>();

/** When each drive's capacity reading was taken, for the TTL below. */
const quotaStamps = new Map<ProviderId, number>();

/**
 * Drives that were asked for a fresh reading while one was already in flight.
 *
 * Without this a `refreshQuota` that lands mid-request is dropped, and the
 * answer that request is about to deliver predates the change we wanted to see.
 */
const quotaAgain = new Set<ProviderId>();

/**
 * How long a capacity reading stays trustworthy.
 *
 * It used to be cached for the whole session, so anything that moved bytes
 * behind the app's back — a transfer still running in another window, a share
 * someone else saved — left the bar stale until a restart. Half a minute keeps
 * the sidebar from hammering the endpoint while still catching up on its own.
 */
const QUOTA_TTL_MS = 30_000;

/**
 * Fetches the active drive's capacity.
 *
 * Kept out of `loadProviderStates` because it costs a round trip per drive and
 * the sidebar only ever shows one. `force` refetches after an upload/delete.
 */
export async function loadQuota(id: ProviderId = app.activeProvider, force = false) {
  if (!force && app.quotas[id] !== undefined) {
    const at = quotaStamps.get(id) ?? 0;
    if (Date.now() - at < QUOTA_TTL_MS) return;
  }
  if (quotaInFlight.has(id)) {
    if (force) quotaAgain.add(id);
    return;
  }
  quotaInFlight.add(id);
  try {
    app.quotas[id] = await api.providerQuota(id);
    quotaStamps.set(id, Date.now());
  } catch {
    // A drive that cannot answer (offline, no endpoint) simply shows no bar —
    // but do not throw away a reading that was already on screen.
    if (app.quotas[id] === undefined) app.quotas[id] = null;
    quotaStamps.set(id, Date.now());
  } finally {
    quotaInFlight.delete(id);
    // Someone asked for a fresh number while we were busy; their change may
    // have landed after this request started, so go again (once).
    if (quotaAgain.delete(id)) void loadQuota(id, true);
  }
}

/**
 * Re-reads capacity right now.
 *
 * Every action that moves bytes (upload, delete, cross-drive copy, share save,
 * emptying the recycle bin) has to call this: those bytes are accounted for
 * server-side the moment the request returns, so a cached number is wrong.
 */
export function refreshQuota(id: ProviderId = app.activeProvider) {
  void loadQuota(id, true);
}

/** Forgets a drive's cached capacity, e.g. after logging out of it. */
export function forgetQuota(id: ProviderId) {
  delete app.quotas[id];
  quotaStamps.delete(id);
}

/**
 * Falls back to a working drive when the remembered one is not logged in.
 *
 * Called only at startup: doing it on every refresh would make it impossible to
 * switch *to* an unlogged drive, because the switcher would bounce straight
 * back (that is exactly the bug this split fixes).
 */
export async function ensureUsableProvider() {
  await loadProviderStates();
  const active = app.providers.find((p) => p.provider === app.activeProvider);
  if (!active || active.authenticated) return;
  const usable = app.providers.find((p) => p.authenticated);
  if (!usable) return;
  app.activeProvider = usable.provider;
  localStorage.setItem(storageKey("provider"), usable.provider);
  void loadQuota(usable.provider);
  requestFilesRefresh();
}

export async function bootstrap() {
  const storedProvider = localStorage.getItem(storageKey("provider")) as ProviderId | null;
  if (storedProvider) app.activeProvider = storedProvider;
  const stored = localStorage.getItem(storageKey("theme")) as ThemeMode | null;
  if (stored === "light" || stored === "dark" || stored === "system") {
    app.themeMode = stored;
  }
  applyTheme();
  await syncSystemTheme();

  try {
    const [auth, settings, transfers] = await Promise.all([
      api.authStatus(),
      api.settingsGet(),
      api.transfersList(),
    ]);
    app.authenticated = auth.authenticated;
    app.authSource = auth.source;
    app.maskedCookie = auth.masked;
    app.cookiePath = auth.cookiePath;
    app.settings = settings;
    app.transfers = transfers;
    // `auth` only describes 夸克; with four drives the boot screen depends on
    // whether *any* of them is signed in. Always landing on the file list is
    // deliberate: an unlogged drive shows "未登录，请先登录" with its own
    // 去登录 button, which is the flow the sidebar switcher is built around.
    await ensureUsableProvider();
    void loadQuota();
    app.view = "files";
    void loadAccount();
    // Mock builds are reviewable straight from a browser; let `?view=` and
    // `?theme=` jump to a screen without hunting for it.
    if (import.meta.env.VITE_MOCK === "1") await watchDevScreen();
  } catch (err) {
    app.bootError = errText(err);
    app.view = "login";
  } finally {
    app.ready = true;
  }

  await events.onTransfer(applyTransfer);
  await events.onTransferRemoved((id) => {
    const index = app.transfers.findIndex((t) => t.id === id);
    if (index >= 0) app.transfers.splice(index, 1);
  });
  await events.onDeeplink((link) => openShareLink(link));
  // The tray's "传输列表" item switches screens from outside the UI.
  await events.onNavigate((view) => {
    if (view === "transfers" || view === "files" || view === "settings" || view === "share") {
      app.view = view;
    }
  });

  // A link on our command line may have arrived before the listener existed.
  try {
    const pending = await api.deeplinkTake();
    if (pending) openShareLink(pending);
  } catch {
    /* no deeplink support in this build */
  }
}

/** Clears the cached profile after the cookie changes. */
export async function refreshAccount(force = false) {
  app.account = null;
  await loadAccount(force);
}

export async function refreshAuth() {
  const auth = await api.authStatus();
  app.authenticated = auth.authenticated;
  app.authSource = auth.source;
  app.maskedCookie = auth.masked;
  app.cookiePath = auth.cookiePath;
  if (!auth.authenticated) app.view = "login";
}

export async function saveSettings(next: Settings) {
  app.settings = await api.settingsSet(next);
  return app.settings;
}

/**
 * Mock-only review aid: `public/dev-screen.json` ({ "view": ..., "theme": ... })
 * is polled so screenshots can walk every screen without a clickable session.
 * The whole block is compiled out of production builds.
 */
async function watchDevScreen() {
  const params = new URLSearchParams(location.search);
  let last = "";
  const apply = (raw: { view?: string; theme?: string }) => {
    const signature = JSON.stringify(raw);
    if (signature === last) return;
    last = signature;
    if (raw.view && raw.view !== app.view) app.view = raw.view as View;
    if (raw.theme === "light" || raw.theme === "dark") setThemeMode(raw.theme);
  };
  const fromQuery = { view: params.get("view") ?? undefined, theme: params.get("theme") ?? undefined };
  if (fromQuery.view || fromQuery.theme) {
    // Query params win; they are how a browser review session pins a screen.
    apply(fromQuery);
    return;
  }
  const tick = async () => {
    try {
      const res = await fetch(`/dev-screen.json?t=${Date.now()}`);
      if (res.ok) apply(await res.json());
    } catch {
      /* not in dev-screen mode */
    }
  };
  await tick();
  window.setInterval(tick, 1200);
}

// `setThemeMode` applies the theme itself; one watcher here would run every
// switch twice and produce a visible flash at the end of the cross-fade.
