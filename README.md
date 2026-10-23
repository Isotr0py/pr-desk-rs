# PR Desk (pr-desk-rs)

PR Desk 风格的 GitHub PR 预评审桌面客户端（Rust + Slint），与 AstrBot 插件
[`astrbot_plugin_github_pr_review`](../astrbot_plugin_github_pr_review) 配套：
插件负责评审管线（LLM 评审、SQLite 存储、QQ 推送），桌面端提供本地高密度工作台 UI，
并通过 sync API 双向同步状态（置顶/优先级/看板/标签/待办等本地操作同步回插件）。

## 构建与运行

- 工具链：**x86_64-pc-windows-gnu**（纯 Rust 链路，无 MSVC/OpenSSL 依赖）
- 网络栈：reqwest + rustls；数据库：rusqlite(bundled)；UI：Slint

```bash
cargo run --release
```

### Slint 在 GNU 下的可用 features 组合

默认 features（`std` + `backend-winit` + `renderer-femtovg`）即可编译。

> 注意：Slint 首次全量编译体积较大（数百个 crate）。构建机内存不足时
> rustc 可能因 Windows 页面文件压力崩溃（STATUS_STACK_BUFFER_OVERRUN / mmap 失败）——
> 用低并发构建规避：`CARGO_INCREMENTAL=0 cargo build -j1`（或先释放内存再正常构建）。

若 femtovg 渲染器在某环境编不过，可退到软件渲染：

```toml
slint = { version = "1", default-features = false,
          features = ["std", "backend-winit", "renderer-software"] }
```

## 配置

首次启动在当前目录生成 `config.toml` 模板：

```toml
server_url = "http://127.0.0.1:6185"   # AstrBot 地址
sync_token = ""                        # 插件配置里的 sync_token（留空 = 仅本地浏览，不同步）
repos = ["owner/repo"]                 # 关注的仓库（拉 open PR）
poll_interval_min = 5                  # 自动同步间隔（分钟）
```

GitHub token 从环境变量 `GITHUB_TOKEN` 读取（不写任何文件）。

## 功能（Phase 1）

- 本地 SQLite（schema 对齐插件 prs/reviews/todos + sync_state 游标 + outbox 改动队列，`user_version` 迁移）
- GitHub 拉取 open PR 列表（启动时 + 手动刷新）
- 同步引擎：`sync_pull` 增量拉 → 本地 upsert；本地改动先入队 → `sync_push` → 冲突服务器赢（服务器值写回本地）；同步按钮 + 每 5 分钟自动
- UI：分区列表（Pinned/看板/待评审/已评审；行含 #、标题、作者、评分、CI/Review 状态、优先级）、右侧详情（评审摘要 + 着色 diff 视图，diff 来自 GitHub API 并本地缓存）
- 快捷键：`j`/`k` 上下、`Enter` 详情、`Esc` 关闭、`p` 置顶、`1`/`2`/`3` 优先级

## 测试

```bash
cargo test
```

覆盖：sync 合并（冲突服务器赢 / applied 不覆盖 / outbox 合并 / pull upsert）、
本地 upsert 幂等、游标/outbox/diff 缓存、diff 行分类与按文件切分、行模型分组。

## Phase 2 待办

- 待办 Tab / 队列 Tab / 看板 Tab 的桌面端版本
- todos 双向同步（当前 sync_push 只推 prs 改动）
- diff 逐文件折叠与跳文件
- 主题跟随 / 自定义样式打磨
- 打包分发（cargo-dist / 安装包）

## License

[GPLv3](LICENSE)（Slint 为 GPLv3/商业双授权，本项目走 GPLv3 路径）。
