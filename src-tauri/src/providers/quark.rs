//! 夸克网盘 (Quark Drive).
//!
//! The first drive this app supported, so most of the logic here is a thin
//! adapter over `libquarkpan` — the personal-drive calls, the cookie handling
//! and the `__puus` refresh all live there. What the adapter adds is:
//!
//! * qualified entry ids (`quark:<fid>`),
//! * turning libquarkpan's stream into the provider-level [`DownloadHandle`],
//! * rapid-upload + chunked-upload reporting through [`UploadSink`].

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use libquarkpan::{QuarkPan, UploadPrepareResult};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{
    native_of, qualify, AuthState, Capabilities, DirListing, DownloadHandle, DriveProvider,
    EntryDto, PathNode, ProviderId, QuotaInfo, RecycleEntry, SearchHit, ShareFile, ShareLink,
    UploadSink,
};
use crate::error::{AppError, AppResult};
use crate::share::{AccountInfo, ShareEntry, ShareListing, ShareSession};

pub struct QuarkProvider {
    client: QuarkPan,
    /// The cookie string, needed by the share endpoints which do not go through
    /// libquarkpan (they mint their own `__puus`).
    cookie: String,
    /// Where the cookie is persisted, for the settings screen.
    cookie_path: String,
    /// Nickname + avatar, cached after the first successful fetch.
    account: tokio::sync::RwLock<Option<AccountInfo>>,
}

impl QuarkProvider {
    pub fn new(client: QuarkPan, cookie: String, cookie_path: String) -> Self {
        Self {
            client,
            cookie,
            cookie_path,
            account: tokio::sync::RwLock::new(None),
        }
    }

