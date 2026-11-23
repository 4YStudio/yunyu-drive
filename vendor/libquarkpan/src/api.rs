use std::cmp::min;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::{Engine as _, engine::general_purpose};
use dashmap::DashMap;
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue, RANGE};
use serde::Serialize;

use crate::error::{QuarkPanError, Result};
use crate::quality;
use crate::model::{
    AuthRequest, AuthResponse, CreateFolderRequest, CreateFolderResponse, DeleteFilesRequest,
    DeleteFilesResponse, DownloadInfo, EmptyData, FileDownloadUrlItem, FinishRequest,
    FinishResponse, GetFilesDownloadUrlsRequest, GetFilesDownloadUrlsResponse, ListFolderResponse,
    ListPage, RenameFileRequest, RenameFileResponse, Response, TaskResponse,
    TransferFilesRequest, TransferFilesResponse, UpAuthAndCommitRequest,
    UpHashRequest, UpHashResponse, UpPartMethodRequest, UpPreRequest, UpPreResponse,
};

const ORIGIN: &str = "https://pan.quark.cn";
const REFERER: &str = "https://pan.quark.cn/";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) quark-cloud-drive/2.5.20 Chrome/100.0.4896.160 Electron/18.3.5.4-b478491100 Safari/537.36 Channel/pckk_other_ch";

#[derive(Clone)]
pub struct ApiConfig {
    pub api_base_url: String,
    pub cookie: Arc<DashMap<String, String>>,
}

#[derive(Clone)]
pub struct ApiClient {
    config: ApiConfig,
    client: reqwest::Client,
    download_client: reqwest::Client,
}

