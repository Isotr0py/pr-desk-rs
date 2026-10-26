//! GitHub 评论 Markdown → 结构化块数组（供 Slint UI 按块渲染）。
//! kind 约定：h1..h6 / p / code / bullet / num / table / quote / hr。

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MdBlockOut {
    pub kind: String,
    pub text: String,
    pub lang: String,
    pub depth: i32,
    pub cols: i32,
    pub cells: Vec<String>,
}

impl MdBlockOut {
    fn new(kind: &str) -> Self {
        MdBlockOut { kind: kind.into(), ..Default::default() }
    }
}

/// 剥 HTML 标签的状态机：`<` 后紧跟字母或 `/` 才视为标签起点，跳过到 `>`；
/// `<!--` 到 `-->` 的注释整段丢弃（GitHub 机器人常塞 codex 状态 JSON 之类的注释）；
/// 否则 `<` 按普通字符保留（避免误伤 `a < b` 之类的比较）。
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut in_comment = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_comment {
            // 见到 --> 才结束注释
            if c == '-' && chars.peek() == Some(&'-') {
                chars.next();
                if chars.peek() == Some(&'>') {
                    chars.next();
                    in_comment = false;
                }
            }
            continue;
        }
        if in_tag {
            if c == '>' {
                in_tag = false;
            }
            continue;
        }
        if c == '<' {
            match chars.peek() {
                Some(&n) if n.is_ascii_alphabetic() || n == '/' => {
                    in_tag = true;
                    continue;
                }
                // <!-- 注释起点（!< 两个字符都确认后跳过整段）
                Some(&'!') => {
                    let mut probe = chars.clone();
                    probe.next();
                    if probe.next() == Some('-') && probe.next() == Some('-') {
                        chars.next(); // '!'
                        chars.next(); // '-'
                        chars.next(); // '-'
                        in_comment = true;
                        continue;
                    }
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

/// 折叠连续空行并去首尾空白。
fn clean_text(s: &str) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in s.trim().lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            blank = true;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank {
                out.push('\n');
            }
        }
        out.push_str(t);
        blank = false;
    }
    out
}

/// 列表栈帧：(有序?, 下一个序号)。
struct ListFrame {
    ordered: bool,
    next: u64,
}

/// 列表项栈帧。
struct ItemFrame {
    text: String,
    ordered: bool,
    num: u64,
    depth: i32,
    task: Option<bool>,
}

