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

## 通用约定

- `provider_login(provider, credential)` 是唯一的登录 IPC，前端按网盘拼 `credential`。
- 登录成功后 `state.invalidate_providers()` 强制重建注册表，再读 `auth_states`。
- 内嵌登录窗口关闭时后端会发 `登录窗口已关闭`，前端必须清掉"等待中"状态，
  否则永久转圈。