    /// Every page of a folder (Quark caps pages at 100 entries).
    async fn list_all(&self, pdir_fid: &str) -> AppResult<Vec<libquarkpan::QuarkEntry>> {
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let result = self
                .client
                .list()
                .pdir_fid(pdir_fid)
                .page(page)
                .size(100)
                .prepare()?
                .request()
                .await?;
            let count = result.entries.len();
            let total = result.total;
            out.extend(result.entries);
            if count < 100 || (total > 0 && out.len() >= total as usize) || page > 200 {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    fn entry(&self, entry: libquarkpan::QuarkEntry) -> EntryDto {
        EntryDto {
            fid: qualify(ProviderId::Quark, &entry.fid),
            pdir_fid: qualify(ProviderId::Quark, &entry.pdir_fid),
            ext: EntryDto::ext_of(&entry.file_name),
            // Quark serves previews from a separate endpoint; the listing does
            // not carry one.
            thumb: None,
            name: entry.file_name,
            size: entry.size,
            is_dir: entry.dir,
            format_type: entry.format_type,
            updated_at: entry.updated_at,
            created_at: entry.created_at,
            provider: ProviderId::Quark.as_str().to_string(),
        }
    }

    fn node(&self, fid: &str, name: &str) -> PathNode {
        PathNode {
            fid: qualify(ProviderId::Quark, fid),
            name: name.to_string(),
        }
    }

    /// Exchanges a share link + passcode for the `stoken` that every other
    /// share call needs.
    async fn share_token(&self, pwd_id: &str, passcode: &str) -> AppResult<Value> {
        let body = json!({ "pwd_id": pwd_id, "passcode": passcode });
        let value = post_json(
            &format!("{QUARK_API}/1/clouddrive/share/sharepage/token?pr=ucpro&fr=pc"),
            &self.cookie,
            &body,
        )
        .await?;
        envelope(value)
    }

    /// One page of a shared folder. `ver=2` makes the API return `data.share`,
    /// and the root folder additionally needs `_fetch_share=1`.
    async fn share_detail(
        &self,
        pwd_id: &str,
        stoken: &str,
        pdir_fid: &str,
        page: u32,
        size: u32,
    ) -> AppResult<Value> {
        let root = pdir_fid == "0" || pdir_fid.is_empty();
        let mut query: Vec<(&str, String)> = vec![
            ("pr", "ucpro".to_string()),
            ("fr", "pc".to_string()),
            ("pwd_id", pwd_id.to_string()),
            ("stoken", stoken.to_string()),
            ("pdir_fid", pdir_fid.to_string()),
            ("_page", page.to_string()),
            ("_size", size.to_string()),
            ("_fetch_total", "1".to_string()),
            ("ver", "2".to_string()),
        ];
        if root {
            query.push(("_fetch_share", "1".to_string()));
        }
        let value = get_json(
            &format!("{QUARK_API}/1/clouddrive/share/sharepage/detail"),
            &self.cookie,
            &query,
        )
        .await?;
        envelope(value)
    }

    /// Breadcrumb from the root down to `fid`, walking `/file/info` upwards.
    ///
    /// Search results only report the immediate parent, and Quark has no
    /// "path of this fid" call, so we climb. Depth is capped because a
    /// pathological tree would otherwise cost one request per level.
    async fn path_to(&self, fid: &str) -> Vec<PathNode> {
        let mut acc = Vec::new();
        let mut cursor = fid.to_string();
        for _ in 0..12 {
            if cursor.is_empty() || cursor == "0" {
                break;
            }
            let value = match get_json(
                &format!("{QUARK_API}/1/clouddrive/file/info"),
                &self.cookie,
                &[
                    ("pr", "ucpro".to_string()),
                    ("fr", "pc".to_string()),
                    ("fid", cursor.clone()),
                ],
            )
            .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let Ok(data) = envelope(value) else {
                break;
            };
            acc.push(PathNode {
                fid: qualify(ProviderId::Quark, &cursor),
                name: text(&data, "file_name"),
            });
            cursor = text(&data, "pdir_fid");
        }
        acc.reverse();
        acc
    }
}

#[async_trait]
impl DriveProvider for QuarkProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Quark
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    async fn auth_state(&self) -> AuthState {
        let account = self.account.read().await.clone();
        let nickname = account.as_ref().map(|a| a.nickname.clone());
        AuthState {
            provider: ProviderId::Quark,
            authenticated: !self.cookie.trim().is_empty(),
            source: "file".to_string(),
            masked: Some(crate::config::mask_cookie(&self.cookie)),
            credential_path: self.cookie_path.clone(),
            nickname,
            avatar: account.and_then(|a| a.avatar),
            capabilities: self.capabilities(),
        }
    }

    async fn logout(&self) -> AppResult<()> {
        // The cookie file is owned by AppState, which clears it and rebuilds us.
        Ok(())
    }

    async fn account(&self) -> AppResult<Option<AccountInfo>> {
        if let Some(cached) = self.account.read().await.clone() {
            return Ok(Some(cached));
        }
        Ok(None)
    }

    /// Caches the profile the app already fetched through `share::account_info`.
    async fn cache_account(&self, info: AccountInfo) {
        *self.account.write().await = Some(info);
    }

    /// Fetches the Quark profile once and caches it.
    ///
    /// `share::account_info` also fills this cache, but the sidebar reaches the
    /// drives through `auth_states`, which only has this trait method — so the
    /// probe and the real UI both end up here.
    async fn profile(&self) -> AppResult<Option<AccountInfo>> {
        if let Some(cached) = self.account.read().await.clone() {
            return Ok(Some(cached));
        }
        if self.cookie.trim().is_empty() {
            return Ok(None);
        }
        let res = crate::share::http()?
            .get("https://pan.quark.cn/account/info?fr=pc&platform=pc")
            .headers(crate::share::quark_headers(&self.cookie))
            .send()
            .await?;
        let value: Value = res.json().await?;
        if value.get("success").and_then(|v| v.as_bool()) != Some(true) {
            return Ok(None);
        }
        let data = value.get("data").cloned().unwrap_or(Value::Null);
        let nickname = text(&data, "nickname");
        if nickname.is_empty() {
            return Ok(None);
        }
        let mut info = AccountInfo {
            nickname,
            avatar: None,
        };
        let url = text(&data, "avatarUri");
        if !url.is_empty() {
            info.avatar = crate::share::fetch_avatar(&crate::share::http()?, &url).await;
        }
        *self.account.write().await = Some(info.clone());
        Ok(Some(info))
    }

    async fn list(&self, dir: &str) -> AppResult<DirListing> {
        let entries = self.list_all(dir).await?;
        let total = entries.len() as u32;
        let mut dtos: Vec<EntryDto> = entries.into_iter().map(|e| self.entry(e)).collect();
        super::sort_entries(&mut dtos);
        Ok(DirListing {
            pdir_fid: qualify(ProviderId::Quark, dir),
            entries: dtos,
            total,
        })
    }

    async fn resolve_path(&self, dir: &str) -> AppResult<Vec<PathNode>> {
        if dir == "0" || dir.is_empty() {
            return Ok(Vec::new());
        }
        // libquarkpan has no parent lookup, so search downwards from the root.
        let mut queue: std::collections::VecDeque<(String, Vec<(String, String)>)> =
            std::collections::VecDeque::new();
        queue.push_back(("0".to_string(), Vec::new()));
        let mut visited = 0u32;
        while let Some((fid, path)) = queue.pop_front() {
            visited += 1;
            if visited > 200 {
                break;
            }
            let entries = match self.list_all(&fid).await {
                Ok(v) => v,
                Err(_) => continue,
            };
            for entry in entries {
                if !entry.dir {
                    continue;
                }
                if entry.fid == dir {
                    let mut chain: Vec<PathNode> = path
                        .iter()
                        .map(|(f, n)| self.node(f, n))
                        .collect();
                    chain.push(self.node(&entry.fid, &entry.file_name));
                    return Ok(chain);
                }
                if path.len() < 10 {
                    let mut next = path.clone();
                    next.push((entry.fid.clone(), entry.file_name.clone()));
                    queue.push_back((entry.fid, next));
                }
            }
        }
        Ok(Vec::new())
    }

    async fn make_dir(&self, parent: &str, name: &str) -> AppResult<String> {
        let name = super::check_name(name)?.to_string();
        let fid = self
            .client
            .create_folder()
            .pdir_fid(parent.to_string())
            .file_name(name)
            .prepare()?
            .request()
            .await?;
        Ok(qualify(ProviderId::Quark, &fid))
    }

    async fn rename(&self, fid: &str, name: &str) -> AppResult<()> {
        let name = super::check_name(name)?.to_string();
        self.client
            .rename()
            .fid(fid.to_string())
            .file_name(name)
            .prepare()?
            .request()
            .await?;
        Ok(())
    }

    async fn delete(&self, fids: &[String]) -> AppResult<()> {
        if fids.is_empty() {
            return Err(AppError::new("未选择任何文件"));
        }
        self.client.delete(fids).await?;
        Ok(())
    }

    async fn transfer_entries(&self, fids: &[String], to: &str, copy: bool) -> AppResult<usize> {
        if fids.is_empty() {
            return Err(AppError::new("未选择任何文件"));
        }
        if to.trim().is_empty() {
            return Err(AppError::new("请选择目标文件夹"));
        }
        if copy {
            self.client.copy_to(fids, to).await?;
        } else {
            self.client.move_to(fids, to).await?;
        }
        Ok(fids.len())
    }

    /// Server-side search.
    ///
    /// Quark has its own index (`/file/search`), so there is no reason to walk
    /// the tree ourselves — the old BFS stopped after 80 folders and silently
    /// missed everything deeper. The hit carries `pdir_fid`, which we resolve
    /// into a breadcrumb so the UI can show where the file lives.
    async fn search(&self, _dir: &str, keyword: &str, limit: u32) -> AppResult<Vec<SearchHit>> {
        let needle = keyword.trim();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let mut hits = Vec::new();
        let mut page = 1u32;
        while hits.len() < limit as usize && page <= 20 {
            let value = get_json(
                &format!("{QUARK_API}/1/clouddrive/file/search"),
                &self.cookie,
                &[
                    ("pr", "ucpro".to_string()),
                    ("fr", "pc".to_string()),
                    ("keyword", needle.to_string()),
                    ("_page", page.to_string()),
                    ("_size", "100".to_string()),
                    ("_fetch_total", "1".to_string()),
                ],
            )
            .await?;
            let data = envelope(value)?;
            let rows = data
                .get("list")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let count = rows.len();
            for row in rows {
                let entry = entry_of_json(&row);
                let parent = entry.pdir_fid.clone();
                let path = self.path_to(&native_of(&parent)).await;
                hits.push(SearchHit { entry, path });
                if hits.len() >= limit as usize {
                    break;
                }
            }
            if count < 100 {
                break;
            }
            page += 1;
        }
        Ok(hits)
    }

    async fn open_download(&self, fid: &str, start: u64) -> AppResult<DownloadHandle> {
        let builder = self.client.download().fid(fid.to_string());
        let builder = if start > 0 {
            builder.start_offset(start)
        } else {
            builder
        };
        let request = builder.prepare()?;
        // `info()` is still worth calling: it mints the url and, more
        // importantly, refreshes the `__puus` cookie the CDN wants. The size we
        // report is 0 here because the transfer job already knows it from the
        // listing, and libquarkpan's DownloadInfo does not carry one.
        //
        // The md5 from that same call is *not* the md5 of the bytes: measured on
        // a 1536-byte upload, a 666 KiB pdf and an 11 MB apk, the published hash
        // never matched what the CDN served (each file downloaded completely).
        // Passing it on made every Quark download end in "文件校验失败", so it
        // stays unset — the size check in the engine is the guard here.
        let _ = request.info().await?;
        let stream = request.stream().await?;
        Ok(DownloadHandle {
            size: 0,
            md5: None,
            stream: Box::new(stream.map(|item| item.map_err(AppError::from))),
        })
    }

    async fn upload(
        &self,
        local: &Path,
        parent: &str,
        name: &str,
        sink: Arc<dyn UploadSink>,
        token: CancellationToken,
    ) -> AppResult<Option<String>> {
        let (md5, sha1, size) = hash_file(local, sink.clone(), token.clone()).await?;
        let client = self.client.clone();
        let prepared = client
            .upload()
            .pdir_fid(parent.to_string())
            .file_name(name.to_string())
            .size(size)
            .md5(md5)
            .sha1(sha1)
            .prepare()
            .await?;
        match prepared {
            UploadPrepareResult::RapidUploaded { fid } => {
                sink.progress(size);
                Ok(Some(qualify(ProviderId::Quark, &fid)))
            }
            UploadPrepareResult::NeedUpload(session) => {
                sink.phase("uploading");
                let file = std::fs::File::open(local)?;
                let reader =
                    tokio::io::BufReader::with_capacity(1 << 20, tokio::fs::File::from_std(file));
                // `progress()` takes an absolute byte count, so carry a running
                // total through the unfold state.
                let counter = Arc::clone(&sink);
                // `(reader, sent, finished)`: once a read fails the stream must
                // end. Yielding the error repeatedly left the reader in place, so
                // a consumer that ignored the error would spin forever on the
                // same message.
                let stream = futures_util::stream::unfold(
                    (reader, 0u64, false),
                    move |(mut reader, sent, finished)| {
                        let counter = Arc::clone(&counter);
                        async move {
                            if finished {
                                return None;
                            }
                            use tokio::io::AsyncReadExt;
                            let mut buf = vec![0u8; 1 << 20];
                            match reader.read(&mut buf).await {
                                Ok(0) => None,
                                Ok(n) => {
                                    buf.truncate(n);
                                    let total = sent + n as u64;
                                    counter.progress(total);
                                    Some((
                                        Ok::<_, std::io::Error>(bytes::Bytes::from(buf)),
                                        (reader, total, false),
                                    ))
                                }
                                Err(err) => Some((Err(err), (reader, sent, true))),
                            }
                        }
                    },
                );
                let result = session
                    .upload_stream(stream)
                    .await
                    .map_err(AppError::from)?;
                Ok(Some(qualify(ProviderId::Quark, &result.fid)))
            }
        }
    }

    async fn refresh(&self) -> AppResult<()> {
        Ok(())
    }

    // ---------------------------------------------------------- shares ---

    async fn share_open(&self, id: &str, passcode: &str) -> AppResult<ShareSession> {
        let data = self.share_token(id, passcode).await?;
        let stoken = text(&data, "stoken");
        if stoken.is_empty() {
            return Err(AppError::new("无法获取分享信息，请检查链接和提取码"));
        }
        let mut title = text(&data, "title");
        let mut file_num = data.get("file_num").and_then(|v| v.as_u64()).unwrap_or(0);
        let mut total_size = data.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        let mut expired_at = data.get("expired_at").and_then(|v| v.as_i64()).unwrap_or(0);
        let mut first_fid = data
            .get("first_fid")
            .and_then(|v| v.as_str())
            .map(|v| v.to_string());
        // The token endpoint omits the counters; the root listing carries them.
        if let Ok(detail) = self.share_detail(id, &stoken, "0", 1, 1).await {
            if let Some(share) = detail.get("share") {
                if title.is_empty() {
                    title = text(share, "title");
                }
                file_num = share
                    .get("file_num")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(file_num);
                total_size = share
                    .get("size")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(total_size);
                expired_at = share
                    .get("expired_at")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(expired_at);
                if let Some(fid) = share.get("first_fid").and_then(|v| v.as_str()) {
                    first_fid = Some(fid.to_string());
                }
            }
        }
        let author = data.get("author").cloned().unwrap_or(Value::Null);
        let owner_avatar = match author.get("avatar_url").and_then(|v| v.as_str()) {
            Some(url) => crate::share::fetch_avatar(&crate::share::http()?, url).await,
            None => None,
        };
        Ok(ShareSession {
            provider: ProviderId::Quark.as_str().to_string(),
            pwd_id: id.to_string(),
            passcode: passcode.to_string(),
            stoken,
            title,
            owner: text(&author, "nick_name"),
            owner_avatar,
            file_num,
            total_size,
            expired_at,
            first_fid,
            can_save: true,
            web_url: Some(format!("https://pan.quark.cn/s/{id}")),
        })
    }

    async fn share_list(
        &self,
        share: &crate::transfer::ShareRef,
        dir: &str,
    ) -> AppResult<ShareListing> {
        let fid = if dir.is_empty() { "0" } else { dir };
        let mut entries = Vec::new();
        let mut page = 1u32;
        let mut total = 0u64;
        loop {
            let data = self
                .share_detail(&share.pwd_id, &share.stoken, fid, page, 100)
                .await?;
            total = data
                .pointer("/metadata/_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(total);
            let rows = entries_of(&data);
            let count = rows.len();
            entries.extend(rows);
            if count < 100 || page > 200 || (total > 0 && entries.len() as u64 >= total) {
                break;
            }
            page += 1;
        }
        Ok(ShareListing {
            pdir_fid: fid.to_string(),
            total: if total > 0 {
                total
            } else {
                entries.len() as u64
            },
            entries,
        })
    }

    async fn share_save(
        &self,
        share: &crate::transfer::ShareRef,
        entries: &[ShareEntry],
        to: Option<&str>,
    ) -> AppResult<u64> {
        if entries.is_empty() {
            return Err(AppError::new("请先选择要转存的内容"));
        }
        let body = json!({
            "fid_list": entries.iter().map(|e| e.fid.clone()).collect::<Vec<_>>(),
            "fid_token_list": entries.iter().map(|e| e.token.clone()).collect::<Vec<_>>(),
            "to_pdir_fid": to.unwrap_or("0"),
            "pwd_id": share.pwd_id,
            "stoken": share.stoken,
            "pdir_fid": "0",
            "scene": "link",
        });
        let cookie = crate::share::ensure_puus(&self.cookie).await;
        let value = post_json(
            &format!("{QUARK_API}/1/clouddrive/share/sharepage/save?pr=ucpro&fr=pc&uc_param_str="),
            &cookie,
            &body,
        )
        .await?;
        let data = envelope(value)?;
        let task_id = text(&data, "task_id");
        if task_id.is_empty() {
            return Ok(entries.len() as u64);
        }
        poll_task(&cookie, &task_id).await?;
        Ok(entries.len() as u64)
    }

    async fn share_walk(
        &self,
        share: &crate::transfer::ShareRef,
        dir: &str,
    ) -> AppResult<Vec<ShareFile>> {
        const MAX_FILES: usize = 5000;
        const MAX_DEPTH: usize = 12;
        let mut out = Vec::new();
        let mut queue: std::collections::VecDeque<(String, std::path::PathBuf, usize)> =
            std::collections::VecDeque::new();
        queue.push_back((dir.to_string(), std::path::PathBuf::new(), 0));
        while let Some((fid, prefix, depth)) = queue.pop_front() {
            if out.len() >= MAX_FILES {
                return Err(AppError::new(
                    "文件夹内文件过多（超过 5000 个），请分次下载",
                ));
            }
            let listing = self.share_list(share, &fid).await?;
            for entry in listing.entries {
                let name = crate::paths::safe_component(&entry.name);
                if entry.is_dir {
                    if depth + 1 > MAX_DEPTH {
                        continue;
                    }
                    let mut next = prefix.clone();
                    next.push(name);
                    queue.push_back((entry.fid, next, depth + 1));
                } else {
                    let mut relative = prefix.clone();
                    relative.push(name);
                    out.push(ShareFile {
                        relative,
                        fid: entry.fid,
                        token: entry.token,
                        name: entry.name,
                        size: entry.size,
                    });
                }
            }
        }
        Ok(out)
    }

    async fn share_link(
        &self,
        share: &crate::transfer::ShareRef,
        fid: &str,
    ) -> AppResult<ShareLink> {
        // Download hosts reject requests without a current `__puus`.
        let cookie = crate::share::ensure_puus(&self.cookie).await;
        let body = json!({
            "fids": [fid],
            "fids_token": [share.fid_token],
            "pwd_id": share.pwd_id,
            "stoken": share.stoken,
        });
        let value = post_json(
            &format!("{QUARK_API}/1/clouddrive/file/share/download?pr=ucpro&fr=pc"),
            &cookie,
            &body,
        )
        .await?;
        let data = envelope(value)?;
        let first = data
            .as_array()
            .and_then(|list| list.first().cloned())
            .or_else(|| {
                data.get("list")
                    .and_then(|v| v.as_array())
                    .and_then(|l| l.first().cloned())
            })
            .ok_or_else(|| AppError::new("分享文件没有返回下载地址"))?;
        let url = text(&first, "download_url");
        if url.is_empty() {
            return Err(AppError::new("分享文件没有返回下载地址"));
        }
        Ok(ShareLink {
            url,
            headers: vec![
                ("Cookie".to_string(), cookie),
                ("Referer".to_string(), "https://pan.quark.cn/".to_string()),
                ("User-Agent".to_string(), crate::share::UA.to_string()),
            ],
            size: first.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
            cleanup_fid: None,
        })
    }

    async fn quota(&self) -> AppResult<Option<QuotaInfo>> {
        // libquarkpan has no quota call, so this is the one raw request the
        // adapter makes. `/1/clouddrive/member` also carries the membership
        // tier; we only read the two capacity fields.
        let url = format!("{QUOTA_BASE}/1/clouddrive/member?pr=ucpro&fr=pc&uc_param_str=");
        let response = reqwest::Client::new()
            .get(&url)
            .header(reqwest::header::COOKIE, &self.cookie)
            .header(
                reqwest::header::USER_AGENT,
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/120.0.0.0 Safari/537.36",
            )
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?;
        let body: serde_json::Value = response.json().await?;
        let used = body.pointer("/data/use_capacity").and_then(|v| v.as_u64());
        let total = body.pointer("/data/total_capacity").and_then(|v| v.as_u64());
        match (used, total) {
            (Some(used), Some(total)) if total > 0 => Ok(Some(QuotaInfo { used, total })),
            _ => Ok(None),
        }
    }

    // ----------------------------------------------------------- recycle ---

    async fn recycle_list(&self, offset: u32, limit: u32) -> AppResult<Vec<RecycleEntry>> {
        // The bin is paged by row count, not by cursor: `_page`/`_size` with
        // the offset folded into the page number.
        let size = limit.max(1);
        let page = offset / size + 1;
        let value = get_json(
            &format!("{QUARK_API}/1/clouddrive/file/recycle/list"),
            &self.cookie,
            &[
                ("pr", "ucpro".to_string()),
                ("fr", "pc".to_string()),
                ("_page", page.to_string()),
                ("_size", size.to_string()),
                ("_fetch_total", "1".to_string()),
            ],
        )
        .await?;
        let data = envelope(value)?;
        let rows = data
            .get("list")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(rows
            .iter()
            .map(|row| {
                let native = text(row, "fid");
                RecycleEntry {
                    fid: qualify(ProviderId::Quark, &native),
                    name: text(row, "file_name"),
                    is_dir: row.get("file_type").and_then(|v| v.as_i64()).unwrap_or(0) == 0,
                    size: row.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                    deleted_at: row.get("updated_at").and_then(|v| v.as_u64()).unwrap_or(0),
                    // The record id is what the bin's own endpoints want; the
                    // fid above is the original file id, which is what restore
                    // and purge actually take.
                    token: text(row, "record_id"),
                    provider: ProviderId::Quark.as_str().to_string(),
                }
            })
            .collect())
    }

    async fn recycle_restore(&self, entries: &[RecycleEntry]) -> AppResult<u32> {
        // `select_mode: 1` + a flat `fid_list` is the only shape the endpoint
        // accepts: `record_list`, `record_ids`, `fids` and a `select_mode: 2`
        // variant all come back as "Bad Parameter".
        let fids: Vec<String> = entries.iter().map(|e| native_of(&e.fid)).collect();
        if fids.is_empty() {
            return Err(AppError::new("请先选择要恢复的内容"));
        }
        let value = post_json(
            &format!("{QUARK_API}/1/clouddrive/file/recycle/recover?pr=ucpro&fr=pc"),
            &self.cookie,
            &json!({ "select_mode": 1, "fid_list": fids }),
        )
        .await?;
        envelope(value)?;
        Ok(entries.len() as u32)
    }

    async fn recycle_purge(&self, entries: &[RecycleEntry]) -> AppResult<u32> {
        let fids: Vec<String> = entries.iter().map(|e| native_of(&e.fid)).collect();
        if fids.is_empty() {
            return Err(AppError::new("请先选择要彻底删除的内容"));
        }
        // Unlike list/recover, this one rejects the request without an Origin
        // header (`401 require login [guest]`).
        let res = crate::share::http()?
            .post(format!(
                "{QUARK_API}/1/clouddrive/file/recycle/remove?pr=ucpro&fr=pc"
            ))
            .headers(crate::share::quark_headers(&self.cookie))
            .header(reqwest::header::ORIGIN, "https://pan.quark.cn")
            .json(&json!({ "select_mode": 1, "fid_list": fids }))
            .send()
            .await?;
        let value: Value = res.json().await?;
        envelope(value)?;
        Ok(entries.len() as u32)
    }

    // ---------------------------------------------------- own shares ---

    async fn create_share(
        &self,
        fids: &[String],
        title: &str,
        days: u32,
        passcode: &str,
    ) -> AppResult<crate::share::ShareCreated> {
        if fids.is_empty() {
            return Err(AppError::new("请先选择要分享的内容"));
        }
        // 1 = no code, 2 = 4-character code. A half-typed code is treated as
        // "no code" rather than being sent and rejected.
        let url_type = if passcode.chars().count() == 4 { 2 } else { 1 };
        let expired_type = match days {
            0 => 1,
            1 => 2,
            7 => 3,
            30 => 4,
            other => {
                return Err(AppError::new(format!(
                    "夸克网盘只支持永久 / 1 天 / 7 天 / 30 天，收到 {other} 天"
                )))
            }
        };
        let mut body = json!({
            "fid_list": fids,
            "title": if title.is_empty() { "分享" } else { title },
            "url_type": url_type,
            "expired_type": expired_type,
        });
        if url_type == 2 {
            body["passcode"] = json!(passcode);
        }
        // Note the path: `/share`, not `/share/create` (that one is a 404).
        let value = post_json(
            &format!("{QUARK_API}/1/clouddrive/share?pr=ucpro&fr=pc"),
            &self.cookie,
            &body,
        )
        .await?;
        let data = envelope(value)?;
        let mut share_id = data
            .pointer("/task_resp/data/share_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if share_id.is_empty() {
            // A folder (or any big selection) comes back as a task with
            // `task_sync: false` and no `task_resp` at all. The share already
            // exists by then, so poll the task and read the id from it —
            // reporting "no share id" here left a live share behind that the
            // user could see on the site but not in the app.
            let task = text(&data, "task_id");
            if task.is_empty() {
                return Err(AppError::new("夸克网盘没有返回分享任务，请稍后重试"));
            }
            let done = poll_task(&self.cookie, &task).await?;
            share_id = text(&done, "share_id");
            if share_id.is_empty() {
                return Err(AppError::new(
                    "夸克网盘已创建分享，但没有返回分享 ID；请到「我的分享」里查看",
                ));
            }
        }
        // The share id is 32 hex; the public link needs the shorter pwd_id that
        // only `/share/password` hands out.
        let value = post_json(
            &format!("{QUARK_API}/1/clouddrive/share/password?pr=ucpro&fr=pc"),
            &self.cookie,
            &json!({ "share_id": share_id }),
        )
        .await?;
        let info = envelope(value)?;
        let url = text(&info, "share_url");
        if url.is_empty() {
            return Err(AppError::new("夸克网盘没有返回分享链接，请稍后重试"));
        }
        Ok(crate::share::ShareCreated {
            url,
            passcode: text(&info, "passcode"),
            expired_at: info
                .get("expired_at")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            share_id,
        })
    }

    /// `POST /1/clouddrive/share/delete` with the 32-hex share id.
    async fn revoke_share(&self, share_id: &str, _fids: &[String]) -> AppResult<()> {
        if share_id.is_empty() {
            return Err(AppError::new("缺少分享 ID"));
        }
        let value = post_json(
            &format!("{QUARK_API}/1/clouddrive/share/delete?pr=ucpro&fr=pc&uc_param_str="),
            &self.cookie,
            &json!({ "share_ids": [share_id] }),
        )
        .await?;
        envelope(value).map(|_| ())
    }

    /// `GET /1/clouddrive/share/mypage/detail` — the shares this cookie owns.
    ///
    /// The response already carries the public link, the passcode and the
    /// counters, so no extra round trip is needed. `/share/mypage/detail` is
    /// the *page* path; the API path keeps the `/1/clouddrive` prefix.
    async fn share_records(&self) -> AppResult<Vec<crate::share::ShareRecord>> {
        let value = get_json(
            &format!("{QUARK_API}/1/clouddrive/share/mypage/detail"),
            &self.cookie,
            &[
                ("pr", "ucpro".to_string()),
                ("fr", "pc".to_string()),
                ("page", "1".to_string()),
                ("size", "100".to_string()),
                ("order_by", "created_at".to_string()),
                ("order", "desc".to_string()),
                ("_fetch_total", "1".to_string()),
            ],
        )
        .await?;
        let data = envelope(value)?;
        let list = data
            .get("list")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .map(|row| crate::share::ShareRecord {
                share_id: text(row, "share_id"),
                url: text(row, "share_url"),
                title: text(row, "title"),
                passcode: text(row, "passcode"),
                // A permanent share is stamped 2100-01-01; report it as 0.
                expired_at: row
                    .get("expired_at")
                    .and_then(|v| v.as_i64())
                    .filter(|at| *at < 4_102_444_800_000)
                    .unwrap_or(0),
                created_at: row.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0),
                file_num: row
                    .get("file_num")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                size: row.get("size").and_then(|v| v.as_i64()).unwrap_or(0),
                visit_count: row
                    .get("visit_user_count")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                save_count: row.get("save_pv").and_then(|v| v.as_i64()).unwrap_or(0),
                download_count: row.get("download_pv").and_then(|v| v.as_i64()).unwrap_or(0),
                expired: row
                    .get("expired_at")
                    .and_then(|v| v.as_i64())
                    .map(|at| at > 0 && at < chrono::Utc::now().timestamp_millis())
                    .unwrap_or(false),
                // The list only names the first file; revoking goes by
                // `share_id`, so the id list stays empty.
                fids: row
                    .get("first_fid")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.is_empty())
                    .map(|v| vec![v.to_string()])
                    .unwrap_or_default(),
            })
            .collect())
    }
}

