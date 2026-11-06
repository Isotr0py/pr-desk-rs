//! Windows 系统托盘（windows-sys 直调 Shell_NotifyIconW，GNU 工具链可编）。
//! 语义：关窗 → 隐藏到托盘而非退出；托盘图标左键还原窗口；右键菜单「显示 / 退出」。
//! 托盘线程跑独立的 Win32 消息循环（消息专用窗口收回调），事件经 Msg channel
//! + ctx.request_repaint() 送达 egui UI 线程。

#![cfg(windows)]

use std::sync::mpsc;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
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

/// 程序内生成 32x32 托盘图标（蓝底圆角方块 + 白色 PR 分支图形），BGRA 像素。
fn icon_pixels() -> ([u8; 32 * 32 * 4], [u8; 32 * 32 / 8]) {
    let mut px = [0u8; 32 * 32 * 4];
    let mut and = [0xFFu8; 32 * 32 / 8]; // AND 掩码：1 = 透明（保底；32bpp alpha 生效时被忽略）
    let set = |px: &mut [u8; 32 * 32 * 4], and: &mut [u8; 32 * 32 / 8], x: i32, y: i32, r: u8, g: u8, b: u8, a: u8| {
        if (0..32).contains(&x) && (0..32).contains(&y) {
            // CreateIcon 的 XOR 位图是自底向上的
            let i = ((31 - y) * 32 + x) as usize * 4;
            px[i] = b;
            px[i + 1] = g;
            px[i + 2] = r;
            px[i + 3] = a;
            let bi = ((31 - y) * 32 + x) as usize;
            and[bi / 8] &= !(1 << (7 - bi % 8));
        }
    };
    // 圆角方块底（r=6）
    for y in 0..32i32 {
        for x in 0..32i32 {
            let in_rect = x >= 2 && x < 30 && y >= 2 && y < 30;
            let corner = |cx: i32, cy: i32| (x - cx).pow(2) + (y - cy).pow(2) > 36;
            let clipped = (x < 8 && y < 8 && corner(8, 8))
                || (x >= 24 && y < 8 && corner(23, 8))
                || (x < 8 && y >= 24 && corner(8, 23))
                || (x >= 24 && y >= 24 && corner(23, 23));
            if in_rect && !clipped {
                set(&mut px, &mut and, x, y, 45, 125, 255, 255);
            }
        }
    }
    // 白色分支图形：左竖线两节点 + 右侧斜线节点（PR glyph）
    let circle = |px: &mut [u8; 32 * 32 * 4], and: &mut [u8; 32 * 32 / 8], cx: i32, cy: i32| {
        for dy in -4..=4 {
            for dx in -4..=4 {
                let d2 = dx * dx + dy * dy;
                if d2 <= 16 && d2 >= 7 {
                    set(px, and, cx + dx, cy + dy, 255, 255, 255, 255);
                }
            }
        }
    };
    let line = |px: &mut [u8; 32 * 32 * 4], and: &mut [u8; 32 * 32 / 8], x0: i32, y0: i32, x1: i32, y1: i32| {
        let steps = ((x1 - x0).abs().max((y1 - y0).abs())) * 2 + 1;
        for s in 0..=steps {
            let x = x0 + (x1 - x0) * s / steps;
            let y = y0 + (y1 - y0) * s / steps;
            for dx in -1..=1 {
                for dy in -1..=1 {
                    set(px, and, x + dx, y + dy, 255, 255, 255, 255);
                }
            }
        }
    };
    line(&mut px, &mut and, 11, 9, 11, 23);
    line(&mut px, &mut and, 12, 10, 21, 20);
    circle(&mut px, &mut and, 11, 9);
    circle(&mut px, &mut and, 11, 23);
    circle(&mut px, &mut and, 22, 21);
    (px, and)
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
    thread_id: ArcWrapper,
    handle: Option<std::thread::JoinHandle<()>>,
}

// 线程 id 回传用的轻量包装
struct ArcWrapper(std::sync::Arc<AtomicU32>);

impl Tray {
    /// 启动托盘（图标立刻可见）。show/quit 文案随启动时语言固定（托盘菜单不热切语言）。
    pub fn start(msg_tx: mpsc::Sender<Msg>, ctx: egui::Context, show: &str, quit: &str) -> Tray {
        let tid = std::sync::Arc::new(AtomicU32::new(0));
        let tid2 = tid.clone();
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
            tid2.store(GetCurrentThreadId(), Ordering::Release);

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
        Tray { thread_id: ArcWrapper(tid), handle: Some(handle) }
    }

    /// 真正退出：停托盘消息循环（图标移除在循环退出后做），join 线程。
    pub fn stop(&mut self) {
        let tid = self.thread_id.0.load(Ordering::Acquire);
        if tid != 0 {
            unsafe { PostThreadMessageW(tid, WM_QUIT, 0, 0) };
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