impl ApiClient {
    pub fn new(config: ApiConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert("Origin", HeaderValue::from_static(ORIGIN));
        headers.insert("Referer", HeaderValue::from_static(REFERER));
        // The CDN does not judge "is this the official client" on the user agent
        // alone: it looks at a whole set of headers. Minting a url with the full
        // browser set and then fetching it with only `Cookie` reads as two
        // different clients and is the classic cause of a download that is
        // silently throttled to a crawl.
        let mut download_headers = headers.clone();
        download_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        download_headers.insert(
            "Accept",
            HeaderValue::from_static("application/json, text/plain, */*"),
        );
        download_headers.insert(
            "Accept-Language",
            HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
        );
        download_headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
        download_headers.insert("Pragma", HeaderValue::from_static("no-cache"));
        download_headers.insert("Sec-Fetch-Dest", HeaderValue::from_static("empty"));
        download_headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
        download_headers.insert("Sec-Fetch-Site", HeaderValue::from_static("same-site"));
        let pool_size: usize = min(num_cpus::get().saturating_mul(2), 16).max(3);
        // API calls are small and finish in well under a minute, so a total
        // deadline is right for them.
        let client = reqwest::Client::builder()
            .user_agent(UA)
            .default_headers(headers.clone())
            .pool_idle_timeout(Duration::from_secs(50))
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(pool_size)
            .timeout(Duration::from_secs(60))
            .build()?;
        // The download client must NOT have a total timeout: `timeout()` is a
        // deadline for the entire request (connect *through* the last body
        // byte), so any file big enough to take longer than it kills the
        // transfer mid-flight — which is exactly the "large download always
        // times out after a while" bug. `read_timeout` is the right tool: it
        // resets after every successful read, so it only fires when the
        // connection genuinely stalls.
        let download_builder = reqwest::Client::builder()
            .user_agent(UA)
            .default_headers(download_headers)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .pool_idle_timeout(Duration::from_secs(50))
            .pool_max_idle_per_host(pool_size)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60));
        let download_client = download_builder.build()?;
        Ok(Self {
            config,
            client,
            download_client,
        })
    }

    async fn resolve_cookies(&self) -> String {
        self.config
            .cookie
            .iter()
            .map(|entry| format!("{}={}", entry.key(), entry.value()))
            .collect::<Vec<_>>()
            .join("; ")
    }

    async fn update_cookie_from_response(&self, res: &reqwest::Response) {
        if let Some(set_cookie) = res
            .headers()
            .get_all("set-cookie")
            .iter()
            .find_map(|v| v.to_str().ok())
        {
            if let Some(puus) = set_cookie
                .split(';')
                .find(|s| s.trim().starts_with("__puus="))
            {
                let new_puus = puus.trim().replace("__puus=", "");
                self.config.cookie.insert("__puus".to_string(), new_puus);
            }
        }
    }

    fn ensure_ok<T, U>(&self, response: Response<T, U>) -> Result<Response<T, U>> {
        if response.status != 200 {
            return Err(QuarkPanError::Api {
                status: response.code,
                message: response.message,
            });
        }
        Ok(response)
    }

    async fn get_json<U>(&self, url: String) -> Result<U>
    where
        U: serde::de::DeserializeOwned,
    {
        let cookie = self.resolve_cookies().await;
        let res = self.client.get(url).header("Cookie", cookie).send().await?;
        let res = res.error_for_status()?;
        self.update_cookie_from_response(&res).await;
        let text = res.text().await?;
        Ok(serde_json::from_str::<U>(&text)?)
    }

    async fn post_json<T, U>(&self, url: String, body: &T) -> Result<U>
    where
        T: Serialize + ?Sized,
        U: serde::de::DeserializeOwned,
    {
        let cookie = self.resolve_cookies().await;
        let res = self
            .client
            .post(url)
            .json(body)
            .header("Cookie", cookie)
            .send()
            .await?;
        let res = res.error_for_status()?;
        self.update_cookie_from_response(&res).await;
        let text = res.text().await?;
        Ok(serde_json::from_str::<U>(&text)?)
    }

    async fn post_xml<U>(&self, url: String, xml: String, headers: HeaderMap) -> Result<U>
    where
        U: serde::de::DeserializeOwned,
    {
        let cookie = self.resolve_cookies().await;
        let res = self
            .client
            .post(url)
            .headers(headers)
            .header("Cookie", cookie)
            .body(xml)
            .send()
            .await?;
        let res = res.error_for_status()?;
        self.update_cookie_from_response(&res).await;
        let text = res.text().await?;
        if text.trim().is_empty() {
            return Ok(serde_json::from_str("{}")?);
        }
        Ok(serde_json::from_str::<U>(&text)?)
    }

    /// The api entries a download url can be minted through.
    ///
    /// They differ only in query parameters, but the urls they hand back are
    /// not equivalent: the CDN routes per entry, so one entry can return a node
    /// that is slow or already unhealthy while another is fine. The first entry
    /// is the plain pc-client call; the other two exist only to rescue a bad
    /// answer from it.
    const DOWNLOAD_ENTRIES: [&'static [(&'static str, &'static str)]; 3] = [
        &[],
        &[("entry", "ft")],
        &[("fr", "android")],
    ];

    async fn download_info_from(
        &self,
        fid: &str,
        extra: &[(&str, &str)],
    ) -> Result<DownloadInfo> {
        let req = GetFilesDownloadUrlsRequest {
            fids: vec![fid.to_string()],
        };
        let mut query = String::from("pr=ucpro&fr=pc");
        for (name, value) in extra {
            query.push('&');
            query.push_str(name);
            query.push('=');
            query.push_str(value);
        }
        let res: GetFilesDownloadUrlsResponse = self
            .post_json(
                format!(
                    "{}/1/clouddrive/file/download?{query}",
                    self.config.api_base_url
                ),
                &req,
            )
            .await?;
        let res = self.ensure_ok(res)?;
        res.data
            .into_iter()
            .next()
            .map(|item: FileDownloadUrlItem| DownloadInfo {
                fid: item.fid,
                download_url: item.download_url,
                md5: item.md5,
            })
            .ok_or_else(|| {
                QuarkPanError::invalid_argument(format!("download url not found for fid {fid}"))
            })
    }

    /// Mints a download url, trying the other client entries when the first one
    /// looks unhealthy.
    ///
    /// A single-file download that needs no rescue still costs exactly one api
    /// call: a url scoring at or above [`quality::FAST_PATH_SCORE`] stops the
    /// search, because the remaining entries would only cost latency.
    pub async fn get_download_info(&self, fid: &str) -> Result<DownloadInfo> {
        let mut best: Option<(i32, DownloadInfo)> = None;
        let mut last_err: Option<QuarkPanError> = None;
        for extra in Self::DOWNLOAD_ENTRIES {
            match self.download_info_from(fid, extra).await {
                Ok(info) => {
                    let score = quality::score(&info.download_url);
                    let better = best
                        .as_ref()
                        .is_none_or(|(best_score, _)| score > *best_score);
                    if better {
                        best = Some((score, info));
                    }
                    if score >= quality::FAST_PATH_SCORE {
                        break;
                    }
                }
                Err(err) => last_err = Some(err),
            }
        }
        match best {
            Some((_, info)) => Ok(info),
            None => Err(last_err.unwrap_or_else(|| {
                QuarkPanError::invalid_argument(format!("download url not found for fid {fid}"))
            })),
        }
    }

    pub async fn get_download_url(&self, fid: &str) -> Result<String> {
        Ok(self.get_download_info(fid).await?.download_url)
    }

    pub async fn create_folder(&self, pdir_fid: &str, file_name: &str) -> Result<String> {
        let req = CreateFolderRequest {
            pdir_fid: pdir_fid.to_string(),
            file_name: file_name.to_string(),
            dir_path: String::new(),
            dir_init_lock: false,
        };
        let res: CreateFolderResponse = self
            .post_json(
                format!(
                    "{}/1/clouddrive/file?pr=ucpro&fr=pc",
                    self.config.api_base_url
                ),
                &req,
            )
            .await?;
        Ok(self.ensure_ok(res)?.data.fid)
    }

    pub async fn rename(&self, fid: &str, file_name: &str) -> Result<()> {
        let req = RenameFileRequest {
            fid: fid.to_string(),
            file_name: file_name.to_string(),
        };
        let res: RenameFileResponse = self
            .post_json(
                format!(
                    "{}/1/clouddrive/file/rename?pr=ucpro&fr=pc&uc_param_str=",
                    self.rename_api_base_url()
                ),
                &req,
            )
            .await?;
        self.ensure_ok(res)?;
        Ok(())
    }

    pub async fn delete<S>(&self, fids: &[S]) -> Result<()>
    where
        S: AsRef<str>,
    {
        if fids.is_empty() {
            return Err(QuarkPanError::invalid_argument(
                "at least one fid is required",
            ));
        }
        let req = DeleteFilesRequest {
            action_type: 2,
            exclude_fids: Vec::new(),
            filelist: fids.iter().map(|fid| fid.as_ref().to_string()).collect(),
        };
        let res: DeleteFilesResponse = self
            .post_json(
                format!(
                    "{}/1/clouddrive/file/delete?pr=ucpro&fr=pc",
                    self.config.api_base_url
                ),
                &req,
            )
            .await?;
        self.ensure_ok(res)?;
        Ok(())
    }

    /// Moves or copies `fids` into `to_pdir_fid` and returns the task id the
    /// caller can poll (`/1/clouddrive/task`). `overwrite` picks action_type 2.
    pub async fn transfer_files(
        &self,
        fids: &[String],
        to_pdir_fid: &str,
        copy: bool,
        overwrite: bool,
    ) -> Result<String> {
        if fids.is_empty() {
            return Err(QuarkPanError::invalid_argument(
                "at least one fid is required",
            ));
        }
        let endpoint = if copy { "copy" } else { "move" };
        let req = TransferFilesRequest {
            action_type: if overwrite { 2 } else { 1 },
            exclude_fids: Vec::new(),
            filelist: fids.to_vec(),
            to_pdir_fid: to_pdir_fid.to_string(),
        };
        let res: TransferFilesResponse = self
            .post_json(
                format!(
                    "{}/1/clouddrive/file/{}?pr=ucpro&fr=pc",
                    self.config.api_base_url, endpoint
                ),
                &req,
            )
            .await?;
        let res = self.ensure_ok(res)?;
        Ok(res.data.task_id)
    }

    /// Polls `/1/clouddrive/task` until the task reports success (`status == 2`).
    pub async fn wait_task(&self, task_id: &str) -> Result<()> {
        if task_id.is_empty() {
            return Ok(());
        }
        for index in 0..60u32 {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let res: TaskResponse = self
                .get_json(format!(
                    "{}/1/clouddrive/task?pr=ucpro&fr=pc&task_id={}&retry_index={}",
                    self.config.api_base_url, task_id, index
                ))
                .await?;
            let res = self.ensure_ok(res)?;
            match res.data.status {
                2 => return Ok(()),
                s if s < 0 => {
                    return Err(QuarkPanError::invalid_argument(format!(
                        "任务失败：{}",
                        res.data.message
                    )))
                }
                _ => {}
            }
        }
        Err(QuarkPanError::invalid_argument("任务超时"))
    }

    pub async fn list_folder(&self, pdir_fid: &str, page: u32, size: u32) -> Result<ListPage> {
        let res: ListFolderResponse = self
            .get_json(
                format!(
                    "{}/1/clouddrive/file/sort?pr=ucpro&fr=pc&&pdir_fid={}&_page={}&_size={}&_fetch_total=1&_fetch_sub_dirs=0&_sort=file_type:asc,updated_at:desc,",
                    self.config.api_base_url, pdir_fid, page, size
                ),
            )
            .await?;
        let res = self.ensure_ok(res)?;
        Ok(ListPage {
            entries: res.data.list,
            page: res.metadata.page,
            size: res.metadata.count,
            total: res.metadata.total,
        })
    }

    pub async fn up_pre(
        &self,
        file_name: &str,
        size: u64,
        pdir_fid: &str,
    ) -> Result<UpPreResponse> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|e| QuarkPanError::invalid_argument(e.to_string()))?
            .as_millis() as u64;
        let req = UpPreRequest {
            file_name: file_name.to_string(),
            size,
            pdir_fid: pdir_fid.to_string(),
            format_type: get_format_type(file_name).to_string(),
            ccp_hash_update: true,
            l_created_at: now,
            l_updated_at: now,
            dir_name: String::new(),
            parallel_upload: false,
        };
        let res: UpPreResponse = self
            .post_json(
                format!(
                    "{}/1/clouddrive/file/upload/pre?pr=ucpro&fr=pc",
                    self.config.api_base_url
                ),
                &req,
            )
            .await?;
        self.ensure_ok(res)
    }

    pub async fn up_hash(&self, md5: &str, sha1: &str, task_id: &str) -> Result<UpHashResponse> {
        let req = UpHashRequest {
            md5: md5.to_string(),
            sha1: sha1.to_string(),
            task_id: task_id.to_string(),
        };
        let res: UpHashResponse = self
            .post_json(
                format!(
                    "{}/1/clouddrive/file/update/hash?pr=ucpro&fr=pc",
                    self.config.api_base_url
                ),
                &req,
            )
            .await?;
        self.ensure_ok(res)
    }

    pub async fn auth(
        &self,
        auth_info: &str,
        auth_meta: &str,
        task_id: &str,
    ) -> Result<AuthResponse> {
        let req = AuthRequest {
            auth_info: auth_info.to_string(),
            auth_meta: auth_meta.to_string(),
            task_id: task_id.to_string(),
        };
        let res: AuthResponse = self
            .post_json(
                format!(
                    "{}/1/clouddrive/file/upload/auth?pr=ucpro&fr=pc",
                    self.config.api_base_url
                ),
                &req,
            )
            .await?;
        self.ensure_ok(res)
    }

    pub async fn finish(&self, obj_key: &str, task_id: &str) -> Result<FinishResponse> {
        let req = FinishRequest {
            obj_key: obj_key.to_string(),
            task_id: task_id.to_string(),
        };
        let res: FinishResponse = self
            .post_json(
                format!(
                    "{}/1/clouddrive/file/upload/finish?pr=ucpro&fr=pc",
                    self.config.api_base_url
                ),
                &req,
            )
            .await?;
        self.ensure_ok(res)
    }

    pub fn up_part_auth_meta(
        &self,
        mime_type: &str,
        utc_time: &str,
        bucket: &str,
        obj_key: &str,
        part_number: u32,
        upload_id: &str,
    ) -> String {
        format!(
            "PUT\n\n{mime_type}\n{utc_time}\nx-oss-date:{utc_time}\nx-oss-user-agent:aliyun-sdk-js/6.6.1 Chrome 98.0.4758.80 on Windows 10 64-bit\n/{bucket}/{obj_key}?partNumber={part_number}&uploadId={upload_id}"
        )
    }

    pub async fn up_part(&self, req: UpPartMethodRequest) -> Result<String> {
        let oss_url = format!(
            "https://{}.{}//{}?partNumber={}&uploadId={}",
            req.bucket, req.upload_url, req.obj_key, req.part_number, req.upload_id
        );
        let res = self
            .client
            .put(oss_url)
            .header("Authorization", req.auth_key)
            .header("Content-Type", req.mime_type)
            .header("x-oss-date", req.utc_time)
            .header(
                "x-oss-user-agent",
                "aliyun-sdk-js/6.6.1 Chrome 98.0.4758.80 on Windows 10 64-bit",
            )
            .header("Referer", REFERER)
            .body(req.part_bytes)
            .send()
            .await?;
        let res = res.error_for_status()?;
        let etag = res
            .headers()
            .get("Etag")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| QuarkPanError::invalid_argument("missing Etag in upload response"))?;
        Ok(etag.to_string())
    }

    pub async fn up_auth_and_commit(&self, req: UpAuthAndCommitRequest) -> Result<()> {
        let xml_body = build_complete_upload_xml(&req.md5s);
        let digest = md5::compute(xml_body.as_bytes());
        let content_md5 = general_purpose::STANDARD.encode(digest.0);
        let callback_base64 = general_purpose::STANDARD.encode(serde_json::to_vec(&req.callback)?);
        let time_str = chrono::Utc::now()
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let auth_meta = format!(
            "POST\n{}\napplication/xml\n{}\nx-oss-callback:{}\nx-oss-date:{}\nx-oss-user-agent:aliyun-sdk-js/6.6.1 Chrome 98.0.4758.80 on Windows 10 64-bit\n/{}/{}?uploadId={}",
            content_md5,
            time_str,
            callback_base64,
            time_str,
            req.bucket,
            req.obj_key,
            req.upload_id
        );
        let auth_res = self.auth(&req.auth_info, &auth_meta, &req.task_id).await?;
        let auth_key = auth_res.data.auth_key;
        let commit_url = format!(
            "https://{}.{}/{}?uploadId={}",
            req.bucket, req.upload_url, req.obj_key, req.upload_id
        );
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", HeaderValue::from_str(&auth_key)?);
        headers.insert("Content-MD5", HeaderValue::from_str(&content_md5)?);
        headers.insert("Content-Type", HeaderValue::from_static("application/xml"));
        headers.insert("x-oss-callback", HeaderValue::from_str(&callback_base64)?);
        headers.insert("x-oss-date", HeaderValue::from_str(&time_str)?);
        headers.insert(
            "x-oss-user-agent",
            HeaderValue::from_static(
                "aliyun-sdk-js/6.6.1 Chrome 98.0.4758.80 on Windows 10 64-bit",
            ),
        );
        headers.insert("Referer", HeaderValue::from_static(REFERER));
        let _: EmptyData = self
            .post_xml(commit_url, xml_body, headers)
            .await
            .or_else(|err| match err {
                QuarkPanError::Serde(_) => Ok(EmptyData {}),
                _ => Err(err),
            })?;
        Ok(())
    }

    /// Opens a byte stream from `start_offset` to the end of the file.
    ///
    /// Kept for callers that only resume; `download_stream_range` is the one
    /// `DownloadBuilder` uses.
    #[allow(dead_code)]
    pub async fn download_stream(
        &self,
        fid: &str,
        start_offset: Option<u64>,
    ) -> Result<crate::model::BoxByteStream> {
        Ok(self
            .download_stream_range(fid, start_offset, None)
            .await?
            .0)
    }

    /// Opens a byte stream for `[start_offset, end_offset)` of the target file.
    ///
    /// `end_offset` is exclusive, so the request asks for
    /// `Range: bytes=start-end-1`. `None` keeps the open-ended behaviour that
    /// resume uses.
    /// The second half of the result is the raw `Content-Range` header.
    ///
    /// A segmented download has to verify that the CDN started where it was
    /// asked to: bytes served from a different offset would be written into the
    /// wrong place and produce a file of exactly the right length that is
    /// silently corrupt. `None` means the CDN did not send the header, which is
    /// not itself an error.
    pub async fn download_stream_range(
        &self,
        fid: &str,
        start_offset: Option<u64>,
        end_offset: Option<u64>,
    ) -> Result<(crate::model::BoxByteStream, Option<String>)> {
        let url = self.get_download_url(fid).await?;
        let cookie = self.resolve_cookies().await;
        let mut req = self.download_client.get(url).header("Cookie", cookie);
        match (start_offset, end_offset) {
            (Some(start), Some(end)) if end > start => {
                req = req.header(RANGE, format!("bytes={start}-{}", end - 1));
            }
            (Some(start), _) => {
                req = req.header(RANGE, format!("bytes={start}-"));
            }
            _ => {}
        }
        let res = req.send().await?;
        let res = res.error_for_status()?;
        if start_offset.is_some() && res.status() != StatusCode::PARTIAL_CONTENT {
            return Err(QuarkPanError::invalid_argument(
                "server did not honor range request for resume download",
            ));
        }
        let served = res
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let stream = res
            .bytes_stream()
            .map(|chunk| chunk.map_err(QuarkPanError::from));
        Ok((Box::pin(stream), served))
    }
}

impl ApiClient {
    fn rename_api_base_url(&self) -> String {
        self.config
            .api_base_url
            .replace("https://drive.quark.cn", "https://drive-pc.quark.cn")
            .replace("http://drive.quark.cn", "http://drive-pc.quark.cn")
    }
}

fn build_complete_upload_xml(md5s: &[String]) -> String {
    let mut xml_body =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<CompleteMultipartUpload>\n");
    for (i, md5) in md5s.iter().enumerate() {
        xml_body.push_str(&format!(
            "<Part>\n<PartNumber>{}</PartNumber>\n<ETag>{}</ETag>\n</Part>\n",
            i + 1,
            md5
        ));
    }
    xml_body.push_str("</CompleteMultipartUpload>");
    xml_body
}

fn get_format_type(file_name: &str) -> &'static str {
    mime_guess::from_path(file_name)
        .first_raw()
        .unwrap_or("application/octet-stream")
}
