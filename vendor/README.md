# vendor/ — 内置的第三方 Rust crate

`src-tauri/Cargo.toml` 通过相对路径依赖这两个 crate，**没有走 crates.io，也没有走 git 依赖**：

```toml
libquarkpan = { path = "../vendor/libquarkpan", default-features = false, features = ["rustls"] }
cloud139    = { path = "../vendor/cloud139" }
```

原因有两个：

1. 这两个上游项目原本是 `src-tauri` 的**同级目录**（`../../quark-cli/libquarkpan`、`../../cloud139`），只在本机成立。仓库一旦推到 GitHub，CI 的 `actions/checkout` 只拿到本仓库，`../..` 不存在，`cargo` 会直接报 `failed to load manifest for dependency`。
2. 我们依赖的**不是上游发版内容，而是带本地补丁的工作区版本**（见下面「打了哪些补丁」）。改用上游 git 依赖会编译失败。

## 来源与版本

| 目录 | 上游 | 版本 | 基线提交 | 许可证 |
| --- | --- | --- | --- | --- |
| `vendor/libquarkpan` | [i-sync/quark-cli](https://github.com/i-sync/quark-cli) 的 `libquarkpan/` 成员 | 0.4.2 | `9713f53` (main) | GPL-3.0-only |
| `vendor/cloud139` | [Cnotech/cloud139](https://github.com/Cnotech/cloud139) | 2.2.0 | `d3ebd27` (main) | MIT |

两者都以**直接链接**的方式进入 `yunyu-drive`，因此 `NOTICE` 里已经声明了对应关系，本项目的 GPL-3.0-or-later 与 GPL-3.0-only 兼容。

## 打了哪些补丁

复制进来的不是干净的 `v0.4.2` / `v2.2.0`，而是上游工作区里**未提交的改动**。这些改动修的是本项目实际踩到的 bug，删掉会出问题：

### libquarkpan（+124 行）

- `src/api.rs` 新增 `transfer_files(&self, fids: &[String], to_pdir_fid: &str, copy: bool, overwrite: bool)` —— 夸克的「移动 / 复制文件」接口（`/file/transfer`，任务型），需要轮询任务结果。
- `src/lib.rs` 新增 `QuarkPan::move_to(fids, to_pdir_fid)` 与 `QuarkPan::copy_to(fids, to_pdir_fid)`，内部调 `transfer_files` + `wait_task`。被 `src-tauri/src/providers/quark.rs` 的移动 / 复制功能使用。
- `src/model.rs` 新增上述接口的请求 / 响应结构体。
- `src/download.rs`：下载客户端 `.timeout(300s)` 改为 `.read_timeout(60s)` —— 修「大文件下载中途整体超时」：总时长限制改成单次读空闲限制。

### cloud139（+9 / -18 行）

- `src/client/api.rs` 的 `get_file_id_by_path` 改为走 `list_personal_files` 分页遍历 —— 修「目录里超过 100 个项目时找不到目标文件」（原实现只取第一页，且只匹配 100 条以内的结果）。

## 与上游的差异如何核对

两个目录的 `src/` 与上游工作区**逐字节一致**。要复核，把上游克隆到同级目录后：

```sh
diff -r ../quark-cli/libquarkpan/src vendor/libquarkpan/src   # 应无输出
diff -r ../cloud139/src        vendor/cloud139/src            # 应无输出
```

## 为了能独立编译做了什么改动

`libquarkpan` 是 `quark-cli` 的 **workspace 成员**，它的 `Cargo.toml` 原本大量使用 `version = { workspace = true }`、`homepage/repository = { workspace = true }`。脱离 workspace 后这些字段无法解析，因此 `vendor/libquarkpan/Cargo.toml` 里把它们**写成了字面值**（值取自 `quark-cli/Cargo.toml` 的 `[workspace.package]`）：

```toml
version = "0.4.2"
homepage = "https://github.com/i-sync/quark-cli"
repository = "https://github.com/i-sync/quark-cli"
```

这是**唯一**一处为了 vendoring 而改的文件；`src/` 下的代码一行未动。

`cloud139` 是独立 crate（没有 workspace 继承），`Cargo.toml` 原样保留。

## 从 `vendor/cloud139` 里删掉了什么

上游仓库同时是 CLI 应用，仓库里有一堆只服务于命令行形态的东西，作为库使用不需要，已删除以减小仓库体积（约 1020K → 568K）：

- `package.json`、`pnpm-lock.yaml`、`scripts/`、`docs/`、`AGENTS.md`、`CLAUDE.md`、`.agents/`
- `src/main.rs`、`src/cli/`、`src/commands/`、`src/presentation/`

`Cargo.toml` 里的 `[[bin]] name = "cloud139"` 段仍指向 `src/main.rs`，但 `cargo` 只在构建该 crate 自己的二进制时才需要它；作为依赖被 `yunyu-drive` 编译时不会触碰。**如果以后要在这个目录里直接 `cargo build`，需要先把那段 `[[bin]]` 删掉。**

`libquarkpan` 只删了 `Cargo.lock` 与 `.gitignore`（`Cargo.lock` 由根 `src-tauri/Cargo.lock` 统一管理）。

## 怎么更新

上游更新后不要直接覆盖，否则会丢掉上面的补丁：

1. 在上游仓库里 `git pull`，确认 `git diff` 里的补丁还在（上游可能已经自己修了，那就该删掉我们的补丁）。
2. 把 `src/` 同步过来：`rsync -a --delete ../quark-cli/libquarkpan/src/ vendor/libquarkpan/src/`（`cloud139` 同理，但注意排除 `main.rs`、`cli/`、`commands/`、`presentation/`）。
3. 上游如果改了 `Cargo.toml`，手工把变化合并过来，**保持 workspace 继承字段为字面值**。
4. 上游如果升了版本号，同步改 `vendor/*/Cargo.toml` 的 `version`，并在根目录跑 `cargo update -p libquarkpan`（或 `-p cloud139`）让 `src-tauri/Cargo.lock` 跟上。
5. `cargo check --manifest-path src-tauri/Cargo.toml` 确认能编译。

## 为什么不 fork

这两个上游是参考项目，本项目没有维护权限。vendoring 是「让 CI 自洽」与「不丢本地补丁」之间唯一不需要外部配合的做法。如果将来补丁被上游合并，可以改回 git 依赖并删掉这个目录。
