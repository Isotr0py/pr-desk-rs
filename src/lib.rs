//! pr-desk-rs 库：本地 SQLite、GitHub 拉取、同步引擎、diff 解析、行分组。
//! UI 无关的纯逻辑都在库里（可用 `cargo test --lib` 单独测试，不编译 Slint）。

pub mod config;
pub mod db;
pub mod diffview;
pub mod github;
pub mod sync;

/// 行数据 → 分组：（组标题, 该组内数据行的索引列表）。
/// 分区顺序：Pinned → 看板 → 待评审 → 已评审；行只出现在一个组里；
/// pinned 独立成组（不再出现在其状态组）。空组不返回。
pub fn build_sections(rows: &[serde_json::Value]) -> Vec<(String, Vec<usize>)> {
    let mut out: Vec<(String, Vec<usize>)> = Vec::new();
    let mut emit = |title: &str, pred: &dyn Fn(&serde_json::Value) -> bool| {
        let idx: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| pred(r))
            .map(|(i, _)| i)
            .collect();
        if !idx.is_empty() {
            out.push((format!("{title}（{}）", idx.len()), idx));
        }
    };
    emit("📌 Pinned", &|r| r["pinned"].as_i64() == Some(1));
    emit("📌 看板", &|r| {
        r["pinned"].as_i64() != Some(1) && r["on_board"].as_i64() == Some(1)
    });
    emit("待评审", &|r| {
        r["pinned"].as_i64() != Some(1)
            && r["on_board"].as_i64() != Some(1)
            && r["status"].as_str() == Some("pending")
    });
    emit("已评审", &|r| {
        r["pinned"].as_i64() != Some(1)
            && r["on_board"].as_i64() != Some(1)
            && r["status"].as_str() != Some("pending")
    });
    out
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
    fn sections_grouping() {
        let rows = vec![
            row(1, 1, 0, "pending"),  // pinned
            row(2, 0, 1, "pending"),  // board
            row(3, 0, 0, "pending"),  // 待评审
            row(4, 0, 0, "reviewed"), // 已评审
        ];
        let sections = build_sections(&rows);
        assert_eq!(sections.len(), 4);
        assert!(sections[0].0.contains("Pinned") && sections[0].1 == vec![0]);
        assert!(sections[1].0.contains("看板") && sections[1].1 == vec![1]);
        assert!(sections[2].0.contains("待评审") && sections[2].1 == vec![2]);
        assert!(sections[3].0.contains("已评审") && sections[3].1 == vec![3]);
    }

    #[test]
    fn sections_empty_hidden() {
        let rows = vec![row(1, 0, 0, "pending")];
        let sections = build_sections(&rows);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].0.contains("待评审"));
    }

    #[test]
    fn sections_pinned_exclusive() {
        // pinned 的行不再出现在其状态组
        let rows = vec![row(1, 1, 1, "pending")];
        let sections = build_sections(&rows);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].0.contains("Pinned"));
    }
}
