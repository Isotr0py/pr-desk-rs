//! 中英文案表；key 缺省回退中文。占位符用 {0} {1}...（调用方 replace）。
//! lang 只认 "en"，其它（含 "zh"）一律走中文表。

/// 中文表（现状文案，key 的最终来源）。
const ZH: &[(&str, &str)] = &[
    // ── 列表分组标题（lib.rs build_sections）──
    ("section_pinned", "Pinned"),
    ("section_board", "看板"),
    ("section_pending", "待评审"),
    ("section_reviewed", "已评审"),
    ("section_rejected", "不采纳"),
    ("section_closed", "已合入 / 已关闭"),
    ("section_title_fmt", "{0}（{1}）"),
    // ── 行内次级活动行（model.rs activity_text）──
    ("reviews_count", "{0} 条评审"),
    // ── 同步状态（sync.rs last_sync_text）──
    ("sync_last", "已同步 {0} 前"),
    // ── 状态栏（main.rs update_status_bar / config_status）──
    ("cfg_unconfigured", "未配置（点右上角设置）"),
    ("cfg_repos", "GitHub 已配置 {0} 个仓库"),
    ("cfg_sync_off", "sync 未启用"),
    ("cfg_sync_on", "sync 已配置"),
    ("sb_local_mode", "本地模式 · 未配置同步服务器"),
    ("sb_wait_sync", "等待首次同步…"),
    ("sb_api_quota", "API 配额 {0}"),
    // ── 通用操作反馈 ──
    ("open_browser_failed", "打开浏览器失败: {0}"),
    ("cfg_save_failed", "配置保存失败: {0}"),
    ("cfg_saved", "配置已保存并生效"),
    ("cfg_reloaded", "配置已重载"),
    ("field_updated", "已更新 {0}（待同步）"),
    // ── 详情面板评审文本 ──
    ("body_score", "综合评分：{0}/10"),
    ("body_score_failed", "评审解析失败（分数为空）"),
    ("body_dimensions", "分项：{0}"),
    ("body_size", "体量：{0}"),
    ("body_flagged", "风险文件："),
    ("flagged_item_fmt", "\n- {0}：{1}"),
    ("dim_readability", "可读性"),
    ("dim_correctness_risk", "正确性风险"),
    ("dim_test_coverage", "测试覆盖"),
    ("dim_security", "安全"),
    ("size_files", "{0} 个文件"),
    ("size_wrap_fmt", "（{0}）"),
    ("size_full_fmt", "{0}（{1}）"),
    ("list_sep", "，"),
    // ── 评审流程（main.rs run_review_task / apply_msg）──
    ("review_start", "开始评审 {0} #{1}…"),
    ("review_running", "评审进行中…"),
    ("review_done", "评审完成：{0} #{1}"),
    ("review_failed", "评审失败：{0} #{1}：{2}"),
    ("review_save_failed", "评审结果入库失败"),
    ("review_already_running", "该 PR 正在评审中"),
    ("review_slots_full", "已有 {0} 个评审进行中，稍后再试"),
    ("llm_not_configured", "请先在设置里配置 LLM API Key 和模型"),
    ("ocr_done_status", "OCR 完成：{0} 条评论（{1} 个文件）"),
    ("ocr_detail_failed", "OCR 明细获取失败：{0}"),
    ("review_copied", "评审报告已复制到剪贴板"),
    ("review_copy_failed", "复制失败：{0}"),
    // ── 后台 worker 状态消息 ──
    ("inbox_fetch_failed", "通知拉取失败: {0}"),
    ("gh_client_init_failed", "GitHub 客户端初始化失败: {0}"),
    ("gh_client_rebuild_failed", "GitHub 客户端重建失败: {0}"),
    ("repo_fetch_failed", "拉取 {0} 失败: {1}"),
    ("gh_refresh_done", "GitHub 刷新完成：{0} 个 open PR"),
    ("sync_done", "已同步服务器列表：拉取 {0} PR/{1} 评审/{2} 待办；推送 {3} 条（冲突 {4}）"),
    ("sync_failed", "同步失败: {0}"),
    ("diff_fetch_failed", "diff 拉取失败: {0}"),
    ("mark_read_failed", "标记已读失败: {0}"),
    ("convo_fetch_failed", "会话拉取失败: {0}"),
    ("files_fetch_failed", "文件列表拉取失败: {0}"),
    ("all_marked_read", "通知已全部标记已读"),
    ("mark_all_read_failed", "全部已读失败: {0}"),
    ("already_in_list", "{0} #{1} 已在列表中"),
    ("added_review", "已加入预评审：{0} #{1}"),
    ("add_review_failed", "加入预评审失败：{0} #{1}"),
    ("models_fetching", "正在拉取模型列表…"),
    ("models_fetched", "拉取到 {0} 个模型"),
    ("models_fetch_failed", "模型列表拉取失败：{0}"),
    ("meta_fill_failed", "补齐 PR 元数据失败 {0} #{1}: {2}"),
    ("live_refresh_failed", "实时刷新失败 {0} #{1}: {2}"),
    ("quota_low_skip", "API 配额不足，跳过实时刷新（整点恢复）"),
    // ── 数据源档案（多数据源切换）──
    ("profile_switched", "已切换数据源：{0}"),
    ("profile_unknown", "未知数据源档案：{0}"),
    ("profile_db_failed", "切换数据源失败（{0}）：{1}"),
    ("profile_invalid_name", "档案名非法（仅允许小写字母/数字/_/-）"),
    ("profile_reserved", "{0} 是保留档案名"),
    ("profile_exists", "档案 {0} 已存在"),
    ("profile_added", "已新增档案：{0}（已复制 default 档服务器配置，可在设置中修改）"),
    ("profile_removed", "已删除档案：{0}"),
    ("profile_remove_denied", "只能删除非当前使用的自定义档案"),
    // ── reviewer.rs 评审进度回调 ──
    ("prog_fetch_pr", "拉取 PR 信息…"),
    ("prog_fetch_diff", "拉取 diff…"),
    ("prog_llm_review", "调用 LLM 评审中…"),
    ("prog_parse_review", "解析评审结果…"),
    ("err_parse_failed", "LLM 评审结果 JSON 解析失败（重试后仍失败）：{0}"),
    ("empty_diff_summary", "diff 为空（可能全部为 lock/生成文件），未评审。"),
    ("oversized_note", "（注：diff 超长，{0} 个文件未纳入评审）"),
    // ── ocr.rs 评审进度回调 ──
    ("prog_create_repo", "创建临时仓库…"),
    ("prog_fetch_pr_diff", "拉取 PR diff…"),
    ("prog_rebuild_base", "重建 base 版本（{0}/{1}）…"),
    ("prog_rebuild_head", "重建 head 版本（{0}/{1}）…"),
    ("prog_ocr_running", "运行 ocr 评审中（最长 30 分钟）…"),
    ("prog_parse_ocr", "解析 ocr 结果…"),
    ("ocr_done", "ocr 完成：{0} 条行级评论，{1} 个文件"),
    ("err_missing_sha", "PR 元信息缺少 base/head sha"),
    ("err_empty_diff", "PR diff 为空，无文件可评审"),
    ("err_ocr_timeout", "ocr 评审超时（30 分钟），已终止"),
    ("err_ocr_spawn", "启动 ocr 失败（检查 ocr_path 配置：{0}）"),
    ("err_ocr_no_result", "ocr 未生成 result.json"),
    ("err_ocr_exit", "ocr 退出码 {0}：{1}"),
    ("err_ocr_failed_status", "ocr 评审失败（status=failed）：{0}"),
];

