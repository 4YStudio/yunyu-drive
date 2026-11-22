//! 版本号的唯一来源。
//!
//! `package.json` 的 `version` 是唯一真源，`build.rs` 把它注入成
//! `YUNYU_VERSION` 这个编译期环境变量。这里再包一层，好处是：
//!
//!   - 全仓库只有这一处出现 `env!("YUNYU_VERSION")`，改注入方式只动一个文件；
//!   - 调用点写 `version::VERSION` 比 `env!("YUNYU_VERSION")` 更清楚它是什么。
//!
//! 注意 `Cargo.toml` 里没有 `version` 字段（缺省 0.0.0），所以
//! **不要**用 `env!("CARGO_PKG_VERSION")` 取版本，那样会得到 0.0.0。

/// 应用版本，例如 `0.1.0`，来自 `package.json`。
pub const VERSION: &str = env!("YUNYU_VERSION");
