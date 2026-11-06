//! pr-desk-rs 库：本地 SQLite、GitHub 拉取、同步引擎、diff 解析、行分组。
//! UI 无关的纯逻辑都在库里（可用 `cargo test --lib` 单独测试，不编译 Slint）。

pub mod circle_crop;
pub mod config;
pub mod db;
pub mod diffview;
pub mod github;
pub mod i18n;
pub mod markdown;
pub mod ocr;
pub mod reviewer;
pub mod selection;
pub mod sync;

use anyhow::Context;

/// exe 所在目录（便携应用锚点）；取不到 exe 路径时回退当前目录。
pub fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// 由 exe 目录推导数据目录；抽出来便于测试注入路径，不碰真实 exe 目录。
fn data_dir_from(exe_dir: &std::path::Path) -> std::path::PathBuf {
    exe_dir.join("data")
}

/// 数据目录锚定到 exe 旁（`exe_dir()/data`），防止 cwd 不同时读到另一份数据。
pub fn exe_data_dir() -> std::path::PathBuf {
    data_dir_from(&exe_dir())
}

/// 数据目录覆盖：默认 exe 旁 data/（便携应用惯例）；启动时可经 `set_data_dir`
/// 锚定到 config.toml 所在目录——开发期从仓库根跑 exe 时，cargo clean 清 target/
/// 不会连数据库一起删掉（真实教训）。
static DATA_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_data_dir(dir: std::path::PathBuf) {
    let _ = DATA_DIR.set(dir);
}

pub fn data_dir() -> std::path::PathBuf {
    DATA_DIR.get().cloned().unwrap_or_else(exe_data_dir)
}

