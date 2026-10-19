//! Which windows are apps, and what `open_app` may launch, on Windows (S24.2). Plain rules
//! over what `win` reads from the system, tested on every OS.

/// A top-level window as `win` reads it, front to back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopWindow {
    pub pid: i32,
    pub title: String,
    pub visible: bool,
    /// Hidden by the shell (a suspended Store app keeps a cloaked window).
    pub cloaked: bool,
    /// `WS_EX_TOOLWINDOW`: palettes and floating toolbars, not apps.
    pub tool: bool,
    /// `WS_EX_APPWINDOW`: shown on the taskbar even when owned.
    pub app_window: bool,
    /// Owned by another window (a dialog), so it is part of that window's app.
    pub owned: bool,
}

impl TopWindow {
    /// The taskbar's rule: a visible, titled window that is neither a tool window nor owned,
    /// unless it asks to be on the taskbar.
    pub fn is_app(&self) -> bool {
        self.visible && !self.cloaked && !self.title.trim().is_empty() && (self.app_window || (!self.tool && !self.owned))
    }
}

/// The processes with an app window, front to back, each once.
pub fn app_pids(windows: &[TopWindow]) -> Vec<i32> {
    let mut pids = Vec::new();
    for window in windows.iter().filter(|w| w.is_app()) {
        if !pids.contains(&window.pid) {
            pids.push(window.pid);
        }
    }
    pids
}

/// What `open_app` launches for a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// A Start menu shortcut whose name is the one asked for.
    Shortcut(String),
    /// The name itself, which Windows finds as a program (`notepad`, `chrome.exe`, a full path).
    Program(String),
}

/// Whether `name` may be handed to the shell: a program name or an executable's path, never a
/// URL, a protocol (`ms-settings:`), or a command line.
pub fn launchable(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("`open_app` needs an app name.".into());
    }
    let drive_path = name.len() > 2 && name.as_bytes()[0].is_ascii_alphabetic() && &name[1..3] == ":\\";
    if name.contains(':') && !(drive_path && name[2..].find(':').is_none()) {
        return Err(format!("`{name}` is not an app name: URLs and protocols cannot be opened."));
    }
    if name.contains(['"', '<', '>', '|', '&', '^', '%']) || name.contains(" /") || name.contains(" -") {
        return Err(format!("`{name}` is not an app name: command lines cannot be run."));
    }
    // A path is a local drive's (never a network share's) and names a program or a shortcut.
    if (name.contains('\\') || name.contains('/')) && !drive_path {
        return Err(format!("`{name}` is not an app: only programs on this PC's drives can be opened."));
    }
    let lower = name.to_ascii_lowercase();
    if drive_path && !(lower.ends_with(".exe") || lower.ends_with(".lnk")) {
        return Err(format!("`{name}` is not an app: only programs (.exe) and shortcuts (.lnk) can be opened."));
    }
    Ok(name)
}

/// The Start menu shortcut named `name` (its file name without `.lnk`, any case), else the
/// name as a program.
pub fn launch_for(name: &str, shortcuts: &[String]) -> Result<Launch, String> {
    let name = launchable(name)?;
    let wanted = name.strip_suffix(".lnk").unwrap_or(name);
    let found = shortcuts.iter().find(|path| {
        let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
        let lower = file.to_ascii_lowercase();
        lower.ends_with(".lnk") && file[..file.len() - 4].eq_ignore_ascii_case(wanted)
    });
    Ok(found.map_or_else(|| Launch::Program(name.to_string()), |path| Launch::Shortcut(path.clone())))
}

