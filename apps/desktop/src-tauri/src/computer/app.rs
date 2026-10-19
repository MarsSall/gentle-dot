//! The app side of computer control (L60): the `computer_*` commands, the `computer://state`
//! event, the panic shortcut, the timeout watcher, and the native dialogs. It runs on macOS and
//! Windows; elsewhere the commands answer "unavailable" and nothing starts. Yolo mode (S24.9) is switched only from
//! here (`computer_set_yolo` and the tray item), never from the MCP helper or the daemon.
//!
//! The glow (S28): a click-through overlay window (`glow`) is ordered in while a session is
//! active and out when it ends. The helper's marks reach it through a channel, so the helper
//! never waits on the overlay; a thread of its own looks up the display (and, for keys, the
//! focused element), moves the overlay over that display, and emits `computer://glow`.

use super::control::Control;
use super::server::Endpoint;
use super::session::{Reason, StateEvent};
use super::{Clock, PermissionKind, Permissions, SystemClock};
use crate::alert::Choice;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

pub const STATE_EVENT: &str = "computer://state";
/// Where the agent just acted, sent to the overlay only (S28).
pub const GLOW_EVENT: &str = "computer://glow";
pub const GLOW_WINDOW: &str = "glow";
pub const GLOW_TITLE: &str = "Gentle Dot Glow";
/// ⌥⇧Esc (Alt+Shift+Esc on Windows) ends the session from anywhere (S24.3).
pub const PANIC_SHORTCUT: &str = "Alt+Shift+Escape";
#[cfg(not(windows))]
pub const GRANT_TEXT: &str = "Allow Gentle Dot to see and control this Mac for 30 minutes?";
#[cfg(windows)]
pub const GRANT_TEXT: &str = "Allow Gentle Dot to see and control this PC for 30 minutes?";
/// Every computer-control dialog defaults to its refusing button (L75 A2).
pub const GRANT_CHOICE: Choice = Choice { allow: "Allow", refuse: "Deny" };
pub const CONFIRM_CHOICE: Choice = Choice { allow: "Allow", refuse: "Cancel" };
#[cfg(not(windows))]
pub const YOLO_TEXT: &str = "Turn on yolo mode? Gentle Dot will send, pay, delete, and submit without asking you first, \
for up to 1 hour. Stop and ⌥⇧Esc still work.";
#[cfg(windows)]
pub const YOLO_TEXT: &str = "Turn on yolo mode? Gentle Dot will send, pay, delete, and submit without asking you first, \
for up to 1 hour. Stop and Alt+Shift+Esc still work.";
pub const YOLO_CHOICE: Choice = Choice { allow: "Turn On", refuse: "Cancel" };

/// The tray's "Yolo mode" item, kept in step with the state.
pub struct YoloMenuItem(pub tauri::menu::CheckMenuItem<tauri::Wry>);

pub struct Computer {
    control: Arc<Control>,
    endpoint: Option<Endpoint>,
    #[cfg(any(target_os = "macos", windows))]
    desktop: Arc<dyn super::Desktop>,
}

/// What the overlay thread hears: the session started or ended, or the agent acted.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
enum Overlay {
    Session(bool),
    Mark(super::glow::Mark),
}

impl Computer {
    /// The helper's address and key for this launch, registered with the daemon over the app's
    /// channel (S24.7, L61); `None` off macOS and Windows, or when the helper did not start.
    pub fn endpoint(&self) -> Option<Endpoint> {
        self.endpoint.clone()
    }
}

