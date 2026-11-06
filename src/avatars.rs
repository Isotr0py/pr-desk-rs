//! GitHub 头像 / 组织图标拉取与本地磁盘缓存。
//! - 解析：`api.github.com/users/<name>` → `avatar_url`（github.com 被墙的网络下可用）
//! - 下载：`avatars.githubusercontent.com/...&s=64` → `data/avatars/<login>-v2.png`（orgs/ 子目录存组织）
//! 文件缺失或超过 7 天由 `prefetch*` 重新拉取；`get_*` 只在缓存新鲜且能解码时返回
//! `Some(slint::Image)`（像素经 Rgba8 SharedPixelBuffer 落位），否则返回 None，UI 走字母降级。
//! 头像落盘前做圆形 alpha 预裁（见 pr_desk_rs::circle_crop），UI 侧不再开 stencil clip；
//! `-v2` 文件名后缀是缓存版本号：预裁引入前的旧方图（无 -v2）自然失效重拉，不会圆/方混用。
//! 组织图标是方形 logo，不裁圆（OrgIcon 组件走 3px 圆角矩形，不是圆形）。

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::collections::HashMap;
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
    pr_desk_rs::exe_data_dir().join("avatars")
}

pub fn avatar_path(login: &str) -> PathBuf {
    // -v2：缓存版本号。v1 是未裁圆的方图，路径错开后旧缓存永远不再命中，
    // 由 prefetch 按 TTL/缺失语义自然重拉成预裁圆图（老 .png 残留无害）。
    cache_dir().join(format!("{}-v2.png", key(login)))
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
    // 加载即失败（如错误页被存成 .png、根本不是 PNG）同样删掉坏缓存，
    // 否则文件永远留在盘上，每次启动都被打错误日志且 prefetch 因 TTL 新鲜不重拉。
    let img = match slint::Image::load_from_path(path) {
        Ok(img) => img,
        Err(e) => {
            tracing::debug!("removing unreadable avatar cache {}: {e}", path.display());
            let _ = std::fs::remove_file(path);
            return None;
        }
    };
    // 立即解码为 Rgba8 像素 buffer：坏文件在这里暴露并走降级，而不是拖到渲染时。
    // 解码失败时删掉坏缓存，下次 prefetch 才能重新拉取。
    let buf = match img.to_rgba8() {
        Some(b) if b.width() > 0 => b,
        _ => {
            tracing::debug!("removing undecodable avatar cache {}", path.display());
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
    load_cached(&avatar_path(login))
}

/// 组织图标入口
pub fn get_org_icon(org: &str) -> Option<slint::Image> {
    load_cached(&org_icon_path(org))
}

// 进程内图片缓存：列表每次重建（刷新/过滤/切档案）都会为每行取头像 + 组织图标，
// 没有它每次重建 = 全量文件读盘 + PNG 解码，全跑在 UI 线程上（实测 108 行 600ms+，
// 切一次档案重建 3 次，界面直接冻住）。slint::Image 是 Rc，克隆廉价。
// 以文件 mtime 做一致性校验：prefetch 写入（tmp+rename）或坏缓存被删都会使 mtime
// 变化/消失，下次访问自动重载，不会用过期图。负缓存（文件缺席 → None）同样成立，
// prefetch 补拉落盘后 mtime 从 None 变 Some，自动命中新图。
// 调用点都在 UI 线程（build_models / Msg::Panel），thread_local 即可，无需 Send。
thread_local! {
    static IMG_CACHE: RefCell<HashMap<PathBuf, (Option<std::time::SystemTime>, Option<slint::Image>)>> =
        RefCell::new(HashMap::new());
}

fn load_cached(path: &Path) -> Option<slint::Image> {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    IMG_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some((mt, img)) = c.get(path) {
            if *mt == mtime {
                return img.clone();
            }
        }
        let img = load(path);
        c.insert(path.to_path_buf(), (mtime, img.clone()));
        img
    })
}

