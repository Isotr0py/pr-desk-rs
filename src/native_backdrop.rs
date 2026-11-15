use raw_window_handle::{HasWindowHandle, RawWindowHandle};

#[cfg(target_os = "macos")]
#[path = "native_backdrop/macos.rs"]
mod platform;
#[cfg(not(any(windows, target_os = "macos")))]
#[path = "native_backdrop/fallback.rs"]
mod platform;
#[cfg(windows)]
#[path = "native_backdrop/windows.rs"]
mod platform;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackdropMode {
    Opaque,
    Native,
    Gradient,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackdropRequest {
    Disabled,
    Glass { dark: bool },
}

impl BackdropRequest {
    pub fn from_theme(theme: &str) -> Self {
        match theme {
            "glass-light" => Self::Glass { dark: false },
            "glass-dark" => Self::Glass { dark: true },
            _ => Self::Disabled,
        }
    }
}

pub struct NativeBackdrop {
    platform: platform::PlatformBackdrop,
    alpha_capable: bool,
}

impl NativeBackdrop {
    pub fn new(cc: &eframe::CreationContext<'_>, theme: &str) -> Self {
        let alpha_bits = cc.gl.as_ref().map(|gl| {
            use eframe::glow::HasContext as _;
            unsafe { gl.get_parameter_i32(eframe::glow::ALPHA_BITS) }
        });
        let alpha_capable = if cfg!(windows) {
            cc.gl.is_some()
        } else {
            alpha_bits.is_some_and(|bits| bits > 0)
        };
        let mut platform = platform::PlatformBackdrop::default();
        platform.sync(
            raw_handle(cc),
            BackdropRequest::from_theme(theme),
            alpha_capable,
            true,
        );
        Self {
            platform,
            alpha_capable,
        }
    }

    pub fn sync(&mut self, frame: &eframe::Frame, theme: &str, visible: bool) {
        self.platform.sync(
            raw_handle(frame),
            BackdropRequest::from_theme(theme),
            self.alpha_capable,
            visible,
        );
    }

    pub fn mode(&self) -> BackdropMode {
        self.platform.mode()
    }

    pub fn backend_name(&self) -> &'static str {
        self.platform.backend_name()
    }

    pub fn diagnostic(&self) -> String {
        self.platform.diagnostic()
    }

    pub fn clear_alpha(&self) -> u8 {
        clear_alpha_for(self.mode())
    }
}

fn clear_alpha_for(mode: BackdropMode) -> u8 {
    if mode == BackdropMode::Native { 0 } else { 255 }
}

fn raw_handle(source: &impl HasWindowHandle) -> Option<RawWindowHandle> {
    source.window_handle().ok().map(|handle| handle.as_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_requests_map_to_glass_variants() {
        assert_eq!(
            BackdropRequest::from_theme("light"),
            BackdropRequest::Disabled
        );
        assert_eq!(
            BackdropRequest::from_theme("dark"),
            BackdropRequest::Disabled
        );
        assert_eq!(
            BackdropRequest::from_theme("glass-light"),
            BackdropRequest::Glass { dark: false }
        );
        assert_eq!(
            BackdropRequest::from_theme("glass-dark"),
            BackdropRequest::Glass { dark: true }
        );
    }

    #[test]
    fn clear_alpha_is_transparent_only_in_native_mode() {
        for (mode, alpha) in [
            (BackdropMode::Opaque, 255),
            (BackdropMode::Gradient, 255),
            (BackdropMode::Native, 0),
        ] {
            assert_eq!(clear_alpha_for(mode), alpha);
        }
    }
}