/// Quota lives on the same host libquarkpan is pointed at, but we do not have
/// that base URL here — the public one answers the same for a personal cookie.
const QUOTA_BASE: &str = "https://drive-pc.quark.cn";

// ------------------------------------------------------- share internals ---
//
// The share endpoints are not part of libquarkpan, so they are spelled out
// here. Two quirks: `sharepage/detail` only returns `data.share` when the
// request asks for `ver=2` (plus `_fetch_share=1` on the root folder), and the
// CDN links only work with a *fresh* `__puus` cookie.

const QUARK_API: &str = "https://drive.quark.cn";

async fn post_json(url: &str, cookie: &str, body: &Value) -> AppResult<Value> {
    let res = crate::share::http()?
        .post(url)
        .headers(crate::share::quark_headers(cookie))
        .json(body)
        .send()
        .await?;
    let status = res.status();
    let text_body = res.text().await?;
    serde_json::from_str::<Value>(&text_body).map_err(|_| {
        AppError::new(format!(
            "夸克接口返回异常（HTTP {}）：{}",
            status.as_u16(),
            text_body.chars().take(120).collect::<String>()
        ))
    })
}

async fn get_json(url: &str, cookie: &str, query: &[(&str, String)]) -> AppResult<Value> {
    let res = crate::share::http()?
        .get(url)
        .headers(crate::share::quark_headers(cookie))
        .query(query)
        .send()
        .await?;
    let status = res.status();
    let text_body = res.text().await?;
    serde_json::from_str::<Value>(&text_body).map_err(|_| {
        AppError::new(format!(
            "夸克接口返回异常（HTTP {}）：{}",
            status.as_u16(),
            text_body.chars().take(120).collect::<String>()
        ))
    })
}

