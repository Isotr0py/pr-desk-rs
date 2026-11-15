use core::ptr::NonNull;

use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameVibrantDark,
    NSAppearanceNameVibrantLight, NSAutoresizingMaskOptions, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindowOrderingMode,
};
use raw_window_handle::RawWindowHandle;

use super::{BackdropMode, BackdropRequest};

pub struct PlatformBackdrop {
    content_view: Option<NonNull<NSView>>,
    blur_view: Option<Retained<NSVisualEffectView>>,
    request: Option<BackdropRequest>,
    alpha_capable: bool,
    visible: bool,
    appearance_dark: Option<bool>,
    attached: bool,
    blur_hidden: bool,
    mode: BackdropMode,
    error: Option<&'static str>,
}

impl Default for PlatformBackdrop {
    fn default() -> Self {
        Self {
            content_view: None,
            blur_view: None,
            request: None,
            alpha_capable: false,
            visible: false,
            appearance_dark: None,
            attached: false,
            blur_hidden: true,
            mode: BackdropMode::Opaque,
            error: None,
        }
    }
}

impl PlatformBackdrop {
    pub fn sync(
        &mut self,
        handle: Option<RawWindowHandle>,
        request: BackdropRequest,
        alpha_capable: bool,
        visible: bool,
    ) {
        let content_view = appkit_view(handle);
        if self.content_view == content_view
            && self.request == Some(request)
            && self.alpha_capable == alpha_capable
            && self.visible == visible
            && self.error.is_none()
        {
            return;
        }

        let view_changed = self.content_view != content_view;
        self.request = Some(request);
        self.alpha_capable = alpha_capable;
        self.visible = visible;
        self.mode = match request {
            BackdropRequest::Disabled => BackdropMode::Opaque,
            BackdropRequest::Glass { .. } => BackdropMode::Gradient,
        };
        self.error = None;

        let mtm = MainThreadMarker::new();
        if view_changed {
            if self.blur_view.is_some() {
                let Some(mtm) = mtm else {
                    self.error = Some("main thread required to remove old NSVisualEffectView");
                    return;
                };
                self.remove_blur(mtm);
            }
            self.content_view = content_view;
        }

        match request {
            BackdropRequest::Disabled => self.hide_for_fallback(mtm, None),
            BackdropRequest::Glass { .. } if !alpha_capable => {
                self.hide_for_fallback(mtm, Some("OpenGL framebuffer has no alpha channel"));
            }
            BackdropRequest::Glass { .. } if !visible => self.hide_for_fallback(mtm, None),
            BackdropRequest::Glass { dark } => {
                let Some(mtm) = mtm else {
                    self.error = Some("main thread required to configure NSVisualEffectView");
                    return;
                };
                let Some(content_view) = self.content_view else {
                    self.error = Some("AppKit NSView handle unavailable");
                    return;
                };
                // SAFETY: The raw-window-handle contract provides a valid NSView, and all AppKit
                // access in this branch is guarded by MainThreadMarker.
                let content_view = unsafe { content_view.as_ref() };
                self.sync_glass(mtm, content_view, dark);
            }
        }
    }

    pub fn mode(&self) -> BackdropMode {
        self.mode
    }

    pub fn backend_name(&self) -> &'static str {
        "macos-nsvisualeffectview"
    }

    pub fn diagnostic(&self) -> String {
        format!(
            "ns_view={:?} attached={} blur_hidden={} request={:?} alpha_capable={} visible={} error={:?}",
            self.content_view,
            self.attached,
            self.blur_hidden,
            self.request,
            self.alpha_capable,
            self.visible,
            self.error
        )
    }

    fn sync_glass(&mut self, mtm: MainThreadMarker, content_view: &NSView, dark: bool) {
        let appearance = if self.appearance_dark != Some(dark) {
            // SAFETY: These AppKit appearance constants are available on macOS 10.14+.
            let name = unsafe {
                if dark {
                    NSAppearanceNameVibrantDark
                } else {
                    NSAppearanceNameVibrantLight
                }
            };
            let Some(appearance) = NSAppearance::appearanceNamed(name) else {
                self.hide_for_fallback(Some(mtm), Some("requested NSAppearance is unavailable"));
                return;
            };
            Some(appearance)
        } else {
            None
        };

        if self.blur_view.is_none() {
            let blur = NSVisualEffectView::initWithFrame(mtm.alloc(), content_view.bounds());
            blur.setMaterial(NSVisualEffectMaterial::UnderWindowBackground);
            blur.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            blur.setState(NSVisualEffectState::FollowsWindowActiveState);
            blur.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            blur.setHidden(true);
            self.blur_view = Some(blur);
            self.blur_hidden = true;
            self.attached = false;
        }

        if let Some(appearance) = appearance {
            let blur = self
                .blur_view
                .as_deref()
                .expect("blur view was just created");
            blur.setAppearance(Some(&appearance));
            self.appearance_dark = Some(dark);
        }

        let blur = self.blur_view.as_deref().expect("blur view must exist");
        // SAFETY: AppKit is being accessed on the main thread, and objc2 retains the returned
        // superview for the duration of this check.
        let directly_attached = unsafe { blur.superview() }
            .is_some_and(|superview| core::ptr::eq(&*superview, content_view));
        if !directly_attached {
            content_view.addSubview_positioned_relativeTo(blur, NSWindowOrderingMode::Below, None);
        }

        // SAFETY: Same main-thread and retained-superview guarantees as above.
        self.attached = unsafe { blur.superview() }
            .is_some_and(|superview| core::ptr::eq(&*superview, content_view));
        if !self.attached {
            blur.setHidden(true);
            self.blur_hidden = true;
            self.mode = BackdropMode::Gradient;
            self.error = Some("failed to attach NSVisualEffectView to contentView");
            return;
        }

        blur.setHidden(false);
        self.blur_hidden = false;
        self.mode = BackdropMode::Native;
        self.error = None;
    }

    fn hide_for_fallback(&mut self, mtm: Option<MainThreadMarker>, error: Option<&'static str>) {
        self.error = error;
        self.attached = self.attached && self.blur_view.is_some();
        if self.blur_view.is_none() {
            self.blur_hidden = true;
            return;
        }
        if let Some(mtm) = mtm {
            self.hide_blur(mtm);
        } else {
            self.error = Some("main thread required to hide NSVisualEffectView");
        }
    }

    fn hide_blur(&mut self, _mtm: MainThreadMarker) {
        if let Some(blur) = &self.blur_view {
            blur.setHidden(true);
        }
        self.blur_hidden = true;
    }

    fn remove_blur(&mut self, _mtm: MainThreadMarker) {
        if let Some(blur) = self.blur_view.take() {
            blur.removeFromSuperview();
        }
        self.attached = false;
        self.blur_hidden = true;
        self.appearance_dark = None;
    }
}

impl Drop for PlatformBackdrop {
    fn drop(&mut self) {
        if let Some(mtm) = MainThreadMarker::new() {
            self.remove_blur(mtm);
        }
    }
}

fn appkit_view(handle: Option<RawWindowHandle>) -> Option<NonNull<NSView>> {
    match handle {
        Some(RawWindowHandle::AppKit(handle)) => Some(handle.ns_view.cast::<NSView>()),
        _ => None,
    }
}