/// Whether a running app answers to `name`: its name or its executable (with or without `.exe`).
pub fn answers_to(name: &str, app_name: &str, executable: Option<&str>) -> bool {
    let name = name.trim();
    let bare = |exe: &str| exe.strip_suffix(".exe").or_else(|| exe.strip_suffix(".EXE")).unwrap_or(exe).to_string();
    app_name.eq_ignore_ascii_case(name)
        || executable.is_some_and(|exe| exe.eq_ignore_ascii_case(name) || bare(exe).eq_ignore_ascii_case(&bare(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(pid: i32, title: &str) -> TopWindow {
        TopWindow { pid, title: title.into(), visible: true, cloaked: false, tool: false, app_window: false, owned: false }
    }

    #[test]
    fn a_visible_titled_window_is_an_app() {
        assert!(window(1, "Notepad").is_app());
    }

    #[test]
    fn hidden_cloaked_untitled_tool_and_owned_windows_are_not_apps() {
        assert!(!TopWindow { visible: false, ..window(1, "x") }.is_app());
        assert!(!TopWindow { cloaked: true, ..window(1, "x") }.is_app());
        assert!(!window(1, "  ").is_app());
        assert!(!TopWindow { tool: true, ..window(1, "x") }.is_app());
        assert!(!TopWindow { owned: true, ..window(1, "x") }.is_app());
    }

    #[test]
    fn an_app_window_flag_puts_even_tool_or_owned_windows_on_the_list() {
        assert!(TopWindow { tool: true, app_window: true, ..window(1, "x") }.is_app());
        assert!(TopWindow { owned: true, app_window: true, ..window(1, "x") }.is_app());
        assert!(!TopWindow { app_window: true, visible: false, ..window(1, "x") }.is_app());
    }

    #[test]
    fn each_app_is_listed_once_front_to_back() {
        let windows = [window(7, "Chrome"), TopWindow { tool: true, ..window(9, "Palette") }, window(3, "Notepad"), window(7, "Chrome 2")];
        assert_eq!(app_pids(&windows), [7, 3]);
    }

    #[test]
    fn program_names_and_executable_paths_can_be_launched() {
        for name in ["Notepad", "chrome.exe", "Visual Studio Code", r"C:\Program Files\App\app.exe", r"C:\Users\me\Desktop\Tool.lnk"] {
            assert_eq!(launchable(name), Ok(name), "{name}");
        }
        assert_eq!(launchable("  Notepad "), Ok("Notepad"));
    }

    #[test]
    fn urls_protocols_command_lines_and_other_files_are_refused() {
        for name in ["", "  ", "ms-settings:", "ms-settings:privacy", "https://example.com", "file:///C:/x.exe", "cmd /c del x", "powershell -c x",
            "a & b", "a | b", r"C:\x\notes.txt", r"\\server\share\app.exe", r"C:\a:b.exe", "%COMSPEC%"]
        {
            assert!(launchable(name).is_err(), "{name:?} should be refused");
        }
        assert!(launchable("ms-settings:").unwrap_err().contains("protocols"));
        assert!(launchable("cmd /c del x").unwrap_err().contains("command lines"));
    }

    #[test]
    fn a_start_menu_shortcut_with_the_name_wins_over_the_program_name() {
        let shortcuts = [r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Google Chrome.lnk".to_string(), r"C:\Users\me\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Notes\Notes.lnk".to_string()];
        assert_eq!(launch_for("google chrome", &shortcuts), Ok(Launch::Shortcut(shortcuts[0].clone())));
        assert_eq!(launch_for("Notes.lnk", &shortcuts), Ok(Launch::Shortcut(shortcuts[1].clone())));
        assert_eq!(launch_for("Chrome", &shortcuts), Ok(Launch::Program("Chrome".into())));
        assert!(launch_for("ms-settings:", &shortcuts).is_err());
    }

    #[test]
    fn a_running_app_answers_to_its_name_or_executable() {
        assert!(answers_to("notepad", "Notepad", Some("Notepad.exe")));
        assert!(answers_to("NOTEPAD.EXE", "Notepad", Some("Notepad.exe")));
        assert!(answers_to("Google Chrome", "Google Chrome", Some("chrome.exe")));
        assert!(answers_to("chrome", "Google Chrome", Some("chrome.exe")));
        assert!(!answers_to("chrom", "Google Chrome", Some("chrome.exe")));
        assert!(!answers_to("Notepad", "Paint", None));
    }
}