/// Creates the session and, on macOS and Windows, starts the MCP helper, the timeout watcher, and
/// the glow overlay. Runs in the app's setup, on the main thread.
pub fn start(app: &AppHandle) -> Computer {
    let (overlay, marks) = std::sync::mpsc::channel::<Overlay>();
    let events = app.clone();
    let session = overlay.clone();
    let control = Arc::new(Control::new(move |event: &StateEvent| {
        if let Err(error) = events.emit(STATE_EVENT, event) {
            eprintln!("gentle-dot: cannot report the computer-control state: {error}");
        }
        if let Some(item) = events.try_state::<YoloMenuItem>() {
            let _ = item.0.set_checked(event.yolo);
        }
        let _ = session.send(Overlay::Session(event.active));
    }));
    #[cfg(target_os = "macos")]
    {
        let number = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let desktop: Arc<dyn super::Desktop> = Arc::new(super::MacDesktop::new(number.clone()));
        let glow = move |mark| {
            let _ = overlay.send(Overlay::Mark(mark));
        };
        let endpoint = start_helper(app, control.clone(), desktop.clone(), glow)
            .map_err(|error| eprintln!("gentle-dot: computer control is unavailable: {error}"))
            .ok();
        watch_timeout(control.clone());
        match build_overlay(app, &number) {
            Ok(window) => run_overlay(app.clone(), window, number, control.clone(), marks),
            Err(error) => eprintln!("gentle-dot: the computer-control glow is unavailable: {error}"),
        }
        Computer { control, endpoint, desktop }
    }
    #[cfg(windows)]
    {
        let desktop: Arc<dyn super::Desktop> = Arc::new(super::win::WinDesktop);
        let glow = move |mark| {
            let _ = overlay.send(Overlay::Mark(mark));
        };
        let endpoint = start_helper(app, control.clone(), desktop.clone(), glow)
            .map_err(|error| eprintln!("gentle-dot: computer control is unavailable: {error}"))
            .ok();
        watch_timeout(control.clone());
        match build_glow(app) {
            Ok(window) => run_glow(app.clone(), window, control.clone(), marks),
            Err(error) => eprintln!("gentle-dot: the computer-control glow is unavailable: {error}"),
        }
        Computer { control, endpoint, desktop }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        drop((overlay, marks));
        Computer { control, endpoint: None }
    }
}

#[cfg(any(target_os = "macos", windows))]
fn start_helper(
    app: &AppHandle,
    control: Arc<Control>,
    desktop: Arc<dyn super::Desktop>,
    glow: impl Fn(super::glow::Mark) + Send + Sync + 'static,
) -> std::io::Result<Endpoint> {
    let token = super::server::new_token();
    let helper = super::mcp::Helper::new(
        desktop,
        Arc::new(NativeDialogs { app: app.clone() }),
        Arc::new(SystemClock),
        control,
        token.clone(),
        std::process::id() as i32,
    )
    .with_glow(glow);
    super::server::start(Arc::new(helper), token)
}

/// The overlay's NSWindow, used on the main thread only (the window lives as long as the app).
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
struct NsWindow(usize);

/// The click-through overlay (S28): transparent, undecorated, never focusable, on every Space,
/// above menus, ignoring the mouse, and hidden until a session starts. Its window number lets
/// the helper skip it when it resolves which app owns a point.
#[cfg(target_os = "macos")]
fn build_overlay(
    app: &AppHandle,
    number: &std::sync::atomic::AtomicU32,
) -> tauri::Result<(tauri::WebviewWindow, NsWindow)> {
    let url = tauri::WebviewUrl::App("index.html?surface=glow".into());
    let window = tauri::WebviewWindowBuilder::new(app, GLOW_WINDOW, url)
        .title(GLOW_TITLE)
        .inner_size(1.0, 1.0)
        .position(0.0, 0.0)
        .transparent(true)
        .decorations(false)
        .always_on_top(true)
        .resizable(false)
        .skip_taskbar(true)
        .visible_on_all_workspaces(true)
        .shadow(false)
        .focused(false)
        .focusable(false)
        .visible(false)
        .build()?;
    window.set_ignore_cursor_events(true)?;
    let ns_window = window.ns_window()?;
    // SAFETY: setup runs on the main thread, and `ns_window` is the live window just built.
    number.store(unsafe { super::macos::prepare_overlay(ns_window) }, std::sync::atomic::Ordering::SeqCst);
    Ok((window, NsWindow(ns_window as usize)))
}

/// Orders the overlay in or out on the main thread and records its window number.
#[cfg(target_os = "macos")]
fn order_overlay(app: &AppHandle, ns_window: NsWindow, number: &Arc<std::sync::atomic::AtomicU32>, visible: bool) {
    let number = number.clone();
    let ordered = app.run_on_main_thread(move || {
        // SAFETY: on the main thread, with the overlay's live NSWindow.
        let n = unsafe { super::macos::order_overlay(ns_window.0 as *mut std::ffi::c_void, visible) };
        if n != 0 {
            number.store(n, std::sync::atomic::Ordering::SeqCst);
        }
    });
    if let Err(error) = ordered {
        eprintln!("gentle-dot: cannot show or hide the glow: {error}");
    }
}

