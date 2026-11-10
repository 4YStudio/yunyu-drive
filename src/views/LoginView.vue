<script setup lang="ts">
/**
 * Login screen for exactly one drive — the one the user is looking at.
 *
 * There is deliberately no drive picker here. The drive is chosen in the
 * sidebar; an unlogged drive shows "未登录，请先登录" in the file list and its
 * "去登录" button lands straight on that drive's own login UI. Asking again
 * which cloud to log into is the step that made this screen confusing.
 *
 * Every drive authenticates differently, so each gets a dedicated layout:
 *   quark  — webview window that harvests the cookie (paste as fallback)
 *   yun139 — webview window that harvests the `authorization` cookie
 *   aliyun — QR code (refresh_token paste as fallback)
 *   pan123 — account + password
 *   baidu  — webview window that harvests `BDUSS`/`STOKEN` (paste as fallback)
 */
import { computed, onActivated, onBeforeUnmount, onDeactivated, onMounted, ref } from "vue";
import type { UnlistenFn } from "@tauri-apps/api/event";

import Icon from "@/components/Icon.vue";
import AliyunQrLogin from "@/components/AliyunQrLogin.vue";
import { api, events } from "@/lib/backend";
import { PROVIDERS, type ProviderId } from "@/lib/types";
import {
  app,
  forgetQuota,
  loadAccount,
  loadProviderStates,
  requestFilesRefresh,
  setActiveProvider,
  toast,
} from "@/store/app";

/**
 * The drive being logged in.
 *
 * Callers that know the drive (a drive's empty state, the settings page, the
 * sidebar) set `loginTarget`; a bare visit falls back to the drive the user is
 * currently looking at.
 */
function takeTarget(): ProviderId | null {
  const next = app.loginTarget;
  // One-shot: a later visit recomputes from `activeProvider`.
  app.loginTarget = null;
  return next;
}

const target = ref<ProviderId>(takeTarget() ?? app.activeProvider);
/** The initial mount already resolved `target`; the first `activated` hook
 * fires right after it and must not resolve it a second time. */
let firstActivation = true;

const credential = ref("");
const username = ref("");
const password = ref("");
const busy = ref(false);
const webBusy = ref(false);
const error = ref<string | null>(null);
const cookiePath = ref("");
let unlisten: UnlistenFn | null = null;
let unlisten139: UnlistenFn | null = null;
let unlistenBaidu: UnlistenFn | null = null;

const meta = computed(
  () => PROVIDERS.find((p) => p.id === target.value) ?? PROVIDERS[0],
);

const state = computed(
  () => app.providers.find((p) => p.provider === target.value) ?? null,
);

const trimmed = computed(() => credential.value.trim());
const looksLikeCookie = computed(
  () => trimmed.value.includes("=") && trimmed.value.length > 16,
);

const canSubmit = computed(() => {
  if (target.value === "pan123") {
    return username.value.trim().length > 0 && password.value.length > 0;
  }
  if (target.value === "quark") return looksLikeCookie.value;
  // 百度网盘 takes a full `Cookie:` header too, so require the `name=value`
  // shape rather than a bare token.
  if (target.value === "baidu") return looksLikeCookie.value;
  return trimmed.value.length > 8;
});

const instructions = computed(() => {
  switch (target.value) {
    case "quark":
      return "在弹出的登录窗口里扫码或账号登录，登录状态会自动保存到本地。";
    case "yun139":
      return "在弹出的登录窗口里用手机号登录中国移动云盘，登录状态会自动保存。";
    case "aliyun":
      return "用阿里云盘 App 扫码登录，登录状态会自动保存并续期。";
    case "pan123":
      return "使用 123 云盘的账号和密码登录，密码只保存在本机。";
    case "baidu":
      return "在弹出的登录窗口里登录百度账号，登录状态会自动保存到本地。";
  }
  return "";
});

const placeholder = computed(() => {
  switch (target.value) {
    case "quark":
      return "__puus=...; __pus=...; __kp=...; __kps=...";
    case "yun139":
      return "粘贴一长串 base64 形式的 token";
    case "aliyun":
      return "粘贴 refresh_token";
    case "baidu":
      return "BDUSS=...; STOKEN=...";
    default:
      return "";
  }
});

