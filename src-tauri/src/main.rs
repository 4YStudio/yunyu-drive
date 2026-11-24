// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    configure_webkitgtk_renderer();

    yunyu_drive_lib::run();
}

/// 规避 WebKitGTK 的合成器/DMABUF 回归：任何动画的首尾帧都会短暂显示该图层的旧表面
/// （上游 bug 327524，2.54.3 仍未修）。Chromium 不受影响，所以同一套 UI 在浏览器里正常、
/// 只在 Tauri 窗口里闪。
///
/// 必须在 `run()` 之前调用：WebKit 的 web process 启动后就读不到后来改的环境变量了。
/// 只在 Linux 上有意义 —— macOS 走 WKWebView、Windows 走 WebView2(Chromium)，两者都不读
/// 任何 `WEBKIT_*`，既不会变成 CPU 渲染也不会掉帧，所以用 cfg 显式挡在外面。
///
/// 档位（环境变量 `YUNYU_WEBKIT_RENDERER`）：
///   （默认）/ norenderer → `WEBKIT_DISABLE_DMABUF_RENDERER=1`，历史验证过不闪的那一档
///   shm                  → `WEBKIT_DMABUF_RENDERER_FORCE_SHM=1`，2.54 起首选，保留 DMABUF
///                          传输通道，只把缓冲换成共享内存；**尚未在真机肉眼确认，验证通过后
///                          可把下面的默认值切到这一档**
///   native / dmabuf / gpu / default → 不设任何变量，走 WebKit 自己的默认（2.54.x 上会闪，
///                          只用于排查/对比）
///   soft                 → 再叠加 `WEBKIT_DISABLE_COMPOSITING_MODE=1`，整页 CPU 渲染，
///                          比闪烁还难用，仅排查用
///
/// 用户自己 export 了 `WEBKIT_DISABLE_DMABUF_RENDERER` 时完全尊重用户的值，不覆盖。
/// 详见 docs/tauri-webkitgtk-flicker.md。
#[cfg(target_os = "linux")]
fn configure_webkitgtk_renderer() {
    use std::env;

    if env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some() {
        return;
    }

    let mode = env::var("YUNYU_WEBKIT_RENDERER")
        .unwrap_or_default()
        .to_ascii_lowercase();

    match mode.as_str() {
        // 回 WebKit 自己的默认路径，只用于排查/对比。
        "native" | "dmabuf" | "gpu" | "default" => {}
        // 2.54 起的首选：DMABUF renderer 保留，缓冲改走共享内存。
        "shm" => {
            env::set_var("WEBKIT_DMABUF_RENDERER_FORCE_SHM", "1");
        }
        // 兜底一档：连加速合成一起关掉，滚动/模糊/圆角全部回 CPU。
        "soft" => {
            env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
            env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        }
        // 默认行为（含 norenderer 及任意未知值）：只关 DMABUF，保留加速合成。
        _ => {
            env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn configure_webkitgtk_renderer() {}