/// 统一 reqwest ClientBuilder 构造：user_agent + 超时 + 可选代理。
/// reqwest 是精简引入（没开 system-proxy），系统代理/HTTP_PROXY 等环境变量一律不生效，
/// 只认配置里的显式 proxy 字段：非空时走该代理（http:// 或 socks5://），空 = 直连。
/// 代理地址非法时返回带中文说明的错误（调用方按各自降级策略处理）。
pub fn http_client_builder(
    proxy: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<reqwest::ClientBuilder> {
    let mut b = reqwest::Client::builder()
        .user_agent("pr-desk-rs")
        .timeout(timeout);
    let proxy = proxy.trim();
    if !proxy.is_empty() {
        // 先查 scheme 再交给 reqwest 解析：无 scheme（如 127.0.0.1:7890）或非常见 scheme
        // 时 reqwest 的报错不直观，这里直接给出格式要求
        let scheme_ok = proxy.starts_with("http://")
            || proxy.starts_with("https://")
            || proxy.starts_with("socks5://")
            || proxy.starts_with("socks5h://");
        anyhow::ensure!(
            scheme_ok,
            "代理地址格式无效：{proxy}（需形如 http://host:port 或 socks5://host:port）"
        );
        let p = reqwest::Proxy::all(proxy)
            .context("代理地址解析失败（需形如 http://host:port 或 socks5://host:port）")?;
        b = b.proxy(p);
    }
    Ok(b)
}

/// 统一 reqwest Client 构造（http_client_builder 的便捷封装）。
pub fn http_client(proxy: &str, timeout: std::time::Duration) -> anyhow::Result<reqwest::Client> {
    Ok(http_client_builder(proxy, timeout)?.build()?)
}

/// 行分组结果。key 是稳定的组标识（"pinned"/"board"/"needs-review"/"reviewed"/
/// "rejected"/"closed"），不随语言/文案变化——UI 层的折叠状态恢复与图标映射按 key 消费；
/// title 仅用于显示（经 i18n 随语言切换，不含数量，数量徽章由 UI 层用组头行的 gcount 画）。
pub struct GroupSection {
    pub key: &'static str,
    pub title: String,
    pub idxs: Vec<usize>,
}

/// 行数据 → 分组。分区顺序固定：Pinned → 看板 → 待评审 → 已评审 → 不采纳 → 已合入/已关闭；
/// 行只出现在一个组里；pinned 独立成组（不再出现在其状态组）。空组不返回。
/// sort_col / sort_asc：组内排序（Excel 式点列标题）。sort_col="upd"（默认）=
/// updated_at 降序，即 SQL 输入序，不动。
/// lang："zh"/"en"，组标题文案经 i18n::tr 取对应语言（key 不受影响）。
pub fn build_sections(
    rows: &[serde_json::Value],
    lang: &str,
    sort_col: &str,
    sort_asc: bool,
) -> Vec<GroupSection> {
    let mut out: Vec<GroupSection> = Vec::new();
    let mut emit = |key: &'static str, title_key: &str, pred: &dyn Fn(&serde_json::Value) -> bool| {
        let mut idx: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| pred(r))
            .map(|(i, _)| i)
            .collect();
        if sort_col != "upd" {
            // 默认列之外才重排（upd 降序 = SQL 输入序，省一次排序且零风险）
            idx.sort_by(|&a, &b| {
                let o = col_cmp(&rows[a], &rows[b], sort_col);
                if sort_asc { o } else { o.reverse() }
            });
        } else if sort_asc {
            // upd 升序（最旧在前）：输入序反转
            idx.reverse();
        }
        if !idx.is_empty() {
            out.push(GroupSection { key, title: i18n::tr(lang, title_key).to_string(), idxs: idx });
        }
    };
    // 组标题纯文本（图标由 UI 层按 PrGroup.key 画 SVG；📌 等字形在部分字体下会豆腐块）
    emit("pinned", "section_pinned", &|r| r["pinned"].as_i64() == Some(1));
    emit("board", "section_board", &|r| {
        r["pinned"].as_i64() != Some(1) && r["on_board"].as_i64() == Some(1)
    });
    emit("needs-review", "section_pending", &|r| {
        r["pinned"].as_i64() != Some(1)
            && r["on_board"].as_i64() != Some(1)
            && r["status"].as_str() == Some("pending")
    });
    emit("reviewed", "section_reviewed", &|r| {
        r["pinned"].as_i64() != Some(1)
            && r["on_board"].as_i64() != Some(1)
            && matches!(r["status"].as_str(), Some("reviewed") | Some("failed"))
    });
    emit("rejected", "section_rejected", &|r| {
        r["pinned"].as_i64() != Some(1)
            && r["on_board"].as_i64() != Some(1)
            && r["status"].as_str() == Some("rejected")
    });
    emit("closed", "section_closed", &|r| {
        r["pinned"].as_i64() != Some(1)
            && r["on_board"].as_i64() != Some(1)
            && matches!(r["status"].as_str(), Some("merged") | Some("closed"))
    });
    out
}

