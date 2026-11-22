use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use cpal::traits::{DeviceTrait, HostTrait};
use interprocess::local_socket::{prelude::*, GenericNamespaced};
use leaneq_proto::{
    read_frame, write_frame, EngineSettings, EngineStatus, EqConfig, IpcCommand, IpcResponse,
};
#[cfg(windows)]
use muda::ContextMenu;
use muda::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};

#[cfg(windows)]
const STARTUP_REGISTRY_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
#[cfg(windows)]
const STARTUP_REGISTRY_VALUE: &str = "leaneq";

struct TrayState {
    pipe: String,
    presets_dir: PathBuf,
    settings_path: PathBuf,
    executable_path: PathBuf,
    settings: EngineSettings,
    status: Option<EngineStatus>,
    engine_error: Option<String>,
    preset_error: Option<String>,
    #[cfg(windows)]
    startup_error: Option<String>,
    #[cfg(windows)]
    start_with_windows: bool,
    inputs: Vec<String>,
    outputs: Vec<String>,
    last_loaded_preset: Option<PathBuf>,
}

impl TrayState {
    fn refresh_status(&mut self) -> Option<EngineStatus> {
        if let Ok(IpcResponse::Devices { inputs, outputs }) =
            exchange(&self.pipe, &IpcCommand::ListDevices)
        {
            self.inputs = inputs;
            self.outputs = outputs;
        } else {
            let (inputs, outputs) = Self::local_device_names();
            self.inputs = inputs;
            self.outputs = outputs;
        }
        let status = match exchange(&self.pipe, &IpcCommand::GetStatus) {
            Ok(IpcResponse::Status(status)) => Some(status),
            _ => None,
        };
        self.status = status.clone();
        if status.is_some() {
            self.engine_error = None;
        }
        status
    }