/// The overlay's own thread: follows the session and draws each mark while it lasts.
#[cfg(target_os = "macos")]
fn run_overlay(
    app: AppHandle,
    (window, ns_window): (tauri::WebviewWindow, NsWindow),
    number: Arc<std::sync::atomic::AtomicU32>,
    control: Arc<Control>,
    marks: std::sync::mpsc::Receiver<Overlay>,
) {
    use tauri::{LogicalPosition, LogicalSize};
    let spawned = std::thread::Builder::new().name("computer-glow".into()).spawn(move || {
        let (mut shown, mut covering) = (false, None);
        for message in marks {
            match message {
                Overlay::Session(active) => {
                    if active != shown {
                        order_overlay(&app, ns_window, &number, active);
                        shown = active;
                    }
                }
                Overlay::Mark(mark) => {
                    // A mark that arrives after Stop or panic is dropped.
                    if !control.is_active(SystemClock.now_ms()) {
                        continue;
                    }
                    let placed =
                        super::glow::place(mark, super::macos::display_frame_at, super::macos::focused_frame);
                    let Some((display, event)) = placed else { continue };
                    if covering != Some(display) {
                        let _ = window.set_position(LogicalPosition::new(display.x, display.y));
                        let _ = window.set_size(LogicalSize::new(display.width, display.height));
                        covering = Some(display);
                    }
                    if !shown {
                        order_overlay(&app, ns_window, &number, true);
                        shown = true;
                    }
                    if let Err(error) = app.emit_to(GLOW_WINDOW, GLOW_EVENT, event) {
                        eprintln!("gentle-dot: cannot draw the glow: {error}");
                    }
                }
            }
        }
    });
    if let Err(error) = spawned {
        eprintln!("gentle-dot: the computer-control glow is unavailable: {error}");
    }
}

/// The glow overlay on Windows (S28): transparent, undecorated, click-through, never activated
/// nor captured, hidden until a session starts. Windows' hit tests skip it, so the window under
/// a point stays the app's.
#[cfg(windows)]
fn build_glow(app: &AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    let url = tauri::WebviewUrl::App("index.html?surface=glow".into());
    let window = tauri::WebviewWindowBuilder::new(app, GLOW_WINDOW, url)
        .title(GLOW_TITLE)
        .inner_size(1.0, 1.0)
        .position(0.0, 0.0)
        .transparent(true)
        .decorations(false)
        .always_on_top(true)
        .resizable(false)
        .skip_taskbar(true)
        .shadow(false)
        .focused(false)
        .focusable(false)
        .visible(false)
        .build()?;
    window.set_ignore_cursor_events(true)?;
    super::win::prepare_overlay(window.hwnd()?.0);
    Ok(window)
}

/// The glow's own thread on Windows: follows the session and draws each mark while it lasts.
/// Marks and displays are in physical pixels; the page gets CSS pixels.
#[cfg(windows)]
fn run_glow(app: AppHandle, window: tauri::WebviewWindow, control: Arc<Control>, marks: std::sync::mpsc::Receiver<Overlay>) {
    let Ok(hwnd) = window.hwnd() else {
        eprintln!("gentle-dot: the computer-control glow is unavailable: no window handle");
        return;
    };
    // The handle crosses to the glow thread as a number; the window lives as long as the app.
    let hwnd = hwnd.0 as usize;
    let spawned = std::thread::Builder::new().name("computer-glow".into()).spawn(move || {
        let window = hwnd as *mut std::ffi::c_void;
        let (mut shown, mut covering) = (false, None);
        for message in marks {
            match message {
                Overlay::Session(active) => {
                    if active != shown {
                        super::win::show_overlay(window, active);
                        shown = active;
                    }
                }
                Overlay::Mark(mark) => {
                    // A mark that arrives after Stop or panic is dropped.
                    if !control.is_active(SystemClock.now_ms()) {
                        continue;
                    }
                    let placed = super::glow::place(mark, super::win::display_frame_at, super::win::focused_frame);
                    let Some((display, event)) = placed else { continue };
                    if covering != Some(display) {
                        super::win::cover(window, display);
                        covering = Some(display);
                    }
                    if !shown {
                        super::win::show_overlay(window, true);
                        shown = true;
                    }
                    let event = super::glow::in_css_pixels(event, super::win::display_scale(display));
                    if let Err(error) = app.emit_to(GLOW_WINDOW, GLOW_EVENT, event) {
                        eprintln!("gentle-dot: cannot draw the glow: {error}");
                    }
                }
            }
        }
    });
    if let Err(error) = spawned {
        eprintln!("gentle-dot: the computer-control glow is unavailable: {error}");
    }
}