/// 英文表；缺 key 回退中文。
const EN: &[(&str, &str)] = &[
    ("section_pinned", "Pinned"),
    ("section_board", "Board"),
    ("section_pending", "To Review"),
    ("section_reviewed", "Reviewed"),
    ("section_rejected", "Rejected"),
    ("section_closed", "Merged / Closed"),
    ("section_title_fmt", "{0} ({1})"),
    ("reviews_count", "{0} reviews"),
    ("sync_last", "Synced {0} ago"),
    ("cfg_unconfigured", "Not configured (click Settings at top right)"),
    ("cfg_repos", "GitHub: {0} repos configured"),
    ("cfg_sync_off", "sync disabled"),
    ("cfg_sync_on", "sync configured"),
    ("sb_local_mode", "Local mode · no sync server configured"),
    ("sb_wait_sync", "Waiting for first sync…"),
    ("sb_api_quota", "API quota {0}"),
    ("open_browser_failed", "Failed to open browser: {0}"),
    ("cfg_save_failed", "Failed to save config: {0}"),
    ("cfg_saved", "Settings saved and applied"),
    ("cfg_reloaded", "Config reloaded"),
    ("field_updated", "Updated {0} (pending sync)"),
    ("body_score", "Overall score: {0}/10"),
    ("body_score_failed", "Failed to parse review (no score)"),
    ("body_dimensions", "Dimensions: {0}"),
    ("body_size", "Size: {0}"),
    ("body_flagged", "Risky files:"),
    ("flagged_item_fmt", "\n- {0}: {1}"),
    ("dim_readability", "Readability"),
    ("dim_correctness_risk", "Correctness risk"),
    ("dim_test_coverage", "Test coverage"),
    ("dim_security", "Security"),
    ("size_files", "{0} files"),
    ("size_wrap_fmt", "({0})"),
    ("size_full_fmt", "{0} ({1})"),
    ("list_sep", ", "),
    ("review_start", "Starting review {0} #{1}…"),
    ("review_running", "Review in progress…"),
    ("review_done", "Review done: {0} #{1}"),
    ("review_failed", "Review failed: {0} #{1}: {2}"),
    ("review_save_failed", "Failed to save review result"),
    ("review_already_running", "This PR is already being reviewed"),
    ("review_slots_full", "{0} reviews already running, try again later"),
    ("llm_not_configured", "Configure LLM API key and model in Settings first"),
    ("ocr_done_status", "OCR done: {0} comments ({1} files)"),
    ("ocr_detail_failed", "Failed to fetch OCR details: {0}"),
    ("review_copied", "Review copied to clipboard"),
    ("review_copy_failed", "Copy failed: {0}"),
    ("inbox_fetch_failed", "Failed to fetch notifications: {0}"),
    ("gh_client_init_failed", "Failed to init GitHub client: {0}"),
    ("gh_client_rebuild_failed", "Failed to rebuild GitHub client: {0}"),
    ("repo_fetch_failed", "Failed to fetch {0}: {1}"),
    ("gh_refresh_done", "GitHub refresh done: {0} open PRs"),
    ("sync_done", "Synced server list: pulled {0} PRs / {1} reviews / {2} todos; pushed {3} ({4} conflicts)"),
    ("sync_failed", "Sync failed: {0}"),
    ("diff_fetch_failed", "Failed to fetch diff: {0}"),
    ("mark_read_failed", "Failed to mark as read: {0}"),
    ("convo_fetch_failed", "Failed to fetch conversation: {0}"),
    ("files_fetch_failed", "Failed to fetch file list: {0}"),
    ("all_marked_read", "All notifications marked as read"),
    ("mark_all_read_failed", "Failed to mark all as read: {0}"),
    ("already_in_list", "{0} #{1} is already in the list"),
    ("added_review", "Added to pre-review: {0} #{1}"),
    ("add_review_failed", "Failed to add to pre-review: {0} #{1}"),
    ("models_fetching", "Fetching model list…"),
    ("models_fetched", "Fetched {0} models"),
    ("models_fetch_failed", "Failed to fetch model list: {0}"),
    ("meta_fill_failed", "Failed to fill PR metadata {0} #{1}: {2}"),
    ("live_refresh_failed", "Live refresh failed {0} #{1}: {2}"),
    ("quota_low_skip", "API quota low, skipping live refresh (resets hourly)"),
    ("profile_switched", "Switched data source: {0}"),
    ("profile_unknown", "Unknown profile: {0}"),
    ("profile_db_failed", "Failed to switch profile ({0}): {1}"),
    ("profile_invalid_name", "Invalid profile name (lowercase letters/digits/_/- only)"),
    ("profile_reserved", "{0} is a reserved profile name"),
    ("profile_exists", "Profile {0} already exists"),
    ("profile_added", "Profile added: {0} (server settings copied from the default profile; edit in Settings)"),
    ("profile_removed", "Profile removed: {0}"),
    ("profile_remove_denied", "Only inactive custom profiles can be removed"),
    ("prog_fetch_pr", "Fetching PR info…"),
    ("prog_fetch_diff", "Fetching diff…"),
    ("prog_llm_review", "Running LLM review…"),
    ("prog_parse_review", "Parsing review result…"),
    ("err_parse_failed", "Failed to parse LLM review JSON (after retry): {0}"),
    ("empty_diff_summary", "Diff is empty (possibly all lock/generated files); not reviewed."),
    ("oversized_note", "(Note: diff too long; {0} files were not reviewed)"),
    ("prog_create_repo", "Creating temp repo…"),
    ("prog_fetch_pr_diff", "Fetching PR diff…"),
    ("prog_rebuild_base", "Rebuilding base version ({0}/{1})…"),
    ("prog_rebuild_head", "Rebuilding head version ({0}/{1})…"),
    ("prog_ocr_running", "Running ocr review (up to 30 min)…"),
    ("prog_parse_ocr", "Parsing ocr result…"),
    ("ocr_done", "ocr done: {0} line comments, {1} files"),
    ("err_missing_sha", "PR metadata missing base/head sha"),
    ("err_empty_diff", "PR diff is empty, nothing to review"),
    ("err_ocr_timeout", "ocr review timed out (30 min), terminated"),
    ("err_ocr_spawn", "Failed to start ocr (check ocr_path config: {0})"),
    ("err_ocr_no_result", "ocr did not produce result.json"),
    ("err_ocr_exit", "ocr exit code {0}: {1}"),
    ("err_ocr_failed_status", "ocr review failed (status=failed): {0}"),
];