    fn local_device_names() -> (Vec<String>, Vec<String>) {
        let host = cpal::default_host();
        let inputs = host
            .input_devices()
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|device| device.name().ok())
            .collect();
        let outputs = host
            .output_devices()
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|device| device.name().ok())
            .collect();
        (inputs, outputs)
    }

    fn save_settings(&self) -> Result<(), String> {
        let contents = self
            .settings
            .to_toml_string()
            .map_err(|error| error.to_string())?;
        fs::write(&self.settings_path, contents).map_err(|error| error.to_string())
    }

    fn restart_engine(&mut self, config: Option<EqConfig>) -> Result<(), String> {
        let _ = exchange(&self.pipe, &IpcCommand::Shutdown);
        for _ in 0..30 {
            if exchange(&self.pipe, &IpcCommand::Ping).is_err() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut daemon = spawn_engine(&self.executable_path, &self.settings, &self.pipe)?;

        for _ in 0..30 {
            if matches!(
                exchange(&self.pipe, &IpcCommand::Ping),
                Ok(IpcResponse::Pong)
            ) {
                let mut config = config.or_else(|| {
                    let path = self
                        .presets_dir
                        .join(format!("{}.toml", self.settings.last_preset));
                    fs::read_to_string(path)
                        .ok()
                        .and_then(|contents| EqConfig::from_toml_str(&contents).ok())
                });
                if let Some(mut active_config) = config.take() {
                    let bypass = active_config.bypass;
                    if !self.settings.last_preset.is_empty() {
                        let preset_path = self
                            .presets_dir
                            .join(format!("{}.toml", self.settings.last_preset));
                        if let Ok(contents) = fs::read_to_string(preset_path) {
                            if let Ok(preset) = EqConfig::from_toml_str(&contents) {
                                active_config = preset;
                                active_config.bypass = bypass;
                            }
                        }
                    }
                    let _ = exchange(&self.pipe, &IpcCommand::SetConfig(active_config.clone()));
                    let _ = exchange(&self.pipe, &IpcCommand::SetBypass(active_config.bypass));
                }
                return Ok(());
            }
            if let Some(status) = daemon.try_wait().map_err(|error| error.to_string())? {
                let mut error = String::new();
                if let Some(mut stderr) = daemon.stderr.take() {
                    stderr
                        .read_to_string(&mut error)
                        .map_err(|error| error.to_string())?;
                }
                let error = error.trim();
                return if error.is_empty() {
                    Err(format!("the audio engine exited with status {status}"))
                } else {
                    Err(error.to_string())
                };
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if daemon
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_none()
        {
            daemon.kill().map_err(|error| error.to_string())?;
            daemon.wait().map_err(|error| error.to_string())?;
        }
        Err("failed to restart the audio engine".into())
    }
}

fn spawn_engine(executable: &Path, settings: &EngineSettings, pipe: &str) -> Result<Child, String> {
    let mut command = Command::new(executable);
    command.arg("daemon");
    command.arg("--input").arg(&settings.input_device);
    command
        .arg("--output")
        .arg(&settings.output_device)
        .arg("--latency-ms")
        .arg(settings.latency_ms.to_string())
        .arg("--pipe")
        .arg(pipe);
    if let Some(sample_rate) = settings.sample_rate {
        command.arg("--sample-rate").arg(sample_rate.to_string());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())
}

fn exchange(pipe_name: &str, command: &IpcCommand) -> Result<IpcResponse, String> {
    let name = pipe_name
        .to_ns_name::<GenericNamespaced>()
        .map_err(|_| "invalid IPC socket name".to_string())?;
    let mut stream = LocalSocketStream::connect(name)
        .map_err(|_| "could not connect to the audio engine".to_string())?;
    write_frame(&mut stream, command).map_err(|error| error.to_string())?;
    read_frame(&mut stream).map_err(|error| error.to_string())
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(windows)]
fn startup_command(executable: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let mut command = vec![b'"' as u16];
    command.extend(executable.as_os_str().encode_wide());
    command.push(b'"' as u16);
    command.push(0);
    command
}

#[cfg(windows)]
fn startup_registration_enabled() -> Result<bool, String> {
    use std::ffi::c_void;

    type Hkey = *mut c_void;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(
            key: Hkey,
            subkey: *const u16,
            options: u32,
            access: u32,
            result: *mut Hkey,
        ) -> i32;
        fn RegQueryValueExW(
            key: Hkey,
            value_name: *const u16,
            reserved: *mut u32,
            value_type: *mut u32,
            data: *mut u8,
            data_size: *mut u32,
        ) -> i32;
        fn RegCloseKey(key: Hkey) -> i32;
    }

    const HKEY_CURRENT_USER: Hkey = 0x8000_0001usize as Hkey;
    const KEY_QUERY_VALUE: u32 = 0x0001;
    const ERROR_FILE_NOT_FOUND: i32 = 2;

    let subkey = wide_null(STARTUP_REGISTRY_KEY);
    let mut key = std::ptr::null_mut();
    // SAFETY: subkey is NUL-terminated, key is a valid output pointer, and the predefined
    // HKEY_CURRENT_USER handle is valid for RegOpenKeyExW.
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    if result != 0 {
        return Err(format!(
            "could not read Windows startup registration (error {result})"
        ));
    }

    let value_name = wide_null(STARTUP_REGISTRY_VALUE);
    // SAFETY: key is an open registry handle, value_name is NUL-terminated, and null data
    // pointers request only the value's metadata.
    let result = unsafe {
        RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // SAFETY: key was successfully opened above and is closed exactly once here.
    unsafe {
        RegCloseKey(key);
    }
    match result {
        0 => Ok(true),
        ERROR_FILE_NOT_FOUND => Ok(false),
        _ => Err(format!(
            "could not read Windows startup registration (error {result})"
        )),
    }
}

#[cfg(windows)]
fn set_startup_registration(enabled: bool, executable: &Path) -> Result<(), String> {
    use std::ffi::c_void;

    type Hkey = *mut c_void;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegCreateKeyExW(
            key: Hkey,
            subkey: *const u16,
            reserved: u32,
            class: *mut u16,
            options: u32,
            access: u32,
            security_attributes: *mut c_void,
            result: *mut Hkey,
            disposition: *mut u32,
        ) -> i32;
        fn RegSetValueExW(
            key: Hkey,
            value_name: *const u16,
            reserved: u32,
            value_type: u32,
            data: *const u8,
            data_size: u32,
        ) -> i32;
        fn RegDeleteValueW(key: Hkey, value_name: *const u16) -> i32;
        fn RegCloseKey(key: Hkey) -> i32;
    }

    const HKEY_CURRENT_USER: Hkey = 0x8000_0001usize as Hkey;
    const KEY_SET_VALUE: u32 = 0x0002;
    const REG_OPTION_NON_VOLATILE: u32 = 0;
    const REG_SZ: u32 = 1;
    const ERROR_FILE_NOT_FOUND: i32 = 2;

    let command = if enabled {
        Some(startup_command(executable))
    } else {
        None
    };
    let data_size = command
        .as_ref()
        .map(|command| {
            u32::try_from(command.len() * std::mem::size_of::<u16>())
                .map_err(|_| "startup command is too long".to_string())
        })
        .transpose()?;

    let subkey = wide_null(STARTUP_REGISTRY_KEY);
    let mut key = std::ptr::null_mut();
    let mut disposition = 0;
    // SAFETY: subkey is NUL-terminated; key and disposition are valid output pointers; the
    // predefined HKEY_CURRENT_USER handle is valid for RegCreateKeyExW.
    let result = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null_mut(),
            &mut key,
            &mut disposition,
        )
    };
    if result != 0 {
        return Err(format!(
            "could not update Windows startup registration (error {result})"
        ));
    }

    let value_name = wide_null(STARTUP_REGISTRY_VALUE);
    let result = if enabled {
        let command = command.expect("startup command created when enabled");
        let data_size = data_size.expect("startup command byte length calculated");
        // SAFETY: key is open, value_name and command are NUL-terminated UTF-16 strings, and
        // the byte length matches the initialized command buffer.
        unsafe {
            RegSetValueExW(
                key,
                value_name.as_ptr(),
                0,
                REG_SZ,
                command.as_ptr().cast(),
                data_size,
            )
        }
    } else {
        // SAFETY: key is open and value_name is NUL-terminated.
        unsafe { RegDeleteValueW(key, value_name.as_ptr()) }
    };
    // SAFETY: key was successfully opened above and is closed exactly once here.
    unsafe {
        RegCloseKey(key);
    }
    if result == 0 || (!enabled && result == ERROR_FILE_NOT_FOUND) {
        Ok(())
    } else {
        Err(format!(
            "could not update Windows startup registration (error {result})"
        ))
    }
}

