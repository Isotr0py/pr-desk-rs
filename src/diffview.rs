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

/// 解析 hunk 头 `@@ -old_start,old_count +new_start,new_count @@`（count 省略 = 1），
/// 返回 (old_start, new_start)。畸形头返回 None（调用方保持行号不变，不炸解析）。
fn parse_hunk_header(line: &str) -> Option<(i32, i32)> {
    // 形如 "@@ -1,3 +1,4 @@"；section 注释（@@ 之后的文字）忽略
    let inner = line.strip_prefix("@@")?.split_once("@@")?.0;
    let mut old_start = None;
    let mut new_start = None;
    for tok in inner.split_whitespace() {
        if let Some(rest) = tok.strip_prefix('-') {
            old_start = rest.split(',').next()?.trim().parse().ok();
        } else if let Some(rest) = tok.strip_prefix('+') {
            new_start = rest.split(',').next()?.trim().parse().ok();
        }
    }
    Some((old_start?, new_start?))
}

/// 给单文件 diff 文本的每一行配真实行号（gutter 用）：
/// - add / context 行 → new 侧行号；del 行 → old 侧行号；hunk / meta 行 → 0（无行号）。
/// - hunk 头重置两侧的计数器；第一个 hunk 之前的 meta 行无行号。
/// 返回 (LineKind, 原文, 行号)，行号 0 表示「不显示」。
pub fn numbered_lines(text: &str) -> Vec<(LineKind, String, i32)> {
    let mut old_ln = 0i32;
    let mut new_ln = 0i32;
    let mut out = Vec::new();
    for line in text.lines() {
        let kind = classify_line(line);
        let mut no = 0;
        match kind {
            LineKind::Hunk => {
                if let Some((o, n)) = parse_hunk_header(line) {
                    old_ln = o;
                    new_ln = n;
                }
            }
            LineKind::Add => {
                if new_ln > 0 {
                    no = new_ln;
                    new_ln += 1;
                }
            }
            LineKind::Del => {
                if old_ln > 0 {
                    no = old_ln;
                    old_ln += 1;
                }
            }
            LineKind::Ctx => {
                if new_ln > 0 && old_ln > 0 {
                    no = new_ln;
                    new_ln += 1;
                    old_ln += 1;
                }
            }
            LineKind::Meta => {}
        }
        out.push((kind, line.to_string(), no));
    }
    out
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

    #[test]
    fn hunk_header_parses_both_forms() {
        assert_eq!(parse_hunk_header("@@ -1,3 +1,4 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("@@ -10 +20 @@ fn main()"), Some((10, 20)));
        assert_eq!(parse_hunk_header("@@ -0,0 +5,2 @@"), Some((0, 5)));
        assert_eq!(parse_hunk_header("@@ garbage"), None);
        assert_eq!(parse_hunk_header("not a hunk"), None);
    }

    #[test]
    fn numbered_lines_multi_hunk_mixed() {
        let text = "\
diff --git a/a.py b/a.py
--- a/a.py
+++ b/a.py
@@ -1,3 +1,4 @@
 ctx1
-del
+add1
+add2
 ctx2
@@ -10,2 +11,3 @@
 ctxA
-delA
+addA
";
        let lines = numbered_lines(text);
        let no: Vec<i32> = lines.iter().map(|(_, _, n)| *n).collect();
        let kinds: Vec<LineKind> = lines.iter().map(|(k, _, _)| *k).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Meta, LineKind::Meta, LineKind::Meta, LineKind::Hunk,
                LineKind::Ctx, LineKind::Del, LineKind::Add, LineKind::Add, LineKind::Ctx,
                LineKind::Hunk,
                LineKind::Ctx, LineKind::Del, LineKind::Add,
            ]
        );
        // meta/hunk 无行号；ctx/add 用 new 侧，del 用 old 侧
        assert_eq!(no, [0, 0, 0, 0, 1, 2, 2, 3, 4, 0, 11, 11, 12]);
    }

    #[test]
    fn numbered_lines_before_first_hunk_have_no_number() {
        let lines = numbered_lines("diff --git a/x b/x\nindex 1..2\n--- a/x\n+++ b/x\n");
        assert!(lines.iter().all(|(_, _, n)| *n == 0));
    }
}