const helpSteps = computed(() => {
  switch (target.value) {
    case "quark":
      return [
        "在浏览器中打开 pan.quark.cn 并完成登录。",
        "按 F12 打开开发者工具，切换到「网络 / Network」。",
        "刷新页面，点开任意 drive.quark.cn 请求。",
        "在「请求头」中找到 Cookie，复制完整值粘贴到上方。",
      ];
    case "yun139":
      return [
        "登录移动云盘网页版（yun.139.com）或手机 App。",
        "按 F12 打开开发者工具，切换到「网络 / Network」。",
        "刷新页面，点开任意请求，在请求头里找 Authorization。",
        "复制 Basic 之后的那串 base64 粘贴到上方。",
      ];
    case "aliyun":
      return [
        "扫码是推荐方式；也可以从其他工具复制已登录的 refresh_token。",
        "令牌会自动刷新并保存在本机，过期前无需再次登录。",
      ];
    case "baidu":
      return [
        "推荐用上面的登录窗口；也可以在浏览器登录 pan.baidu.com 后手动粘贴 Cookie。",
        "按 F12 打开开发者工具，切换到「网络 / Network」，刷新页面。",
        "点开任意 pan.baidu.com 请求，复制请求头里的完整 Cookie 值。",
      ];
    default:
      return [];
  }
});

/** Leaves the login screen without logging anything new in. */
function leave() {
  app.view = "files";
  requestFilesRefresh();
}

/** Leaves the login screen once a drive reports a usable credential. */
async function finishLogin() {
  await loadProviderStates();
  const current = app.providers.find((p) => p.provider === target.value);
  if (current?.authenticated) {
    setActiveProvider(target.value);
    credential.value = "";
    username.value = "";
    password.value = "";
    app.authenticated = true;
    app.view = "files";
    requestFilesRefresh();
    void loadAccount(true);
    toast(`已登录${meta.value.name}`, "success");
  }
}

async function loginWithWeb() {
  if (webBusy.value) return;
  webBusy.value = true;
  error.value = null;
  try {
    if (target.value === "yun139") await api.openWebLogin139();
    else if (target.value === "baidu") await api.openWebLoginBaidu();
    else await api.openWebLogin();
  } catch (err) {
    error.value = String(err);
    webBusy.value = false;
  }
}

async function pasteFromClipboard() {
  try {
    const text = await navigator.clipboard.readText();
    if (text) credential.value = text.trim();
  } catch {
    toast("无法读取剪贴板，请手动粘贴", "error");
  }
}

async function submit() {
  if (!canSubmit.value) {
    error.value =
      target.value === "pan123" ? "请填写账号和密码" : "内容长度不足，请检查后重试";
    return;
  }
  busy.value = true;
  error.value = null;
  try {
    if (target.value === "quark") {
      await api.setCookie(trimmed.value);
      try {
        const total = await api.validateCookie();
        toast(`登录成功，云端共 ${total} 个条目`, "success");
      } catch (err) {
        toast(`已保存 Cookie，但校验失败：${err}`, "error");
      }
      const auth = await api.authStatus();
      app.authenticated = auth.authenticated;
      app.authSource = auth.source;
      app.maskedCookie = auth.masked;
      app.cookiePath = auth.cookiePath;
      await loadProviderStates();
      app.view = "files";
      requestFilesRefresh();
      void loadAccount(true);
      return;
    }

    // 123 云盘 packs both fields into one string; see `provider_login`.
    const value =
      target.value === "pan123"
        ? `${username.value.trim()}\u001f${password.value}`
        : trimmed.value;
    const auth = await api.providerLogin(target.value, value);
    if (!auth.authenticated) {
      error.value = "凭据无效，请检查后重试";
      return;
    }
    await finishLogin();
  } catch (err) {
    error.value = String(err);
  } finally {
    busy.value = false;
  }
}

/** The QR component just authenticated 阿里云盘; refresh and leave. */
async function onAliyunDone() {
  await loadProviderStates();
  target.value = "aliyun";
  await finishLogin();
}

