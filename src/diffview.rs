//! diff 行分类（UI 着色用）。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Add,    // + 新增行（绿）
    Del,    // - 删除行（红）
    Hunk,   // @@ hunk 头（蓝紫）
    Meta,   // diff --git / index / --- / +++ / new file / deleted file（加粗深色底）
    Ctx,    // 上下文（灰）
}

pub fn classify_line(line: &str) -> LineKind {
    if line.starts_with("diff --git")
        || line.starts_with("index ")
        || line.starts_with("---")
        || line.starts_with("+++")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
    {
        LineKind::Meta
    } else if line.starts_with("@@") {
        LineKind::Hunk
    } else if line.starts_with('+') {
        LineKind::Add
    } else if line.starts_with('-') {
        LineKind::Del
    } else {
        LineKind::Ctx
    }
}

/// 按 `diff --git` 头切分为文件块。
pub fn split_files(diff: &str) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut cur = String::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let (Some(p), c) = (cur_path.take(), std::mem::take(&mut cur)) {
                if !c.is_empty() {
                    files.push((p, c));
                }
            }
            // diff --git a/path b/path
            cur_path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.trim().to_string())
                .or_else(|| Some(rest.to_string()));
        }
        if cur_path.is_some() {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    if let (Some(p), c) = (cur_path.take(), std::mem::take(&mut cur)) {
        if !c.is_empty() {
            files.push((p, c));
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify() {
        assert_eq!(classify_line("+added"), LineKind::Add);
        assert_eq!(classify_line("-removed"), LineKind::Del);
        assert_eq!(classify_line("@@ -1 +1 @@"), LineKind::Hunk);
        assert_eq!(classify_line("diff --git a/x b/x"), LineKind::Meta);
        assert_eq!(classify_line("index 111..222 100644"), LineKind::Meta);
        assert_eq!(classify_line("--- a/x"), LineKind::Meta);
        assert_eq!(classify_line("+++ b/x"), LineKind::Meta);
        assert_eq!(classify_line(" context"), LineKind::Ctx);
        assert_eq!(classify_line(""), LineKind::Ctx);
        // ---/+++ 以内容行形式出现时（前导空格）不算 meta
        assert_eq!(classify_line(" --- not meta"), LineKind::Ctx);
    }

    #[test]
    fn split() {
        let diff = "diff --git a/a.py b/a.py\nindex 1..2 100644\n--- a/a.py\n+++ b/a.py\n@@ -1 +1 @@\n-x\n+y\ndiff --git a/b.py b/b.py\nindex 3..4 100644\n--- a/b.py\n+++ b/b.py\n@@ -1 +1 @@\n-m\n+n\n";
        let files = split_files(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, "a.py");
        assert_eq!(files[1].0, "b.py");
        assert!(files[0].1.contains("-x"));
        assert!(files[1].1.contains("+n"));
    }
}