/// 列排序比较器（升序语义；调用方按 sort_asc 决定是否反转）。
/// 未知列回退 updated_at。
fn col_cmp(a: &serde_json::Value, b: &serde_json::Value, col: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let str_of = |r: &serde_json::Value, k: &str| r[k].as_str().unwrap_or("").to_lowercase();
    let num = |r: &serde_json::Value, k: &str| r[k].as_i64().unwrap_or(0);
    match col {
        "priority" => num(a, "priority").cmp(&num(b, "priority")),
        "num" => num(a, "number").cmp(&num(b, "number")),
        "title" => str_of(a, "title").cmp(&str_of(b, "title")),
        "author" => str_of(a, "author").cmp(&str_of(b, "author")),
        "turn" => str_of(a, "turn").cmp(&str_of(b, "turn")),
        "review" => str_of(a, "review_decision").cmp(&str_of(b, "review_decision")),
        // CI：failure < pending < 无/其它 < success（升序 = 失败在前）
        "ci" => {
            let rank = |r: &serde_json::Value| match r["ci_status"].as_str().unwrap_or("") {
                "failure" => 0,
                "pending" => 1,
                "success" => 3,
                _ => 2,
            };
            rank(a).cmp(&rank(b))
        }
        // Size：XS < S < M < L < XL
        "size" => {
            let rank = |r: &serde_json::Value| match r["size_label"].as_str().unwrap_or("") {
                "XS" => 0,
                "S" => 1,
                "M" => 2,
                "L" => 3,
                "XL" => 4,
                _ => 2,
            };
            rank(a).cmp(&rank(b))
        }
        "changes" => {
            let ca = num(a, "additions") + num(a, "deletions");
            let cb = num(b, "additions") + num(b, "deletions");
            ca.cmp(&cb)
        }
        "score" => {
            let sa = a["score"].as_f64().unwrap_or(0.0);
            let sb = b["score"].as_f64().unwrap_or(0.0);
            sa.partial_cmp(&sb).unwrap_or(Ordering::Equal)
        }
        // "upd"/"age"/默认：updated_at 是 ISO8601 字符串，字典序即时间序
        _ => str_of(a, "updated_at").cmp(&str_of(b, "updated_at")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(n: i64, pinned: i64, on_board: i64, status: &str) -> serde_json::Value {
        serde_json::json!({
            "repo": "o/r", "number": n, "title": format!("t{n}"), "author": "a",
            "pinned": pinned, "on_board": on_board, "status": status,
        })
    }

    #[test]
    fn data_dir_from_joins_under_injected_dir() {
        assert_eq!(
            data_dir_from(std::path::Path::new("C:/fake/exe-dir")),
            std::path::Path::new("C:/fake/exe-dir").join("data")
        );
        assert_eq!(
            data_dir_from(std::path::Path::new(".")),
            std::path::PathBuf::from("./data")
        );
    }

    #[test]
    fn sections_grouping() {
        let rows = vec![
            row(1, 1, 0, "pending"),  // pinned
            row(2, 0, 1, "pending"),  // board
            row(3, 0, 0, "pending"),  // 待评审
            row(4, 0, 0, "reviewed"), // 已评审
        ];
        let sections = build_sections(&rows, "zh", "upd", false);
        assert_eq!(sections.len(), 4);
        // key 稳定不随语言变；title 仅用于显示
        assert_eq!(sections[0].key, "pinned");
        assert!(sections[0].title.contains("Pinned") && sections[0].idxs == vec![0]);
        assert_eq!(sections[1].key, "board");
        assert!(sections[1].title.contains("看板") && sections[1].idxs == vec![1]);
        assert_eq!(sections[2].key, "needs-review");
        assert!(sections[2].title.contains("待评审") && sections[2].idxs == vec![2]);
        assert_eq!(sections[3].key, "reviewed");
        assert!(sections[3].title.contains("已评审") && sections[3].idxs == vec![3]);
        // 英文：组标题随语言切换，key 不变（数量徽章由 UI 层画，标题不含计数）
        let sections = build_sections(&rows, "en", "upd", false);
        assert_eq!(sections.len(), 4);
        assert_eq!(sections[1].key, "board");
        assert_eq!(sections[1].title, "Board");
        assert_eq!(sections[2].key, "needs-review");
        assert_eq!(sections[2].title, "To Review");
    }

    #[test]
    fn sections_empty_hidden() {
        let rows = vec![row(1, 0, 0, "pending")];
        let sections = build_sections(&rows, "zh", "upd", false);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].key, "needs-review");
        assert!(sections[0].title.contains("待评审"));
    }

    #[test]
    fn sections_pinned_exclusive() {
        // pinned 的行不再出现在其状态组
        let rows = vec![row(1, 1, 1, "pending")];
        let sections = build_sections(&rows, "zh", "upd", false);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].key, "pinned");
        assert!(sections[0].title.contains("Pinned"));
    }
}