/**
 * `<KeepAlive>` keeps this screen mounted across visits, so `target` would
 * otherwise stay frozen on whichever drive was logged in first — the sidebar
 * says 阿里云盘 while the panel shows the 123 云盘 form. Re-resolve on every
 * activation after the initial mount.
 */
onActivated(() => {
  if (firstActivation) {
    firstActivation = false;
    return;
  }
  const next = takeTarget() ?? app.activeProvider;
  if (next === target.value) return;
  // Anything half-typed belongs to the drive we are leaving.
  target.value = next;
  credential.value = "";
  username.value = "";
  password.value = "";
  error.value = null;
  if (webBusy.value) {
    webBusy.value = false;
    void api.closeWebLogin().catch(() => {});
  }
});

/** Leaving the screen while the login webview is open must not leave that
 * window (and its watcher) running in the background. */
onDeactivated(() => {
  if (webBusy.value) {
    webBusy.value = false;
    void api.closeWebLogin().catch(() => {});
  }
});

async function logout() {
  busy.value = true;
  try {
    await api.providerLogout(target.value);
    forgetQuota(target.value);
    await loadProviderStates();
    toast(`已退出${meta.value.name}`, "success");
  } catch (err) {
    error.value = String(err);
  } finally {
    busy.value = false;
  }
}

onMounted(async () => {
  await loadProviderStates();
  try {
    const auth = await api.authStatus();
    cookiePath.value = auth.cookiePath;
  } catch {
    /* ignore */
  }

  unlisten = await events.onWebLogin(async (evt) => {
    webBusy.value = false;
    if (evt.ok) {
      toast(evt.message || "登录成功", "success");
      const auth = await api.authStatus();
      app.authenticated = auth.authenticated;
      app.authSource = auth.source;
      app.maskedCookie = auth.masked;
      app.cookiePath = auth.cookiePath;
      target.value = "quark";
      await loadProviderStates();
      app.view = "files";
      requestFilesRefresh();
      void loadAccount(true);
    } else if (evt.message && evt.message !== "登录窗口已关闭") {
      toast(evt.message, "error");
    }
  });
  unlisten139 = await events.onWebLogin139(async (evt) => {
    webBusy.value = false;
    if (evt.ok) {
      toast(evt.message || "登录成功", "success");
      target.value = "yun139";
      await loadProviderStates();
      const current = app.providers.find((p) => p.provider === "yun139");
      if (current?.authenticated) {
        setActiveProvider("yun139");
        app.view = "files";
        requestFilesRefresh();
      }
    } else if (evt.message && evt.message !== "登录窗口已关闭") {
      toast(evt.message, "error");
    }
  });
  unlistenBaidu = await events.onWebLoginBaidu(async (evt) => {
    webBusy.value = false;
    if (evt.ok) {
      toast(evt.message || "登录成功", "success");
      target.value = "baidu";
      await loadProviderStates();
      const current = app.providers.find((p) => p.provider === "baidu");
      if (current?.authenticated) {
        setActiveProvider("baidu");
        app.view = "files";
        requestFilesRefresh();
      }
    } else if (evt.message && evt.message !== "登录窗口已关闭") {
      toast(evt.message, "error");
    }
  });
});

onBeforeUnmount(() => {
  unlisten?.();
  unlisten = null;
  unlisten139?.();
  unlisten139 = null;
  unlistenBaidu?.();
  unlistenBaidu = null;
  if (webBusy.value) void api.closeWebLogin().catch(() => {});
});
</script>

