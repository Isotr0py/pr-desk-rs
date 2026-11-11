//! GitHub 头像 / 组织图标拉取与本地磁盘缓存（egui 版）。
//! - 解析：`api.github.com/users/<name>` → `avatar_url`（github.com 被墙的网络下可用）
//! - 下载：`avatars.githubusercontent.com/...&s=64` → `data/avatars/<login>-v2.png`（orgs/ 子目录存组织）
//! - 文件缺失或超过 7 天由 `prefetch*` 重新拉取；`get_*` 只在缓存新鲜且能解码时返回
//!   `Some(egui::TextureHandle)`，否则返回 None，UI 走字母降级。
//! 头像落盘前做圆形 alpha 预裁（见 pr_desk_rs::circle_crop），UI 侧不再裁圆；
//! `-v2` 文件名后缀是缓存版本号：预裁引入前的旧方图（无 -v2）自然失效重拉，不会圆/方混用。
//! 组织图标是方形 logo，不裁圆。

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
    // 数据目录可被 set_data_dir 重锚定（防 cargo clean 清掉 target/ 里的库）
    pr_desk_rs::data_dir().join("avatars")
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

/// 读盘 + 解码为 RGBA（坏文件删除并返回 None——错误页存成 .png、解码失败等
/// 都走这里，否则坏缓存永远留在盘上且 prefetch 因 TTL 新鲜不重拉）。
fn load_rgba(path: &Path) -> Option<egui::ColorImage> {
    if !fresh(path) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let img = match image::load_from_memory(&bytes) {
        Ok(img) => img,
        Err(e) => {
            tracing::debug!("removing undecodable avatar cache {}: {e}", path.display());
            let _ = std::fs::remove_file(path);
            return None;
        }
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        let _ = std::fs::remove_file(path);
        return None;
    }
    Some(egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba))
}

/// 同步读缓存；未命中 / 过期 / 解码失败均返回 None（UI 用首字母占位）。
pub fn get_avatar(ctx: &egui::Context, login: &str) -> Option<egui::TextureHandle> {
    load_cached(ctx, &avatar_path(login))
}

/// 组织图标入口
pub fn get_org_icon(ctx: &egui::Context, org: &str) -> Option<egui::TextureHandle> {
    load_cached(ctx, &org_icon_path(org))
}

// 进程内纹理缓存：列表每次重建（刷新/过滤/切档案）都会为每行取头像 + 组织图标，
// 没有它每次重建 = 全量文件读盘 + PNG 解码（实测 108 行 600ms+）。
// 以文件 mtime 做一致性校验：prefetch 写入（tmp+rename）或坏缓存被删都会使 mtime
// 变化/消失，下次访问自动重载，不会用过期图。负缓存（文件缺席 → None）同样成立，
// prefetch 补拉落盘后 mtime 从 None 变 Some，自动命中新图。
// 调用点都在 UI 线程（渲染时取纹理），thread_local 即可，无需 Send。
thread_local! {
    static TEX_CACHE: RefCell<HashMap<PathBuf, (Option<std::time::SystemTime>, Option<egui::TextureHandle>)>> =
        RefCell::new(HashMap::new());
}

fn load_cached(ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    TEX_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some((mt, tex)) = c.get(path) {
            if *mt == mtime {
                return tex.clone();
            }
        }
        let tex = load_rgba(path).map(|img| {
            ctx.load_texture(
                format!("avatar:{}", path.display()),
                img,
                egui::TextureOptions::LINEAR,
            )
        });
        c.insert(path.to_path_buf(), (mtime, tex.clone()));
        tex
    })
}

/// 经共享 GitHub 客户端解析头像 URL 再下载（github.com/<login>.png 的重定向在
/// 本机网络下不可用；api.github.com + avatars.githubusercontent.com 可达）。
/// 复用 GitHub 客户端而不是自建——独立 reqwest 客户端在 worker 运行时下偶发
/// 请求挂死（原因未查明），共享客户端的连接池已被证明稳定。
async fn fetch_one(gh: &pr_desk_rs::github::GitHub, name: String, path: PathBuf, circle: bool) -> bool {
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
    //（FF D8 FF），默认头像才是 PNG——统一解码再按 PNG 落盘，否则按 .png
    // 扩展名解析 JPEG 会解码失败，缓存被反复删了重拉
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
    // 头像做圆形 alpha 预裁（circle_crop）：透明圆角烘进像素后 UI 不再裁圆。
    // 组织图标是方形 logo，不裁。
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

async fn prefetch_to(gh: &pr_desk_rs::github::GitHub, path_of: fn(&str) -> PathBuf, names: Vec<String>, circle: bool) -> usize {
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
pub async fn prefetch(gh: &pr_desk_rs::github::GitHub, logins: Vec<String>) -> usize {
    prefetch_to(gh, avatar_path, logins, true).await
}

/// 后台预取组织图标（方形 logo，不做圆形预裁）。
pub async fn prefetch_orgs(gh: &pr_desk_rs::github::GitHub, orgs: Vec<String>) -> usize {
    prefetch_to(gh, org_icon_path, orgs, false).await
}
