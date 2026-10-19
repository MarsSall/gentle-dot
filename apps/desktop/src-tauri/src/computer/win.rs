//! The Windows desktop for computer control (S24 on Windows): input through `SendInput`, and
//! screenshots of the monitor under the pointer through GDI. Gentle Dot's own windows are
//! left out of every capture by their display affinity (`WDA_EXCLUDEFROMCAPTURE`), set where
//! the windows are created.
//!
//! Points are physical pixels of the virtual screen: the app is per-monitor DPI aware, so the
//! pointer, the monitors, and `SendInput` all use them.
//!
//! Apps are processes with a taskbar window (`winapps`). An app's identity is its process:
//! the executable's file name stands for the macOS bundle id, and its file description for the
//! app name. Store apps draw inside a frame window owned by `ApplicationFrameHost.exe`; their
//! identity is the process of the frame's core window, so Settings is Settings, not its host.
//! An app Windows will not identify (a process that cannot be queried, a frame whose app is not
//! found) is the blocked `UNIDENTIFIED` app, so an action aimed at it is refused.
//!
//! Elements come from UI Automation (`winaccess` maps them), with a 0.5 s timeout so a hung app
//! cannot stall the helper, as the Accessibility timeout does on macOS.

use super::coords::{fit_long_edge, Point, ScreenshotGeometry, MAX_LONG_EDGE};
use super::encode::{bgra_to_jpeg, JPEG_QUALITY};
use super::blocklist::{blocked_name, blocked_reason, AppIdentity, UNIDENTIFIED};
use super::risk::{AxElement, Focus};
use super::glow::Frame;
use super::winaccess::{element, is_default, unreadable, UIA_BUTTON, UIA_WINDOW};
use super::winapps::{
    answers_to, app_pids, executable_target, hosted_page, request, shortcut_named, strip_ci, Request, TopWindow,
};
use super::winput::{releases, steps, Step, VirtualScreen};
use super::{AppInfo, Capture, InputEvent, MouseButton};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows_sys::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC, GetMonitorInfoW, MonitorFromPoint,
    ReleaseDC, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    CAPTUREBLT, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST, SRCCOPY,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DrawIconEx, EnumChildWindows, EnumWindows, GetAncestor, GetClassNameW, GetCursorInfo,
    GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetIconInfo, GetSystemMetrics, GetWindow, GetWindowLongPtrW,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow,
    ShowWindow, WindowFromPoint, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, GA_ROOT, GUITHREADINFO, GWL_EXSTYLE, GW_OWNER,
    ICONINFO, SM_CXCURSOR, SM_CXVIRTUALSCREEN, SM_CYCURSOR, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SW_RESTORE, SW_SHOWNORMAL, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    SetWindowLongPtrW, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE,
    SetWindowPos,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

/// Pause between the events of one action, so apps see distinct presses (as on macOS).
const EVENT_GAP: Duration = Duration::from_millis(15);

/// A monitor's rectangle in physical pixels of the virtual screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorFrame {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub fn virtual_screen() -> VirtualScreen {
    // SAFETY: plain queries.
    unsafe {
        VirtualScreen {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

pub fn pointer_location() -> Option<Point> {
    let mut at = POINT { x: 0, y: 0 };
    // SAFETY: `at` is a valid out-pointer.
    (unsafe { GetCursorPos(&mut at) } != 0).then(|| Point { x: f64::from(at.x), y: f64::from(at.y) })
}

/// The monitor showing `at`, or the nearest one.
pub fn monitor_at(at: Point) -> Option<MonitorFrame> {
    let point = POINT { x: at.x.round() as i32, y: at.y.round() as i32 };
    // SAFETY: plain query; `info` is a valid out-pointer whose size is set.
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return None;
        }
        let mut info: MONITORINFO = zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        let RECT { left, top, right, bottom } = info.rcMonitor;
        Some(MonitorFrame { x: left, y: top, width: right - left, height: bottom - top })
    }
}

fn input(step: Step) -> INPUT {
    let mouse = |dx, dy, data: i32, flags| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    let keyboard = |vk: u16, scan: u16, flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    match step {
        Step::Move { x, y } => mouse(x, y, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK),
        Step::Button { button, down } => {
            let flags = match (button, down) {
                (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
            };
            mouse(0, 0, 0, flags)
        }
        Step::Wheel { horizontal, delta } => {
            mouse(0, 0, delta, if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL })
        }
        Step::Key { vk, down, extended } => {
            let mut flags = if down { 0 } else { KEYEVENTF_KEYUP };
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            keyboard(vk, 0, flags)
        }
        Step::Unicode { unit, down } => {
            keyboard(0, unit, KEYEVENTF_UNICODE | if down { 0 } else { KEYEVENTF_KEYUP })
        }
    }
}

fn send(step: Step) -> Result<(), String> {
    let record = input(step);
    // SAFETY: one valid INPUT record of the size passed.
    if unsafe { SendInput(1, &record, size_of::<INPUT>() as i32) } == 1 {
        Ok(())
    } else {
        Err(format!("Windows refused the input: {}", std::io::Error::last_os_error()))
    }
}

