//! End-to-end probes for the share implementations.
//!
//! These talk to the real drives with the credentials in
//! `~/.config/yunyu-drive/`, so they are `#[ignore]`d. Run them with
//!
//! ```text
//! cargo test --lib share_live -- --ignored --nocapture
//! ```

use std::sync::Arc;

use crate::error::AppResult;
use crate::providers::{DriveProvider, ProviderId};
use crate::share::{self, ShareSession};

fn config_dir() -> std::path::PathBuf {
    crate::config::AppPaths::resolve().unwrap().config_dir
}

fn drive(provider: ProviderId) -> Option<Arc<dyn DriveProvider>> {
    let dir = config_dir();
    let built: AppResult<Arc<dyn DriveProvider>> = match provider {
        ProviderId::Quark => {
            let cookie = std::fs::read_to_string(dir.join("cookie.txt")).ok()?;
            let client = libquarkpan::QuarkPan::builder()
                .cookie(cookie.trim())
                .api_base_url("https://drive.quark.cn")
                .prepare()
                .ok()?;
            Ok(Arc::new(crate::providers::quark::QuarkProvider::new(
                client,
                cookie.trim().to_string(),
                dir.join("cookie.txt").to_string_lossy().to_string(),
            )))
        }
        ProviderId::Yun139 => crate::providers::yun139::Yun139Provider::load(&dir).map(|p| {
            let p: Arc<dyn DriveProvider> = Arc::new(p);
            p
        }),
        ProviderId::Pan123 => crate::providers::pan123::Pan123Provider::load(&dir).map(|p| {
            let p: Arc<dyn DriveProvider> = Arc::new(p);
            p
        }),
        ProviderId::Aliyun => crate::providers::aliyun::AliyunProvider::load(&dir).map(|p| {
            let p: Arc<dyn DriveProvider> = Arc::new(p);
            p
        }),
        ProviderId::Baidu => crate::providers::baidu::BaiduProvider::load(&dir).map(|p| {
            let p: Arc<dyn DriveProvider> = Arc::new(p);
            p
        }),
    };
    match built {
        Ok(d) => Some(d),
        Err(err) => {
            println!("  [skip] {} 未登录：{err}", provider.as_str());
            None
        }
    }
}

fn show(session: &ShareSession, entries: &[crate::share::ShareEntry]) {
    println!(
        "  title={:?} owner={:?} files={} size={} canSave={} avatar={}",
        session.title,
        session.owner,
        session.file_num,
        session.total_size,
        session.can_save,
        session.owner_avatar.is_some()
    );
    for entry in entries.iter().take(8) {
        println!(
            "    {:<40} dir={:<5} size={:<12} token={}",
            entry.name,
            entry.is_dir,
            entry.size,
            if entry.token.is_empty() {
                "-".to_string()
            } else {
                entry.token.chars().take(18).collect::<String>()
            }
        );
    }
}

async fn probe(provider: ProviderId, link: &str) {
    println!("\n=== {} ===", provider.as_str());
    let Some(drive) = drive(provider) else { return };
    println!(
        "  capabilities share_open={} share_create={}",
        drive.capabilities().share_open,
        drive.capabilities().share_create
    );
    let target = match share::parse_link(link) {
        Ok(t) => t,
        Err(err) => {
            println!("  parse ERR {err}");
            return;
        }
    };
    println!("  parsed id={} pwd={:?}", target.id, target.passcode);
    let session = match drive.share_open(&target.id, &target.passcode).await {
        Ok(s) => s,
        Err(err) => {
            println!("  share_open ERR {err}");
            return;
        }
    };
    let dir = match provider {
        ProviderId::Aliyun => "root",
        ProviderId::Baidu => "/",
        _ => "0",
    };
    let share_ref = crate::transfer::ShareRef {
        provider: provider.as_str().to_string(),
        pwd_id: session.pwd_id.clone(),
        stoken: session.stoken.clone(),
        fid_token: String::new(),
        passcode: session.passcode.clone(),
    };
    match drive.share_list(&share_ref, dir).await {
        Ok(listing) => {
            show(&session, &listing.entries);
            println!("  total={}", listing.total);
            // Descend into the first folder to prove paging/nesting works.
            if let Some(folder) = listing.entries.iter().find(|e| e.is_dir) {
                println!("  → 进入「{}」", folder.name);
                match drive.share_list(&share_ref, &folder.fid).await {
                    Ok(inner) => {
                        for entry in inner.entries.iter().take(5) {
                            println!(
                                "    {:<40} dir={:<5} size={}",
                                entry.name, entry.is_dir, entry.size
                            );
                        }
                        println!("  inner total={}", inner.total);
                    }
                    Err(err) => println!("  inner ERR {err}"),
                }
            }
            match drive.share_walk(&share_ref, dir).await {
                Ok(files) => {
                    println!("  walk {} files", files.len());
                    for file in files.iter().take(4) {
                        println!("    {} ({} B)", file.relative.display(), file.size);
                    }
                }
                Err(err) => println!("  walk ERR {err}"),
            }
            // Walk gives the deepest first file; a share whose root holds only
            // folders has none in `listing.entries`.
            let first_file: Option<(String, String, String, u64)> = match drive
                .share_walk(&share_ref, dir)
                .await
            {
                Ok(files) => files
                    .iter()
                    .min_by_key(|f| f.size)
                    .map(|f| (f.fid.clone(), f.token.clone(), f.name.clone(), f.size)),
                Err(_) => None,
            };
            if let Some((fid, token, name, _size)) = first_file.as_ref() {
                let mut single = share_ref.clone();
                single.fid_token = token.clone();
                println!("  link 「{name}」（最小文件）…");
                match drive.share_link(&single, fid).await {
                    Ok(link) => {
                        println!(
                            "    url={}… size={} cleanup={:?}",
                            link.url.chars().take(90).collect::<String>(),
                            link.size,
                            link.cleanup_fid
                        );
                        match share::probe_link(&link.url, &link.headers).await {
                            Ok(info) => println!("    GET → {info}"),
                            Err(err) => println!("    GET ERR {err}"),
                        }
                        if let Some(fid) = &link.cleanup_fid {
                            drive.share_cleanup(fid).await;
                            println!("    cleanup done");
                        }
                    }
                    Err(err) => println!("    link ERR {err}"),
                }
            }
            if let Some(first) = listing.entries.iter().find(|e| !e.is_dir) {
                let mut single = share_ref.clone();
                single.fid_token = first.token.clone();
                println!("  link 「{}」…", first.name);
                match drive.share_link(&single, &first.fid).await {
                    Ok(link) => {
                        println!(
                            "    url={}… size={} cleanup={:?}",
                            link.url.chars().take(90).collect::<String>(),
                            link.size,
                            link.cleanup_fid
                        );
                        match share::probe_link(&link.url, &link.headers).await {
                            Ok(info) => println!("    GET → {info}"),
                            Err(err) => println!("    GET ERR {err}"),
                        }
                        if let Some(fid) = &link.cleanup_fid {
                            drive.share_cleanup(fid).await;
                            println!("    cleanup done");
                        }
                    }
                    Err(err) => println!("    link ERR {err}"),
                }
            }
        }
        Err(err) => println!("  share_list ERR {err}"),
    }
}