fn resolve_presets_dir(requested: &Path) -> PathBuf {
    if requested.is_dir() {
        return requested.to_path_buf();
    }

    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            let beside_executable = parent.join(requested);
            if beside_executable.is_dir() {
                return beside_executable;
            }
        }
    }
    requested.to_path_buf()
}

fn preset_matches(actual: &EqConfig, preset: &EqConfig) -> bool {
    actual == preset
}

fn scan_presets(directory: &Path) -> Vec<(String, PathBuf)> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut presets = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("toml") {
            if let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) {
                presets.push((name.to_string(), path));
            }
        }
    }
    presets.sort_by_key(|preset| preset.0.to_lowercase());
    presets
}

#[cfg(not(windows))]
fn generate_icon() -> [u8; 32 * 32 * 4] {
    let mut rgba = [0u8; 32 * 32 * 4];
    for y in 0..32usize {
        for x in 0..32usize {
            let index = (y * 32 + x) * 4;
            let dx = if x < 16 { x as i32 - 16 } else { x as i32 - 15 };
            let dy = if y < 16 { y as i32 - 16 } else { y as i32 - 15 };
            let dx = dx.abs();
            let dy = dy.abs();
            let inside = dx < 15 && dy < 15;

            if inside {
                rgba[index] = 28;
                rgba[index + 1] = 34;
                rgba[index + 2] = 46;
                rgba[index + 3] = 255;

                let slider =
                    (7..=9).contains(&x) || (15..=17).contains(&x) || (23..=25).contains(&x);
                if slider {
                    let horizontal = (7..=24).contains(&y);
                    if horizontal {
                        rgba[index] = 118;
                        rgba[index + 1] = 190;
                        rgba[index + 2] = 255;
                    }
                }

                let knob = (10..=12).contains(&y) && (7..=9).contains(&x)
                    || (18..=20).contains(&y) && (15..=17).contains(&x)
                    || (14..=16).contains(&y) && (23..=25).contains(&x);
                if knob {
                    rgba[index] = 238;
                    rgba[index + 1] = 246;
                    rgba[index + 2] = 255;
                }
            }
        }
    }
    rgba
}