/// Posts `event`, one input at a time with a short gap, like the macOS helper. When an input
/// fails halfway, whatever it left pressed is released before the error is reported.
pub fn post(event: &InputEvent) -> Result<(), String> {
    let plan = steps(event, &virtual_screen())?;
    for (done, step) in plan.iter().enumerate() {
        if let Err(error) = send(*step) {
            for release in releases(&plan[..done]) {
                let _ = send(release);
            }
            return Err(error);
        }
        std::thread::sleep(EVENT_GAP);
    }
    Ok(())
}

/// The GDI objects of one capture, released in reverse whatever happens.
struct Canvas {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut c_void,
}

impl Canvas {
    /// A top-down 32-bit BGRA bitmap of `width × height`, selected into a memory context.
    fn new(width: u32, height: u32) -> Result<Canvas, String> {
        // SAFETY: GDI calls with valid arguments; every handle is checked and released by Drop.
        unsafe {
            let screen = GetDC(null_mut());
            if screen.is_null() {
                return Err("cannot read the screen".into());
            }
            let mut canvas = Canvas { screen, memory: null_mut(), bitmap: null_mut(), previous: null_mut(), bits: null_mut() };
            canvas.memory = CreateCompatibleDC(screen);
            if canvas.memory.is_null() {
                return Err("cannot prepare the screenshot".into());
            }
            let mut info: BITMAPINFO = zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Negative: rows top to bottom, as the JPEG encoder reads them.
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..zeroed()
            };
            canvas.bitmap = CreateDIBSection(screen, &info, DIB_RGB_COLORS, &mut canvas.bits, null_mut(), 0);
            if canvas.bitmap.is_null() || canvas.bits.is_null() {
                return Err("cannot prepare the screenshot bitmap".into());
            }
            let previous = SelectObject(canvas.memory, canvas.bitmap);
            // HGDI_ERROR is -1; without the bitmap selected the pixels read would not be the capture.
            if previous.is_null() || previous as isize == -1 {
                return Err("cannot prepare the screenshot bitmap".into());
            }
            canvas.previous = previous;
            Ok(canvas)
        }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        // SAFETY: each handle was created above and is released once.
        unsafe {
            if !self.previous.is_null() {
                SelectObject(self.memory, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.memory.is_null() {
                DeleteDC(self.memory);
            }
            ReleaseDC(null_mut(), self.screen);
        }
    }
}

/// Draws the pointer where it is and at the screenshot's scale (macOS captures show it too).
fn draw_pointer(canvas: &Canvas, frame: MonitorFrame, width: u32, height: u32) {
    // SAFETY: `info` and `icon` are valid out-pointers; the icon's bitmaps are released.
    unsafe {
        let mut info: CURSORINFO = zeroed();
        info.cbSize = size_of::<CURSORINFO>() as u32;
        if GetCursorInfo(&mut info) == 0 || info.flags & CURSOR_SHOWING == 0 || info.hCursor.is_null() {
            return;
        }
        let mut icon: ICONINFO = zeroed();
        let (hot_x, hot_y) = if GetIconInfo(info.hCursor, &mut icon) != 0 {
            for bitmap in [icon.hbmMask, icon.hbmColor] {
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
            }
            (icon.xHotspot as i32, icon.yHotspot as i32)
        } else {
            (0, 0)
        };
        let scale = |offset: i32, length: i32, size: u32| (i64::from(offset) * i64::from(size) / i64::from(length.max(1))) as i32;
        let (cursor_w, cursor_h) = (GetSystemMetrics(SM_CXCURSOR), GetSystemMetrics(SM_CYCURSOR));
        let (draw_w, draw_h) = (scale(cursor_w, frame.width, width).max(1), scale(cursor_h, frame.height, height).max(1));
        let x = scale(info.ptScreenPos.x - frame.x, frame.width, width) - scale(hot_x, frame.width, width);
        let y = scale(info.ptScreenPos.y - frame.y, frame.height, height) - scale(hot_y, frame.height, height);
        DrawIconEx(canvas.memory, x, y, info.hCursor, draw_w, draw_h, 0, null_mut(), DI_NORMAL);
    }
}

/// A screenshot of the monitor under the pointer, downscaled to `MAX_LONG_EDGE`, Gentle Dot's
/// windows excluded.
pub fn capture() -> Result<Capture, String> {
    let pointer = pointer_location().ok_or("cannot read the pointer position")?;
    let frame = monitor_at(pointer).ok_or("no display under the pointer")?;
    if frame.width <= 0 || frame.height <= 0 {
        return Err("the display under the pointer has no size".into());
    }
    let (width, height) = fit_long_edge(frame.width as u32, frame.height as u32, MAX_LONG_EDGE);
    let canvas = Canvas::new(width, height)?;
    // SAFETY: both contexts are valid; the bitmap holds `width × height` 32-bit pixels.
    let bytes = unsafe {
        SetStretchBltMode(canvas.memory, HALFTONE);
        SetBrushOrgEx(canvas.memory, 0, 0, null_mut());
        let copied = StretchBlt(
            canvas.memory, 0, 0, width as i32, height as i32,
            canvas.screen, frame.x, frame.y, frame.width, frame.height,
            SRCCOPY | CAPTUREBLT,
        );
        if copied == 0 {
            return Err(format!("cannot take the screenshot: {}", std::io::Error::last_os_error()));
        }
        draw_pointer(&canvas, frame, width, height);
        GdiFlush();
        std::slice::from_raw_parts(canvas.bits as *const u8, width as usize * height as usize * 4).to_vec()
    };
    drop(canvas);
    let data = bgra_to_jpeg(&bytes, width, height, width as usize * 4, JPEG_QUALITY)?;
    let origin = Point { x: f64::from(frame.x), y: f64::from(frame.y) };
    let geometry = ScreenshotGeometry::for_display(origin, f64::from(frame.width), width, height);
    Ok(Capture { data, mime_type: "image/jpeg", geometry })
}

/// The Store apps' frame window and the window of the app it hosts.
const FRAME_CLASS: &str = "ApplicationFrameWindow";
const CORE_CLASS: &str = "Windows.UI.Core.CoreWindow";
/// How deep the Start menu folders are searched for shortcuts.
const SHORTCUT_DEPTH: usize = 4;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 128];
    // SAFETY: the buffer's length is passed.
    let length = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

