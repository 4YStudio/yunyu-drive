<script setup lang="ts">
/**
 * 阿里云盘扫码登录: fetch a passport QR code, render it, then poll until the
 * phone confirms.
 *
 * The QR is drawn to a canvas by the `qrcode` package — no network request and
 * no external image, so it also works in a packaged build with a strict CSP.
 */
import { onActivated, onBeforeUnmount, onDeactivated, ref } from "vue";
import QRCode from "qrcode";

import Icon from "@/components/Icon.vue";
import { api } from "@/lib/backend";
import { errText } from "@/lib/format";
import { toast } from "@/store/app";

const emit = defineEmits<{ done: [] }>();

const canvas = ref<HTMLCanvasElement | null>(null);
const status = ref<"idle" | "loading" | "waiting" | "scanned" | "done" | "expired">(
  "loading",
);
const message = ref<string | null>(null);

type Qr = { content: string; t: number; ck: string };
let current: Qr | null = null;
let timer: ReturnType<typeof setTimeout> | null = null;
let stopped = false;
/** Give the user a few minutes to scan; the server expires the code sooner,
 * but `expired` offers an immediate refresh, so this only bounds the wait. */
let deadline = 0;
/** Consecutive poll failures; a real error should not hide behind "expired". */
let failures = 0;
/** Exchange retries after the phone confirmed; passport burns the code, so the
 * backend replays the stored payload and we should not hammer it forever. */
let exchanges = 0;

function stopPolling() {
  if (timer) {
    clearTimeout(timer);
    timer = null;
  }
}

async function draw(content: string) {
  if (!canvas.value) return;
  await QRCode.toCanvas(canvas.value, content, {
    width: 200,
    margin: 1,
    // Match the app's palette so the code sits on the panel instead of a white
    // sticker. Error correction stays high enough for a phone screen.
    color: { dark: "#0b0b0c", light: "#ffffff" },
    errorCorrectionLevel: "M",
  });
}

async function start() {
  stopPolling();
  stopped = false;
  failures = 0;
  exchanges = 0;
  status.value = "loading";
  message.value = null;
  try {
    const qr = await api.aliyunQrStart();
    current = qr;
    await draw(qr.content);
    status.value = "waiting";
    deadline = Date.now() + 240_000;
    schedulePoll();
  } catch (err) {
    status.value = "expired";
    message.value = errText(err);
  }
}

/** The passport service starts returning odd statuses when polled too fast;
 * the reference client waits 3 s, so stay in the same order of magnitude. */
const POLL_MS = 2000;

function schedulePoll() {
  stopPolling();
  timer = setTimeout(poll, POLL_MS);
}

async function poll() {
  if (stopped || !current) return;
  if (Date.now() > deadline) {
    status.value = "expired";
    message.value = "二维码已过期，请刷新";
    return;
  }
  try {
    const res = await api.aliyunQrPoll(current);
    failures = 0;
    if (res.authenticated) {
      status.value = "done";
      message.value = res.message ?? "登录成功";
      toast("阿里云盘登录成功", "success");
      emit("done");
      return;
    }
    if (res.state === "scanned") {
      status.value = "scanned";
      message.value = "已扫码，请在手机上确认";
    } else if (res.state === "confirmed" && res.message) {
      // Confirmed but the token exchange failed: the QR code is spent, so this
      // is the real error. Retry a couple of times, then say so plainly.
      exchanges += 1;
      status.value = "scanned";
      message.value = res.message;
      if (exchanges >= 3) {
        status.value = "expired";
        return;
      }
    } else if (res.state === "expired" || res.state === "failed") {
      status.value = "expired";
      message.value = res.message ?? "二维码已失效";
      return;
    } else if (res.message) {
      // Server still says "waiting" but the backend hit a problem (e.g. the
      // token exchange failed): surface it instead of pretending all is well.
      message.value = res.message;
    } else {
      status.value = "waiting";
      message.value = null;
    }
    schedulePoll();
  } catch (err) {
    // The backend reports exchange/step failures as errors. Retrying a network
    // blip is right; hiding a real failure until the deadline is wrong, so show
    // the message at once and stop after a few consecutive failures.
    failures += 1;
    message.value = errText(err);
    if (failures >= 3) {
      status.value = "expired";
      return;
    }
    schedulePoll();
  }
}

onBeforeUnmount(() => {
  stopped = true;
  stopPolling();
});

/** `<KeepAlive>` keeps the login screen mounted while the user browses files;
 * a QR poll every 2 s in the background is wasted traffic and can expire the
 * code before it is scanned. Pause on the way out, resume on the way back. */
onDeactivated(() => {
  stopped = true;
  stopPolling();
});

onActivated(() => {
  if (status.value !== "waiting" && status.value !== "scanned") return;
  stopped = false;
  schedulePoll();
});

void start();
</script>

<template>
  <div class="qr">
    <div class="qr-frame">
      <canvas ref="canvas" width="200" height="200" aria-label="阿里云盘登录二维码" />
      <div v-if="status === 'loading'" class="qr-overlay">
        <Icon name="loader" :size="20" spin />
      </div>
      <div v-else-if="status === 'expired'" class="qr-overlay qr-overlay--solid">
        <p>{{ message ?? "二维码已失效" }}</p>
        <button class="btn btn--ghost" @click="start">
          <Icon name="refresh" :size="13" />
          <span>刷新二维码</span>
        </button>
      </div>
    </div>

    <p class="qr-hint">
      <template v-if="status === 'scanned' && message">
        <Icon name="alert" :size="12.5" />
        <span class="qr-error">{{ message }}</span>
      </template>
      <template v-else-if="status === 'scanned'">
        <Icon name="check-circle" :size="12.5" />
        <span>已扫码，请在手机上点「确认登录」</span>
      </template>
      <template v-else-if="status === 'done'">
        <Icon name="check-circle" :size="12.5" />
        <span>登录成功</span>
      </template>
      <template v-else>
        <Icon name="info" :size="12.5" />
        <span>用阿里云盘 App 扫描二维码登录</span>
      </template>
    </p>
  </div>
</template>

<style scoped>
.qr {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 12px;
  margin-top: 4px;
}
.qr-frame {
  position: relative;
  display: grid;
  place-items: center;
  width: 216px;
  height: 216px;
  padding: 8px;
  border-radius: var(--radius-md);
  border: 1px solid var(--line);
  background: #fff;
}
.qr-frame canvas {
  display: block;
  width: 200px;
  height: 200px;
  image-rendering: pixelated;
}
.qr-overlay {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  border-radius: var(--radius-md);
  background: color-mix(in srgb, var(--bg) 82%, transparent);
  color: var(--text-2);
  font-size: 11.8px;
  text-align: center;
  padding: 12px;
}
.qr-overlay--solid {
  background: var(--bg);
}
.qr-hint {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 0;
  font-size: 11.6px;
  color: var(--text-3);
}
.qr-error {
  color: #e0554f;
  line-height: 1.5;
}
</style>