/// Unwraps `{status, code, data}` and turns a bad code into a readable error.
fn envelope(value: Value) -> AppResult<Value> {
    let status = value.get("status").and_then(|v| v.as_i64()).unwrap_or(0);
    let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
    if status != 200 || code != 0 {
        let message = value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        return Err(AppError::new(describe(status, code, &message)));
    }
    Ok(value.get("data").cloned().unwrap_or(Value::Null))
}

fn describe(status: i64, code: i64, message: &str) -> String {
    match code {
        41004 => "分享链接已过期".to_string(),
        41006 => "提取码不正确".to_string(),
        41016 => "分享内容包含违规文件，无法转存".to_string(),
        _ if !message.is_empty() => format!("{message}（code={code}）"),
        _ => format!("夸克接口返回异常（status={status}, code={code}）"),
    }
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// One row of a `/file/search` (or `/file/info`) payload as a UI entry.
fn entry_of_json(row: &Value) -> EntryDto {
    let name = text(row, "file_name");
    EntryDto {
        fid: qualify(ProviderId::Quark, &text(row, "fid")),
        pdir_fid: qualify(ProviderId::Quark, &text(row, "pdir_fid")),
        ext: EntryDto::ext_of(&name),
        name,
        size: row.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
        is_dir: row.get("dir").and_then(|v| v.as_bool()).unwrap_or(false),
        format_type: text(row, "format_type"),
        updated_at: row.get("updated_at").and_then(|v| v.as_u64()).unwrap_or(0),
        created_at: row.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
        provider: ProviderId::Quark.as_str().to_string(),
        thumb: None,
    }
}

fn entries_of(data: &Value) -> Vec<ShareEntry> {
    data.get("list")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .map(|item| ShareEntry {
                    fid: text(item, "fid"),
                    token: text(item, "share_fid_token"),
                    name: text(item, "file_name"),
                    size: item.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                    is_dir: item.get("dir").and_then(|v| v.as_bool()).unwrap_or(false),
                    updated_at: item.get("updated_at").and_then(|v| v.as_i64()).unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Polls a share-save (or share-create) task until it reports success.
///
/// Returns the finished task payload: a create task carries the `share_id`
/// the caller needs, and a save task carries nothing useful.
async fn poll_task(cookie: &str, task_id: &str) -> AppResult<Value> {
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let url = format!(
            "{QUARK_API}/1/clouddrive/task?pr=ucpro&fr=pc&task_id={task_id}&retry_index=0"
        );
        let Ok(value) = get_json(&url, cookie, &[]).await else {
            continue;
        };
        let Ok(data) = envelope(value) else {
            continue;
        };
        match data.get("status").and_then(|v| v.as_i64()) {
            Some(2) => return Ok(data),
            Some(3) => {
                let message = text(&data, "message");
                return Err(AppError::new(if message.is_empty() {
                    "转存失败，请稍后重试".to_string()
                } else {
                    format!("转存失败：{message}")
                }));
            }
            _ => {}
        }
    }
    Err(AppError::new("任务超时，请稍后在「我的文件」中查看"))
}

/// Hashes a file, reporting bytes hashed through `sink`.
async fn hash_file(
    path: &Path,
    sink: Arc<dyn UploadSink>,
    token: CancellationToken,
) -> AppResult<(String, String, u64)> {
    use md5::{Digest, Md5};
    use sha1::Sha1;
    use tokio::io::AsyncReadExt;

    sink.phase("hashing");
    let mut file = tokio::fs::File::open(path).await?;
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(AppError::new("已取消")),
            n = file.read(&mut buf) => n?,
        };
        if n == 0 {
            break;
        }
        md5.update(&buf[..n]);
        sha1.update(&buf[..n]);
        total += n as u64;
        sink.progress(total);
    }
    Ok((
        format!("{:x}", md5.finalize()),
        format!("{:x}", sha1.finalize()),
        total,
    ))
}