fn build_menu(state: &TrayState) -> Menu {
    let menu = Menu::new();

    let mut status_text = match &state.status {
        Some(status) => format!("leaneq: Connected ({} Hz)", status.sample_rate),
        None => state.engine_error.as_ref().map_or_else(
            || "leaneq: Disconnected".to_string(),
            |error| format!("leaneq: Disconnected ({error})"),
        ),
    };
    if let Some(error) = &state.preset_error {
        status_text.push_str(&format!(" - preset error: {error}"));
    }
    #[cfg(windows)]
    if let Some(error) = &state.startup_error {
        status_text.push_str(&format!(" - startup error: {error}"));
    }
    let _ = menu.append(&MenuItem::new(status_text, false, None));
    if let Some(status) = &state.status {
        let _ = menu.append(&MenuItem::new(
            format!(
                "Routing: {} -> {}",
                status.input_device, status.output_device
            ),
            false,
            None,
        ));
    }
    let _ = menu.append(&PredefinedMenuItem::separator());

    let bypass = state
        .status
        .as_ref()
        .is_some_and(|status| status.config.bypass);
    let bypass_item =
        CheckMenuItem::with_id("bypass", "Bypass (Disable equalizer)", true, bypass, None);
    let _ = menu.append(&bypass_item);
    let _ = menu.append(&PredefinedMenuItem::separator());

    let inputs_menu = Submenu::with_id("input-devices", "Input device", true);
    for (index, device) in state.inputs.iter().enumerate() {
        let id = format!("input:{index}");
        let checked = device == &state.settings.input_device
            || state
                .status
                .as_ref()
                .is_some_and(|status| status.input_device == *device);
        let _ = inputs_menu.append(&CheckMenuItem::with_id(id, device, true, checked, None));
    }
    let _ = menu.append(&inputs_menu);

    let outputs_menu = Submenu::with_id("output-devices", "Output device", true);
    let _ = outputs_menu.append(&CheckMenuItem::with_id(
        "output:default",
        "Windows default output device",
        true,
        state.settings.output_device.is_empty(),
        None,
    ));
    for (index, device) in state.outputs.iter().enumerate() {
        let id = format!("output:{index}");
        let _ = outputs_menu.append(&CheckMenuItem::with_id(
            id,
            device,
            true,
            device == &state.settings.output_device,
            None,
        ));
    }
    let _ = menu.append(&outputs_menu);
    let _ = menu.append(&PredefinedMenuItem::separator());

    let presets = scan_presets(&state.presets_dir);
    if presets.is_empty() {
        let _ = menu.append(&MenuItem::new("(No .toml files in presets/)", false, None));
    } else {
        for (name, path) in presets {
            let checked = if let Some(actual) = &state.status {
                if let Ok(text) = fs::read_to_string(&path) {
                    EqConfig::from_toml_str(&text)
                        .ok()
                        .is_some_and(|preset| preset_matches(&actual.config, &preset))
                } else {
                    false
                }
            } else {
                state
                    .last_loaded_preset
                    .as_ref()
                    .is_some_and(|last| last == &path)
            };

            let id = format!("preset:{}", path.to_string_lossy());
            let item = CheckMenuItem::with_id(MenuId::new(id), name, true, checked, None);
            let _ = menu.append(&item);
        }
    }

    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(
        "refresh",
        "Refresh status and presets",
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(
        "open-presets",
        "Open presets folder",
        true,
        None,
    ));
    #[cfg(windows)]
    {
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&CheckMenuItem::with_id(
            "start-with-windows",
            "Start with Windows",
            true,
            state.start_with_windows,
            None,
        ));
    }
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id("close-tray", "Exit tray", true, None));
    menu
}

