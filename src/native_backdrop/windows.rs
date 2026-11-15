use core::ffi::c_void;

use raw_window_handle::RawWindowHandle;
use windows_sys::Win32::Foundation::{HWND, S_OK};
use windows_sys::Win32::Graphics::Dwm::{
    DWM_SYSTEMBACKDROP_TYPE, DWMSBT_NONE, DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DwmGetWindowAttribute, DwmSetWindowAttribute,
};

use super::{BackdropMode, BackdropRequest};

pub struct PlatformBackdrop {
    hwnd: Option<isize>,
    request: Option<BackdropRequest>,
    alpha_capable: bool,
    visible: bool,
    mode: BackdropMode,
    backdrop_type: Option<i32>,
    last_hresult: Option<i32>,
    dark_titlebar: Option<bool>,
}

impl Default for PlatformBackdrop {
    fn default() -> Self {
        Self {
            hwnd: None,
            request: None,
            alpha_capable: false,
            visible: false,
            mode: BackdropMode::Opaque,
            backdrop_type: None,
            last_hresult: None,
            dark_titlebar: None,
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
        let hwnd = match handle {
            Some(RawWindowHandle::Win32(handle)) => Some(handle.hwnd.get()),
            _ => None,
        };
        if self.hwnd == hwnd
            && self.request == Some(request)
            && self.alpha_capable == alpha_capable
            && self.visible == visible
        {
            return;
        }
        self.hwnd = hwnd;
        self.request = Some(request);
        self.alpha_capable = alpha_capable;
        self.visible = visible;
        if !visible {
            return;
        }

        let dark = request == BackdropRequest::Glass { dark: true };
        if self.dark_titlebar != Some(dark) {
            if let Some(hwnd) = hwnd {
                set_dark_titlebar(hwnd, dark);
            }
            self.dark_titlebar = Some(dark);
        }

        match request {
            BackdropRequest::Disabled => {
                self.mode = BackdropMode::Opaque;
                self.backdrop_type = None;
                self.last_hresult =
                    hwnd.map(|hwnd| set_backdrop(hwnd, DWMSBT_NONE).err().unwrap_or(S_OK));
            }
            BackdropRequest::Glass { .. } => {
                let result = if alpha_capable {
                    hwnd.ok_or(-1)
                        .and_then(|hwnd| set_backdrop(hwnd, DWMSBT_TRANSIENTWINDOW))
                } else {
                    Err(-2)
                };
                self.mode = glass_mode(alpha_capable, result);
                self.last_hresult = Some(result.err().unwrap_or(S_OK));
                self.backdrop_type = result.ok().filter(|_| self.mode == BackdropMode::Native);
            }
        }
    }

    pub fn mode(&self) -> BackdropMode {
        self.mode
    }

    pub fn backend_name(&self) -> &'static str {
        "windows-dwm-acrylic"
    }

    pub fn diagnostic(&self) -> String {
        format!(
            "hwnd={:?} backdrop_type={:?} hresult={:?} alpha_capable={}",
            self.hwnd, self.backdrop_type, self.last_hresult, self.alpha_capable
        )
    }
}

fn glass_mode(alpha_capable: bool, result: Result<i32, i32>) -> BackdropMode {
    if alpha_capable && result == Ok(DWMSBT_TRANSIENTWINDOW) {
        BackdropMode::Native
    } else {
        BackdropMode::Gradient
    }
}

fn set_backdrop(hwnd: isize, requested: DWM_SYSTEMBACKDROP_TYPE) -> Result<i32, i32> {
    let set_hr = unsafe {
        DwmSetWindowAttribute(
            hwnd as HWND,
            DWMWA_SYSTEMBACKDROP_TYPE as u32,
            &requested as *const _ as *const c_void,
            size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
        )
    };
    if set_hr != S_OK {
        return Err(set_hr);
    }
    let mut actual = 0;
    let get_hr = unsafe {
        DwmGetWindowAttribute(
            hwnd as HWND,
            DWMWA_SYSTEMBACKDROP_TYPE as u32,
            &mut actual as *mut _ as *mut c_void,
            size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
        )
    };
    if get_hr != S_OK {
        return Err(get_hr);
    }
    if actual != requested {
        return Err(-3);
    }
    Ok(actual)
}

fn set_dark_titlebar(hwnd: isize, dark: bool) {
    let enabled: i32 = i32::from(dark);
    unsafe {
        DwmSetWindowAttribute(
            hwnd as HWND,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &enabled as *const _ as *const c_void,
            size_of::<i32>() as u32,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_mode_requires_alpha_and_confirmed_acrylic() {
        assert_eq!(
            glass_mode(true, Ok(DWMSBT_TRANSIENTWINDOW)),
            BackdropMode::Native
        );
        assert_eq!(
            glass_mode(false, Ok(DWMSBT_TRANSIENTWINDOW)),
            BackdropMode::Gradient
        );
        assert_eq!(glass_mode(true, Ok(DWMSBT_NONE)), BackdropMode::Gradient);
        assert_eq!(glass_mode(true, Err(-1)), BackdropMode::Gradient);
    }

    #[test]
    fn missing_handle_and_regular_theme_use_safe_modes() {
        let mut backdrop = PlatformBackdrop::default();
        backdrop.sync(None, BackdropRequest::Glass { dark: true }, true, true);
        assert_eq!(backdrop.mode(), BackdropMode::Gradient);
        backdrop.sync(None, BackdropRequest::Disabled, true, true);
        assert_eq!(backdrop.mode(), BackdropMode::Opaque);
    }
}