<template>
  <section class="login">
    <div class="backdrop-grid" aria-hidden="true" />

    <!-- One drive, one login page: no chooser in between. -->
    <div class="panel">
      <button class="back" @click="leave">
        <Icon name="chevron-left" :size="14" />
        <span>返回文件列表</span>
      </button>

      <div class="panel-head">
        <img :src="meta.logo" alt="" class="panel-logo" draggable="false" />
        <div class="panel-title">
          <h1 class="title">{{ meta.name }}</h1>
          <p class="sub sub--tight">{{ instructions }}</p>
        </div>
      </div>

      <!-- Quark: webview window first, cookie paste as fallback. -->
      <template v-if="target === 'quark'">
        <button class="btn btn--solid btn--wide" :disabled="webBusy" @click="loginWithWeb">
          <Icon v-if="webBusy" name="loader" :size="14" spin />
          <Icon v-else name="external" :size="14" />
          <span>{{ webBusy ? "等待登录窗口…" : "打开登录窗口" }}</span>
        </button>
        <p v-if="webBusy" class="hint">
          <Icon name="info" :size="12.5" />
          <span>已弹出登录窗口，登录成功后会自动继续；关掉窗口即可取消。</span>
        </p>

        <details class="manual">
          <summary>
            <Icon name="keyboard" :size="13" />
            <span>手动粘贴 Cookie</span>
          </summary>
          <label class="field">
            <span class="field-label">Cookie</span>
            <textarea
              v-model="credential"
              class="cookie-input"
              rows="4"
              spellcheck="false"
              :placeholder="placeholder"
            />
          </label>
          <div class="actions">
            <button class="btn btn--ghost" @click="pasteFromClipboard">
              <Icon name="copy" :size="14" />
              <span>粘贴剪贴板</span>
            </button>
            <button class="btn btn--solid" :disabled="busy || !canSubmit" @click="submit">
              <span>登录</span>
            </button>
          </div>
          <ol class="help-list">
            <li v-for="(step, i) in helpSteps" :key="i">{{ step }}</li>
          </ol>
        </details>
      </template>

      <!-- 中国移动云盘: webview window first, token paste as fallback. -->
      <template v-else-if="target === 'yun139'">
        <button class="btn btn--solid btn--wide" :disabled="webBusy" @click="loginWithWeb">
          <Icon v-if="webBusy" name="loader" :size="14" spin />
          <Icon v-else name="external" :size="14" />
          <span>{{ webBusy ? "等待登录窗口…" : "打开登录窗口" }}</span>
        </button>
        <p v-if="webBusy" class="hint">
          <Icon name="info" :size="12.5" />
          <span>已弹出登录窗口，登录成功后会自动继续；关掉窗口即可取消。</span>
        </p>

        <details class="manual">
          <summary>
            <Icon name="keyboard" :size="13" />
            <span>手动粘贴 Token</span>
          </summary>
          <label class="field">
            <span class="field-label">Token</span>
            <textarea
              v-model="credential"
              class="cookie-input"
              rows="4"
              spellcheck="false"
              :placeholder="placeholder"
            />
          </label>
          <div class="actions">
            <button class="btn btn--ghost" @click="pasteFromClipboard">
              <Icon name="copy" :size="14" />
              <span>粘贴剪贴板</span>
            </button>
            <button class="btn btn--solid" :disabled="busy || !canSubmit" @click="submit">
              <span>登录</span>
            </button>
          </div>
          <ol class="help-list">
            <li v-for="(step, i) in helpSteps" :key="i">{{ step }}</li>
          </ol>
        </details>
      </template>

      <!-- 阿里云盘: QR code, token paste as fallback. -->
      <template v-else-if="target === 'aliyun'">
        <AliyunQrLogin @done="onAliyunDone" />
        <details class="manual">
          <summary>
            <Icon name="keyboard" :size="13" />
            <span>或者粘贴 refresh_token</span>
          </summary>
          <label class="field">
            <span class="field-label">refresh_token</span>
            <textarea
              v-model="credential"
              class="cookie-input"
              rows="3"
              spellcheck="false"
              :placeholder="placeholder"
            />
          </label>
          <div class="actions">
            <button class="btn btn--ghost" @click="pasteFromClipboard">
              <Icon name="copy" :size="14" />
              <span>粘贴剪贴板</span>
            </button>
            <button class="btn btn--solid" :disabled="busy || !canSubmit" @click="submit">
              <span>登录</span>
            </button>
          </div>
        </details>
      </template>

      <!-- 百度网盘: webview window first, cookie paste as fallback. -->
      <template v-else-if="target === 'baidu'">
        <button class="btn btn--solid btn--wide" :disabled="webBusy" @click="loginWithWeb">
          <Icon v-if="webBusy" name="loader" :size="14" spin />
          <Icon v-else name="external" :size="14" />
          <span>{{ webBusy ? "等待登录窗口…" : "打开登录窗口" }}</span>
        </button>
        <p v-if="webBusy" class="hint">
          <Icon name="info" :size="12.5" />
          <span>已弹出登录窗口，登录成功后会自动继续；关掉窗口即可取消。</span>
        </p>

        <details class="manual">
          <summary>
            <Icon name="keyboard" :size="13" />
            <span>手动粘贴 Cookie</span>
          </summary>
          <label class="field">
            <span class="field-label">Cookie</span>
            <textarea
              v-model="credential"
              class="cookie-input"
              rows="4"
              spellcheck="false"
              :placeholder="placeholder"
            />
          </label>
          <div class="actions">
            <button class="btn btn--ghost" @click="pasteFromClipboard">
              <Icon name="copy" :size="14" />
              <span>粘贴剪贴板</span>
            </button>
            <button class="btn btn--solid" :disabled="busy || !canSubmit" @click="submit">
              <span>登录</span>
            </button>
          </div>
          <ol class="help-list">
            <li v-for="(step, i) in helpSteps" :key="i">{{ step }}</li>
          </ol>
        </details>
      </template>

      <!-- 123 云盘: account + password. -->
      <template v-else-if="target === 'pan123'">
        <label class="field">
          <span class="field-label">账号</span>
          <input
            v-model="username"
            class="text-input"
            spellcheck="false"
            autocomplete="off"
            placeholder="手机号或邮箱"
          />
        </label>
        <label class="field">
          <span class="field-label">密码</span>
          <input
            v-model="password"
            class="text-input"
            type="password"
            autocomplete="off"
            placeholder="6–16 位密码"
            @keyup.enter="submit"
          />
        </label>
      </template>

      <p v-if="error" class="error">
        <Icon name="alert" :size="13" />
        <span>{{ error }}</span>
      </p>

      <div class="foot">
        <button
          v-if="target === 'pan123' || target === 'aliyun'"
          class="btn btn--solid"
          :disabled="busy || !canSubmit"
          @click="submit"
        >
          <Icon v-if="busy" name="loader" :size="14" spin />
          <span>{{ busy ? "登录中…" : state?.authenticated ? "重新登录" : "登录" }}</span>
        </button>
        <button v-if="state?.authenticated" class="btn" :disabled="busy" @click="logout">
          退出登录
        </button>
      </div>

      <p class="help-note">
        凭据保存在
        <span class="mono">~/.config/yunyu-drive/</span>，
        文件权限为 0600，仅本机可读。
        <template v-if="target === 'quark' && cookiePath">
          Cookie 位于 <span class="mono">{{ cookiePath }}</span>。
        </template>
      </p>
    </div>
  </section>