fn run_action(state: &mut TrayState, menu_id: &MenuId) -> Option<bool> {
    let id = menu_id.as_ref();
    if let Some(index) = id
        .strip_prefix("input:")
        .and_then(|value| value.parse::<usize>().ok())
    {
        if let Some(device) = state.inputs.get(index) {
            let previous = state.status.as_ref().map(|status| status.config.clone());
            state.settings.input_device.clone_from(device);
            state.engine_error = state
                .save_settings()
                .and_then(|()| state.restart_engine(previous))
                .err();
        }
        return None;
    }
    if let Some(device_index) = id.strip_prefix("output:") {
        let previous = state.status.as_ref().map(|status| status.config.clone());
        match device_index {
            "default" => state.settings.output_device.clear(),
            value => {
                if let Ok(index) = value.parse::<usize>() {
                    if let Some(device) = state.outputs.get(index) {
                        state.settings.output_device.clone_from(device);
                    }
                }
            }
        }
        state.engine_error = state
            .save_settings()
            .and_then(|()| state.restart_engine(previous))
            .err();
        return None;
    }
    match id {
        "bypass" => {
            let new_bypass = !state
                .status
                .as_ref()
                .is_some_and(|status| status.config.bypass);
            let _ = exchange(&state.pipe, &IpcCommand::SetBypass(new_bypass));
            None
        }
        "refresh" => None,
        "open-presets" => {
            let _ = Command::new("explorer").arg(&state.presets_dir).spawn();
            None
        }
        #[cfg(windows)]
        "start-with-windows" => {
            let enabled = !state.start_with_windows;
            match set_startup_registration(enabled, &state.executable_path) {
                Ok(()) => {
                    state.start_with_windows = enabled;
                    state.startup_error = None;
                }
                Err(error) => state.startup_error = Some(error),
            }
            None
        }
        "close-tray" => Some(true),
        preset_id if preset_id.starts_with("preset:") => {
            let preset_path = PathBuf::from(preset_id.trim_start_matches("preset:"));
            state.preset_error = apply_preset(state, preset_path).err();
            None
        }
        _ => None,
    }
}

fn apply_preset(state: &mut TrayState, path: PathBuf) -> Result<(), String> {
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let config = EqConfig::from_toml_str(&text)
        .map_err(|error| format!("invalid preset {}: {error}", path.display()))?;

    match exchange(&state.pipe, &IpcCommand::SetConfig(config.clone()))? {
        IpcResponse::Ok => {}
        IpcResponse::Error(error) => return Err(format!("engine rejected preset: {error}")),
        _ => return Err("engine returned an unexpected response while loading preset".into()),
    }
    match exchange(&state.pipe, &IpcCommand::GetStatus)? {
        IpcResponse::Status(status) if status.config == config => {}
        IpcResponse::Status(_) => {
            return Err("engine did not report the selected preset as active".into());
        }
        IpcResponse::Error(error) => return Err(format!("could not verify preset: {error}")),
        _ => return Err("engine returned an unexpected response while verifying preset".into()),
    }

    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("could not determine preset name from {}", path.display()))?
        .to_string();
    state.last_loaded_preset = Some(path);
    state.settings.last_preset = name;
    state.save_settings()?;
    Ok(())
}

