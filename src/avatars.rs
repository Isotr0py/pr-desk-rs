//! GitHub 头像 / 组织图标拉取与本地磁盘缓存。
//! - 用户头像：`https://github.com/<login>.png?size=64` → `data/avatars/<login>.png`
//! - 组织图标：`https://github.com/<org>.png?size=32`   → `data/avatars/orgs/<org>.png`
//! 文件缺失或超过 7 天由 `prefetch*` 重新拉取；`get_*` 只在缓存新鲜且能解码时返回
//! `Some(slint::Image)`（像素经 Rgba8 SharedPixelBuffer 落位），否则返回 None，UI 走字母降级。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// login/org 名为 URL 路径片段，落盘前过滤掉非 [a-z0-9-] 字符并小写化。
fn key(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_lowercase()
}

pub fn cache_dir() -> PathBuf {
    PathBuf::from("data").join("avatars")
}

pub fn avatar_path(login: &str) -> PathBuf {
    cache_dir().join(format!("{}.png", key(login)))
}

pub fn org_icon_path(org: &str) -> PathBuf {
    cache_dir().join("orgs").join(format!("{}.png", key(org)))
}

/// 缓存是否在 TTL（7 天）内；元数据读取失败一律视为不新鲜。
fn fresh(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e < CACHE_TTL).unwrap_or(false))
        .unwrap_or(false)
}

fn load(path: &Path) -> Option<slint::Image> {
    if !fresh(path) {
        return None;
    }
    let img = slint::Image::load_from_path(path).ok()?;
    // 立即解码为 Rgba8 像素 buffer：坏文件在这里暴露并走降级，而不是拖到渲染时。
    // 解码失败（如错误页被存成 .png）时删掉坏缓存，下次 prefetch 才能重新拉取。
    let buf = match img.to_rgba8() {
        Some(b) if b.width() > 0 => b,
        _ => {
            let _ = std::fs::remove_file(path);
            return None;
        }
    };
    Some(slint::Image::from_rgba8(buf))
}

/// 同步读缓存；未命中 / 过期 / 解码失败均返回 None（UI 用首字母占位）。
// 集成方在 PrRow 增加 image 字段后由 model::to_row 调用
#[allow(dead_code)]
pub fn get_avatar(login: &str) -> Option<slint::Image> {
    load(&avatar_path(login))
}

// 同上：组织图标入口
#[allow(dead_code)]
pub fn get_org_icon(org: &str) -> Option<slint::Image> {
    load(&org_icon_path(org))
}

async fn fetch_one(client: &reqwest::Client, url: String, path: PathBuf) -> bool {
    let bytes = match client.get(&url).send().await {
        Ok(resp) => match resp.bytes().await {
            Ok(b) => b,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    // GitHub 对不存在的名字也会 302 到默认头像图；过小的响应视为错误页，不写缓存
    if bytes.len() < 64 {
        return false;
    }
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    // 先写临时文件再 rename，避免 UI 线程读到写了一半的 PNG
    let tmp = path.with_extension("part");
    if std::fs::write(&tmp, &bytes).is_err() {
        return false;
    }
    std::fs::rename(&tmp, &path).is_ok()
}

async fn prefetch_to(path_of: fn(&str) -> PathBuf, size: u32, names: Vec<String>) -> usize {
    let names: BTreeSet<String> = names
        .into_iter()
        .filter(|n| !n.trim().is_empty())
        .filter(|n| !fresh(&path_of(n)))
        .collect();
    if names.is_empty() {
        return 0;
    }
    let client = match reqwest::Client::builder()
        .user_agent("pr-desk-rs")
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let mut set = tokio::task::JoinSet::new();
    for name in names {
        let client = client.clone();
        let url = format!("https://github.com/{name}.png?size={size}");
        let path = path_of(&name);
        set.spawn(async move { fetch_one(&client, url, path).await });
    }
    let mut fetched = 0;
    while let Some(res) = set.join_next().await {
        if matches!(res, Ok(true)) {
            fetched += 1;
        }
    }
    fetched
}

/// 后台预取用户头像；返回实际新拉取的数量（全部为新鲜缓存时返回 0，调用方据此决定是否重绘）。
pub async fn prefetch(logins: Vec<String>) -> usize {
    prefetch_to(avatar_path, 64, logins).await
}

/// 后台预取组织图标（32px）。
pub async fn prefetch_orgs(orgs: Vec<String>) -> usize {
    prefetch_to(org_icon_path, 32, orgs).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1x1 红色 PNG
    const ONE_PX_PNG: [u8; 70] = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8,
        0xCF, 0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn key_sanitizes() {
        assert_eq!(key("Octo-Cat"), "octo-cat");
        assert_eq!(key("../etc/passwd"), "etcpasswd");
        assert_eq!(key("中文名"), "");
    }

    #[test]
    fn load_missing_or_stale_is_none() {
        let p = std::env::temp_dir().join("pr-desk-avatars-test-missing.png");
        let _ = std::fs::remove_file(&p);
        assert!(load(&p).is_none());
    }

    #[test]
    fn load_png_decodes_to_rgba8() {
        let dir = std::env::temp_dir().join("pr-desk-avatars-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("one.png");
        std::fs::write(&p, ONE_PX_PNG).unwrap();
        let img = load(&p).expect("valid PNG should decode");
        assert_eq!(img.size().width, 1);
        assert_eq!(img.size().height, 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_garbage_is_none() {
        let dir = std::env::temp_dir().join("pr-desk-avatars-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.png");
        std::fs::write(&p, b"not a png at all, but long enough to pass the size check......").unwrap();
        assert!(load(&p).is_none());
        let _ = std::fs::remove_file(&p);
    }
}