</template>

<style scoped>
.login {
  position: relative;
  display: grid;
  place-items: center;
  height: 100%;
  padding: 32px;
  overflow: auto;
  background: var(--bg);
}
.backdrop-grid {
  position: absolute;
  inset: 0;
  transform: translateY(0);
  background-image: radial-gradient(var(--line-strong) 1px, transparent 1px);
  background-size: 22px 22px;
  mask-image: radial-gradient(circle at 50% 30%, black 0%, transparent 72%);
  opacity: 0.55;
  pointer-events: none;
}
.panel {
  position: relative;
  width: min(100%, 460px);
  padding: 30px 30px 22px;
  border-radius: var(--radius-xl);
  background: var(--bg);
  border: 1px solid var(--line);
  box-shadow: var(--shadow-lg);
  /*
   * WebKitGTK keeps a compositing layer alive only while the element actually
   * has a transform. Without this identity value the layer is destroyed when the
   * entrance animation finishes and WebKit re-rasterises the whole panel — one
   * stale frame, i.e. the flicker. See docs/tauri-webkitgtk-flicker.md.
   */
  /*
   * No entrance transition at all — deliberate, and measured.
   *
   * A transform transition here replays when WebKitGTK re-inserts the element
   * (`<KeepAlive>` re-attachment), and the replay happens *after* the animation
   * has already settled: the probe shows `ty=0` for several frames, then
   * `ty=8.7`, then back to 0. That is the flash. CSS animations replay the same
   * way. The identity transform stays because it keeps the compositing layer
   * alive (see docs/tauri-webkitgtk-flicker.md, cause 3).
   */
  transform: translateY(0);
}
/*
 * No `scale()` and no opacity: scaling a box that carries a large shadow makes
 * WebKitGTK re-rasterise it every frame, and fading is what re-painted the old
 * screen when KeepAlive replayed the enter hook (same doc, causes 1 and 3).
 */
