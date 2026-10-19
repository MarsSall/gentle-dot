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

use super::coords::{fit_long_edge, Point, ScreenshotGeometry, MAX_LONG_EDGE};
use super::encode::{bgra_to_jpeg, JPEG_QUALITY};
use super::blocklist::{blocked_reason, AppIdentity};
use super::winapps::{answers_to, app_pids, launch_for, launchable, Launch, TopWindow};
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
    SW_RESTORE, SW_SHOWNORMAL, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

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
/// process of the core window inside it.
fn app_pid(root: HWND) -> i32 {
    let frame_pid = window_pid(root);
    if class_name(root) != FRAME_CLASS {
        return frame_pid;
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
    if search.found != 0 { search.found } else { frame_pid }
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
                pid: app_pid(hwnd),
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
        let mut buffer = vec![0u16; 1024];
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

/// The app owning the foreground window.
pub fn frontmost_app() -> Option<AppIdentity> {
    // SAFETY: plain queries.
    let root = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOT) };
    (!root.is_null()).then(|| identity(app_pid(root)))
}

/// The app owning the window with the keyboard focus, which can differ from the foreground
/// window's (a Store app's frame, a panel of another process).
pub fn focused_app() -> Option<AppIdentity> {
    // SAFETY: plain queries; `info` is a valid out-pointer whose size is set.
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_null() {
            return None;
        }
        let thread = GetWindowThreadProcessId(foreground, null_mut());
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(thread, &mut info) == 0 || info.hwndFocus.is_null() {
            return None;
        }
        let pid = window_pid(info.hwndFocus);
        (pid != 0).then(|| identity(pid))
    }
}

/// The app owning the topmost window under `at`. Click-through windows (the glow) are skipped
/// by Windows itself; a point with no window has no owner.
pub fn window_owner_at(at: Point) -> Option<AppIdentity> {
    let point = POINT { x: at.x.round() as i32, y: at.y.round() as i32 };
    // SAFETY: plain queries.
    let root = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
    if root.is_null() {
        return None;
    }
    let pid = app_pid(root);
    (pid != 0).then(|| identity(pid))
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

/// Asks the shell to open a program or a shortcut.
fn shell_open(target: &str) -> Result<(), String> {
    let (verb, file) = (wide("open"), wide(target));
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
            Err(format!("cannot open `{target}`: Windows could not find or start it"))
        }
    }
}

/// Opens an app, or brings it forward when it already runs. A running app is found by its name
/// or executable; otherwise a Start menu shortcut with that name, otherwise the program name.
pub fn open_app(name: &str) -> Result<(), String> {
    let wanted = launchable(name)?;
    let own = std::process::id() as i32;
    for (hwnd, window) in top_windows() {
        if !window.is_app() {
            continue;
        }
        let id = identity(window.pid);
        if answers_to(wanted, &id.name, id.bundle_id.as_deref()) {
            if let Some(label) = blocked_reason(&id, own) {
                return Err(format!("{label} is off limits, so it was not opened."));
            }
            bring_to_front(hwnd);
            return Ok(());
        }
    }
    match launch_for(wanted, &start_menu_shortcuts())? {
        Launch::Shortcut(path) | Launch::Program(path) => shell_open(&path),
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