fn title(hwnd: HWND) -> String {
    // SAFETY: the buffer holds the reported length plus the terminator.
    unsafe {
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
    }
}

/// The process that created `hwnd`, or 0.
fn window_pid(hwnd: HWND) -> i32 {
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out-pointer.
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    pid as i32
}

/// The app a top-level window belongs to: its own process, or for a Store app's frame the
/// process of the core window inside it (none when that window is not found).
fn app_pid(root: HWND) -> Option<i32> {
    let frame_pid = window_pid(root);
    if class_name(root) != FRAME_CLASS {
        return (frame_pid != 0).then_some(frame_pid);
    }
    struct Search {
        frame_pid: i32,
        found: i32,
    }
    unsafe extern "system" fn visit(child: HWND, state: LPARAM) -> i32 {
        // SAFETY: `state` is the `Search` passed below, alive for the whole enumeration.
        let search = unsafe { &mut *(state as *mut Search) };
        let pid = window_pid(child);
        if class_name(child) == CORE_CLASS && pid != search.frame_pid {
            search.found = pid;
            return 0;
        }
        1
    }
    let mut search = Search { frame_pid, found: 0 };
    // SAFETY: the callback only reads windows and writes `search`, which outlives the call.
    unsafe { EnumChildWindows(root, Some(visit), &mut search as *mut Search as LPARAM) };
    (search.found != 0).then_some(search.found)
}

/// Every top-level window, front to back, with the app it belongs to.
fn top_windows() -> Vec<(HWND, TopWindow)> {
    unsafe extern "system" fn collect(hwnd: HWND, state: LPARAM) -> i32 {
        // SAFETY: `state` is the vector passed below, alive for the whole enumeration.
        unsafe { (*(state as *mut Vec<HWND>)).push(hwnd) };
        1
    }
    let mut handles: Vec<HWND> = Vec::new();
    // SAFETY: the callback only appends to `handles`, which outlives the call.
    unsafe { EnumWindows(Some(collect), &mut handles as *mut Vec<HWND> as LPARAM) };
    handles
        .into_iter()
        .map(|hwnd| {
            // SAFETY: plain queries on a window handle (a vanished window just reads as empty).
            let (visible, style, owner) =
                unsafe { (IsWindowVisible(hwnd) != 0, GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32, GetWindow(hwnd, GW_OWNER)) };
            let mut cloaked = 0u32;
            // SAFETY: `cloaked` is a valid out-pointer of the size passed.
            let read = unsafe {
                DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut c_void, size_of::<u32>() as u32)
            };
            let window = TopWindow {
                // Listed under its host while its app is not found; never a target then.
                pid: app_pid(hwnd).unwrap_or_else(|| window_pid(hwnd)),
                title: title(hwnd),
                visible,
                cloaked: read == 0 && cloaked != 0,
                tool: style & WS_EX_TOOLWINDOW != 0,
                app_window: style & WS_EX_APPWINDOW != 0,
                owned: !owner.is_null(),
            };
            (hwnd, window)
        })
        .collect()
}

/// The executable's full path, when the process lets itself be queried.
fn process_path(pid: i32) -> Option<String> {
    // SAFETY: the handle is checked and closed; the buffer's length is passed and updated.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if process.is_null() {
            return None;
        }
        // The longest path Windows supports.
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut length);
        CloseHandle(process);
        (ok != 0).then(|| String::from_utf16_lossy(&buffer[..length as usize]))
    }
}

/// The executable's file description (what Task Manager shows as the app name).
fn file_description(path: &str) -> Option<String> {
    let path = wide(path);
    // SAFETY: every buffer and out-pointer is valid; `VerQueryValueW` points into `data`, which
    // lives until the strings are copied out.
    unsafe {
        let size = GetFileVersionInfoSizeW(path.as_ptr(), null_mut());
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr() as *mut c_void) == 0 {
            return None;
        }
        let query = |sub: &str| -> Option<(*const c_void, u32)> {
            let (mut pointer, mut length) = (null_mut::<c_void>(), 0u32);
            let ok = VerQueryValueW(data.as_ptr() as *const c_void, wide(sub).as_ptr(), &mut pointer, &mut length);
            (ok != 0 && !pointer.is_null() && length > 0).then_some((pointer as *const c_void, length))
        };
        let (translation, length) = query("\\VarFileInfo\\Translation")?;
        if length < 4 {
            return None;
        }
        let pair = std::slice::from_raw_parts(translation as *const u16, 2);
        let (text, length) = query(&format!("\\StringFileInfo\\{:04x}{:04x}\\FileDescription", pair[0], pair[1]))?;
        let units = std::slice::from_raw_parts(text as *const u16, length as usize);
        let description = String::from_utf16_lossy(units).trim_end_matches('\0').trim().to_string();
        (!description.is_empty()).then_some(description)
    }
}

