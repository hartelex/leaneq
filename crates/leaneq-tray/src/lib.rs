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

struct TrayState {
    pipe: String,
    presets_dir: PathBuf,
    settings_path: PathBuf,
    executable_path: PathBuf,
    settings: EngineSettings,
    status: Option<EngineStatus>,
    engine_error: Option<String>,
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

    let status_text = match &state.status {
        Some(status) => format!("leaneq: Connected ({} Hz)", status.sample_rate),
        None => state.engine_error.as_ref().map_or_else(
            || "leaneq: Disconnected".to_string(),
            |error| format!("leaneq: Disconnected ({error})"),
        ),
    };
    let _ = menu.append(&MenuItem::new(status_text, false, None));
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
        "close-tray" => Some(true),
        preset_id if preset_id.starts_with("preset:") => {
            let preset_path = PathBuf::from(preset_id.trim_start_matches("preset:"));
            if preset_path.is_file() {
                if let Ok(text) = fs::read_to_string(&preset_path) {
                    if let Ok(config) = EqConfig::from_toml_str(&text) {
                        if let Ok(IpcResponse::Ok) =
                            exchange(&state.pipe, &IpcCommand::SetConfig(config))
                        {
                            state.last_loaded_preset = Some(preset_path);
                            if let Some(path) = &state.last_loaded_preset {
                                if let Some(name) = path.file_stem().and_then(|stem| stem.to_str())
                                {
                                    state.settings.last_preset = name.to_string();
                                    let _ = state.save_settings();
                                }
                            }
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
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
    let mut state = TrayState {
        pipe,
        presets_dir,
        settings_path,
        executable_path,
        settings,
        status: None,
        engine_error: startup_error,
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