/// 经共享 GitHub 客户端解析头像 URL 再下载（github.com/<login>.png 的重定向在
/// 本机网络下不可用；api.github.com + avatars.githubusercontent.com 可达）。
/// 复用 GitHub 客户端而不是自建——独立 reqwest 客户端在 worker 运行时下偶发
/// 请求挂死（原因未查明），共享客户端的连接池已被证明稳定。
async fn fetch_one(gh: &crate::github::GitHub, name: String, path: PathBuf, circle: bool) -> bool {
    let Ok(Some(avatar_url)) = gh.fetch_avatar_url(&name).await else {
        tracing::info!("avatar: {name} 的 /users 解析失败");
        return false;
    };
    let bytes = match gh.fetch_bytes(&avatar_url).await {
        Ok(b) => b,
        Err(e) => {
            tracing::info!("avatar: {name} 下载失败 {avatar_url}: {e}");
            return false;
        }
    };
    // GitHub 对不存在的名字也会 302 到默认头像图；过小的响应视为错误页，不写缓存
    if bytes.len() < 64 {
        return false;
    }
    // 只写真图片（网络错误页/代理拦截页不落盘）。CDN 对真人照片回 JPEG
    //（FF D8 FF），默认头像才是 PNG——统一解码再按 PNG 落盘，否则 image::open
    // 按 .png 扩展名解析 JPEG 会解码失败，缓存被反复删了重拉
    let is_png = bytes.starts_with(b"\x89PNG\r\n\x1a\n");
    let is_jpeg = bytes.starts_with(b"\xFF\xD8\xFF");
    if !is_png && !is_jpeg {
        tracing::info!("avatar: {name} 下载内容不是 PNG/JPEG，丢弃");
        return false;
    }
    let Ok(img) = image::load_from_memory(&bytes) else {
        tracing::info!("avatar: {name} 图片解码失败，丢弃");
        return false;
    };
    // 头像做圆形 alpha 预裁（circle_crop）：透明圆角烘进像素后 UI 不再开
    // stencil clip 裁圆。组织图标是方形 logo，不裁（OrgIcon 走圆角矩形 clip）。
    let img = if circle {
        image::DynamicImage::ImageRgba8(pr_desk_rs::circle_crop::circle_crop(&img.to_rgba8()))
    } else {
        img
    };
    let mut buf = Vec::new();
    if img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png).is_err() {
        return false;
    }
    let bytes = buf;
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

async fn prefetch_to(gh: &crate::github::GitHub, path_of: fn(&str) -> PathBuf, names: Vec<String>, circle: bool) -> usize {
    let names: BTreeSet<String> = names
        .into_iter()
        .filter(|n| !n.trim().is_empty())
        .filter(|n| !fresh(&path_of(n)))
        .collect();
    if names.is_empty() {
        return 0;
    }
    tracing::info!("avatar prefetch: {} 个待拉取", names.len());
    let mut set = tokio::task::JoinSet::new();
    for name in names {
        let path = path_of(&name);
        let gh = gh.clone();
        set.spawn(async move { fetch_one(&gh, name, path, circle).await });
    }
    let mut fetched = 0;
    while let Some(res) = set.join_next().await {
        if matches!(res, Ok(true)) {
            fetched += 1;
        }
    }
    tracing::info!("avatar prefetch: 拉取完成 {fetched}");
    fetched
}

/// 后台预取用户头像；返回实际新拉取的数量（全部为新鲜缓存时返回 0，调用方据此决定是否重绘）。
pub async fn prefetch(gh: &crate::github::GitHub, logins: Vec<String>) -> usize {
    prefetch_to(gh, avatar_path, logins, true).await
}

/// 后台预取组织图标（方形 logo，不做圆形预裁）。
pub async fn prefetch_orgs(gh: &crate::github::GitHub, orgs: Vec<String>) -> usize {
    prefetch_to(gh, org_icon_path, orgs, false).await
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
    fn avatar_path_is_v2_versioned() {
        // 预裁圆引入缓存版本号：旧方图（无 -v2）永不命中，自然重拉
        let p = avatar_path("Octo-Cat");
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "octo-cat-v2.png");
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

    #[test]
    fn load_garbage_deletes_file() {
        let dir = std::env::temp_dir().join("pr-desk-avatars-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad-deleted.png");
        std::fs::write(&p, b"not a png at all, but long enough to pass the size check......").unwrap();
        assert!(p.exists());
        assert!(load(&p).is_none());
        // 坏缓存必须被删掉，下次 prefetch 才会重新拉取（fresh 对不存在的文件返回 false）
        assert!(!p.exists(), "garbage cache file should be removed");
    }
}
