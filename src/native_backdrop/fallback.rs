use raw_window_handle::RawWindowHandle;

use super::{BackdropMode, BackdropRequest};

pub struct PlatformBackdrop {
    mode: BackdropMode,
}

impl Default for PlatformBackdrop {
    fn default() -> Self {
        Self {
            mode: BackdropMode::Opaque,
        }
    }
}

impl PlatformBackdrop {
    pub fn restore(&self) {}

    pub fn sync(
        &mut self,
        _handle: Option<RawWindowHandle>,
        request: BackdropRequest,
        _alpha_capable: bool,
        _visible: bool,
    ) {
        self.mode = match request {
            BackdropRequest::Disabled => BackdropMode::Opaque,
            BackdropRequest::Glass { .. } => BackdropMode::Gradient,
        };
    }

    pub fn mode(&self) -> BackdropMode {
        self.mode
    }

    pub fn backend_name(&self) -> &'static str {
        "gradient-fallback"
    }

    pub fn diagnostic(&self) -> String {
        "native backdrop unavailable on this platform".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_opaque_only_for_regular_themes() {
        let mut backdrop = PlatformBackdrop::default();
        backdrop.sync(None, BackdropRequest::Glass { dark: false }, true, true);
        assert_eq!(backdrop.mode(), BackdropMode::Gradient);
        backdrop.sync(None, BackdropRequest::Disabled, true, true);
        assert_eq!(backdrop.mode(), BackdropMode::Opaque);
    }
}