/// Reports the timeout when it happens, not only at the next tool call.
#[cfg(any(target_os = "macos", windows))]
fn watch_timeout(control: Arc<Control>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        control.expire(SystemClock.now_ms());
    });
}

/// Keeps a Gentle Dot window out of every screenshot (Windows), as macOS leaves the app's own
/// windows out of the capture.
#[cfg(windows)]
pub fn exclude_from_capture(window: &tauri::WebviewWindow) {
    let excluded = window.hwnd().map_err(|e| e.to_string()).and_then(|hwnd| super::win::exclude_from_capture(hwnd.0));
    if let Err(error) = excluded {
        eprintln!("gentle-dot: a window may show in computer-control screenshots: {error}");
    }
}

/// Ends the session (Stop button, tray item, panic shortcut). Queued actions are dropped.
pub fn stop(app: &AppHandle, reason: Reason) {
    if let Some(computer) = app.try_state::<Computer>() {
        computer.control.end(reason);
    }
}

#[tauri::command]
pub fn computer_endpoint(computer: State<'_, Computer>) -> Option<Endpoint> {
    computer.endpoint.clone()
}

#[tauri::command]
pub fn computer_permissions(computer: State<'_, Computer>) -> Permissions {
    #[cfg(any(target_os = "macos", windows))]
    return computer.desktop.permissions();
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = computer;
        Permissions { accessibility: false, screen_recording: false, screenshots_supported: false }
    }
}

#[tauri::command]
pub fn computer_request_permission(kind: PermissionKind) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return super::request_permission(kind);
    // Windows asks for no permission: there is nothing to grant.
    #[cfg(windows)]
    {
        let _ = kind;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = kind;
        Err("Computer control is only available on macOS.".into())
    }
}

#[tauri::command]
pub fn computer_stop(app: AppHandle) {
    stop(&app, Reason::Stopped);
}

/// Turns yolo mode on after the native confirmation, or off. Blocks on the dialog, so it runs
/// off the main thread; the tray item calls it from a thread of its own.
pub fn set_yolo(app: &AppHandle, enabled: bool) -> Result<StateEvent, String> {
    let computer = app.try_state::<Computer>().ok_or("Computer control is not ready.")?;
    #[cfg(any(target_os = "macos", windows))]
    return Ok(computer.control.set_yolo(enabled, SystemClock.now_ms(), &NativeDialogs { app: app.clone() }));
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (computer, enabled);
        Err("Computer control is only available on macOS.".into())
    }
}

/// The tray item: the opposite of the current state.
pub fn toggle_yolo(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let on = app.try_state::<Computer>().is_some_and(|c| c.control.yolo(SystemClock.now_ms()));
        if let Err(error) = set_yolo(&app, !on) {
            eprintln!("gentle-dot: cannot switch yolo mode: {error}");
        }
    });
}