/// An app's identity: its executable's file name and its display name, or `process <pid>`
/// when Windows does not let the process be queried (an elevated one, say).
pub fn identity(pid: i32) -> AppIdentity {
    let path = process_path(pid);
    let executable = path.as_deref().and_then(|p| p.rsplit('\\').next()).map(str::to_string);
    let stem = executable.as_deref().map(|exe| exe.rsplit_once('.').map_or(exe, |(stem, _)| stem).to_string());
    let name = path
        .as_deref()
        .and_then(file_description)
        .or(stem)
        .unwrap_or_else(|| format!("process {pid}"));
    AppIdentity { pid, bundle_id: executable, name }
}

/// An identity Windows actually gave: the executable is known.
fn known(id: AppIdentity) -> Option<AppIdentity> {
    id.bundle_id.is_some().then_some(id)
}

fn is_frame_host(id: &AppIdentity) -> bool {
    id.bundle_id.as_deref().is_some_and(|exe| exe.eq_ignore_ascii_case("ApplicationFrameHost.exe"))
}

/// The process id of an app whose process is not even known (a failed focus query).
const NO_PROCESS: i32 = 0;

/// The app Windows does not identify, which the blocklist refuses.
fn unidentified(pid: i32) -> AppIdentity {
    AppIdentity { pid, bundle_id: None, name: UNIDENTIFIED.to_string() }
}

/// The app an action aimed at a top-level window targets. A File Explorer window showing a
/// blocked page (Credential Manager) is that page; an app Windows does not identify is
/// `UNIDENTIFIED`.
fn target_of(root: HWND) -> AppIdentity {
    let Some(pid) = app_pid(root) else { return unidentified(window_pid(root)) };
    let Some(id) = known(identity(pid)) else { return unidentified(pid) };
    match hosted_page(id.bundle_id.as_deref(), &title(root)) {
        Some(page) => AppIdentity { pid: id.pid, bundle_id: None, name: page.to_string() },
        None => id,
    }
}

/// The app owning the foreground window.
pub fn frontmost_app() -> Option<AppIdentity> {
    // SAFETY: plain queries.
    let root = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOT) };
    (!root.is_null()).then(|| target_of(root))
}

/// The app owning the focused element, which can differ from the foreground window's (a panel
/// of another process): UI Automation's answer, else the focus window's. A Store app's frame
/// stands for the app inside it; a focus Windows does not identify is `UNIDENTIFIED`.
pub fn focused_app() -> Option<AppIdentity> {
    let resolve = |pid: i32| {
        let id = identity(pid);
        (!is_frame_host(&id)).then(|| known(id).unwrap_or_else(|| unidentified(pid)))
    };
    if let Some(id) = focused_pid().and_then(resolve) {
        return Some(id);
    }
    // SAFETY: plain queries; `info` is a valid out-pointer whose size is set.
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_null() {
            return None;
        }
        let thread = GetWindowThreadProcessId(foreground, null_mut());
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = size_of::<GUITHREADINFO>() as u32;
        // Asked and not answered: where the keys go is unknown, so they are refused. No process
        // has id 0, so the helper never mistakes this for the front app itself.
        if GetGUIThreadInfo(thread, &mut info) == 0 {
            return Some(unidentified(NO_PROCESS));
        }
        // No focus window: keys go to the foreground window, which is checked as the front app.
        if info.hwndFocus.is_null() {
            return None;
        }
        Some(resolve(window_pid(info.hwndFocus)).unwrap_or_else(|| target_of(GetAncestor(info.hwndFocus, GA_ROOT))))
    }
}

/// The app owning the topmost window under `at`. Click-through windows (the glow) are skipped
/// by Windows itself; a point with no window has no owner.
pub fn window_owner_at(at: Point) -> Option<AppIdentity> {
    let point = POINT { x: at.x.round() as i32, y: at.y.round() as i32 };
    // SAFETY: plain queries.
    let root = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
    (!root.is_null()).then(|| target_of(root))
}

/// The running apps, front to back.
pub fn list_apps() -> Vec<AppInfo> {
    let active = frontmost_app().map(|app| app.pid);
    let windows: Vec<TopWindow> = top_windows().into_iter().map(|(_, window)| window).collect();
    app_pids(&windows)
        .into_iter()
        .map(|pid| {
            let id = identity(pid);
            AppInfo { name: id.name, bundle_id: id.bundle_id, active: active == Some(pid) }
        })
        .collect()
}

/// Brings a window to the front. Windows lets only the foreground's input thread do that, so the
/// helper joins it for the call when asking alone is refused.
fn bring_to_front(hwnd: HWND) {
    // SAFETY: plain window calls; the input threads are detached again before returning.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd) != 0 {
            return;
        }
        let foreground = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
        let own = GetCurrentThreadId();
        let attached = foreground != 0 && foreground != own && AttachThreadInput(own, foreground, 1) != 0;
        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        if attached {
            AttachThreadInput(own, foreground, 0);
        }
    }
}