#[tokio::test]
#[ignore]
async fn share_live_quark() {
    probe(
        ProviderId::Quark,
        "https://pan.quark.cn/s/af886c338574?pwd=6M1m",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn share_live_yun139() {
    probe(
        ProviderId::Yun139,
        "https://yun.139.com/shareweb/#/w/i/2yA8EKhoE66zy?pwd=oa5t",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn share_live_pan123() {
    probe(
        ProviderId::Pan123,
        "https://1818505815.share.123pan.cn/123pan/yYphjv-HgZXH?pwd=YfKu",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn share_live_aliyun() {
    probe(ProviderId::Aliyun, "https://www.alipan.com/s/iqj4mzirYtX").await;
}

#[tokio::test]
#[ignore]
async fn share_live_baidu() {
    probe(
        ProviderId::Baidu,
        "https://pan.baidu.com/s/1Gb3CPF1uBTgEaH5BdXwbAQ",
    )
    .await;
}

/// Saves the first file of a share and reports where it landed.
#[tokio::test]
#[ignore]
async fn share_live_save() {
    for (provider, link) in [
        (ProviderId::Quark, "https://pan.quark.cn/s/af886c338574?pwd=6M1m"),
        (ProviderId::Yun139, "https://yun.139.com/shareweb/#/w/i/2yA8EKhoE66zy?pwd=oa5t"),
        (
            ProviderId::Pan123,
            "https://1818505815.share.123pan.cn/123pan/yYphjv-HgZXH?pwd=YfKu",
        ),
        (ProviderId::Aliyun, "https://www.alipan.com/s/iqj4mzirYtX"),
        (
            ProviderId::Baidu,
            "https://pan.baidu.com/s/1Gb3CPF1uBTgEaH5BdXwbAQ",
        ),
    ] {
        println!("\n=== save {} ===", provider.as_str());
        let Some(drive) = drive(provider) else { continue };
        let Ok(target) = share::parse_link(link) else {
            continue;
        };
        let Ok(session) = drive.share_open(&target.id, &target.passcode).await else {
            println!("  open failed");
            continue;
        };
        let share_ref = crate::transfer::ShareRef {
            provider: provider.as_str().to_string(),
            pwd_id: session.pwd_id.clone(),
            stoken: session.stoken.clone(),
            fid_token: String::new(),
            passcode: session.passcode.clone(),
        };
        let dir = match provider {
            ProviderId::Aliyun => "root",
            ProviderId::Baidu => "/",
            _ => "0",
        };
        // Shares whose root holds only folders need a walk to reach a file.
        let Ok(files) = drive.share_walk(&share_ref, dir).await else {
            println!("  walk failed");
            continue;
        };
        let Some(file) = files.iter().min_by_key(|f| f.size) else {
            println!("  no file to save");
            continue;
        };
        let entry = crate::share::ShareEntry {
            fid: file.fid.clone(),
            token: file.token.clone(),
            name: file.name.clone(),
            size: file.size,
            is_dir: false,
            updated_at: 0,
        };
        println!("  saving 「{}」 ({} B)", entry.name, entry.size);
        match drive.share_save(&share_ref, &[entry], None).await {
            Ok(count) => println!("  saved {count}"),
            Err(err) => println!("  save ERR {err}"),
        }
    }
}

/// Prints every drive's nickname + avatar, exactly as the sidebar sees them.
///
/// This is the probe for the `auth_states` path: it goes through `profile()`,
/// which is the call that may hit the network once per process.
#[tokio::test]
#[ignore]
async fn share_live_profiles() {
    for provider in ProviderId::all() {
        let Some(drive) = drive(provider) else { continue };
        let auth = drive.auth_state().await;
        if !auth.authenticated {
            println!("{}: 未登录", provider.as_str());
            continue;
        }
        let profile = drive.profile().await;
        match profile {
            Ok(Some(info)) => println!(
                "{}: nickname={:?} avatar={} (auth_state avatar={})",
                provider.as_str(),
                info.nickname,
                info.avatar
                    .as_ref()
                    .map(|a| format!("{} bytes", a.len()))
                    .unwrap_or_else(|| "none".to_string()),
                auth.avatar.is_some()
            ),
            Ok(None) => println!("{}: profile none", provider.as_str()),
            Err(err) => println!("{}: profile ERR {err}", provider.as_str()),
        }
    }
}

/// Verifies the avatar inline-conversion used by the share header.
#[tokio::test]
#[ignore]
async fn share_live_avatars() {
    let client = share::http().unwrap();
    for url in [
        "https://p1.music.126.net/x.jpg",
        "https://pan.quark.cn/favicon.ico",
    ] {
        match share::fetch_avatar(&client, url).await {
            Some(data) => println!("{} → {} bytes", url, data.len()),
            None => println!("{} → none", url),
        }
    }
}

/// Dumps the share page so the field extraction can be checked.
#[tokio::test]
#[ignore]
async fn share_live_baidu_page() {
    let dir = config_dir();
    let Ok(drive) = crate::providers::baidu::BaiduProvider::load(&dir) else {
        println!("not logged in");
        return;
    };
    let page = drive.debug_share_page("Gb3CPF1uBTgEaH5BdXwbAQ").await;
    match page {
        Ok(text) => {
            println!("len={}\n{}", text.len(), text.chars().take(1200).collect::<String>());
            for key in ["shareid", "share_uk", "linkusername", "share_photo"] {
                println!("  contains {key}: {}", text.contains(key));
            }
            if let Some(pos) = text.find("\"shareid\"") {
                let start = pos.saturating_sub(40);
                println!("  ctx: {:?}", &text[start..(pos + 80).min(text.len())]);
            }
        }
        Err(err) => println!("ERR {err}"),
    }
}

/// Prints the raw `dlFromOutLinkV3` payload to see why no url comes back.
#[tokio::test]
#[ignore]
async fn share_live_yun139_raw() {
    let dir = config_dir();
    let Ok(drive) = crate::providers::yun139::Yun139Provider::load(&dir) else {
        println!("not logged in");
        return;
    };
    let out = drive
        .debug_share_post(
            "/IOutLink/dlFromOutLinkV3",
            &serde_json::json!({
                "dlFromOutLinkReqV3": {
                    "account": drive.debug_account().await,
                    "linkID": "2yA8EKhoE66zy",
                    "passwd": "oa5t",
                    "coIDLst": { "item": ["Fsc6rEmba5WukmSs07jJFH6f7e403WvLr"] },
                }
            }),
        )
        .await;
    println!("{out}");
}

/// Removes the files the save probe dropped into the user's drives.
#[tokio::test]
#[ignore]
async fn share_live_cleanup_saved() {
    for (provider, name) in [
        (ProviderId::Quark, "跨界对决"),
        (ProviderId::Yun139, "AxureRP9"),
        (ProviderId::Pan123, "临时文件"),
        (ProviderId::Aliyun, "临时文件"),
    ] {
        println!("\n=== cleanup {} ===", provider.as_str());
        let Some(drive) = drive(provider) else { continue };
        let root = match provider {
            ProviderId::Yun139 => "/",
            _ => "0",
        };
        let Ok(listing) = drive.list(root).await else {
            println!("  list failed");
            continue;
        };
        let hits: Vec<String> = listing
            .entries
            .iter()
            .filter(|e| e.name == name)
            .map(|e| crate::providers::native_of(&e.fid))
            .collect();
        println!("  found {} × {name}", hits.len());
        if hits.is_empty() {
            continue;
        }
        match drive.delete(&hits).await {
            Ok(()) => println!("  deleted"),
            Err(err) => println!("  delete ERR {err}"),
        }
    }
}

/// engine does, and checks the byte count against the size the drive reported.
#[tokio::test]
#[ignore]
async fn share_live_download() {
    use std::sync::atomic::AtomicU64;
    for (provider, link) in [
        (ProviderId::Quark, "https://pan.quark.cn/s/af886c338574?pwd=6M1m"),
        (ProviderId::Yun139, "https://yun.139.com/shareweb/#/w/i/2yA8EKhoE66zy?pwd=oa5t"),
        (
            ProviderId::Pan123,
            "https://1818505815.share.123pan.cn/123pan/yYphjv-HgZXH?pwd=YfKu",
        ),
        (ProviderId::Aliyun, "https://www.alipan.com/s/iqj4mzirYtX"),
    ] {
        println!("\n=== download {} ===", provider.as_str());
        let Some(drive) = drive(provider) else { continue };
        let Ok(target) = share::parse_link(link) else {
            continue;
        };
        let Ok(session) = drive.share_open(&target.id, &target.passcode).await else {
            println!("  open failed");
            continue;
        };
        let share_ref = crate::transfer::ShareRef {
            provider: provider.as_str().to_string(),
            pwd_id: session.pwd_id.clone(),
            stoken: session.stoken.clone(),
            fid_token: String::new(),
            passcode: session.passcode.clone(),
        };
        let dir = match provider {
            ProviderId::Aliyun => "root",
            _ => "0",
        };
        let Ok(files) = drive.share_walk(&share_ref, dir).await else {
            println!("  walk failed");
            continue;
        };
        // Keep the probe quick: a few hundred MB over a shared link is minutes.
        let Some(file) = files
            .iter()
            .filter(|f| f.size > 0 && f.size < 150 * 1024 * 1024)
            .min_by_key(|f| f.size)
        else {
            println!("  no small file (<150 MB)");
            continue;
        };
        let mut single = share_ref.clone();
        single.fid_token = file.token.clone();
        let Ok(link_info) = drive.share_link(&single, &file.fid).await else {
            println!("  link failed");
            continue;
        };
        let part = std::env::temp_dir().join(format!("shareprobe-{}", provider.as_str()));
        let _ = std::fs::remove_file(&part);
        let token = tokio_util::sync::CancellationToken::new();
        let done = Arc::new(AtomicU64::new(0));
        let started = std::time::Instant::now();
        match share::stream_to_file(&link_info.url, &link_info.headers, &part, done.clone(), &token)
            .await
        {
            Ok(written) => {
                let expected = if link_info.size > 0 {
                    link_info.size
                } else {
                    file.size
                };
                let on_disk = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                println!(
                    "  {} → {} bytes on disk (expected {}) in {:?}",
                    file.name,
                    written,
                    expected,
                    started.elapsed()
                );
                println!(
                    "  {}",
                    if on_disk == written && (expected == 0 || written == expected) {
                        "OK"
                    } else {
                        "MISMATCH"
                    }
                );
            }
            Err(err) => println!("  stream ERR {err}"),
        }
        let _ = std::fs::remove_file(&part);
        if let Some(fid) = &link_info.cleanup_fid {
            drive.share_cleanup(fid).await;
        }
    }
}

/// accept folder entries in their transfer call.
#[tokio::test]
#[ignore]
async fn share_live_save_folder() {
    for (provider, link) in [
        (ProviderId::Quark, "https://pan.quark.cn/s/af886c338574?pwd=6M1m"),
        (ProviderId::Yun139, "https://yun.139.com/shareweb/#/w/i/2yA8EKhoE66zy?pwd=oa5t"),
        (
            ProviderId::Pan123,
            "https://1818505815.share.123pan.cn/123pan/yYphjv-HgZXH?pwd=YfKu",
        ),
        (ProviderId::Aliyun, "https://www.alipan.com/s/iqj4mzirYtX"),
    ] {
        println!("\n=== save folder {} ===", provider.as_str());
        let Some(drive) = drive(provider) else { continue };
        let Ok(target) = share::parse_link(link) else { continue };
        let Ok(session) = drive.share_open(&target.id, &target.passcode).await else {
            println!("  open failed");
            continue;
        };
        let share_ref = crate::transfer::ShareRef {
            provider: provider.as_str().to_string(),
            pwd_id: session.pwd_id.clone(),
            stoken: session.stoken.clone(),
            fid_token: String::new(),
            passcode: session.passcode.clone(),
        };
        let dir = match provider {
            ProviderId::Aliyun => "root",
            _ => "0",
        };
        let Ok(listing) = drive.share_list(&share_ref, dir).await else {
            println!("  list failed");
            continue;
        };
        let folders: Vec<_> = listing.entries.iter().filter(|e| e.is_dir).cloned().collect();
        if folders.is_empty() {
            println!("  no folder at root");
            continue;
        }
        let folder = folders[0].clone();
        println!("  folder 「{}」", folder.name);
        match drive.share_save(&share_ref, std::slice::from_ref(&folder), None).await {
            Ok(n) => println!("  saved {n}"),
            Err(err) => println!("  save ERR {err}"),
        }
    }
}

/// Lists the root of a drive — used to check what the save probes left behind.
#[tokio::test]
#[ignore]
async fn share_live_root_listing() {
    for provider in [ProviderId::Aliyun, ProviderId::Pan123, ProviderId::Quark, ProviderId::Yun139] {
        let Some(drive) = drive(provider) else { continue };
        let root = match provider {
            ProviderId::Yun139 => "/",
            _ => "0",
        };
        match drive.list(root).await {
            Ok(listing) => {
                println!("\n=== {} ({} entries) ===", provider.as_str(), listing.entries.len());
                for e in &listing.entries {
                    println!("  {} {} ({} B)", if e.is_dir { "DIR " } else { "FILE" }, e.name, e.size);
                }
            }
            Err(err) => println!("\n=== {} ===\n  list ERR {err}", provider.as_str()),
        }
    }
}

/// Finds a video in the signed-in 阿里 drive and downloads the first chunk.
#[tokio::test]
#[ignore]
async fn share_live_aliyun_own_video() {
    use futures_util::StreamExt;
    let dir = config_dir();
    let Ok(drive) = crate::providers::aliyun::AliyunProvider::load(&dir) else {
        return;
    };
    let mut queue = std::collections::VecDeque::new();
    queue.push_back("0".to_string());
    let mut seen = 0;
    while let Some(parent) = queue.pop_front() {
        seen += 1;
        if seen > 40 {
            break;
        }
        let Ok(listing) = drive.list(&parent).await else { continue };
        for entry in &listing.entries {
            if entry.is_dir {
                queue.push_back(crate::providers::native_of(&entry.fid));
                continue;
            }
            if !entry.name.to_lowercase().ends_with(".mkv") {
                continue;
            }
            let native = crate::providers::native_of(&entry.fid);
            println!("try {} {} bytes", entry.name, entry.size);
            match drive.open_download(&native, 0).await {
                Ok(mut handle) => {
                    let mut got = 0u64;
                    let mut err = None;
                    while let Some(chunk) = handle.stream.next().await {
                        match chunk {
                            Ok(bytes) => {
                                got += bytes.len() as u64;
                                if got > 2 << 20 {
                                    break;
                                }
                            }
                            Err(e) => {
                                err = Some(e.to_string());
                                break;
                            }
                        }
                    }
                    println!("  ok read={got} size={} err={err:?}", handle.size);
                    return;
                }
                Err(e) => println!("  ERR {e}"),
            }
        }
    }
    println!("no mkv found");
}

/// Restores specific 123 ids from the recycle bin (recovery helper).
#[tokio::test]
#[ignore]
async fn share_live_pan123_restore_ids() {
    let dir = config_dir();
    let Ok(drive) = crate::providers::pan123::Pan123Provider::load(&dir) else {
        return;
    };
    let ids: Vec<i64> = std::env::var("PAN123_RESTORE")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if ids.is_empty() {
        println!("set PAN123_RESTORE=1,2,3");
        return;
    }
    let list: Vec<serde_json::Value> = ids.iter().map(|id| serde_json::json!({ "FileId": id })).collect();
    let body = serde_json::json!({ "driveId": 0, "fileTrashInfoList": list, "operation": false });
    match drive
        .debug_post("https://www.123pan.cn/a/api/file/trash", body)
        .await
    {
        Ok(v) => println!("restore → {v}"),
        Err(e) => println!("restore ERR {e}"),
    }
}

/// Prints both the 123 root and 临时文件 so leftovers are visible.
#[tokio::test]
#[ignore]
async fn share_live_pan123_dump() {
    let dir = config_dir();
    let Ok(drive) = crate::providers::pan123::Pan123Provider::load(&dir) else {
        return;
    };
    for (label, fid) in [("root", "0"), ("临时文件", "17116059")] {
        match drive.list(fid).await {
            Ok(l) => {
                println!("{label} ({} entries):", l.entries.len());
                for e in &l.entries {
                    println!(
                        "  {} {} ({} B) id={}",
                        if e.is_dir { "DIR " } else { "FILE" },
                        e.name,
                        e.size,
                        crate::providers::native_of(&e.fid)
                    );
                }
            }
            Err(e) => println!("{label} ERR {e}"),
        }
    }
}

/// Lists the 123 recycle bin so lost files can be found and restored.
#[tokio::test]
#[ignore]
async fn share_live_pan123_trash_list() {
    let dir = config_dir();
    let Ok(drive) = crate::providers::pan123::Pan123Provider::load(&dir) else {
        return;
    };
    let url = "https://www.123pan.cn/api/file/list/new?driveId=0&limit=100&next=0\
               &orderBy=file_id&orderDirection=desc&parentFileId=0&trashed=true\
               &SearchData=&Page=1&OnlyLookAbnormalFile=0";
    match drive.debug_get(url).await {
        Ok(v) => {
            let rows = v.pointer("/data/InfoList").and_then(|x| x.as_array()).cloned().unwrap_or_default();
            println!("trash has {} rows", rows.len());
            for row in rows {
                println!(
                    "  {} {} ({} B) id={} parent={}",
                    row.get("Type").map(|t| t.to_string()).unwrap_or_default(),
                    row.get("FileName").and_then(|x| x.as_str()).unwrap_or(""),
                    row.get("Size").map(|s| s.to_string()).unwrap_or_default(),
                    row.get("FileId").map(|s| s.to_string()).unwrap_or_default(),
                    row.get("ParentFileId").map(|s| s.to_string()).unwrap_or_default(),
                );
            }
        }
        Err(e) => println!("trash ERR {e}"),
    }
}

/// Deletes one 123 file by exact native id.
#[tokio::test]
#[ignore]
async fn share_live_pan123_delete_id() {
    let dir = config_dir();
    let Ok(drive) = crate::providers::pan123::Pan123Provider::load(&dir) else {
        return;
    };
    let ids: Vec<String> = std::env::var("PAN123_DELETE")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if ids.is_empty() {
        println!("set PAN123_DELETE=<native id>");
        return;
    }
    for id in ids {
        match drive.delete(std::slice::from_ref(&id)).await {
            Ok(()) => println!("deleted {id}"),
            Err(e) => println!("delete {id} ERR {e}"),
        }
    }
}

/// Verifies the real 123 copy path end to end, then deletes only what it made.
#[tokio::test]
#[ignore]
async fn share_live_pan123_copy_real() {
    let dir = config_dir();
    let Ok(drive) = crate::providers::pan123::Pan123Provider::load(&dir) else {
        return;
    };
    let drive: Arc<dyn DriveProvider> = Arc::new(drive);
    let target = "3383124"; // Umi
    let before = drive.list(target).await.map(|l| l.entries).unwrap_or_default();
    let before_names: Vec<String> = before.iter().map(|e| e.name.clone()).collect();
    println!("Umi before: {} entries", before.len());
    match drive
        .transfer_entries(&["21128189".to_string()], target, true)
        .await
    {
        Ok(n) => println!("copy → {n}"),
        Err(e) => {
            println!("copy ERR {e}");
            return;
        }
    }
    let after = drive.list(target).await.map(|l| l.entries).unwrap_or_default();
    let fresh: Vec<_> = after
        .iter()
        .filter(|e| !before_names.contains(&e.name))
        .cloned()
        .collect();
    println!("Umi after: {} entries, new: {}", after.len(), fresh.len());
    for e in &fresh {
        println!(
            "  NEW {} {} ({} B)",
            if e.is_dir { "DIR " } else { "FILE " },
            e.name,
            e.size
        );
        match drive.delete(&[crate::providers::native_of(&e.fid)]).await {
            Ok(()) => println!("  cleaned {}", e.name),
            Err(err) => println!("  cleanup ERR {err}"),
        }
    }
    let parent = drive.list("17116059").await.map(|l| l.entries).unwrap_or_default();
    println!("临时文件 still has {} entries", parent.len());
}

/// Prints a folder by native id so leftovers are visible.
#[tokio::test]
#[ignore]
async fn share_live_pan123_ls() {
    let dir = config_dir();
    let Ok(drive) = crate::providers::pan123::Pan123Provider::load(&dir) else {
        return;
    };
    let fid = std::env::var("PAN123_LS").unwrap_or_else(|_| "3383124".into());
    match drive.list(&fid).await {
        Ok(l) => {
            println!("{} entries:", l.entries.len());
            for e in &l.entries {
                if e.name.contains("奥运会") {
                    println!("  {} {} ({})", if e.is_dir { "DIR " } else { "FILE" }, e.name, crate::providers::native_of(&e.fid));
                }
            }
        }
        Err(e) => println!("ERR {e}"),
    }
}

/// Verifies the real 123 move path: root → 临时文件 → back.
#[tokio::test]
#[ignore]
async fn share_live_pan123_move_verify() {
    let dir = config_dir();
    let Ok(drive) = crate::providers::pan123::Pan123Provider::load(&dir) else {
        return;
    };
    let drive: Arc<dyn DriveProvider> = Arc::new(drive);
    let src_id = "49472830"; // 奥运会项目框架.7z in the root
    let folder = "17116059"; // 临时文件
    println!(
        "root before: {} entries, 临时文件 before: {}",
        drive.list("0").await.map(|l| l.entries.len()).unwrap_or(0),
        drive.list(folder).await.map(|l| l.entries.len()).unwrap_or(0)
    );
    match drive.transfer_entries(&[src_id.to_string()], folder, false).await {
        Ok(n) => println!("move in → {n}"),
        Err(e) => println!("move in ERR {e}"),
    }
    println!(
        "root after: {} entries, 临时文件 after: {}",
        drive.list("0").await.map(|l| l.entries.len()).unwrap_or(0),
        drive.list(folder).await.map(|l| l.entries.len()).unwrap_or(0)
    );
    match drive.transfer_entries(&[src_id.to_string()], "0", false).await {
        Ok(n) => println!("move back → {n}"),
        Err(e) => println!("move back ERR {e}"),
    }
    let root = drive.list("0").await.map(|l| l.entries).unwrap_or_default();
    println!("root final: {} entries", root.len());
    for e in &root {
        println!("  {} {}", if e.is_dir { "DIR " } else { "FILE " }, e.name);
    }
    println!(
        "临时文件 final: {} entries",
        drive.list(folder).await.map(|l| l.entries.len()).unwrap_or(0)
    );
}

/// Renames one 123 file by native id (repairs names the probes changed).
#[tokio::test]
#[ignore]
async fn share_live_pan123_rename_id() {
    let dir = config_dir();
    let Ok(drive) = crate::providers::pan123::Pan123Provider::load(&dir) else {
        return;
    };
    let spec = std::env::var("PAN123_RENAME").unwrap_or_default();
    let Some((id, name)) = spec.split_once('=') else {
        println!("set PAN123_RENAME=<id>=<new name>");
        return;
    };
    match drive.rename(id.trim(), name.trim()).await {
        Ok(()) => println!("renamed {id} → {name}"),
        Err(e) => println!("rename ERR {e}"),
    }
}

/// Server-side search on every drive. Pass `SEARCH_TERM=<word>` to change the
/// keyword; defaults to something that exists on most accounts.
#[tokio::test]
#[ignore]
async fn live_search() {
    let term = std::env::var("SEARCH_TERM").unwrap_or_else(|_| "a".to_string());
    for provider in ProviderId::all() {
        let Some(drive) = drive(provider) else { continue };
        let root = match provider {
            ProviderId::Aliyun => "root",
            ProviderId::Baidu => "/",
            _ => "0",
        };
        let started = std::time::Instant::now();
        match drive.search(root, &term, 20).await {
            Ok(hits) => {
                println!(
                    "=== {} ({} hits, {} ms) ===",
                    provider.as_str(),
                    hits.len(),
                    started.elapsed().as_millis()
                );
                for hit in hits.iter().take(8) {
                    let crumb = hit
                        .path
                        .iter()
                        .map(|n| n.name.as_str())
                        .collect::<Vec<_>>()
                        .join("/");
                    println!(
                        "  {} {}  [{}]  via {}",
                        if hit.entry.is_dir { "DIR " } else { "FILE" },
                        hit.entry.name,
                        hit.entry.size,
                        crumb
                    );
                }
            }
            Err(err) => println!("=== {} ERR {err} ===", provider.as_str()),
        }
    }
}

/// Recycle bins on every drive: list, then restore the newest row back where it
/// came from. Only touches what it created — nothing is deleted here.
#[tokio::test]
#[ignore]
async fn live_recycle() {
    for provider in ProviderId::all() {
        let Some(drive) = drive(provider) else { continue };
        match drive.recycle_list(0, 10).await {
            Ok(rows) => {
                println!("=== {} ({} rows) ===", provider.as_str(), rows.len());
                for row in rows.iter().take(6) {
                    println!(
                        "  {} {}  {} B  deleted={}  token={}",
                        if row.is_dir { "DIR " } else { "FILE" },
                        row.name,
                        row.size,
                        row.deleted_at,
                        row.token
                    );
                }
            }
            Err(err) => println!("=== {} LIST ERR {err} ===", provider.as_str()),
        }
    }
}

/// Full recycle round-trip on the drives that support it: create a folder,
/// delete it (into the bin), find it in the bin, restore it, then delete and
/// purge it. Leaves nothing behind on success.
#[tokio::test]
#[ignore]
async fn live_recycle_roundtrip() {
    for provider in ProviderId::all() {
        if !provider_has_recycle(provider) {
            continue;
        }
        let Some(drive) = drive(provider) else { continue };
        let root = match provider {
            ProviderId::Aliyun => "root",
            _ => "0",
        };
        let name = format!("yunyu-recycle-probe-{}", std::process::id());
        println!("=== {} ===", provider.as_str());
        if let Err(err) = drive.make_dir(root, &name).await {
            println!("  make_dir ERR {err}");
            continue;
        }
        // Quark's listing lags a beat behind `create_folder`; retry before
        // declaring the folder missing.
        let mut created = None;
        for _ in 0..6 {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            match drive.list(root).await {
                Ok(list) => {
                    if let Some(entry) = list.entries.into_iter().find(|e| e.name == name) {
                        created = Some(entry);
                        break;
                    }
                }
                Err(err) => {
                    println!("  list ERR {err}");
                    break;
                }
            }
        }
        let Some(created) = created else {
            println!("  created folder not found in listing");
            continue;
        };
        println!("  created {}", created.fid);
        // `delete` takes native ids: the providers strip the qualifier
        // themselves only for *incoming* requests, not for these.
        let native = crate::providers::native_of(&created.fid);
        if let Err(err) = drive.delete(&[native]).await {
            println!("  delete ERR {err}");
            continue;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let bin = match drive.recycle_list(0, 50).await {
            Ok(rows) => rows,
            Err(err) => {
                println!("  recycle_list ERR {err}");
                continue;
            }
        };
        let row = bin.into_iter().find(|r| r.name == name);
        let Some(row) = row else {
            println!("  deleted folder not in bin");
            continue;
        };
        println!("  in bin: {} ({} B)", row.fid, row.size);
        match drive.recycle_restore(std::slice::from_ref(&row)).await {
            Ok(n) => println!("  restored {n}"),
            Err(err) => {
                println!("  restore ERR {err}");
                continue;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let back = drive
            .list(root)
            .await
            .map(|l| l.entries.iter().any(|e| e.name == name))
            .unwrap_or(false);
        println!("  back in listing: {back}");
        if !back {
            continue;
        }
        // Clean up for good: delete again, then purge from the bin.
        if let Some(entry) = drive
            .list(root)
            .await
            .ok()
            .and_then(|l| l.entries.into_iter().find(|e| e.name == name))
        {
            let _ = drive.delete(&[crate::providers::native_of(&entry.fid)]).await;
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            if let Ok(bin) = drive.recycle_list(0, 50).await {
                let rows: Vec<_> = bin.into_iter().filter(|r| r.name == name).collect();
                if !rows.is_empty() {
                    match drive.recycle_purge(&rows).await {
                        Ok(n) => println!("  purged {n}"),
                        Err(err) => println!("  purge ERR {err}"),
                    }
                }
            }
        }
    }
}

fn provider_has_recycle(provider: ProviderId) -> bool {
    matches!(
        provider,
        ProviderId::Quark | ProviderId::Pan123 | ProviderId::Aliyun
    )
}

/// Removes leftovers named `yunyu-recycle-probe-*` from the root of every
/// recycle-capable drive (they end up in the bin, which is as far as 阿里/123
/// allow us to go).
#[tokio::test]
#[ignore]
async fn live_recycle_cleanup() {
    for provider in ProviderId::all() {
        if !provider_has_recycle(provider) {
            continue;
        }
        let Some(drive) = drive(provider) else { continue };
        let root = match provider {
            ProviderId::Aliyun => "root",
            _ => "0",
        };
        let Ok(list) = drive.list(root).await else { continue };
        let fids: Vec<String> = list
            .entries
            .iter()
            .filter(|e| e.name.starts_with("yunyu-recycle-probe"))
            .map(|e| crate::providers::native_of(&e.fid))
            .collect();
        if fids.is_empty() {
            continue;
        }
        println!("{}: removing {} leftover(s)", provider.as_str(), fids.len());
        if let Err(err) = drive.delete(&fids).await {
            println!("  ERR {err}");
        }
    }
}

/// Prints the root listing of one drive (`PROBE_PROVIDER=quark`).
#[tokio::test]
#[ignore]
async fn live_root_listing_one() {
    let name = std::env::var("PROBE_PROVIDER").unwrap_or_else(|_| "quark".to_string());
    let provider = ProviderId::parse(&name).expect("unknown provider");
    let Some(drive) = drive(provider) else { return };
    let root = match provider {
        ProviderId::Aliyun => "root",
        ProviderId::Baidu => "/",
        _ => "0",
    };
    match drive.list(root).await {
        Ok(list) => {
            println!("=== {} ({} entries, total {}) ===", name, list.entries.len(), list.total);
            for entry in list.entries.iter().take(20) {
                println!("  {} {}", if entry.is_dir { "DIR " } else { "FILE" }, entry.name);
            }
        }
        Err(err) => println!("ERR {err}"),
    }
}

/// Purges `yunyu-recycle-probe-*` from the bin where the drive allows it
/// (Quark). 阿里/123 have no per-item purge, so their empty probe folders stay
/// in the bin until it expires on its own.
#[tokio::test]
#[ignore]
async fn live_recycle_purge_probes() {
    for provider in ProviderId::all() {
        if !provider_has_recycle(provider) {
            continue;
        }
        let Some(drive) = drive(provider) else { continue };
        let Ok(bin) = drive.recycle_list(0, 100).await else { continue };
        let rows: Vec<_> = bin
            .into_iter()
            .filter(|r| r.name.starts_with("yunyu-recycle-probe"))
            .collect();
        if rows.is_empty() {
            continue;
        }
        println!("{}: {} probe row(s) in bin", provider.as_str(), rows.len());
        match drive.recycle_purge(&rows).await {
            Ok(n) => println!("  purged {n}"),
            Err(err) => println!("  purge ERR {err}"),
        }
    }
}

/// Empties 阿里云盘's bin — only safe while it contains nothing but probes.
#[tokio::test]
#[ignore]
async fn live_recycle_clear_aliyun() {
    let provider = ProviderId::Aliyun;
    let Some(drive) = drive(provider) else { return };
    let Ok(bin) = drive.recycle_list(0, 100).await else { return };
    let foreign: Vec<_> = bin.iter().filter(|r| !r.name.starts_with("yunyu-recycle-probe")).collect();
    if !foreign.is_empty() {
        println!("refusing to clear: {} non-probe row(s) present", foreign.len());
        return;
    }
    println!("bin holds {} probe row(s); clearing", bin.len());
    match drive.recycle_clear().await {
        Ok(()) => println!("cleared"),
        Err(err) => println!("clear ERR {err}"),
    }
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    if let Ok(after) = drive.recycle_list(0, 100).await {
        println!("bin now holds {} row(s)", after.len());
    }
}

/// Prints the first few raw rows of one drive's listing, thumbnails included.
#[tokio::test]
#[ignore]
async fn live_thumbnail_fields() {
    for provider in ProviderId::all() {
        let Some(drive) = drive(provider) else { continue };
        let root = match provider {
            ProviderId::Aliyun => "root",
            ProviderId::Baidu => "/",
            _ => "0",
        };
        let Ok(list) = drive.list(root).await else { continue };
        println!("=== {} ({} entries) ===", provider.as_str(), list.entries.len());
        for entry in list.entries.iter().take(4) {
            println!("  {} thumb={:?}", entry.name, entry.thumb);
        }
    }
}

/// Walks into `PROBE_DIR` on one drive (`PROBE_PROVIDER`, `PROBE_DIR`) and
/// prints the thumbnail field of the first rows.
#[tokio::test]
#[ignore]
async fn live_thumbnails_in_dir() {
    let name = std::env::var("PROBE_PROVIDER").unwrap_or_else(|_| "aliyun".to_string());
    let dir_name = std::env::var("PROBE_DIR").unwrap_or_else(|_| "头像".to_string());
    let provider = ProviderId::parse(&name).expect("unknown provider");
    let Some(drive) = drive(provider) else { return };
    let root = match provider {
        ProviderId::Aliyun => "root",
        ProviderId::Baidu => "/",
        _ => "0",
    };
    // `PROBE_DIR` may be a slash-separated path; each segment is a folder name.
    let mut cursor = root.to_string();
    for segment in dir_name.split('/') {
        let list = match drive.list(&cursor).await {
            Ok(list) => list,
            Err(err) => {
                println!("list {cursor} ERR {err}");
                return;
            }
        };
        let Some(folder) = list.entries.iter().find(|e| e.name == segment) else {
            println!("no folder {segment} under {cursor}");
            return;
        };
        // The trait takes native ids; the listing hands back qualified ones.
        cursor = crate::providers::native_of(&folder.fid);
    }
    match drive.list(&cursor).await {
        Ok(inner) => {
            println!("=== {} / {} ({} entries) ===", name, dir_name, inner.entries.len());
            for entry in inner.entries.iter().take(5) {
                println!("  {} -> {:?}", entry.name, entry.thumb);
            }
        }
        Err(err) => println!("ERR {err}"),
    }
}

/// Fetches the preview urls a listing hands out and reports whether they are
/// actually downloadable (they are pre-signed and short-lived).
#[tokio::test]
#[ignore]
async fn live_thumbnail_download() {
    let name = std::env::var("PROBE_PROVIDER").unwrap_or_else(|_| "aliyun".to_string());
    let dir_name = std::env::var("PROBE_DIR").unwrap_or_else(|_| "Umi/头像".to_string());
    let provider = ProviderId::parse(&name).expect("unknown provider");
    let Some(drive) = drive(provider) else { return };
    let root = match provider {
        ProviderId::Aliyun => "root",
        ProviderId::Baidu => "/",
        _ => "0",
    };
    let mut cursor = root.to_string();
    for segment in dir_name.split('/') {
        let Ok(list) = drive.list(&cursor).await else { return };
        let Some(folder) = list.entries.iter().find(|e| e.name == segment) else {
            println!("no folder {segment}");
            return;
        };
        cursor = crate::providers::native_of(&folder.fid);
    }
    let Ok(list) = drive.list(&cursor).await else { return };
    let Some(entry) = list.entries.iter().find(|e| e.thumb.is_some()) else {
        println!("no thumbnails under {dir_name}");
        return;
    };
    let url = entry.thumb.clone().unwrap_or_default();
    println!("{} -> {} chars", entry.name, url.len());
    std::fs::write("/tmp/thumb_url.txt", &url).ok();
    let client = crate::share::http().expect("client");
    let started = std::time::Instant::now();
    match crate::share::fetch_image(&client, &url, None).await {
        Some(data) => println!(
            "inlined {} bytes in {:?} (prefix {})",
            data.len(),
            started.elapsed(),
            &data[..data.len().min(32)]
        ),
        None => println!("fetch FAILED in {:?}", started.elapsed()),
    }
}

/// Prints the thumbnail styles 139 reports for one folder.
#[tokio::test]
#[ignore]
async fn live_yun139_thumb_styles() {
    let provider = ProviderId::Yun139;
    let Some(drive) = drive(provider) else { return };
    let Ok(root) = drive.list("0").await else { return };
    let Some(folder) = root.entries.iter().find(|e| e.name == "手机图片") else {
        println!("no 手机图片");
        return;
    };
    let Ok(inner) = drive.list(&crate::providers::native_of(&folder.fid)).await else { return };
    let Some(entry) = inner.entries.iter().find(|e| e.thumb.is_some()) else {
        println!("no thumbs");
        return;
    };
    println!("{} thumb len {}", entry.name, entry.thumb.as_deref().unwrap_or("").len());
}

/// End-to-end cross-drive copy: make a tiny file on the source drive, copy it
/// to the target, compare the bytes and clean both sides up.
///
/// ```text
/// CROSS_FROM=quark CROSS_TO=aliyun cargo test --lib live_cross -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn live_cross() {
    let from_id = std::env::var("CROSS_FROM")
        .ok()
        .and_then(|v| ProviderId::parse(&v))
        .unwrap_or(ProviderId::Quark);
    let to_id = std::env::var("CROSS_TO")
        .ok()
        .and_then(|v| ProviderId::parse(&v))
        .unwrap_or(ProviderId::Aliyun);
    println!("=== cross {} -> {} ===", from_id.as_str(), to_id.as_str());
    let (Some(from), Some(to)) = (drive(from_id), drive(to_id)) else {
        return;
    };
    let root = |p: ProviderId| match p {
        ProviderId::Aliyun => "root",
        _ => "0",
    };

    // 1. a source file with recognisable bytes
    let body = b"yunyu cross-drive probe\n".repeat(64);
    let temp = std::env::temp_dir().join(format!("yunyu-cross-{}.txt", std::process::id()));
    tokio::fs::write(&temp, &body).await.unwrap();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("yunyu-cross-{stamp}.txt");
    let sink: Arc<dyn crate::providers::UploadSink> =
        Arc::new(crate::providers::CounterSink::new(Arc::new(std::sync::atomic::AtomicU64::new(0))));
    let token = tokio_util::sync::CancellationToken::new();
    let uploaded = match from.upload(&temp, root(from_id), &name, sink, token).await {
        Ok(fid) => fid,
        Err(err) => {
            println!("  upload to source failed: {err}");
            let _ = tokio::fs::remove_file(&temp).await;
            return;
        }
    };
    println!("  source file: {uploaded:?}");

    // 2. the cross job itself
    let job = crate::transfer::CrossJob {
        files: vec![crate::transfer::CrossFile {
            fid: uploaded.clone().unwrap_or_default(),
            name: name.clone(),
            size: body.len() as u64,
            to_parent: root(to_id).to_string(),
        }],
        to_pdir_fid: root(to_id).to_string(),
        copy: true,
        name: name.clone(),
    };
    let temp_root = std::env::temp_dir().join(format!("yunyu-cross-{}", std::process::id()));
    if let Err(err) =
        crate::transfer::run_cross_direct(from.clone(), to.clone(), temp_root, job).await
    {
        println!("  cross failed: {err}");
        let _ = tokio::fs::remove_file(&temp).await;
        panic!("cross-drive transfer failed");
    }

    // 3. read the target back
    let listing = to.list(root(to_id)).await.expect("list target");
    if listing.entries.iter().all(|e| e.name != name) {
        println!(
            "  target root has: {:?}",
            listing
                .entries
                .iter()
                .filter(|e| e.name.starts_with("yunyu-cross"))
                .map(|e| (e.name.clone(), e.size))
                .collect::<Vec<_>>()
        );
    }
    let landed = listing.entries.iter().find(|e| e.name == name);
    println!("  target copy: {:?}", landed.as_ref().map(|e| e.size));
    assert!(landed.is_some(), "copy missing on the target drive");
    assert_eq!(landed.unwrap().size, body.len() as u64);

    // 4. clean up both sides (the source is a copy job, so it is still there)
    let _ = to
        .delete(&[crate::providers::native_of(&landed.unwrap().fid)])
        .await;
    if let Some(fid) = uploaded {
        let _ = from.delete(&[fid]).await;
    }
    let _ = tokio::fs::remove_file(&temp).await;
}

/// Prints the md5 the drive publishes for a freshly uploaded file next to the
/// md5 of the bytes actually served, to explain a校验失败.
#[tokio::test]
#[ignore]
async fn live_md5_probe() {
    let provider = std::env::var("PROBE_PROVIDER")
        .ok()
        .and_then(|v| ProviderId::parse(&v))
        .unwrap_or(ProviderId::Quark);
    let Some(drive) = drive(provider) else { return };
    let root = match provider {
        ProviderId::Aliyun => "root",
        _ => "0",
    };
    let body = b"yunyu md5 probe\n".repeat(64);
    let temp = std::env::temp_dir().join(format!("yunyu-md5-{}.txt", std::process::id()));
    tokio::fs::write(&temp, &body).await.unwrap();
    let name = format!("yunyu-md5-{}.txt", std::process::id());
    let sink: Arc<dyn crate::providers::UploadSink> = Arc::new(
        crate::providers::CounterSink::new(Arc::new(std::sync::atomic::AtomicU64::new(0))),
    );
    let token = tokio_util::sync::CancellationToken::new();
    let Ok(Some(fid)) = drive.upload(&temp, root, &name, sink, token).await else {
        println!("upload failed");
        return;
    };
    let native = crate::providers::native_of(&fid);
    use futures_util::StreamExt;
    let handle = drive.open_download(&native, 0).await.expect("open");
    println!("published md5: {:?} size {}", handle.md5, handle.size);
    let mut buf = Vec::new();
    let mut stream = handle.stream;
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.unwrap());
    }
    use md5::Digest as _;
    println!("served {} bytes md5 {:x}", buf.len(), md5::Md5::digest(&buf));
    println!("local  {} bytes md5 {:x}", body.len(), md5::Md5::digest(&body));
    let _ = drive.delete(&[native]).await;
    let _ = tokio::fs::remove_file(&temp).await;
}

/// Downloads the first file in the root and compares the published md5 with the
/// bytes actually served — tells us whether md5 verification is safe per drive.
#[tokio::test]
#[ignore]
async fn live_md5_real() {
    let provider = std::env::var("PROBE_PROVIDER")
        .ok()
        .and_then(|v| ProviderId::parse(&v))
        .unwrap_or(ProviderId::Quark);
    let Some(drive) = drive(provider) else { return };
    let root = match provider {
        ProviderId::Aliyun => "root",
        _ => "0",
    };
    let Ok(list) = drive.list(root).await else { return };
    use futures_util::StreamExt;
    use md5::Digest as _;
    for entry in list.entries.iter().filter(|e| !e.is_dir && e.size > 0).take(4) {
        println!("file {} size {}", entry.name, entry.size);
        let native = crate::providers::native_of(&entry.fid);
        let Ok(handle) = drive.open_download(&native, 0).await else { continue };
        let published = handle.md5.clone();
        let mut buf = Vec::new();
        let mut stream = handle.stream;
        while let Some(Ok(chunk)) = stream.next().await {
            buf.extend_from_slice(&chunk);
            if buf.len() > 8_000_000 {
                break;
            }
        }
        println!(
            "  published {:?} served {} bytes md5 {:x}",
            published,
            buf.len(),
            md5::Md5::digest(&buf)
        );
    }
}

/// Removes the files and folders the probes left behind. Only names with our own
/// `yunyu-` prefix are touched, and always by exact id.
#[tokio::test]
#[ignore]
async fn live_probe_cleanup() {
    for provider in ProviderId::all() {
        let Some(drive) = drive(provider) else { continue };
        let root = match provider {
            ProviderId::Aliyun => "root",
            _ => "0",
        };
        let Ok(list) = drive.list(root).await else { continue };
        let ids: Vec<String> = list
            .entries
            .iter()
            .filter(|e| {
                e.name.starts_with("yunyu-cross-")
                    || e.name.starts_with("yunyu-recycle-probe-")
                    || e.name.starts_with("yunyu-md5-")
            })
            .map(|e| crate::providers::native_of(&e.fid))
            .collect();
        if ids.is_empty() {
            println!("{}: clean", provider.as_str());
            continue;
        }
        match drive.delete(&ids).await {
            Ok(()) => println!("{}: removed {} probe item(s)", provider.as_str(), ids.len()),
            Err(err) => println!("{}: cleanup failed: {err}", provider.as_str()),
        }
    }
}

/// Downloads a real file with a speed cap and reports the achieved rate.
///
/// ```text
/// CAP_KBPS=512 PROBE_PROVIDER=quark cargo test --lib live_throttle -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn live_throttle() {
    let provider = std::env::var("PROBE_PROVIDER")
        .ok()
        .and_then(|v| ProviderId::parse(&v))
        .unwrap_or(ProviderId::Quark);
    let cap: u64 = std::env::var("CAP_KBPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512);
    let Some(drive) = drive(provider) else { return };
    let root = match provider {
        ProviderId::Aliyun => "root",
        _ => "0",
    };
    let min_size: u64 = std::env::var("MIN_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600_000);
    let Ok(list) = drive.list(root).await else { return };
    let Some(entry) = list
        .entries
        .iter()
        .filter(|e| !e.is_dir && e.size > min_size)
        .min_by_key(|e| e.size)
    else {
        println!("no file over 600 KB in the root");
        return;
    };
    let dir = std::env::temp_dir().join(format!("yunyu-throttle-{}", std::process::id()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let part = dir.join("probe.part");
    let final_path = dir.join("probe.bin");
    crate::transfer::set_speed_limit(cap);
    let started = std::time::Instant::now();
    let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let token = tokio_util::sync::CancellationToken::new();
    let result = crate::transfer::run_provider_download_for_test(
        &drive,
        &crate::providers::native_of(&entry.fid),
        &part,
        &final_path,
        entry.size,
        done,
        token,
    )
    .await;
    let elapsed = started.elapsed();
    crate::transfer::set_speed_limit(0);
    match result {
        Ok(()) => {
            let rate = entry.size as f64 / elapsed.as_secs_f64() / 1024.0;
            println!(
                "{} ({} B) in {:.1}s => {:.0} KiB/s (cap {} KiB/s)",
                entry.name,
                entry.size,
                elapsed.as_secs_f64(),
                rate,
                cap
            );
            assert!(rate <= cap as f64 * 1.35, "cap not enforced: {rate:.0}");
        }
        Err(err) => println!("download failed: {err}"),
    }
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

/// Uploads a real file with a speed cap and reports the achieved rate.
///
/// ```text
/// UP_CAP_KBPS=256 UP_MB=8 cargo test --lib live_upload_throttle -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn live_upload_throttle() {
    let provider = std::env::var("PROBE_PROVIDER")
        .ok()
        .and_then(|v| ProviderId::parse(&v))
        .unwrap_or(ProviderId::Quark);
    let cap: u64 = std::env::var("UP_CAP_KBPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let mb: usize = std::env::var("UP_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let Some(drive) = drive(provider) else { return };
    let root = match provider {
        ProviderId::Aliyun => "root",
        _ => "0",
    };
    let dir = std::env::temp_dir().join(format!("yunyu-upcap-{}", std::process::id()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let local = dir.join("cap.bin");
    let block: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let mut file = tokio::fs::File::create(&local).await.unwrap();
    for _ in 0..mb {
        use tokio::io::AsyncWriteExt;
        file.write_all(&block).await.unwrap();
    }
    drop(file);
    let size = (mb * 1024 * 1024) as u64;
    let name = format!("yunyu-upcap-{}.bin", std::process::id());
    crate::transfer::set_speed_limit(cap);
    let started = std::time::Instant::now();
    let sink = crate::transfer::throttled_sink(Arc::new(std::sync::atomic::AtomicU64::new(0)));
    let result = drive
        .upload(
            &local,
            root,
            &name,
            sink,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    let elapsed = started.elapsed();
    crate::transfer::set_speed_limit(0);
    match result {
        Ok(fid) => {
            let rate = size as f64 / elapsed.as_secs_f64() / 1024.0;
            println!(
                "{name} ({size} B) in {:.1}s => {:.0} KiB/s (cap {cap} KiB/s)",
                elapsed.as_secs_f64(),
                rate
            );
            assert!(rate <= cap as f64 * 1.4, "cap not enforced: {rate:.0}");
            if let Some(fid) = fid {
                let _ = drive.delete(&[crate::providers::native_of(&fid)]).await;
            }
        }
        Err(err) => println!("upload failed: {err}"),
    }
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

/// Finds a small file to share, walking into sub-folders when the root has
/// none (阿里 and 123 keep everything inside dated folders).
async fn small_file(drive: &Arc<dyn DriveProvider>, root: &str) -> Option<crate::providers::EntryDto> {
    let mut queue: Vec<(String, u8)> = vec![(root.to_string(), 0)];
    let mut best: Option<crate::providers::EntryDto> = None;
    while let Some((fid, depth)) = queue.pop() {
        let Ok(list) = drive.list(&fid).await else { continue };
        for entry in list.entries {
            if entry.is_dir {
                // Two levels is enough to reach the dated folders; deeper walks
                // would spend minutes on a large drive.
                if depth < 2 {
                    queue.push((crate::providers::native_of(&entry.fid), depth + 1));
                }
                continue;
            }
            if entry.size == 0 || entry.size > 40_000_000 {
                continue;
            }
            let better = best.as_ref().map(|b| entry.size < b.size).unwrap_or(true);
            if better {
                best = Some(entry);
            }
        }
    }
    best
}

/// Creates a share of one real file on every drive that can, prints the link,
/// then revokes it again so nothing is left behind.
///
/// ```text
/// cargo test --lib live_create_share -- --ignored --nocapture --test-threads 1
/// ```
#[tokio::test]
#[ignore]
async fn live_create_share() {
    for provider in ProviderId::all() {
        let Some(drive) = drive(provider) else { continue };
        if !drive.capabilities().share_create {
            println!("{}: share_create not supported", provider.as_str());
            continue;
        }
        let root = match provider {
            ProviderId::Aliyun => "root",
            _ => "0",
        };
        // Smallest file: 123 needs a size, 百度 rejects empty files.
        let Some(entry) = small_file(&drive, root).await else {
            println!("{}: no small file to share", provider.as_str());
            continue;
        };
        let native = crate::providers::native_of(&entry.fid);
        println!(
            "{}: sharing 「{}」 ({} B)",
            provider.as_str(),
            entry.name,
            entry.size
        );
        match drive
            .create_share(
                std::slice::from_ref(&native),
                &entry.name,
                7,
                "ab12",
            )
            .await
        {
            Ok(created) => {
                println!(
                    "  url={} passcode={} expired_at={} share_id={}",
                    created.url, created.passcode, created.expired_at, created.share_id
                );
                assert!(created.url.starts_with("http"), "link looks wrong");
                // A share with a code must hand a code back. 139 mints its own
                // instead of taking ours, so only the length is guaranteed.
                assert_eq!(created.passcode.chars().count(), 4, "passcode not echoed");
            }
            Err(err) => println!("  create_share ERR {err}"),
        }
    }
}

/// Creates a share without asking for a passcode, to see which drives invent
/// one (百度) and which allow none (夸克/阿里/123).
#[tokio::test]
#[ignore]
async fn live_create_share_open() {
    for provider in ProviderId::all() {
        let Some(drive) = drive(provider) else { continue };
        if !drive.capabilities().share_create {
            continue;
        }
        let root = match provider {
            ProviderId::Aliyun => "root",
            _ => "0",
        };
        let Some(entry) = small_file(&drive, root).await else { continue };
        let native = crate::providers::native_of(&entry.fid);
        match drive.create_share(&[native], "", 0, "").await {
            Ok(created) => println!(
                "{}: permanent, passcode={:?} url={}",
                provider.as_str(),
                created.passcode,
                created.url
            ),
            Err(err) => println!("{}: open share ERR {err}", provider.as_str()),
        }
    }
}

/// Shares a *folder* — the case that used to report "no share id" while
/// leaving a live share behind.
///
/// Quark answers a folder with `task_sync: false` and no `task_resp` at all, so
/// the id only shows up in the polled task payload. Both the link and the
/// revocation are checked here, which is what proves the id is the real one.
#[tokio::test]
#[ignore]
async fn live_create_share_folder() {
    for provider in [ProviderId::Quark, ProviderId::Yun139, ProviderId::Pan123] {
        let Some(drive) = drive(provider) else { continue };
        if !drive.capabilities().share_create {
            continue;
        }
        let root = "0";
        let Ok(listing) = drive.list(root).await else { continue };
        let Some(folder) = listing.entries.iter().find(|e| e.is_dir) else {
            println!("{}: no folder to share", provider.as_str());
            continue;
        };
        let native = crate::providers::native_of(&folder.fid);
        println!("{}: sharing folder 「{}」", provider.as_str(), folder.name);
        match drive
            .create_share(std::slice::from_ref(&native), &folder.name, 7, "")
            .await
        {
            Ok(created) => {
                println!("  url={} id={}", created.url, created.share_id);
                assert!(created.url.starts_with("http"), "link looks wrong");
                match drive.revoke_share(&created.share_id, &[]).await {
                    Ok(()) => println!("  revoked"),
                    Err(err) => println!("  revoke ERR {err}"),
                }
            }
            Err(err) => println!("  create_share ERR {err}"),
        }
    }
}

/// Checks whether a share code actually guards a 123 link.
///
/// The create call accepts `pwd`, but `/b/api/share/info` reports
/// `HasPwd:false` for a share we just created with one. If opening it with a
/// wrong code succeeds, the code is decorative and the UI must not promise it.
#[tokio::test]
#[ignore]
async fn live_123_passcode_guard() {
    let Some(drive) = drive(ProviderId::Pan123) else { return };
    let Some(entry) = small_file(&drive, "0").await else {
        println!("no small file to share");
        return;
    };
    let native = crate::providers::native_of(&entry.fid);
    let Ok(created) = drive
        .create_share(std::slice::from_ref(&native), "yunyu-passcode-probe", 7, "ab12")
        .await
    else {
        println!("create failed");
        return;
    };
    println!("created {} passcode={:?}", created.url, created.passcode);
    let share_key = created
        .url
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    for code in ["ab12", "zzzz", ""] {
        match drive.share_open(&share_key, code).await {
            Ok(session) => println!(
                "  code={code:?} -> OPENED, file_num={} title={:?}",
                session.file_num, session.title
            ),
            Err(err) => println!("  code={code:?} -> rejected: {err}"),
        }
    }
    // Leave nothing behind.
    if drive.create_share(&[native], "", 7, "").await.is_ok() {
        println!("(a second share was created; delete both by hand)");
    }
}


/// Creates a share on every drive that can, then revokes it, so the probe
/// leaves the account as it found it.
///
/// ```text
/// cargo test --lib live_revoke_share -- --ignored --nocapture --test-threads 1
/// ```
#[tokio::test]
#[ignore]
async fn live_revoke_share() {
    for provider in ProviderId::all() {
        let Some(drive) = drive(provider) else { continue };
        if !drive.capabilities().share_create {
            continue;
        }
        let root = match provider {
            ProviderId::Aliyun => "root",
            _ => "0",
        };
        let Some(entry) = small_file(&drive, root).await else {
            println!("{}: no small file", provider.as_str());
            continue;
        };
        let native = crate::providers::native_of(&entry.fid);
        let created = match drive
            .create_share(std::slice::from_ref(&native), "yunyu-revoke-probe", 7, "ab12")
            .await
        {
            Ok(created) => created,
            Err(err) => {
                println!("{}: create failed: {err}", provider.as_str());
                continue;
            }
        };
        println!(
            "{}: created {} id={}",
            provider.as_str(),
            created.url,
            created.share_id
        );
        match drive
            .revoke_share(&created.share_id, std::slice::from_ref(&native))
            .await
        {
            Ok(()) => {
                // A revoked link must stop opening; give the server a moment.
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                let key = created
                    .url
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                match drive.share_open(&key, "ab12").await {
                    Ok(_) => println!("  revoked, but the link still opens"),
                    Err(err) => println!("  revoked, link now dead: {err}"),
                }
            }
            Err(err) => println!("  revoke failed: {err}"),
        }
    }
}

/// Lists the shares each drive reports as already created. Read-only.
#[tokio::test]
#[ignore]
async fn live_share_records() {
    for provider in crate::providers::ProviderId::all() {
        let Some(drive) = drive(provider) else {
            println!("{}: not logged in", provider.as_str());
            continue;
        };
        match drive.share_records().await {
            Ok(records) => {
                println!("{}: {} shares", provider.as_str(), records.len());
                for row in records.iter().take(6) {
                    println!(
                        "  id={} title={:?} url={} pwd={:?} files={} created={} expired={} visits={} saves={} fids={}",
                        row.share_id,
                        row.title,
                        row.url,
                        row.passcode,
                        row.file_num,
                        row.created_at,
                        row.expired,
                        row.visit_count,
                        row.save_count,
                        row.fids.len(),
                    );
                }
            }
            Err(err) => println!("{}: list failed: {err}", provider.as_str()),
        }
    }
}

/// Probes 139's create-share endpoint.
///
/// The web app posts `{getOutLinkReq: {...}}` to `/richlifeApp/devapp/IOutLink/
/// getOutLink` and reads `data.getOutLinkResSet[0].linkUrl`; the path is not
/// obvious from the response name, so this prints the raw payload.
#[tokio::test]
#[ignore]
async fn share_live_yun139_create() {
    let dir = config_dir();
    let Ok(drive) = crate::providers::yun139::Yun139Provider::load(&dir) else {
        println!("not logged in");
        return;
    };
    let children = drive.debug_children("0").await;
    println!("root children: {children:?}");
    let Some((name, file_id)) = children.iter().find(|(n, _)| !n.is_empty()) else {
        println!("empty root");
        return;
    };
    println!("sharing {name:?} id={file_id}");
    let account = drive.debug_account().await;
    let body = serde_json::json!({
        "getOutLinkReq": {
            "linkType": 0,
            "subLinkType": 0,
            "encrypt": 1,
            "coIDLst": { "item": [file_id] },
            "caIDLst": { "item": [] },
            "pubType": 1,
            "dedicatedName": name,
            "period": 7,
            "periodUnit": 1,
            "viewerLst": {},
            "userDomainId": account,
            "extInfo": { "isWatermark": 0, "shareChannel": "3001" },
        }
    });
    let out = drive.debug_share_post("/IOutLink/getOutLink", &body).await;
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
}

// ---------------------------------------------------------------- engine ---

/// Lists the largest files in each drive's root, so a manual run can pick one
/// big enough to actually be split.
#[tokio::test]
#[ignore]
async fn engine_live_list() {
    for provider in [ProviderId::Quark, ProviderId::Aliyun] {
        let Some(drive) = drive(provider) else { continue };
        println!("\n=== {} ===", provider.as_str());
        match drive.list("0").await {
            Ok(listing) => {
                let mut files: Vec<_> = listing.entries.iter().filter(|e| !e.is_dir).collect();
                files.sort_by_key(|e| std::cmp::Reverse(e.size));
                for entry in files.iter().take(10) {
                    println!("  {:>14}  {}", entry.size, entry.name);
                }
                println!("  ({} files, {} entries total)", files.len(), listing.total);
            }
            Err(err) => println!("  list ERR {err}"),
        }
    }
}

/// Lists aliyun folders so a nested file can be found for the engine probe.
#[tokio::test]
#[ignore]
async fn engine_live_aliyun_tree() {
    let Some(drive) = drive(ProviderId::Aliyun) else {
        println!("no aliyun");
        return;
    };
    let Ok(root) = drive.list("0").await else {
        println!("list failed");
        return;
    };
    for entry in root.entries.iter() {
        println!("{}  {}  size={}", if entry.is_dir { "DIR " } else { "FILE" }, entry.name, entry.size);
    }
    for entry in root.entries.iter().filter(|e| e.is_dir).take(4) {
        let native = crate::providers::native_of(&entry.fid);
        match drive.list(&native).await {
            Ok(inner) => {
                println!("\n-- {} ({} entries)", entry.name, inner.total);
                let mut files: Vec<_> = inner.entries.iter().filter(|e| !e.is_dir).collect();
                files.sort_by_key(|e| std::cmp::Reverse(e.size));
                for f in files.iter().take(6) {
                    println!("   {:>14}  {}", f.size, f.name);
                }
            }
            Err(err) => println!("\n-- {} ERR {err}", entry.name),
        }
    }
}

// -------------------------------------------------------- engine download ---

/// Downloads `fid` twice — once through the segmented engine, once through the
/// single stream that existed before it — and compares the bytes.
///
/// The single stream is the ground truth here: it is the code that shipped, so
/// anything the engine produces has to match it exactly. A length check alone
/// would not catch a plan that writes the right number of bytes at the wrong
/// offsets, which is the failure mode that matters.
async fn engine_compare(drive: Arc<dyn DriveProvider>, fid: &str, name: &str, size: u64) {
    use sha2::{Digest, Sha256};

    let dir = std::env::temp_dir().join(format!("yunyu-engine-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let segmented_path = dir.join(format!("seg-{name}"));
    let plain_path = dir.join(format!("plain-{name}"));
    let _ = std::fs::remove_file(&segmented_path);
    let _ = std::fs::remove_file(&plain_path);
    crate::engine::discard_progress(&segmented_path);

    // ---- segmented ----
    let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let token = tokio_util::sync::CancellationToken::new();
    let source = crate::engine::source::provider_source(
        Arc::clone(&drive),
        fid.to_string(),
        name.to_string(),
    );
    let provider = drive.id().as_str();
    let started = std::time::Instant::now();
    let outcome = crate::engine::download_with_stats(
        Arc::clone(&drive),
        Arc::clone(&source),
        provider,
        &segmented_path,
        size,
        Arc::clone(&done),
        0,
        token,
        None,
    )
    .await;
    let elapsed = started.elapsed();
    match outcome {
        Ok(true) => {}
        Ok(false) => {
            println!("  engine declined to segment (falls back to one stream)");
            return;
        }
        Err(err) => {
            println!("  engine ERR {err}");
            return;
        }
    }
    let seg_len = std::fs::metadata(&segmented_path).unwrap().len();
    let seg_mb = seg_len as f64 / (1024.0 * 1024.0);
    println!(
        "  segmented: {seg_len} B in {:.2}s → {:.2} MB/s",
        elapsed.as_secs_f64(),
        seg_mb / elapsed.as_secs_f64()
    );
    assert_eq!(seg_len, size, "segmented file has the wrong length");

    // ---- single stream, the reference ----
    let started = std::time::Instant::now();
    let handle = drive.open_download(fid, 0).await.expect("open_download");
    let mut stream = handle.stream;
    {
        use futures_util::StreamExt;
        use std::io::Write;
        let mut file = std::fs::File::create(&plain_path).unwrap();
        while let Some(item) = stream.next().await {
            let chunk = item.expect("stream chunk");
            file.write_all(&chunk).unwrap();
        }
    }
    let plain_elapsed = started.elapsed();
    let plain_len = std::fs::metadata(&plain_path).unwrap().len();
    println!(
        "  single   : {plain_len} B in {:.2}s → {:.2} MB/s",
        plain_elapsed.as_secs_f64(),
        plain_len as f64 / (1024.0 * 1024.0) / plain_elapsed.as_secs_f64()
    );

    let hash = |path: &std::path::Path| -> String {
        let mut hasher = Sha256::new();
        let mut file = std::fs::File::open(path).unwrap();
        std::io::copy(&mut file, &mut hasher).unwrap();
        format!("{:x}", hasher.finalize())
    };
    let seg_hash = hash(&segmented_path);
    let plain_hash = hash(&plain_path);
    println!("  segmented sha256 {seg_hash}");
    println!("  single    sha256 {plain_hash}");
    assert_eq!(seg_hash, plain_hash, "segmented bytes differ from single stream");
    println!("  ✓ identical");

    let _ = std::fs::remove_file(&segmented_path);
    let _ = std::fs::remove_file(&plain_path);
    let _ = std::fs::remove_dir(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn engine_live_quark() {
    let Some(drive) = drive(ProviderId::Quark) else {
        println!("quark not logged in");
        return;
    };
    // 45 MiB: past the 8 MiB threshold, so the engine splits it, small enough
    // to download twice in a test.
    let fid = "quark:df1d3c3b0b6b4f8bb0c5b0b4f2a3e1d0";
    let _ = fid;
    let listing = drive.list("0").await.expect("list");
    let target = listing
        .entries
        .iter()
        .filter(|e| !e.is_dir)
        .max_by_key(|e| e.size)
        .expect("a file");
    println!(
        "\n=== quark: {} ({} B) ===",
        target.name, target.size
    );
    let native = crate::providers::native_of(&target.fid);
    engine_compare(Arc::clone(&drive), &native, &target.name, target.size).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn engine_live_aliyun() {
    let Some(drive) = drive(ProviderId::Aliyun) else {
        println!("aliyun not logged in");
        return;
    };
    let root = drive.list("0").await.expect("list root");
    let mut best: Option<(String, String, u64)> = None;
    for folder in root.entries.iter().filter(|e| e.is_dir) {
        let native = crate::providers::native_of(&folder.fid);
        let Ok(inner) = drive.list(&native).await else {
            continue;
        };
        for file in inner.entries.iter().filter(|e| !e.is_dir) {
            // Smallest file above the segment threshold: enough to exercise the
            // split without downloading hundreds of megabytes twice.
            if file.size < 8 * 1024 * 1024 {
                continue;
            }
            if best.as_ref().is_none_or(|(_, _, size)| file.size < *size) {
                best = Some((file.fid.clone(), file.name.clone(), file.size));
            }
        }
    }
    let Some((fid, name, size)) = best else {
        println!("no suitable aliyun file");
        return;
    };
    println!("\n=== aliyun: {name} ({size} B) ===");
    let native = crate::providers::native_of(&fid);
    engine_compare(Arc::clone(&drive), &native, &name, size).await;
}

/// Runs the engine on a real Quark file, then verifies the result by fetching
/// eight random 64 KiB windows directly and comparing them with what landed on
/// disk. Cheaper than hashing the whole file twice and it catches a shifted
/// write, which a length check cannot.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn engine_live_quark_verify() {
    use futures_util::StreamExt;
    let Some(drive) = drive(ProviderId::Quark) else { return };
    let listing = drive.list("0").await.unwrap();
    let target = listing
        .entries
        .iter()
        .filter(|e| !e.is_dir)
        .max_by_key(|e| e.size)
        .unwrap();
    let native = crate::providers::native_of(&target.fid);
    let size = target.size;
    let dir = std::env::temp_dir().join(format!("yunyu-verify-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(&target.name);
    let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let token = tokio_util::sync::CancellationToken::new();
    let source = crate::engine::source::provider_source(
        Arc::clone(&drive),
        native.clone(),
        path.display().to_string(),
    );

    let started = std::time::Instant::now();
    let outcome = crate::engine::download_with_stats(
        Arc::clone(&drive),
        Arc::clone(&source),
        "quark",
        &path,
        size,
        Arc::clone(&done),
        0,
        token.clone(),
        None,
    )
    .await;
    let secs = started.elapsed().as_secs_f64();
    println!(
        "  engine: {:?} in {secs:.1}s -> {:.1} KB/s, done={}",
        outcome.as_ref().map(|ok| *ok),
        size as f64 / 1024.0 / secs,
        done.load(std::sync::atomic::Ordering::Relaxed)
    );
    if !matches!(outcome, Ok(true)) {
        println!("  engine did not finish, skipping verification");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    let local = std::fs::read(&path).unwrap();
    println!("  file length {} (expected {size})", local.len());
    let window = 64 * 1024u64;
    let mut bad = 0;
    for k in 0..8u64 {
        let begin = (size / 9) * k % size.saturating_sub(window);
        let end = begin + window;
        let crate::providers::RangeResult::Body(handle) =
            source.open(begin, Some(end)).await
        else {
            println!("  window {k} at {begin}: open failed");
            bad += 1;
            continue;
        };
        let mut stream = handle.stream;
        let mut remote = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    remote.extend_from_slice(&chunk);
                    if remote.len() as u64 >= window {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let want = &remote[..remote.len().min(window as usize)];
        let got = &local[begin as usize..(begin as usize + want.len()).min(local.len())];
        if want == got {
            println!("  window {k} at {begin}: match ({} B)", want.len());
        } else {
            bad += 1;
            println!("  window {k} at {begin}: MISMATCH ({} B)", want.len());
        }
    }
    println!("  mismatches: {bad}");
    let _ = std::fs::remove_dir_all(&dir);
}
