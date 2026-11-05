//! 多选集合的纯语义（UI 无关，可单测）：Ctrl 点选 toggle、Shift 范围选、
//! 普通点击单选、刷新后按身份修剪。选择集合保持点选顺序且不重复。
//!
//! 集成注意：anchor 存身份（repo, number）而非下标——刷新重建后下标漂移，
//! 身份不漂；范围选时在调用方传入的 view 序（view_visible）上现查两端下标，
//! anchor 已不在视图里时由调用方退化为普通点击。

/// PR 身份键（repo, number）。
pub type PrKey = (String, i64);

pub fn is_selected(selection: &[PrKey], key: &PrKey) -> bool {
    selection.iter().any(|k| k == key)
}

/// Ctrl+点击：在集合内则移除，不在则追加（保持点选顺序）。返回操作后是否选中。
pub fn toggle(selection: &mut Vec<PrKey>, key: &PrKey) -> bool {
    if let Some(pos) = selection.iter().position(|k| k == key) {
        selection.remove(pos);
        false
    } else {
        selection.push(key.clone());
        true
    }
}

/// 普通点击：清空后仅选中当前行（单选语义，selection 与主选中行一致化）。
pub fn select_single(selection: &mut Vec<PrKey>, key: &PrKey) {
    selection.clear();
    selection.push(key.clone());
}

/// Shift+点击：view 序（可见行展示序）上 anchor 到 target 之间（含两端）全部并入
/// selection（已有项不重复、不动顺序；新增项按 view 序追加）。
/// anchor 或 target 不在 view 里时退化为单选 target。
pub fn extend_range(selection: &mut Vec<PrKey>, view: &[PrKey], anchor: &PrKey, target: &PrKey) {
    let ai = view.iter().position(|k| k == anchor);
    let ti = view.iter().position(|k| k == target);
    let (Some(ai), Some(ti)) = (ai, ti) else {
        if !is_selected(selection, target) {
            selection.push(target.clone());
        }
        return;
    };
    let (lo, hi) = if ai <= ti { (ai, ti) } else { (ti, ai) };
    for k in &view[lo..=hi] {
        if !is_selected(selection, k) {
            selection.push(k.clone());
        }
    }
}

/// 刷新/重建后修剪：只保留仍存在于 keys（当前行身份数组）里的选中项。
/// 返回被移除的项数（调用方可据此决定是否回写 UI 计数）。
pub fn retain_existing(selection: &mut Vec<PrKey>, keys: &[PrKey]) -> usize {
    let before = selection.len();
    selection.retain(|k| keys.iter().any(|x| x == k));
    before - selection.len()
}

/// 批量 toggle 的目标值：全部已置位（非 0）→ 0（清除），否则 → 1（置位）。
/// 空集合返回 1（无意义调用也安全）。
pub fn batch_toggle_target<'a>(values: impl Iterator<Item = &'a i64>) -> i64 {
    let mut any = false;
    let all_set = values.inspect(|_| any = true).all(|v| *v != 0);
    if any && all_set { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(repo: &str, n: i64) -> PrKey {
        (repo.to_string(), n)
    }

    #[test]
    fn toggle_adds_then_removes_keeping_order() {
        let mut s = vec![k("o/r", 1)];
        assert!(toggle(&mut s, &k("o/r", 2)));
        assert!(toggle(&mut s, &k("a/b", 3)));
        assert_eq!(s, vec![k("o/r", 1), k("o/r", 2), k("a/b", 3)]);
        // 再点已选项 = 移出，其余顺序不动
        assert!(!toggle(&mut s, &k("o/r", 2)));
        assert_eq!(s, vec![k("o/r", 1), k("a/b", 3)]);
        // 重复 toggle 同一项可再进
        assert!(toggle(&mut s, &k("o/r", 2)));
        assert_eq!(s, vec![k("o/r", 1), k("a/b", 3), k("o/r", 2)]);
    }

    #[test]
    fn select_single_clears_previous() {
        let mut s = vec![k("o/r", 1), k("o/r", 2)];
        select_single(&mut s, &k("a/b", 9));
        assert_eq!(s, vec![k("a/b", 9)]);
    }

    #[test]
    fn extend_range_both_directions() {
        let view = vec![k("o/r", 1), k("o/r", 2), k("o/r", 3), k("o/r", 4)];
        let mut s = Vec::new();
        // 正向
        extend_range(&mut s, &view, &k("o/r", 1), &k("o/r", 3));
        assert_eq!(s, vec![k("o/r", 1), k("o/r", 2), k("o/r", 3)]);
        // 反向（从下往上 shift）结果一致
        let mut s2 = Vec::new();
        extend_range(&mut s2, &view, &k("o/r", 3), &k("o/r", 1));
        assert_eq!(s2, s);
    }

    #[test]
    fn extend_range_merges_without_duplicates() {
        let view = vec![k("o/r", 1), k("o/r", 2), k("o/r", 3), k("o/r", 4)];
        // 已有集合里含 range 内的一项（Ctrl 点过 #2）：不重复、原有相对顺序保留
        let mut s = vec![k("o/r", 2), k("x/y", 7)];
        extend_range(&mut s, &view, &k("o/r", 1), &k("o/r", 3));
        assert_eq!(s, vec![k("o/r", 2), k("x/y", 7), k("o/r", 1), k("o/r", 3)]);
    }

    #[test]
    fn extend_range_missing_endpoint_falls_back_to_single() {
        let view = vec![k("o/r", 1), k("o/r", 2)];
        let mut s = Vec::new();
        // anchor 不在视图（被过滤/折叠掉）：退化为单选 target
        extend_range(&mut s, &view, &k("o/r", 99), &k("o/r", 2));
        assert_eq!(s, vec![k("o/r", 2)]);
        // 两端任一缺失都走同一退化分支（target 恒为被点行，正常路径必在视图内）
        extend_range(&mut s, &view, &k("o/r", 1), &k("o/r", 99));
        assert_eq!(s, vec![k("o/r", 2), k("o/r", 99)]);
    }

    #[test]
    fn retain_existing_prunes_stale_keys() {
        let mut s = vec![k("o/r", 1), k("o/r", 2), k("a/b", 3)];
        let keys = vec![k("o/r", 2), k("o/r", 1)];
        assert_eq!(retain_existing(&mut s, &keys), 1);
        assert_eq!(s, vec![k("o/r", 1), k("o/r", 2)]);
        assert_eq!(retain_existing(&mut s, &keys), 0);
    }

    #[test]
    fn batch_toggle_target_all_set_clears_otherwise_sets() {
        assert_eq!(batch_toggle_target([1, 1, 2].iter()), 0);
        assert_eq!(batch_toggle_target([1, 0, 1].iter()), 1);
        assert_eq!(batch_toggle_target([0, 0].iter()), 1);
        assert_eq!(batch_toggle_target(Vec::<i64>::new().iter()), 1);
    }
}