/// The frontmost app window of `pid`.
fn app_window_of(pid: i32) -> Option<HWND> {
    top_windows().into_iter().find(|(_, window)| window.pid == pid && window.is_app()).map(|(hwnd, _)| hwnd)
}

/// Brings an app back to the front (after a dialog took the focus).
pub fn activate(pid: i32) {
    if let Some(hwnd) = app_window_of(pid) {
        bring_to_front(hwnd);
    }
}

/// The `.lnk` files of the user's and everyone's Start menu.
fn start_menu_shortcuts() -> Vec<String> {
    fn walk(dir: &std::path::Path, depth: usize, found: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            // Links are not followed: only folders and files the Start menu itself holds.
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            if kind.is_dir() && depth > 0 {
                walk(&path, depth - 1, found);
            } else if kind.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
                found.push(path.to_string_lossy().into_owned());
            }
        }
    }
    let mut found = Vec::new();
    for root in ["APPDATA", "ProgramData"] {
        if let Some(base) = std::env::var_os(root) {
            let programs = std::path::Path::new(&base).join(r"Microsoft\Windows\Start Menu\Programs");
            walk(&programs, SHORTCUT_DEPTH, &mut found);
        }
    }
    found.sort();
    found
}

/// Asks the shell to start a program.
fn shell_open(program: &str) -> Result<(), String> {
    let (verb, file) = (wide("open"), wide(program));
    // SAFETY: valid wide strings; COM is initialized for the call (the shell may need it) and
    // uninitialized only when this call initialized it.
    unsafe {
        let initialized = CoInitializeEx(null_mut(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
        let result = ShellExecuteW(null_mut(), verb.as_ptr(), file.as_ptr(), null_mut(), null_mut(), SW_SHOWNORMAL);
        if initialized >= 0 {
            CoUninitialize();
        }
        if result as isize > 32 {
            Ok(())
        } else {
            Err(format!("cannot open `{program}`: Windows could not start it"))
        }
    }
}

/// `name` with `.exe`, as Windows names programs.
fn with_exe(name: &str) -> String {
    if strip_ci(name, ".exe").is_some() { name.to_string() } else { format!("{name}.exe") }
}

/// The program a shortcut starts, when it is a local program started with no arguments.
fn shortcut_target(path: &str) -> Result<String, String> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    let unreadable = || format!("cannot tell what `{path}` opens, so it was not opened.");
    let utf16 = |units: &[u16]| String::from_utf16_lossy(&units[..units.iter().position(|&u| u == 0).unwrap_or(units.len())]);
    // Declared first so the shell link is released before COM is left.
    let _apartment = Apartment::join();
    let file = wide(path);
    // SAFETY: calls on live COM objects with buffers of the lengths passed.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|_| unreadable())?;
        link.cast::<IPersistFile>().map_err(|_| unreadable())?.Load(PCWSTR(file.as_ptr()), STGM_READ).map_err(|_| unreadable())?;
        let mut target = vec![0u16; 32768];
        link.GetPath(&mut target, null_mut(), 0).map_err(|_| unreadable())?;
        let mut arguments = vec![0u16; 32768];
        link.GetArguments(&mut arguments).map_err(|_| unreadable())?;
        let target = utf16(&target);
        if target.trim().is_empty() {
            return Err(unreadable());
        }
        executable_target(&target, &utf16(&arguments))
    }
}

/// The program registered for `name` under App Paths (the user's entry first).
fn app_path(name: &str) -> Option<String> {
    let key = wide(&format!(r"Software\Microsoft\Windows\CurrentVersion\App Paths\{}", with_exe(name)));
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let mut buffer = vec![0u16; 32768];
        let mut bytes = (buffer.len() * 2) as u32;
        // SAFETY: the buffer's size in bytes is passed and updated; REG_EXPAND_SZ is expanded.
        let status = unsafe {
            RegGetValueW(root, key.as_ptr(), std::ptr::null(), RRF_RT_REG_SZ, null_mut(), buffer.as_mut_ptr() as *mut c_void, &mut bytes)
        };
        if status == 0 {
            let units = &buffer[..(bytes as usize / 2).min(buffer.len())];
            return Some(String::from_utf16_lossy(units).trim_end_matches('\0').to_string());
        }
    }
    None
}

/// The program `name` in an absolute folder of PATH (never the current folder).
fn search_exe(name: &str) -> Option<String> {
    let file = with_exe(name);
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(&file))
        .find(|candidate| candidate.is_file())
        .map(|found| found.to_string_lossy().into_owned())
}

fn off_limits(label: &str) -> String {
    format!("{label} is off limits, so it was not opened.")
}