#[derive(Default)]
struct Handler {
    state: Option<TrayState>,
    tray: Option<tray_icon::TrayIcon>,
}

impl ApplicationHandler for Handler {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        _event: WindowEvent,
    ) {
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(
                event,
                TrayIconEvent::Click { .. } | TrayIconEvent::DoubleClick { .. }
            ) {
                if let Some(state) = &mut self.state {
                    state.refresh_status();
                    let menu = build_menu(state);
                    if let Some(tray) = &self.tray {
                        tray.set_menu(Some(Box::new(menu)));
                    }
                }
            }
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let mut should_close = false;
            if let Some(state) = &mut self.state {
                should_close = run_action(state, event.id()) == Some(true);
                state.refresh_status();
                let menu = build_menu(state);
                if let Some(tray) = &self.tray {
                    tray.set_menu(Some(Box::new(menu.clone())));
                    #[cfg(windows)]
                    if !should_close {
                        // SAFETY: tray.window_handle() is the live HWND owned by this TrayIcon.
                        unsafe {
                            menu.show_context_menu_for_hwnd(tray.window_handle() as isize, None);
                        }
                    }
                }
            }
            if should_close {
                event_loop.exit();
            }
        }
    }
}

pub fn run(
    pipe: String,
    requested_presets_dir: PathBuf,
    settings_path: PathBuf,
    executable_path: PathBuf,
    startup_error: Option<String>,
) -> Result<(), String> {
    let presets_dir = resolve_presets_dir(&requested_presets_dir);
    let settings = fs::read_to_string(&settings_path)
        .ok()
        .and_then(|contents| EngineSettings::from_toml_str(&contents).ok())
        .unwrap_or_default();
    let last_loaded_preset = if settings.last_preset.is_empty() {
        None
    } else {
        Some(presets_dir.join(format!("{}.toml", settings.last_preset)))
    };
    #[cfg(windows)]
    let (start_with_windows, startup_registration_error) = match startup_registration_enabled() {
        Ok(enabled) => (enabled, None),
        Err(error) => (false, Some(error)),
    };
    let mut state = TrayState {
        pipe,
        presets_dir,
        settings_path,
        executable_path,
        settings,
        status: None,
        engine_error: startup_error,
        preset_error: None,
        #[cfg(windows)]
        startup_error: startup_registration_error,
        #[cfg(windows)]
        start_with_windows,
        inputs: Vec::new(),
        outputs: Vec::new(),
        last_loaded_preset,
    };
    state.refresh_status();

    let event_loop = match EventLoop::new() {
        Ok(event_loop) => event_loop,
        Err(error) => return Err(error.to_string()),
    };
    event_loop.set_control_flow(ControlFlow::Wait);

    let menu = build_menu(&state);
    #[cfg(windows)]
    let icon =
        tray_icon::Icon::from_resource(1, Some((32, 32))).map_err(|error| error.to_string())?;
    #[cfg(not(windows))]
    let icon = {
        let icon_rgba = generate_icon();
        tray_icon::Icon::from_rgba(icon_rgba.to_vec(), 32, 32).map_err(|error| error.to_string())?
    };
    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("leaneq")
        .with_icon(icon)
        .build()
    {
        Ok(tray) => tray,
        Err(error) => return Err(error.to_string()),
    };

    let mut handler = Handler {
        state: Some(state),
        tray: Some(tray),
    };
    event_loop
        .run_app(&mut handler)
        .map_err(|error| error.to_string())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn startup_command_quotes_executable_paths_with_spaces() {
        let command = startup_command(Path::new(r"C:\Program Files\leaneq\leaneq.exe"));
        let command = String::from_utf16(&command[..command.len() - 1])
            .expect("startup command should be valid UTF-16");

        assert_eq!(command, r#""C:\Program Files\leaneq\leaneq.exe""#);
    }
}
