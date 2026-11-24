# 各网盘登录方式

每个网盘有独立的登录入口（`src/views/LoginView.vue`：先选网盘，再进入该网盘的登录页）。
凭据统一写到 `~/.config/yunyu-drive/`，权限 `0600`。

| 网盘 | 主路径 | 降级路径 | 凭据文件 |
| --- | --- | --- | --- |
| 夸克 | 内嵌窗口登录，抓 Cookie | 粘贴 Cookie | `cookie.txt` |
| 中国移动云盘 | 内嵌窗口登录，抓 `authorization` Cookie | 粘贴 Token | `yun139.toml` |
| 阿里云盘 | App 扫码（QR） | 粘贴 `refresh_token` | `aliyun.json` |
| 123 云盘 | 账号 + 密码 | — | `pan123.json` |
| 百度网盘 | 内嵌窗口登录，抓整包 Cookie | 粘贴 Cookie | `baidu.json` |
| 天翼云盘 | App 扫码（QR） | — | `cloud189.json` |

## 中国移动云盘

`yun.139.com` 的 Web 前端把登录凭据直接写在 Cookie 里，模块 `c276`：

```js
function v(e, t, a = 30) {                    // 写
  document.cookie = e + "=" + t + "; path=/;" + o + "; max-age=" + i;
}
function B(e) {                               // 读
  for (var t = e + "=", a = document.cookie.split(";"), ...) { ... }
}
```

要点：

- **没有 `Domain` 属性**，所以是 host-only cookie；WebKit 读回来时 domain 可能为空。
  判断归属时只能排除"已知是别家"的域，不能要求以 `139.com` 结尾。
- **没有 `escape()`**，值原样存放，不需要 percent-decode（代码里仍然解一次，无副作用）。
- 登录后的第一个 API 请求由 axios 拦截器写入 Cookie `authorization`，值形如
  `Basic base64("pc:<账号>:<token>")`。`cloud139` 的 `Config.authorization`
  只存 `Basic ` 之后的部分。
- 因为凭据是 host-only cookie，抓取逻辑见 `src-tauri/src/login.rs::cookie_value_139`。

拿到后立刻用 `list("0")` 验证一次；被拒绝就回滚到上一个 `yun139.toml`，
否则界面会显示"已登录"而实际列表报错。

## 阿里云盘

扫码流程参考 `aliyundrive-cli/src/drive/login/`，实现在
`src-tauri/src/providers/aliyun_qr.rs`。

1. `GET passport.aliyundrive.com/newlogin/qrcode/generate.do?appName=aliyun_drive&fromSite=52&appEntrance=web`
   → `{ t, ck, content: { data: { codeContent } } }`
2. `POST .../qrcode/query.do?appName=aliyun_drive&fromSite=52`（form: `t`, `ck`）
   → `data.qrCodeStatus` ∈ `NEW | SCANED | CONFIRMED | EXPIRED`，`data.bizExt` 为 base64。
3. `CONFIRMED` 时 `bizExt` 解 base64 得 JSON，取 `pds_login_result.accessToken`
   （App 侧 token）。**注意字段是 camelCase**，`biz_ext` 才是 snake_case。
4. `GET auth.aliyundrive.com/v2/oauth/authorize?...` 拿 `SESSIONID` Cookie。
5. `POST auth.aliyundrive.com/v2/oauth/token_login`（Cookie: `SESSIONID=…`，body 为 App token）
   → **`{"goto": "https://www.aliyundrive.com/sign/callback?code=<授权码>&…"}`**
   —— 授权码是 `goto` URL 的查询参数，不是 JSON 字段。早期实现把 `goto` 当对象取
   `/goto/authorization_code`，永远取不到，于是扫码后表现为"二维码过期"。
6. `POST api.aliyundrive.com/token/get`（Cookie 同上，body `{ code, loginType: "normal" }`）
   → `refresh_token` / `access_token`，存进 `aliyun.json`。

## 123 云盘

Android 协议，见 `src-tauri/src/providers/pan123.rs`。`provider_login` 把
`账号\u001f密码` 打包成一个字符串传进来，登录返回后立即 `list("0")` 验证，
避免拿到无效 token 却报告成功。

## 百度网盘

走网页会话，实现在 `src-tauri/src/providers/baidu.rs` + `src-tauri/src/login.rs`。

- 内嵌窗口打开 `pan.baidu.com`，登录后收割 **整包 cookie**（不按名字白名单：百度会改名、
  加 `BDUSS_BFESS` / `STOKEN_BFESS` 这类变体）。
- 少 `PANPSC` 时接口一律回 `errno: -6`（未登录），所以登录窗口要等到 `pan.baidu.com`
  的 cookie 真正出现才收割。
- 再用 `GET /api/gettemplatevariable?fields=["uk","username","bdstoken","is_vip"]`
  换 `bdstoken` 与昵称，连同 cookie 一起存进 `baidu.json`。
- 不需要开发者 app_key（OpenAPI 路线需要，本机没有）。

## 天翼云盘

