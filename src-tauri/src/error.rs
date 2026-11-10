use serde::{Serialize, Serializer};

/// A flat error type that serialises to a plain string for the frontend.
#[derive(Debug, Clone)]
pub struct AppError(pub String);

impl AppError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AppError {}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl From<libquarkpan::QuarkPanError> for AppError {
    fn from(value: libquarkpan::QuarkPanError) -> Self {
        Self(describe(&value))
    }
}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self(format!("文件读写失败：{value}"))
    }
}

impl From<reqwest::Error> for AppError {
    fn from(value: reqwest::Error) -> Self {
        if value.is_timeout() {
            return Self("网络超时，请检查连接".to_string());
        }
        if value.is_connect() {
            return Self("无法连接到夸克服务器".to_string());
        }
        if let Some(status) = value.status() {
            return Self(match status.as_u16() {
                401 | 403 => "登录状态已失效，请重新设置 Cookie".to_string(),
                404 => "请求的内容不存在或已被删除".to_string(),
                429 => "请求过于频繁，请稍后再试".to_string(),
                code if code >= 500 => format!("夸克服务器暂时不可用（{code}），请稍后重试"),
                code => format!("接口错误（{code}）"),
            });
        }
        Self("网络异常，请稍后重试".to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        Self(format!("json: {value}"))
    }
}

impl From<tokio::task::JoinError> for AppError {
    fn from(value: tokio::task::JoinError) -> Self {
        Self(format!("task: {value}"))
    }
}

fn describe(err: &libquarkpan::QuarkPanError) -> String {
    use libquarkpan::QuarkPanError as E;
    match err {
        E::MissingField(field) => format!("缺少必要参数：{field}"),
        E::InvalidArgument(msg) => msg.clone(),
        E::Cancelled => "已取消".to_string(),
        E::Api { status, message } => match status {
            401 | 403 => "登录状态已失效，请重新设置 Cookie".to_string(),
            429 => "请求过于频繁，请稍后再试".to_string(),
            _ => format!("接口错误（{status}）：{message}"),
        },
        E::Http(e) => {
            if let Some(status) = e.status() {
                // Quark answers auth failures with a bare HTTP status, so keep the
                // message readable instead of dumping the whole request URL.
                return match status.as_u16() {
                    401 | 403 => "登录状态已失效，请重新设置 Cookie".to_string(),
                    404 => "请求的内容不存在或已被删除".to_string(),
                    429 => "请求过于频繁，请稍后再试".to_string(),
                    code if code >= 500 => format!("夸克服务器暂时不可用（{code}），请稍后重试"),
                    code => format!("接口错误（{code}）"),
                };
            }
            if e.is_timeout() {
                "网络超时，请检查连接".to_string()
            } else if e.is_connect() {
                "无法连接到夸克服务器".to_string()
            } else {
                "网络异常，请稍后重试".to_string()
            }
        }
        E::Serde(e) => format!("数据解析失败：{e}"),
        E::Io(e) => format!("文件读写失败：{e}"),
        E::HeaderValue(e) => format!("请求头无效：{e}"),
        E::UrlParse(e) => format!("地址解析失败：{e}"),
    }
}

pub type AppResult<T> = Result<T, AppError>;
