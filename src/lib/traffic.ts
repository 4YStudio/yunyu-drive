import { formatSize } from "@/lib/format";
import { app, loadTraffic } from "@/store/app";
import type { ProviderId } from "@/lib/types";

/** Below this much allowance left, even an unknown-size folder is risky. */
const LOW_BYTES = 1024 ** 3;

/**
 * A reason to warn before starting a download.
 *
 * `short` fits in a dialog line; `detail` spells out the numbers so a user can
 * judge whether to continue (123 lets the transfer start and then cuts it off
 * when the allowance runs out, which leaves an incomplete file).
 */
export interface TrafficWarning {
  short: string;
  detail: string;
}

/**
 * Checks a planned download against the drive's monthly download allowance.
 *
 * Only 123 云盘 meters downloads separately from storage, so every other drive
 * returns `null` immediately. A drive we cannot read (offline, upgraded
 * account without the field) also returns `null`: a missing reading must never
 * block a download.
 */
export async function trafficWarning(
  provider: ProviderId,
  bytes: number,
  hasFolders: boolean,
): Promise<TrafficWarning | null> {
  if (provider !== "pan123") return null;
  await loadTraffic(provider);
  const traffic = app.traffics[provider];
  if (!traffic || traffic.left <= 0) return null;
  const left = formatSize(traffic.left);

  if (bytes > traffic.left) {
    const need = formatSize(bytes);
    const short = `剩余下载流量 ${left}，不够这次 ${need}`;
    return {
      short,
      detail: `123 云盘免费账号每月有下载额度，超额后传输会被中断，文件可能下载不完整。还差 ${formatSize(bytes - traffic.left)}，可以等下个月额度刷新，或下载更少的内容。`,
    };
  }

  if (hasFolders && traffic.left < LOW_BYTES) {
    return {
      short: `剩余下载流量只有 ${left}`,
      detail:
        "这次包含文件夹，实际大小要读取后才知道；额度不足时传输会被中断，文件可能下载不完整。",
    };
  }

  return null;
}