走网页版会话签名接口，实现在 `src-tauri/src/providers/cloud189.rs` +
`src-tauri/src/providers/cloud189_qr.rs`。没有可用的密码登录：会话密钥只有扫码
（或密码登录 + 短信二次校验）才拿得到，所以登录页只提供二维码。

**下载连接数**：天翼云盘按**单连接**限速，不是按账号 —— 2026-11 对同一 31 MB 文件
实测（每档两轮各 4 秒取中位）：1 路 1.2 MB/s、16 路 4.0、32 路 7.5、64 路 8.5、
96 路 10.2、**128 路 10.7**、192 路 10.1。所以 `engine/budget.rs` 里天翼的
profile 是 floor 8 / ceiling 128（默认 GENERIC 只有 64），且它与夸克、阿里各自
独立计账（`engine/mod.rs` 的 `bucket()`），不共用连接预算。

扫码链路（`appId=8025431004`，这个 appId 的登录页仍会把参数直接写在 HTML 里）：

1. `GET cloud.189.cn/unifyLoginForPC.action?appId=8025431004&clientType=10020&…`
   跟随重定向落到 `open.e.189.cn/api/logbox/oauth2/unifyAccountLogin.do`，
   从页面里正则取 `lt = "…"`、`paramId = "…"`、`reqId = "…"`。
   **注意**：页面里还有一句 `lt="二维码登录成功"`，正则必须锚定带空格的 `lt = "`。
2. `POST …/oauth2/getUUID.do` 拿 `uuid` / `encryuuid`。二维码内容就是 `uuid` 本身。
3. 轮询 `POST …/oauth2/qrcodeLoginState.do`（`clientType=1`，`date` 是北京时间
   `%Y-%m-%d%H:%M:%S.%3f`，**日期与小时之间没有分隔符**）：`-106` 待扫码、
   `-11002` 已扫码待确认、`-11001` 过期、`0` 成功并返回 `redirectUrl`。
   缺 `user-finger` 头会回 `22001 非正确来源`。
4. `POST api.cloud.189.cn/getSessionForPC.action?redirectURL=…` 换到
   `sessionKey` / `sessionSecret`，存进 `cloud189.json`（0600）。

签名（每个业务接口都要）：

```
plain = "SessionKey=<k>&Operate=<METHOD>&RequestURI=<path>&Date=<GMT>"
        [ + "&params=<AES-ECB-hex-upper>" ]
Signature = HMAC-SHA1(sessionSecret, plain) 的**大写** hex
```

`params` 是 AES-128-ECB + PKCS7、key = `sessionSecret[:16]`、输出大写 hex；
`RequestURI` 只取 path（不含 host 与 query）；所有请求再附一段
`clientType=TELEPC&version=7.2.4.0&channelId=web_cloud.189.cn&rand=…`。
未签名的请求统一回 `{"errorCode":"InvalidArgument","errorMsg":"date/signature is null"}`。

### 分享（`open/*` 家族）

分享用的是另一套签名，跟会话接口完全不同，实现在 `cloud189.rs` 的 `open_post()`：

- `AccessToken` 不能直接用 `cloud189.json` 里那个，必须用 sessionKey 现换一次：
  `GET api.cloud.189.cn/open/oauth2/getAccessTokenBySsKey.action?sessionKey=…`，
  头 `AppKey: 601102120`、`Timestamp`、`Signature = md5("AppKey=…&Timestamp=…&sessionKey=…")`
  （**小写** hex）、`Sign-Type: 1`。换来的 token 缓存在内存里，进程退出即失效。
- 业务调用：头 `AccessToken` / `Timestamp` / `Signature` / `Sign-Type: 1`，
  签名集合 = `{AccessToken, Timestamp}` **加上 POST 的 form 字段**，
  `Signature = md5(按 key 排序的 "k=v" 用 & 连接)`。
- **必须用 POST + form-urlencoded**。同样的参数放 query 会回
  `accessTokenSignature is not match`。
- 链接里的短码先 `checkAccessCode.action` 换成数字 `shareId`，之后所有调用都用它。
- 文件夹分享列目录必须带 `shareMode=3`（取 `getShareInfoByCodeV2` 返回的原值），
  否则一律 `Argument invalid`。
- `createShareLink.action` 的 `expireTime` 是**天数**（`1`/`7`/`30`，`0` 或 `2099` 为永久），
  `withAccessCode` 与自定义提取码都会被忽略：服务端总是发一个 4 位访问码。
- 多选分享没有可用接口（`createBatchShare.action` 试过所有 body 形态都是
  `Argument invalid`），所以一次只能分享一个文件或文件夹。
- 取消分享回的是**空响应体**，不是 JSON。

## 通用约定

- `provider_login(provider, credential)` 是唯一的登录 IPC，前端按网盘拼 `credential`。
- 登录成功后 `state.invalidate_providers()` 强制重建注册表，再读 `auth_states`。
- 内嵌登录窗口关闭时后端会发 `登录窗口已关闭`，前端必须清掉"等待中"状态，
  否则永久转圈。
