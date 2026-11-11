/**
 * Clipboard writes that survive the environments this app actually runs in.
 *
 * `navigator.clipboard` needs a secure context and a focused window. The
 * packaged app is a `tauri://` (or `http://localhost`) WebView where the API
 * exists but can still reject — for instance while a modal just closed and the
 * document has not regained focus yet — and the mock build is served over plain
 * `http://127.0.0.1`, which is treated as secure, yet headless checks reject
 * it anyway. When that happens we fall back to the old `execCommand` path
 * instead of dropping the copy on the floor.
 */
export async function copyText(text: string): Promise<boolean> {
  if (!text) return false;
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    /* fall through to the textarea trick */
  }
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.top = "0";
  area.style.left = "0";
  area.style.opacity = "0";
  area.style.pointerEvents = "none";
  document.body.appendChild(area);
  area.select();
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch {
    ok = false;
  }
  area.remove();
  return ok;
}