fn lookup(table: &'static [(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// 取文案：lang=="en" 走英文表（缺 key 回退中文），其它一律中文。
/// key 在两张表都不存在时返回空串（调用方 key 拼写错误的兜底，靠测试覆盖防呆）。
pub fn tr(lang: &str, key: &str) -> &'static str {
    let s = if lang == "en" {
        lookup(EN, key).or_else(|| lookup(ZH, key))
    } else {
        lookup(ZH, key)
    };
    s.unwrap_or("")
}

/// tr + 占位替换：依次把 {0} {1}… 换成 args。
pub fn trf(lang: &str, key: &str, args: &[&str]) -> String {
    let mut s = tr(lang, key).to_string();
    for (i, a) in args.iter().enumerate() {
        s = s.replace(&format!("{{{i}}}"), a);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_have_same_keys() {
        let zh: std::collections::BTreeSet<_> = ZH.iter().map(|(k, _)| *k).collect();
        let en: std::collections::BTreeSet<_> = EN.iter().map(|(k, _)| *k).collect();
        assert_eq!(zh, en, "中英文案表 key 集合不一致");
    }

    #[test]
    fn tr_zh_and_en() {
        assert_eq!(tr("zh", "section_pending"), "待评审");
        assert_eq!(tr("en", "section_pending"), "To Review");
        // 未知 lang 回退中文
        assert_eq!(tr("fr", "section_pending"), "待评审");
        assert_eq!(tr("", "section_pending"), "待评审");
        // 未知 key 回退空串
        assert_eq!(tr("zh", "no_such_key"), "");
    }

    #[test]
    fn trf_replaces_placeholders() {
        assert_eq!(trf("zh", "added_review", &["o/r", "42"]), "已加入预评审：o/r #42");
        assert_eq!(trf("en", "added_review", &["o/r", "42"]), "Added to pre-review: o/r #42");
        // 多占位 + 顺序无关
        assert_eq!(
            trf("en", "section_title_fmt", &["Board", "3"]),
            "Board (3)"
        );
    }
}
