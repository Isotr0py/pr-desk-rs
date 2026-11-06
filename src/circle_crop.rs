//! 头像圆形 alpha 预裁：把圆角透明烘进像素，UI 不再靠 stencil clip 裁圆。
//! femtovg 每个圆形 clip 都是一次 stencil pass——列表里 25-50 个头像滚动时
//! 在集显上是最大的渲染热点；预裁后 Image 自带透明圆角，clip 整个省掉。
//! 放在 lib（而不是二进制侧的 avatars.rs）是为了 `cargo test --no-default-features --lib`
//! 能覆盖（avatars.rs 依赖 slint::Image，只能随 UI feature 编译）。

/// 把方形 RGBA 图裁成圆形：内切圆内保持原 alpha，圆外 alpha=0，
/// 边缘留约 1px 线性过渡带抗锯齿。非方形图按短边内切圆居中裁。
pub fn circle_crop(img: &image::RgbaImage) -> image::RgbaImage {
    let (w, h) = img.dimensions();
    let mut out = img.clone();
    if w == 0 || h == 0 {
        return out;
    }
    let r = w.min(h) as f32 / 2.0;
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    for y in 0..h {
        for x in 0..w {
            // 以像素中心采样：d ≤ r-0.5 全覆盖，d ≥ r+0.5 全透明，中间线性过渡
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let cov = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            let p = out.get_pixel_mut(x, y);
            p[3] = (p[3] as f32 * cov).round() as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32) -> image::RgbaImage {
        image::RgbaImage::from_pixel(w, h, image::Rgba([200, 30, 30, 255]))
    }

    #[test]
    fn corners_transparent_center_opaque() {
        let out = circle_crop(&solid(16, 16));
        for (x, y) in [(0, 0), (15, 0), (0, 15), (15, 15)] {
            assert_eq!(out.get_pixel(x, y)[3], 0, "角 ({x},{y}) 应全透明");
        }
        assert_eq!(out.get_pixel(8, 8)[3], 255, "中心应保持不透明");
        // 颜色通道不被改动
        assert_eq!(out.get_pixel(8, 8)[0], 200);
    }

    #[test]
    fn edge_has_antialias_transition() {
        let out = circle_crop(&solid(64, 64));
        // 正上方边缘带（x=32, y 很小）应存在 0<alpha<255 的过渡像素
        let has_partial = (0..6).any(|y| {
            let a = out.get_pixel(32, y)[3];
            a > 0 && a < 255
        });
        assert!(has_partial, "圆边缘应有约 1px 的抗锯齿过渡");
    }

    #[test]
    fn preserves_existing_alpha_and_handles_nonsquare() {
        let mut img = solid(20, 10);
        img.get_pixel_mut(10, 5)[3] = 128; // 中心半透明
        let out = circle_crop(&img);
        assert_eq!(out.get_pixel(10, 5)[3], 128, "圆内原 alpha 应保持");
        assert_eq!(out.get_pixel(0, 0)[3], 0);
        assert_eq!(out.dimensions(), (20, 10));
        // 空图不 panic
        let _ = circle_crop(&image::RgbaImage::new(0, 0));
    }
}
