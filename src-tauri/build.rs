use std::path::PathBuf;

fn main() {
    // `tauri-codegen` embeds the window/tray icon at compile time, but
    // `tauri-build` only emits `rerun-if-changed` for `tauri.conf.json` and
    // `capabilities/`. Without the line below, replacing an icon does *not*
    // invalidate the cached codegen output — the binary (and therefore the
    // taskbar/window icon) silently keeps the old artwork while the files under
    // /usr/share/icons are already new. That is exactly how a "I replaced the
    // logo but the icon is still the old one" bug happens.
    println!("cargo:rerun-if-changed=icons");

    // 版本号的唯一来源是仓库根目录的 package.json。
    //
    // 以前版本号要在三处手工同步：package.json、tauri.conf.json、
    // src-tauri/Cargo.toml。tauri.conf.json 的 `version` 已经改成
    // "../package.json"（Tauri 允许填一个含 version 字段的 JSON 路径），
    // 于是安装包名、deb/rpm 元数据、macOS 的 CFBundleShortVersionString
    // 都由 package.json 决定。
    //
    // Cargo.toml 的 version 没法指向外部文件，所以它不再承载版本信息
    // （缺省即 0.0.0），改由这里把 package.json 的 version 以编译期环境变量
    // 的形式注入，代码里用 env!("YUNYU_VERSION") 读取。
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let package_json = manifest_dir.join("..").join("package.json");

    println!("cargo:rerun-if-changed={}", package_json.display());

    let raw = std::fs::read_to_string(&package_json).unwrap_or_else(|e| {
        panic!(
            "读取 {} 失败：{e}\n版本号的唯一来源是这个文件，请确认它存在。",
            package_json.display()
        )
    });

    let json: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
        panic!("解析 {} 失败：{e}", package_json.display())
    });

    let version = json
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            panic!(
                "{} 里没有字符串类型的 version 字段",
                package_json.display()
            )
        });

    // 空版本号会静默产出 "app 0.0.0" 的安装包，早点炸掉更好。
    if version.trim().is_empty() {
        panic!("{} 的 version 是空的", package_json.display());
    }

    println!("cargo:rustc-env=YUNYU_VERSION={version}");

    tauri_build::build()
}