.title {
  margin: 0 0 6px;
  font-size: 19px;
  font-weight: 600;
  letter-spacing: -0.02em;
}
.sub {
  margin: 0 0 16px;
  font-size: 12.2px;
  line-height: 1.65;
  color: var(--text-2);
}
.sub--tight {
  margin: 6px 0 0;
}

/* ---------------- login step ---------------- */
.back {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  margin: -6px 0 16px -6px;
  padding: 5px 8px;
  border-radius: var(--radius-xs);
  font-size: 11.8px;
  color: var(--text-2);
  transition: background-color var(--dur-1) var(--ease-out), color var(--dur-1) var(--ease-out);
}
.back:hover {
  background: var(--bg-hover);
  color: var(--text);
}
.panel-head {
  display: flex;
  align-items: flex-start;
  gap: 14px;
  margin-bottom: 18px;
}
.panel-logo {
  flex: none;
  width: 40px;
  height: 40px;
  object-fit: contain;
  margin-top: 2px;
}
.panel-title .title {
  font-size: 17px;
}
.btn--wide {
  width: 100%;
  height: 38px;
  justify-content: center;
}
.hint {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 10px 0 0;
  font-size: 11.6px;
  color: var(--text-3);
}
.error {
  display: flex;
  align-items: flex-start;
  gap: 7px;
  margin: 12px 0 0;
  padding: 9px 11px;
  border-radius: var(--radius-xs);
  font-size: 11.8px;
  line-height: 1.55;
  color: var(--text);
  background: var(--bg-active);
}
.manual {
  margin-top: 14px;
  padding-top: 14px;
  border-top: 1px solid var(--line);
  font-size: 11.8px;
  color: var(--text-2);
}
.manual summary {
  display: flex;
  align-items: center;
  gap: 6px;
  cursor: pointer;
  list-style: none;
  font-weight: 520;
  color: var(--text-2);
  transition: color var(--dur-1) var(--ease-out);
}
.manual summary::-webkit-details-marker {
  display: none;
}
.manual summary:hover {
  color: var(--text);
}
.field {
  display: flex;
  flex-direction: column;
  gap: 7px;
  margin-top: 12px;
}
.field-label {
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 0.05em;
  text-transform: uppercase;
  color: var(--text-3);
}
.cookie-input,
.text-input {
  width: 100%;
  padding: 10px 12px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--line);
  background: var(--bg-sunken);
  font-size: 11.8px;
  line-height: 1.6;
  outline: none;
  transition: border-color var(--dur-2) var(--ease-out),
    box-shadow var(--dur-2) var(--ease-out);
}
.cookie-input {
  font-family: var(--font-mono);
  font-size: 11.5px;
  resize: vertical;
}
.text-input {
  height: 36px;
  padding: 0 12px;
}
.cookie-input:focus,
.text-input:focus {
  border-color: var(--line-strong);
  box-shadow: 0 0 0 3.5px var(--bg-active);
}
.cookie-input::placeholder,
.text-input::placeholder {
  color: var(--text-3);
}
.actions {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 12px;
}
.foot {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 18px;
}
.help-list {
  margin: 12px 0 0;
  padding-left: 18px;
  display: flex;
  flex-direction: column;
  gap: 5px;
  line-height: 1.6;
  color: var(--text-3);
}
.help-list li::marker {
  color: var(--text-3);
}
.help-note {
  margin: 16px 0 0;
  padding-top: 14px;
  border-top: 1px solid var(--line);
  font-size: 11.2px;
  line-height: 1.65;
  color: var(--text-3);
}
</style>