pub fn parse_markdown(md: &str) -> Vec<MdBlockOut> {
    let mut out: Vec<MdBlockOut> = Vec::new();
    if md.trim().is_empty() {
        return out;
    }
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_GFM);
    let parser = Parser::new_ext(md, opts);

    let mut text = String::new(); // 当前行内累加器（段落/标题/引用/表格单元格共用）
    let mut heading: Option<u8> = None;
    let mut in_code = false;
    let mut code_text = String::new();
    let mut code_lang = String::new();
    let mut lists: Vec<ListFrame> = Vec::new();
    let mut items: Vec<ItemFrame> = Vec::new();
    let mut quote_depth = 0usize;
    let mut quote_lines: Vec<String> = Vec::new();
    let mut in_table = false;
    let mut cols: i32 = 0;
    let mut cells: Vec<String> = Vec::new();
    let mut in_cell = false;
    let mut cell_text = String::new();
    let mut html_buf = String::new();
    let mut image_alts: Vec<String> = Vec::new();

    // 块级 HTML 缓冲 → 遇到非 HTML 事件或结束时冲刷成一个 p 块
    let flush_html = |html_buf: &mut String, out: &mut Vec<MdBlockOut>| {
        let t = clean_text(&strip_html_tags(html_buf));
        html_buf.clear();
        if !t.is_empty() {
            let mut b = MdBlockOut::new("p");
            b.text = t;
            out.push(b);
        }
    };
    // 列表项文字 → 块（任务勾选框/序号前缀在此附加），发完清空避免重复
    let emit_item = |out: &mut Vec<MdBlockOut>, item: &mut ItemFrame| {
        let t = clean_text(&item.text);
        item.text.clear();
        if t.is_empty() {
            return;
        }
        let mut t = t;
        if let Some(checked) = item.task {
            t = format!("{} {}", if checked { "☑" } else { "☐" }, t);
        }
        if item.ordered {
            t = format!("{}. {}", item.num, t);
        }
        let mut b = MdBlockOut::new(if item.ordered { "num" } else { "bullet" });
        b.text = t;
        b.depth = item.depth;
        out.push(b);
    };
    let push_text = |text: &mut String,
                         items: &mut Vec<ItemFrame>,
                         image_alts: &mut Vec<String>,
                         s: &str| {
        if let Some(alt) = image_alts.last_mut() {
            alt.push_str(s);
        } else if let Some(item) = items.last_mut() {
            item.text.push_str(s);
        } else {
            text.push_str(s);
        }
    };

    for ev in parser {
        // 块级 HTML 缓冲在任意其他事件前冲刷（HTML 自身事件除外）
        if !matches!(ev, Event::Html(_)) && !html_buf.is_empty() {
            flush_html(&mut html_buf, &mut out);
        }
        match ev {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    heading = Some(level as u8);
                    text.clear();
                }
                Tag::Paragraph => {
                    if !in_cell {
                        text.clear();
                    }
                }
                Tag::CodeBlock(kind) => {
                    in_code = true;
                    code_text.clear();
                    code_lang = match kind {
                        CodeBlockKind::Fenced(l) => l.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                }
                Tag::List(first) => {
                    // 嵌套子列表开始：外层项自身文字已到齐，先按原顺序发出
                    if let Some(item) = items.last_mut() {
                        emit_item(&mut out, item);
                    }
                    match first {
                        Some(n) => lists.push(ListFrame { ordered: true, next: n }),
                        None => lists.push(ListFrame { ordered: false, next: 0 }),
                    }
                }
                Tag::Item => {
                    let depth = lists.len().saturating_sub(1) as i32;
                    let (ordered, num) = match lists.last_mut() {
                        Some(f) if f.ordered => {
                            let n = f.next;
                            f.next += 1;
                            (true, n)
                        }
                        _ => (false, 0),
                    };
                    items.push(ItemFrame { text: String::new(), ordered, num, depth, task: None });
                }
                Tag::BlockQuote(_) => quote_depth += 1,
                Tag::Table(aligns) => {
                    in_table = true;
                    cols = aligns.len() as i32;
                    cells.clear();
                }
                Tag::TableCell => {
                    in_cell = true;
                    cell_text.clear();
                }
                Tag::Image { .. } => image_alts.push(String::new()),
                _ => {} // Link/Strong/Emphasis/Strikethrough 等：标记剥掉只留文字
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    let level = heading.take().unwrap_or(1).clamp(1, 6);
                    let t = clean_text(&text);
                    text.clear();
                    if !t.is_empty() {
                        let mut b = MdBlockOut::new(&format!("h{level}"));
                        b.text = t;
                        out.push(b);
                    }
                }
                TagEnd::Paragraph => {
                    let t = clean_text(&text);
                    text.clear();
                    if t.is_empty() {
                        continue;
                    }
                    if !items.is_empty() {
                        continue; // 列表项内的段落文字归并到 item，End(Item) 时统一发出
                    }
                    if in_table {
                        continue; // 表格单元格文字在 End(TableCell) 归并
                    }
                    if quote_depth > 0 {
                        quote_lines.push(t);
                        continue;
                    }
                    let mut b = MdBlockOut::new("p");
                    b.text = t;
                    out.push(b);
                }
                TagEnd::CodeBlock => {
                    in_code = false;
                    let mut b = MdBlockOut::new("code");
                    b.text = code_text.trim_end_matches('\n').to_string();
                    b.lang = std::mem::take(&mut code_lang);
                    out.push(b);
                }
                TagEnd::List(_) => {
                    lists.pop();
                }
                TagEnd::Item => {
                    if let Some(mut item) = items.pop() {
                        emit_item(&mut out, &mut item);
                    }
                }
                TagEnd::BlockQuote(_) => {
                    quote_depth = quote_depth.saturating_sub(1);
                    if quote_depth == 0 && !quote_lines.is_empty() {
                        let mut b = MdBlockOut::new("quote");
                        b.text = quote_lines.join("\n");
                        quote_lines.clear();
                        out.push(b);
                    }
                }
                TagEnd::Table => {
                    in_table = false;
                    let mut b = MdBlockOut::new("table");
                    b.cols = cols;
                    b.cells = std::mem::take(&mut cells);
                    out.push(b);
                }
                TagEnd::TableCell => {
                    in_cell = false;
                    let t = clean_text(&cell_text);
                    if !t.is_empty() || in_table {
                        cells.push(t);
                    }
                }
                TagEnd::Image => {
                    if let Some(alt) = image_alts.pop() {
                        push_text(&mut text, &mut items, &mut image_alts, &format!("[图: {alt}]"));
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_code {
                    code_text.push_str(&t);
                } else if in_cell {
                    cell_text.push_str(&t);
                } else {
                    push_text(&mut text, &mut items, &mut image_alts, &t);
                }
            }
            Event::Code(t) => {
                // 行内 code：剥掉反引号只留文字
                if in_cell {
                    cell_text.push_str(&t);
                } else {
                    push_text(&mut text, &mut items, &mut image_alts, &t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_code {
                    code_text.push('\n');
                } else if in_cell {
                    cell_text.push(' ');
                } else {
                    push_text(&mut text, &mut items, &mut image_alts, "\n");
                }
            }
            Event::Rule => {
                out.push(MdBlockOut::new("hr"));
            }
            Event::TaskListMarker(checked) => {
                if let Some(item) = items.last_mut() {
                    item.task = Some(checked);
                }
            }
            Event::Html(h) => {
                html_buf.push_str(&h);
            }
            Event::InlineHtml(h) => {
                // 行内 HTML（<sub>、<br> 等）：剥标签后直接并入文字
                let stripped = strip_html_tags(&h);
                if in_cell {
                    cell_text.push_str(&stripped);
                } else {
                    push_text(&mut text, &mut items, &mut image_alts, &stripped);
                }
            }
            _ => {} // FootnoteReference/InlineMath/DisplayMath 等忽略
        }
    }
    if !html_buf.is_empty() {
        flush_html(&mut html_buf, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_and_bold_stripped() {
        let blocks = parse_markdown("# 标题 **加粗**\n\n## 二级 `code`");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, "h1");
        assert_eq!(blocks[0].text, "标题 加粗");
        assert_eq!(blocks[1].kind, "h2");
        assert_eq!(blocks[1].text, "二级 code");
    }

    #[test]
    fn paragraph_inline_marks_stripped() {
        let blocks = parse_markdown("这是 **粗体** 和 *斜体* 与 `x = 1` 还有 ~~删除线~~。");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, "p");
        assert_eq!(blocks[0].text, "这是 粗体 和 斜体 与 x = 1 还有 删除线。");
    }

    #[test]
    fn link_keeps_text_image_placeholder() {
        let blocks = parse_markdown("看 [这里](https://example.com) 和 ![截图](https://a/b.png)");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, "p");
        assert_eq!(blocks[0].text, "看 这里 和 [图: 截图]");
        assert!(!blocks[0].text.contains("https://"));
    }

    #[test]
    fn fenced_code_with_lang() {
        let blocks = parse_markdown("```rust\nfn main() {\n    println!(\"hi\");\n}\n```");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, "code");
        assert_eq!(blocks[0].lang, "rust");
        assert_eq!(blocks[0].text, "fn main() {\n    println!(\"hi\");\n}");
    }

    #[test]
    fn gfm_table_flattened() {
        let md = "| 名称 | 值 |\n| --- | --- |\n| a | 1 |\n| b | 2 |";
        let blocks = parse_markdown(md);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, "table");
        assert_eq!(blocks[0].cols, 2);
        assert_eq!(blocks[0].cells.len(), 3 * 2); // (行数+1)*cols
        assert_eq!(blocks[0].cells, vec!["名称", "值", "a", "1", "b", "2"]);
    }

    #[test]
    fn nested_list_depth() {
        let blocks = parse_markdown("- 顶层\n  - 二层\n1. 第一\n2. 第二");
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].kind, "bullet");
        assert_eq!(blocks[0].depth, 0);
        assert_eq!(blocks[1].kind, "bullet");
        assert_eq!(blocks[1].depth, 1);
        assert_eq!(blocks[2].kind, "num");
        assert_eq!(blocks[2].text, "1. 第一");
        assert_eq!(blocks[3].text, "2. 第二");
    }

    #[test]
    fn task_list_marker_prefix() {
        let blocks = parse_markdown("- [x] 已完成\n- [ ] 待办");
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].text.starts_with("☑ "));
        assert!(blocks[1].text.starts_with("☐ "));
        assert!(blocks[0].text.contains("已完成"));
        assert!(blocks[1].text.contains("待办"));
    }

    #[test]
    fn html_details_summary_stripped() {
        let md = "<details>\n<summary>展开日志</summary>\n\n内部 **内容**\n\n</details>";
        let blocks = parse_markdown(md);
        let all: Vec<&str> = blocks.iter().map(|b| b.text.as_str()).collect();
        assert!(all.iter().any(|t| t.contains("展开日志")), "blocks={blocks:?}");
        assert!(all.iter().any(|t| t.contains("内部 内容")), "blocks={blocks:?}");
        for b in &blocks {
            assert!(!b.text.contains('<') || b.text.contains("< "), "tag leaked: {b:?}");
            assert!(!b.text.contains("details"));
        }
    }

    #[test]
    fn html_comparison_not_stripped() {
        // `a < b` 的 < 后不是字母/斜杠，不能被当标签剥掉
        let blocks = parse_markdown("条件 a < b 时成立");
        assert_eq!(blocks[0].text, "条件 a < b 时成立");
    }

    #[test]
    fn html_comment_stripped() {
        // GitHub 机器人（如 codex）常塞 <!-- 状态 JSON --> 注释，整段不显示
        let md = "<!-- codex-review-state:v1 {\"status\":\"failed\"} -->\n\n**正文**";
        let blocks = parse_markdown(md);
        assert_eq!(blocks.len(), 1, "blocks={blocks:?}");
        assert_eq!(blocks[0].text, "正文");
        // 多行注释
        let md2 = "<!-- 第一行\n第二行 -->\n\n可见";
        let blocks2 = parse_markdown(md2);
        assert_eq!(blocks2.len(), 1);
        assert_eq!(blocks2[0].text, "可见");
    }

    #[test]
    fn quote_flattened() {
        let blocks = parse_markdown("> 第一段\n>\n> 第二段");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, "quote");
        assert_eq!(blocks[0].text, "第一段\n第二段");
    }

    #[test]
    fn hr_and_empty_input() {
        assert!(parse_markdown("").is_empty());
        assert!(parse_markdown("   \n\n  ").is_empty());
        let blocks = parse_markdown("上文\n\n---\n\n下文");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[1].kind, "hr");
    }

    #[test]
    fn blank_paragraphs_dropped() {
        let blocks = parse_markdown("甲\n\n\n\n乙");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "甲");
        assert_eq!(blocks[1].text, "乙");
    }
}