#[tauri::command]
pub async fn computer_set_yolo(app: AppHandle, enabled: bool) -> Result<StateEvent, String> {
    tauri::async_runtime::spawn_blocking(move || set_yolo(&app, enabled)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn computer_status(computer: State<'_, Computer>) -> StateEvent {
    computer.control.status(SystemClock.now_ms())
}

/// Dialogs shown by the app itself (an `NSAlert` or a Windows task dialog, see `alert`), never
/// the webview.
#[cfg(any(target_os = "macos", windows))]
struct NativeDialogs {
    app: AppHandle,
}

#[cfg(any(target_os = "macos", windows))]
impl NativeDialogs {
    fn ask(&self, message: &str, choice: Choice) -> bool {
        crate::alert::ask(&self.app, "Gentle Dot", message, choice)
    }
}

#[cfg(any(target_os = "macos", windows))]
impl super::Dialogs for NativeDialogs {
    fn ask_grant(&self) -> bool {
        self.ask(GRANT_TEXT, GRANT_CHOICE)
    }

    fn confirm(&self, message: &str) -> bool {
        self.ask(message, CONFIRM_CHOICE)
    }

    fn ask_yolo(&self) -> bool {
        self.ask(YOLO_TEXT, YOLO_CHOICE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(windows))]
    fn the_grant_dialog_asks_exactly_what_the_spec_says() {
        assert_eq!(GRANT_TEXT, "Allow Gentle Dot to see and control this Mac for 30 minutes?");
        assert_eq!(GRANT_CHOICE, Choice { allow: "Allow", refuse: "Deny" });
    }

    #[test]
    #[cfg(not(windows))]
    fn the_yolo_dialog_says_what_it_skips_and_for_how_long() {
        assert_eq!(
            YOLO_TEXT,
            "Turn on yolo mode? Gentle Dot will send, pay, delete, and submit without asking you first, \
for up to 1 hour. Stop and ⌥⇧Esc still work."
        );
        assert_eq!(YOLO_CHOICE, Choice { allow: "Turn On", refuse: "Cancel" });
    }

    #[test]
    #[cfg(windows)]
    fn on_windows_the_dialogs_name_this_pc_and_alt_shift_esc() {
        assert_eq!(GRANT_TEXT, "Allow Gentle Dot to see and control this PC for 30 minutes?");
        assert_eq!(GRANT_CHOICE, Choice { allow: "Allow", refuse: "Deny" });
        assert_eq!(
            YOLO_TEXT,
            "Turn on yolo mode? Gentle Dot will send, pay, delete, and submit without asking you first, \
for up to 1 hour. Stop and Alt+Shift+Esc still work."
        );
        assert_eq!(YOLO_CHOICE, Choice { allow: "Turn On", refuse: "Cancel" });
    }

    #[test]
    fn every_computer_dialog_defaults_to_its_refusing_button() {
        for choice in [GRANT_CHOICE, CONFIRM_CHOICE, YOLO_CHOICE] {
            assert_eq!(choice.alert_buttons(), [choice.refuse, choice.allow]);
            assert!(!choice.allows_response(Some(crate::alert::REFUSE_RESPONSE)));
            assert!(!choice.allows_response(None));
            // Windows: the refusing button is the default; only the allowing id allows.
            assert!(!choice.allows_task_button(Some(crate::alert::TASK_REFUSE_ID)));
            assert!(!choice.allows_task_button(None));
            assert!(choice.allows_task_button(Some(crate::alert::TASK_ALLOW_ID)));
        }
    }

    #[test]
    fn the_panic_shortcut_is_option_shift_escape() {
        let shortcut: tauri_plugin_global_shortcut::Shortcut = PANIC_SHORTCUT.parse().unwrap();
        let expected = tauri_plugin_global_shortcut::Shortcut::new(
            Some(tauri_plugin_global_shortcut::Modifiers::ALT | tauri_plugin_global_shortcut::Modifiers::SHIFT),
            tauri_plugin_global_shortcut::Code::Escape,
        );
        assert_eq!(shortcut, expected);
    }

    #[test]
    fn permission_kinds_read_the_l60_names() {
        let kind: PermissionKind = serde_json::from_str("\"screenRecording\"").unwrap();
        assert_eq!(kind, PermissionKind::ScreenRecording);
        let kind: PermissionKind = serde_json::from_str("\"accessibility\"").unwrap();
        assert_eq!(kind, PermissionKind::Accessibility);
    }

    #[test]
    fn permissions_serialize_with_the_l60_field_names() {
        let value = serde_json::to_value(Permissions { accessibility: true, screen_recording: false, screenshots_supported: true }).unwrap();
        assert_eq!(value, serde_json::json!({"accessibility": true, "screenRecording": false, "screenshotsSupported": true}));
    }
}