/// Opens an app, or brings it forward when it already runs, the way `open -a` does on macOS: by a
/// running app's name or executable, else a Start menu shortcut with that name, else the program
/// App Paths or PATH names. Whatever the route, only a program on a local drive is started, with
/// no arguments, and never one the blocklist names.
pub fn open_app(name: &str) -> Result<(), String> {
    let request = request(name)?;
    let own = std::process::id() as i32;
    if let Request::Name(wanted) = &request {
        for (hwnd, _) in top_windows().into_iter().filter(|(_, window)| window.is_app()) {
            // The same identity an action aimed at the window would get (hosted pages, Store apps).
            let id = target_of(hwnd);
            if answers_to(wanted, &id.name, id.bundle_id.as_deref()) {
                if let Some(label) = blocked_reason(&id, own) {
                    return Err(off_limits(label));
                }
                bring_to_front(hwnd);
                return Ok(());
            }
        }
    }
    let program = match &request {
        Request::Path(path) if strip_ci(path, ".lnk").is_some() => shortcut_target(path)?,
        Request::Path(path) => executable_target(path, "")?,
        Request::Name(wanted) => match shortcut_named(wanted, &start_menu_shortcuts()) {
            Some(shortcut) => shortcut_target(shortcut)?,
            None => {
                let found = app_path(wanted).or_else(|| search_exe(wanted));
                executable_target(&found.ok_or_else(|| format!("cannot find an app named `{wanted}`"))?, "")?
            }
        },
    };
    if !std::path::Path::new(&program).is_file() {
        return Err(format!("`{program}` does not exist, so it was not opened."));
    }
    if let Some(label) = blocked_name(&program) {
        return Err(off_limits(label));
    }
    shell_open(&program)
}

/// How many ancestors of the element under a point are read (text inside a button), as on macOS.
const AX_ANCESTORS: usize = 4;
/// How far up the focused element its window is looked for.
const WINDOW_SEARCH: usize = 32;
/// At most this many of a window's buttons are checked for the default one. The query itself is
/// one cross-process call, bounded by the UI Automation timeout.
const DEFAULT_BUTTON_SCAN: i32 = 1024;
/// UI Automation gives up on an unresponsive app after this long.
const UIA_TIMEOUT_MS: u32 = 500;

mod uia {
    pub use windows::core::{Interface, BSTR};
    pub use windows::Win32::System::Variant::VARIANT;
    pub use windows::Win32::Foundation::POINT;
    pub use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
    pub use windows::Win32::UI::Accessibility::{
        CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationLegacyIAccessiblePattern,
        IUIAutomationValuePattern, TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_LegacyIAccessiblePatternId,
        UIA_LegacyIAccessibleStatePropertyId, UIA_ValuePatternId,
    };
}

/// A UI Automation client for one call, on a COM apartment it owns when it had to join one.
struct Automation {
    client: uia::IUIAutomation,
    /// Declared after `client` so the client is released before COM is left.
    _apartment: Apartment,
}

struct Apartment(bool);

