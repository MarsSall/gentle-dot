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
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, SetLastError, HWND, LPARAM, POINT, RECT};
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

/// The window's class, or none when Windows does not say (every window has one).
fn class_name(hwnd: HWND) -> Option<String> {
    let mut buffer = [0u16; 256];
    // SAFETY: the buffer's length is passed.
    let length = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    (length > 0).then(|| String::from_utf16_lossy(&buffer[..length as usize]))
}

/// The window's title, `Some("")` for a window without one, or none when Windows did not answer.
fn title_checked(hwnd: HWND) -> Option<String> {
    // SAFETY: the buffer holds the reported length plus the terminator; the last error is reset
    // first, so a zero length is told apart from a failed query.
    unsafe {
        SetLastError(0);
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return (GetLastError() == 0).then(String::new);
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        (copied > 0 || GetLastError() == 0).then(|| String::from_utf16_lossy(&buffer[..copied.max(0) as usize]))
    }
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
    // A window whose class Windows does not report could be a Store frame: no app is assumed.
    if class_name(root)? != FRAME_CLASS {
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
        if class_name(child).as_deref() == Some(CORE_CLASS) && pid != search.frame_pid {
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
    // The Store frame's host is never an app of its own: its app was not found.
    if is_frame_host(&id) {
        return unidentified(pid);
    }
    // A File Explorer window whose title Windows does not report could be a blocked page.
    let Some(title) = title_checked(root) else {
        let explorer = id.bundle_id.as_deref().is_some_and(|exe| exe.eq_ignore_ascii_case("explorer.exe"));
        return if explorer { unidentified(pid) } else { id };
    };
    match hosted_page(id.bundle_id.as_deref(), &title) {
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

/// The app the keys go to: the focus window's top-level app, through the same window checks as
/// every target, and the process of the focus window itself (Windows sends keys there), the more
/// restrictive of the two. UI Automation's focused element, when it answers, is checked too: a
/// refused one, or one in another process (a panel), is what is returned. A
/// refused app other than Gentle Dot carries no process id, so the helper never drops it as the
/// front app; a focus window Windows does not report is the refused Unidentified app.
pub fn focused_app() -> Option<AppIdentity> {
    let own = std::process::id() as i32;
    let refused = |id: &AppIdentity| blocked_reason(id, own).is_some();
    // UI Automation's answer: none when it did not answer; a Store frame's host leaves it to the window.
    let from_uia = focused_pid().and_then(|pid| {
        let id = identity(pid);
        (!is_frame_host(&id)).then(|| known(id).unwrap_or_else(|| unidentified(NO_PROCESS)))
    });
    // The focus window's answer: an error when Windows did not say, none when no window has the focus.
    // SAFETY: plain queries; `info` is a valid out-pointer whose size is set.
    let from_window: Result<Option<AppIdentity>, ()> = unsafe {
        let foreground = GetForegroundWindow();
        let thread = if foreground.is_null() { 0 } else { GetWindowThreadProcessId(foreground, null_mut()) };
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = size_of::<GUITHREADINFO>() as u32;
        if thread == 0 || GetGUIThreadInfo(thread, &mut info) == 0 {
            Err(())
        } else if info.hwndFocus.is_null() {
            Ok(None)
        } else {
            // The window with the focus may be another process's (a child window it hosts): that
            // process receives the keys, so it is checked too, the more restrictive one returned.
            let root = target_of(GetAncestor(info.hwndFocus, GA_ROOT));
            let pid = window_pid(info.hwndFocus);
            let own_window = if pid == 0 {
                unidentified(NO_PROCESS)
            } else {
                let id = identity(pid);
                if is_frame_host(&id) { root.clone() } else { known(id).unwrap_or_else(|| unidentified(NO_PROCESS)) }
            };
            Ok(Some(if !refused(&root) && (refused(&own_window) || own_window.pid != root.pid) { own_window } else { root }))
        }
    };
    let answers = [from_window.clone().ok().flatten(), from_uia.clone()];
    if let Some(blocked) = answers.iter().flatten().find(|id| refused(id)) {
        // Gentle Dot keeps its own id, which is what refuses it when its executable is renamed.
        let pid = if blocked.pid == own { own } else { NO_PROCESS };
        return Some(AppIdentity { pid, ..blocked.clone() });
    }
    match from_window {
        Err(()) => Some(unidentified(NO_PROCESS)),
        // A focused element in another process (a panel) receives the keys; the helper checks it.
        Ok(Some(window)) => Some(from_uia.filter(|uia| uia.pid != window.pid).unwrap_or(window)),
        // No focus window: keys go to the foreground window, checked as the front app.
        Ok(None) => from_uia,
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
    pub use windows::core::Interface;
    pub use windows::Win32::System::Variant::VARIANT;
    pub use windows::Win32::Foundation::POINT;
    pub use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
    pub use windows::Win32::UI::Accessibility::{
        CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationLegacyIAccessiblePattern,
        IUIAutomationValuePattern, TreeScope_Descendants, UIA_ControlTypePropertyId,
        UIA_IsLegacyIAccessiblePatternAvailablePropertyId, UIA_IsValuePatternAvailablePropertyId,
        UIA_LegacyIAccessiblePatternId, UIA_LegacyIAccessibleStatePropertyId, UIA_PROPERTY_ID, UIA_ValuePatternId,
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
        // Without its timeouts a hung app could stall the helper: no client then (what it would
        // have read is unreadable, which asks for a confirmation).
        let timeouts = uia::Interface::cast::<uia::IUIAutomation2>(&client).ok()?;
        // SAFETY: plain setters on a live client.
        unsafe {
            timeouts.SetConnectionTimeout(UIA_TIMEOUT_MS).ok()?;
            timeouts.SetTransactionTimeout(UIA_TIMEOUT_MS).ok()?;
        }
        Some(Automation { client, _apartment: apartment })
    }

    fn parent(&self, of: &uia::IUIAutomationElement) -> Option<uia::IUIAutomationElement> {
        // SAFETY: calls on live COM objects.
        unsafe { self.client.ControlViewWalker().ok()?.GetParentElement(of).ok() }
    }

    /// Whether `of` ends its chain of ancestors: the desktop itself, or a top-level window.
    fn ends_chain(&self, of: &uia::IUIAutomationElement) -> bool {
        // SAFETY: calls on live COM objects.
        let root = unsafe { self.client.GetRootElement() }
            .and_then(|root| unsafe { self.client.CompareElements(&root, of) })
            .is_ok_and(|same| same.as_bool());
        root || control_type(of) == UIA_WINDOW
    }
}

fn control_type(of: &uia::IUIAutomationElement) -> i32 {
    // SAFETY: a call on a live COM object.
    unsafe { of.CurrentControlType() }.map_or(0, |t| t.0)
}

/// The legacy accessible state, where the default button is marked.
/// A boolean property; an error when UI Automation did not answer.
fn property_bool(of: &uia::IUIAutomationElement, property: uia::UIA_PROPERTY_ID) -> Result<bool, ()> {
    // SAFETY: a call on a live COM object.
    let value = unsafe { of.GetCurrentPropertyValue(property) }.map_err(|_| ())?;
    bool::try_from(&value).map_err(|_| ())
}

/// The legacy accessible state, where the default button is marked: none for an element without
/// one, an error when UI Automation did not answer.
fn legacy_state(of: &uia::IUIAutomationElement) -> Result<Option<u32>, ()> {
    if !property_bool(of, uia::UIA_IsLegacyIAccessiblePatternAvailablePropertyId)? {
        return Ok(None);
    }
    // SAFETY: calls on live COM objects.
    unsafe {
        let legacy = of
            .GetCurrentPatternAs::<uia::IUIAutomationLegacyIAccessiblePattern>(uia::UIA_LegacyIAccessiblePatternId)
            .map_err(|_| ())?;
        legacy.CurrentState().map(Some).map_err(|_| ())
    }
}

/// An element in the risk check's terms. Any property UI Automation does not report (type, name,
/// help text, whether it is a password, whether it has a value, the value) makes it an unreadable
/// element, which asks for a confirmation. A password field's value is never read.
fn describe(of: &uia::IUIAutomationElement) -> AxElement {
    // SAFETY: calls on live COM objects.
    unsafe {
        let (Ok(kind), Ok(name), Ok(help), Ok(password)) =
            (of.CurrentControlType(), of.CurrentName(), of.CurrentHelpText(), of.CurrentIsPassword())
        else {
            return unreadable();
        };
        let Ok(has_value) = property_bool(of, uia::UIA_IsValuePatternAvailablePropertyId) else { return unreadable() };
        let value = if password.as_bool() || !has_value {
            None
        } else {
            let read = of
                .GetCurrentPatternAs::<uia::IUIAutomationValuePattern>(uia::UIA_ValuePatternId)
                .and_then(|pattern| pattern.CurrentValue());
            match read {
                Ok(value) => Some(value.to_string()),
                Err(_) => return unreadable(),
            }
        };
        element(kind.0, Some(name.to_string()), Some(help.to_string()), value)
    }
}

/// The element under `at`, then its ancestors (as many as macOS reads); empty when unknown.
/// UI Automation that does not answer makes the element an unreadable one, which asks for a
/// confirmation: an action is never let through because Windows did not say what is there.
pub fn element_at(at: Point) -> Vec<AxElement> {
    let Some(automation) = Automation::new() else { return vec![unreadable()] };
    let point = uia::POINT { x: at.x.round() as i32, y: at.y.round() as i32 };
    // SAFETY: a call on a live COM object.
    let Ok(mut current) = (unsafe { automation.client.ElementFromPoint(point) }) else { return vec![unreadable()] };
    // The hit test went through Gentle Dot's click-through glow while another app owns the window
    // there: what is under the glow cannot be read, so it asks for a confirmation.
    let own = std::process::id() as i32;
    // SAFETY: a call on a live COM object.
    // A process Windows does not report counts as Gentle Dot's: the element is not read either.
    let hit_own = unsafe { current.CurrentProcessId() }.map_or(true, |pid| pid == own);
    // Only Gentle Dot's own window there (whose actions are refused anyway) is described.
    if hit_own && !window_owner_at(at).is_some_and(|owner| owner.pid == own) {
        return vec![unreadable()];
    }
    let mut chain = vec![describe(&current)];
    while chain.len() <= AX_ANCESTORS {
        let Some(parent) = automation.parent(&current) else {
            // Cut short before the window: what holds the element is not known, so it asks.
            if !automation.ends_chain(&current) {
                chain.push(unreadable());
            }
            break;
        };
        chain.push(describe(&parent));
        current = parent;
    }
    chain
}

/// The window holding `of`: its nearest Window ancestor, as macOS reads `AXWindow`.
/// Its window; none when there is none (the desktop); an error when the way up was cut short.
fn window_of(automation: &Automation, of: &uia::IUIAutomationElement) -> Result<Option<uia::IUIAutomationElement>, ()> {
    let mut current = of.clone();
    for _ in 0..WINDOW_SEARCH {
        // SAFETY: a call on a live COM object.
        let kind = unsafe { current.CurrentControlType() }.map_err(|_| ())?;
        if kind.0 == UIA_WINDOW {
            return Ok(Some(current));
        }
        match automation.parent(&current) {
            Some(parent) => current = parent,
            None if automation.ends_chain(&current) => return Ok(None),
            None => return Err(()),
        }
    }
    Err(())
}

/// The window's default button (`AXDefaultButton` on macOS). One cross-process call fetches every
/// button of the window with its legacy state, within the UI Automation timeout.
/// The window's default button, if it has one; an error when Windows did not answer.
fn default_button(
    automation: &Automation,
    window: &uia::IUIAutomationElement,
) -> windows::core::Result<Option<uia::IUIAutomationElement>> {
    // SAFETY: calls on live COM objects.
    unsafe {
        let buttons = automation.client.CreatePropertyCondition(uia::UIA_ControlTypePropertyId, &uia::VARIANT::from(UIA_BUTTON))?;
        let cache = automation.client.CreateCacheRequest()?;
        cache.AddProperty(uia::UIA_LegacyIAccessibleStatePropertyId)?;
        let found = window.FindAllBuildCache(uia::TreeScope_Descendants, &buttons, &cache)?;
        let count = found.Length()?.min(DEFAULT_BUTTON_SCAN);
        // Every button is read: one that cannot be could be the default, so the scan fails.
        for i in 0..count {
            let button = found.GetElement(i)?;
            let state = button.GetCachedPropertyValue(uia::UIA_LegacyIAccessibleStatePropertyId)?;
            let state = i32::try_from(&state)?;
            if is_default(state as u32) {
                return Ok(Some(button));
            }
        }
        Ok(None)
    }
}

/// The focused element, whether it is its window's default button, and that button.
/// A focus UI Automation does not report is an unreadable element (Return and Space then ask for
/// a confirmation), and a default button it does not report an unreadable one (Return asks).
pub fn focused_element() -> Option<Focus> {
    let unknown = || Some(Focus { element: unreadable(), is_default_button: false, default_button: None });
    let Some(automation) = Automation::new() else { return unknown() };
    // SAFETY: a call on a live COM object.
    let Ok(focused) = (unsafe { automation.client.GetFocusedElement() }) else { return unknown() };
    let default = match window_of(&automation, &focused) {
        Ok(Some(window)) => default_button(&automation, &window).map_err(|_| ()),
        Ok(None) => Ok(None),
        Err(()) => Err(()),
    };
    // A comparison or state UI Automation does not answer counts as the default button.
    // SAFETY: a call on a live COM object.
    let same = |button: &uia::IUIAutomationElement| unsafe { automation.client.CompareElements(button, &focused) }.map_or(true, |b| b.as_bool());
    let marked = legacy_state(&focused).map_or(true, |state| state.is_some_and(is_default));
    let is_default_button = marked || default.as_ref().is_ok_and(|d| d.as_ref().is_some_and(same));
    let default_button = match &default {
        Ok(found) => found.as_ref().map(describe),
        Err(()) => Some(unreadable()),
    };
    Some(Focus { element: describe(&focused), is_default_button, default_button })
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
