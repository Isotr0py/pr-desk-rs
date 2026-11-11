//! Windows 系统托盘（windows-sys 直调 Shell_NotifyIconW，GNU 工具链可编）。
//! 语义：关窗 → 隐藏到托盘而非退出；托盘图标左键还原窗口；右键菜单「显示 / 退出」。
//! 托盘线程跑独立的 Win32 消息循环（消息专用窗口收回调），事件经 Msg channel
//! + ctx.request_repaint() 送达 egui UI 线程。

#![cfg(windows)]

use std::sync::mpsc;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::state::Msg;

const WM_TRAY: u32 = WM_USER + 1;
const IDM_SHOW: usize = 1;
const IDM_QUIT: usize = 2;

/// 托盘线程上下文（堆分配，生命周期覆盖整个线程；地址经 lpParam 传给 wndproc）。
struct TrayCtx {
    msg_tx: mpsc::Sender<Msg>,
    ctx: egui::Context,
    show_text: Vec<u16>,
    quit_text: Vec<u16>,
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 托盘图标：内嵌 PR 形状图标（assets/app-icon-32.png，Slint 版 app.svg 的离线渲染，
/// 运行时不带 SVG 渲染依赖）。CreateIcon 的 XOR 位图是 BGRA 自底向上。
fn icon_pixels() -> ([u8; 32 * 32 * 4], [u8; 32 * 32 / 8]) {
    let png = include_bytes!("../assets/app-icon-32.png");
    let img = image::load_from_memory(png)
        .expect("内嵌图标 PNG 解码失败")
        .to_rgba8();
    let mut xor = [0u8; 32 * 32 * 4];
    let mut and = [0xFFu8; 32 * 32 / 8]; // AND 掩码：1 = 透明（保底；32bpp alpha 生效时被忽略）
    for y in 0..32u32 {
        for x in 0..32u32 {
            let p = img.get_pixel(x, y).0;
            let i = ((31 - y) * 32 + x) as usize * 4;
            xor[i] = p[2];
            xor[i + 1] = p[1];
            xor[i + 2] = p[0];
            xor[i + 3] = p[3];
            if p[3] > 0 {
                let bi = ((31 - y) * 32 + x) as usize;
                and[bi / 8] &= !(1 << (7 - bi % 8));
            }
        }
    }
    (xor, and)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCREATE => {
                let cs = lparam as *const CREATESTRUCTW;
                let ctx = (*cs).lpCreateParams;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx as isize);
                return 1;
            }
            WM_TRAY => {
                let ctx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const TrayCtx;
                if ctx.is_null() {
                    return 0;
                }
                let ctx = &*ctx;
                match lparam as u32 {
                    WM_LBUTTONUP => {
                        tracing::info!("托盘 wndproc：左键");
                        let _ = ctx.msg_tx.send(Msg::TrayShow);
                        ctx.ctx.request_repaint();
                    }
                    WM_RBUTTONUP => {
                        let menu = CreatePopupMenu();
                        if !menu.is_null() {
                            AppendMenuW(menu, MF_STRING, IDM_SHOW, ctx.show_text.as_ptr());
                            AppendMenuW(menu, MF_STRING, IDM_QUIT, ctx.quit_text.as_ptr());
                            let mut pt: POINT = std::mem::zeroed();
                            GetCursorPos(&mut pt);
                            // TrackPopupMenu 前置要求：本线程窗口先到前台，
                            // 否则点菜单外部不收起菜单（Win32 老坑）
                            SetForegroundWindow(hwnd);
                            let cmd = TrackPopupMenu(
                                menu,
                                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                                pt.x,
                                pt.y,
                                0,
                                hwnd,
                                std::ptr::null(),
                            );
                            PostMessageW(hwnd, WM_NULL, 0, 0);
                            DestroyMenu(menu);
                            let m = match cmd as usize {
                                IDM_SHOW => Some(Msg::TrayShow),
                                IDM_QUIT => Some(Msg::TrayQuit),
                                _ => None,
                            };
                            if let Some(m) = m {
                                let _ = ctx.msg_tx.send(m);
                                ctx.ctx.request_repaint();
                            }
                        }
                    }
                    _ => {}
                }
                return 0;
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

pub struct Tray {
    hwnd: usize,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Tray {
    /// 启动托盘（图标立刻可见）。show/quit 文案随启动时语言固定（托盘菜单不热切语言）。
    pub fn start(msg_tx: mpsc::Sender<Msg>, ctx: egui::Context, show: &str, quit: &str) -> Tray {
        // hwnd 经 channel 回传（stop 需要 PostMessageW(hwnd, WM_QUIT)——
        // PostThreadMessage 的线程消息 hwnd=NULL，会被按窗口过滤的 GetMessageW 跳过）
        let (hwnd_tx, hwnd_rx) = std::sync::mpsc::channel::<usize>();
        let show = show.to_string();
        let quit = quit.to_string();
        let handle = std::thread::spawn(move || unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let class_name = to_wide("PRDeskTrayWnd");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst,
                lpszClassName: class_name.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let ctx = Box::new(TrayCtx {
                msg_tx,
                ctx,
                show_text: to_wide(&show),
                quit_text: to_wide(&quit),
            });
            let ctx_raw = Box::into_raw(ctx);
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                to_wide("PR Desk Tray").as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinst,
                ctx_raw as *mut core::ffi::c_void,
            );
            if hwnd.is_null() {
                tracing::warn!("托盘窗口创建失败");
                return;
            }
            // 图标：程序内生成的 32x32 BGRA → CreateIcon
            let (px, and) = icon_pixels();
            let icon = CreateIcon(hinst, 32, 32, 1, 32, and.as_ptr(), px.as_ptr());
            let tooltip = to_wide("PR Desk");
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            nid.uCallbackMessage = WM_TRAY;
            nid.hIcon = icon;
            nid.szTip[..tooltip.len().saturating_sub(1)].copy_from_slice(&tooltip[..tooltip.len() - 1]);
            if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
                tracing::warn!("托盘图标添加失败");
                DestroyWindow(hwnd);
                return;
            }
            let _ = hwnd_tx.send(hwnd as usize);

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, hwnd, 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            Shell_NotifyIconW(NIM_DELETE, &nid);
            DestroyIcon(icon);
            drop(Box::from_raw(ctx_raw));
            DestroyWindow(hwnd);
            UnregisterClassW(class_name.as_ptr(), hinst);
        });
        let hwnd = hwnd_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or(0);
        if hwnd == 0 {
            tracing::warn!("托盘窗口 hwnd 回传超时/失败");
        }
        Tray { hwnd, handle: Some(handle) }
    }

    /// 真正退出：停托盘消息循环（图标移除在循环退出后做），join 线程。
    pub fn stop(&mut self) {
        if self.hwnd != 0 {
            // PostMessageW(hwnd, WM_QUIT)：GetMessageW(hwnd 过滤）能收到；
            // PostThreadMessage 的 hwnd=NULL 会被该过滤跳过（僵尸进程的病根）
            unsafe { PostMessageW(self.hwnd as HWND, WM_QUIT, 0, 0) };
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