impl Apartment {
    /// Joins the thread's COM apartment; a thread already in another apartment keeps it.
    fn join() -> Apartment {
        // SAFETY: COM initialization on this thread, balanced by Drop when it succeeded.
        Apartment(unsafe { uia::CoInitializeEx(None, uia::COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx on this thread.
            unsafe { uia::CoUninitialize() };
        }
    }
}

impl Automation {
    fn new() -> Option<Automation> {
        let apartment = Apartment::join();
        // SAFETY: creating the system's UI Automation object.
        let client: uia::IUIAutomation =
            unsafe { uia::CoCreateInstance(&uia::CUIAutomation8, None, uia::CLSCTX_INPROC_SERVER) }.ok()?;
        if let Ok(timeouts) = uia::Interface::cast::<uia::IUIAutomation2>(&client) {
            // SAFETY: plain setters on a live client.
            unsafe {
                let _ = timeouts.SetConnectionTimeout(UIA_TIMEOUT_MS);
                let _ = timeouts.SetTransactionTimeout(UIA_TIMEOUT_MS);
            }
        }
        Some(Automation { client, _apartment: apartment })
    }

    fn parent(&self, of: &uia::IUIAutomationElement) -> Option<uia::IUIAutomationElement> {
        // SAFETY: calls on live COM objects.
        unsafe { self.client.ControlViewWalker().ok()?.GetParentElement(of).ok() }
    }
}

fn text(value: windows::core::Result<uia::BSTR>) -> Option<String> {
    value.ok().map(|text| text.to_string())
}

fn control_type(of: &uia::IUIAutomationElement) -> i32 {
    // SAFETY: a call on a live COM object.
    unsafe { of.CurrentControlType() }.map_or(0, |t| t.0)
}

/// The legacy accessible state, where the default button is marked.
fn legacy_state(of: &uia::IUIAutomationElement) -> Option<u32> {
    // SAFETY: calls on live COM objects.
    unsafe {
        let legacy = of.GetCurrentPatternAs::<uia::IUIAutomationLegacyIAccessiblePattern>(uia::UIA_LegacyIAccessiblePatternId).ok()?;
        legacy.CurrentState().ok()
    }
}

/// An element in the risk check's terms. A password field's value is never read, nor is the
/// value of a field that does not say whether it is one.
fn describe(of: &uia::IUIAutomationElement) -> AxElement {
    // SAFETY: calls on live COM objects.
    unsafe {
        let password = of.CurrentIsPassword().map_or(true, |p| p.as_bool());
        let value = if password {
            None
        } else {
            of.GetCurrentPatternAs::<uia::IUIAutomationValuePattern>(uia::UIA_ValuePatternId)
                .ok()
                .and_then(|pattern| text(pattern.CurrentValue()))
        };
        element(control_type(of), text(of.CurrentName()), text(of.CurrentHelpText()), value)
    }
}

/// The element under `at`, then its ancestors (as many as macOS reads); empty when unknown.
pub fn element_at(at: Point) -> Vec<AxElement> {
    let Some(automation) = Automation::new() else { return Vec::new() };
    let point = uia::POINT { x: at.x.round() as i32, y: at.y.round() as i32 };
    // SAFETY: a call on a live COM object.
    let Ok(mut current) = (unsafe { automation.client.ElementFromPoint(point) }) else { return Vec::new() };
    // The hit test went through Gentle Dot's click-through glow while another app owns the window
    // there: what is under the glow cannot be read, so it asks for a confirmation.
    let own = std::process::id() as i32;
    // SAFETY: a call on a live COM object.
    // A process Windows does not report counts as Gentle Dot's: the element is not read either.
    let hit_own = unsafe { current.CurrentProcessId() }.map_or(true, |pid| pid == own);
    if hit_own && window_owner_at(at).is_some_and(|owner| owner.pid != own) {
        return vec![unreadable()];
    }
    let mut chain = vec![describe(&current)];
    while chain.len() <= AX_ANCESTORS {
        let Some(parent) = automation.parent(&current) else { break };
        chain.push(describe(&parent));
        current = parent;
    }
    chain
}

/// The window holding `of`: its nearest Window ancestor, as macOS reads `AXWindow`.
fn window_of(automation: &Automation, of: &uia::IUIAutomationElement) -> Option<uia::IUIAutomationElement> {
    let mut current = of.clone();
    for _ in 0..WINDOW_SEARCH {
        if control_type(&current) == UIA_WINDOW {
            return Some(current);
        }
        current = automation.parent(&current)?;
    }
    None
}

/// The window's default button (`AXDefaultButton` on macOS). One cross-process call fetches every
/// button of the window with its legacy state, within the UI Automation timeout.
fn default_button(automation: &Automation, window: &uia::IUIAutomationElement) -> Option<uia::IUIAutomationElement> {
    // SAFETY: calls on live COM objects.
    unsafe {
        let buttons = automation.client.CreatePropertyCondition(uia::UIA_ControlTypePropertyId, &uia::VARIANT::from(UIA_BUTTON)).ok()?;
        let cache = automation.client.CreateCacheRequest().ok()?;
        cache.AddProperty(uia::UIA_LegacyIAccessibleStatePropertyId).ok()?;
        let found = window.FindAllBuildCache(uia::TreeScope_Descendants, &buttons, &cache).ok()?;
        let count = found.Length().ok()?.min(DEFAULT_BUTTON_SCAN);
        (0..count).filter_map(|i| found.GetElement(i).ok()).find(|button| {
            let state = button.GetCachedPropertyValue(uia::UIA_LegacyIAccessibleStatePropertyId).ok();
            state.and_then(|value| i32::try_from(&value).ok()).is_some_and(|state| is_default(state as u32))
        })
    }
}

/// The focused element, whether it is its window's default button, and that button.
pub fn focused_element() -> Option<Focus> {
    let automation = Automation::new()?;
    // SAFETY: a call on a live COM object.
    let focused = unsafe { automation.client.GetFocusedElement() }.ok()?;
    let default = window_of(&automation, &focused).and_then(|window| default_button(&automation, &window));
    // SAFETY: a call on a live COM object.
    let same = |button: &uia::IUIAutomationElement| unsafe { automation.client.CompareElements(button, &focused) }.is_ok_and(|b| b.as_bool());
    let is_default_button = legacy_state(&focused).is_some_and(is_default) || default.as_ref().is_some_and(same);
    Some(Focus { element: describe(&focused), is_default_button, default_button: default.as_ref().map(describe) })
}

/// The process of the focused UI Automation element.
fn focused_pid() -> Option<i32> {
    let automation = Automation::new()?;
    // SAFETY: calls on live COM objects.
    let pid = unsafe { automation.client.GetFocusedElement().ok()?.CurrentProcessId().ok()? };
    (pid > 0).then_some(pid)
}

/// Keeps a window out of every screenshot and screen capture, this helper's included.
pub fn exclude_from_capture(hwnd: *mut c_void) -> Result<(), String> {
    // SAFETY: a plain call on a window handle of this process.
    if unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) } != 0 {
        Ok(())
    } else {
        Err(format!("cannot keep it out of screenshots: {}", std::io::Error::last_os_error()))
    }
}

/// The frame of the display showing `at`, in physical pixels (S28).
pub fn display_frame_at(at: Point) -> Option<Frame> {
    monitor_at(at).map(|m| Frame { x: f64::from(m.x), y: f64::from(m.y), width: f64::from(m.width), height: f64::from(m.height) })
}

/// The display's physical pixels per CSS pixel (its scale, 1.5 at 150 %).
pub fn display_scale(display: Frame) -> f64 {
    let center = POINT { x: (display.x + display.width / 2.0) as i32, y: (display.y + display.height / 2.0) as i32 };
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    // SAFETY: plain queries with valid out-pointers.
    let read = unsafe { GetDpiForMonitor(MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST), MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    if read == 0 && dpi_x > 0 { f64::from(dpi_x) / 96.0 } else { 1.0 }
}

/// The focused element's frame in physical pixels, when it reports one (S28).
pub fn focused_frame() -> Option<Frame> {
    let automation = Automation::new()?;
    // SAFETY: calls on live COM objects.
    let rect = unsafe { automation.client.GetFocusedElement().ok()?.CurrentBoundingRectangle().ok()? };
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    (width > 0 && height > 0).then(|| Frame { x: f64::from(rect.left), y: f64::from(rect.top), width: f64::from(width), height: f64::from(height) })
}

/// Readies the glow's window: never activated, off the taskbar and Alt+Tab, never captured.
/// Clicks already pass through it (set by Tauri).
pub fn prepare_overlay(hwnd: *mut c_void) {
    // SAFETY: plain calls on a window of this process.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW) as isize);
    }
    if let Err(error) = exclude_from_capture(hwnd) {
        eprintln!("gentle-dot: the glow may show in computer-control screenshots: {error}");
    }
}

/// Shows the glow above everything without taking the focus from the app being controlled, or
/// hides it.
pub fn show_overlay(hwnd: *mut c_void, visible: bool) {
    // SAFETY: plain calls on a window of this process.
    unsafe {
        if visible {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        } else {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Moves the glow over a display, without activating it.
pub fn cover(hwnd: *mut c_void, display: Frame) {
    // SAFETY: a plain call on a window of this process.
    unsafe {
        SetWindowPos(
            hwnd, HWND_TOPMOST, display.x as i32, display.y as i32, display.width as i32, display.height as i32,
            SWP_NOACTIVATE,
        );
    }
}

/// The Windows desktop, as the helper sees it. Windows asks for no permission: the helper sees
/// and drives the desktop of the signed-in user, except windows of higher integrity (elevated
/// apps), which Windows itself does not let it drive.
pub struct WinDesktop;

impl super::Desktop for WinDesktop {
    fn permissions(&self) -> super::Permissions {
        super::Permissions { accessibility: true, screen_recording: true, screenshots_supported: true }
    }

    fn capture(&self) -> Result<Capture, String> {
        capture()
    }

    fn post(&self, event: &InputEvent) -> Result<(), String> {
        post(event)
    }

    fn list_apps(&self) -> Result<Vec<AppInfo>, String> {
        Ok(list_apps())
    }

    fn open_app(&self, name: &str) -> Result<(), String> {
        open_app(name)
    }

    fn element_at(&self, at: Point) -> Vec<AxElement> {
        element_at(at)
    }

    fn focused_element(&self) -> Option<Focus> {
        focused_element()
    }

    fn frontmost_app(&self) -> Option<AppIdentity> {
        frontmost_app()
    }

    fn focused_app(&self) -> Option<AppIdentity> {
        focused_app()
    }

    fn window_owner_at(&self, at: Point) -> Option<AppIdentity> {
        window_owner_at(at)
    }

    fn activate(&self, pid: i32) {
        activate(pid)
    }
}

/// These need the user's interactive desktop (not an SSH or service session): they take a real
/// screenshot and move the real pointer, then put it back.
#[cfg(test)]
mod tests {
    use super::*;

    fn per_monitor_aware() {
        use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
        // SAFETY: plain call; the app itself is already per-monitor aware.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    }

    #[test]
    fn this_process_is_identified_by_its_executable_and_a_name() {
        let own = identity(std::process::id() as i32);
        let exe = own.bundle_id.expect("the test process can query itself");
        assert!(exe.to_ascii_lowercase().ends_with(".exe"), "{exe}");
        assert!(!own.name.is_empty() && !own.name.starts_with("process "), "{}", own.name);
        assert_eq!(identity(-5).name, "process -5", "an unknown process keeps its number");
    }

    #[test]
    #[ignore = "interactive Windows desktop: run in the user's own session"]
    fn a_screenshot_is_a_jpeg_of_the_monitor_under_the_pointer() {
        per_monitor_aware();
        let capture = capture().unwrap();
        let frame = monitor_at(pointer_location().unwrap()).unwrap();
        let image = image::load_from_memory_with_format(&capture.data, image::ImageFormat::Jpeg).unwrap().to_rgb8();
        assert_eq!((image.width(), image.height()), (capture.geometry.image_width, capture.geometry.image_height));
        assert!(image.width().max(image.height()) <= MAX_LONG_EDGE);
        assert_eq!(capture.geometry.origin, Point { x: f64::from(frame.x), y: f64::from(frame.y) });
        let first = *image.get_pixel(0, 0);
        assert!(image.pixels().any(|p| *p != first), "a real desktop is not one flat color");
    }

    #[test]
    #[ignore = "interactive Windows desktop: run in the user's own session"]
    fn a_move_lands_on_the_pixel_asked_for_and_the_pointer_is_put_back() {
        per_monitor_aware();
        let start = pointer_location().unwrap();
        let frame = monitor_at(start).unwrap();
        let target = Point { x: f64::from(frame.x + frame.width / 3), y: f64::from(frame.y + frame.height / 3) };
        post(&InputEvent::Move(target)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let landed = pointer_location().unwrap();
        post(&InputEvent::Move(start)).unwrap();
        assert!((landed.x - target.x).abs() <= 1.0 && (landed.y - target.y).abs() <= 1.0, "{landed:?} != {target:?}");
    }
}
