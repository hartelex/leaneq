#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use clap::Parser;
use interprocess::local_socket::{prelude::*, GenericNamespaced};
use leaneq_proto::{EngineSettings, EqConfig, IpcCommand, IpcResponse};

#[derive(Debug, Parser)]
#[command(name = "leaneq daemon")]
struct DaemonArgs {
    #[arg(long)]
    input: Option<String>,
    #[arg(long)]
    output: Option<String>,
    #[arg(long)]
    latency_ms: Option<u32>,
    #[arg(long)]
    sample_rate: Option<u32>,
    #[arg(long)]
    pipe: Option<String>,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn AttachConsole(process_id: u32) -> i32;
    fn CreateMutexW(
        attributes: *mut core::ffi::c_void,
        initial_owner: i32,
        name: *const u16,
    ) -> *mut core::ffi::c_void;
    fn GetLastError() -> u32;
    fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
}

#[cfg(windows)]
struct TrayMutex(*mut core::ffi::c_void);

#[cfg(windows)]
impl Drop for TrayMutex {
    fn drop(&mut self) {
        // SAFETY: CreateMutexW returns an owned kernel handle that is closed exactly once here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn attach_parent_console() {
    // SAFETY: AttachConsole takes a process identifier; ATTACH_PARENT_PROCESS targets this process's parent.
    unsafe {
        let _ = AttachConsole(u32::MAX);
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}

#[cfg(windows)]
fn acquire_tray_mutex() -> Result<Option<TrayMutex>, String> {
    use std::os::windows::ffi::OsStrExt;

    let name: Vec<u16> = OsStr::new("Local\\leaneq_tray_mutex")
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: name is NUL-terminated and remains alive for the call; null attributes and no owner are valid.
    let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err("failed to create the tray mutex".into());
    }
    // SAFETY: GetLastError has no preconditions and reads the calling thread's last Win32 error.
    let already_exists = unsafe { GetLastError() } == 183;
    if already_exists {
        // SAFETY: handle is a valid handle just returned by CreateMutexW.
        unsafe {
            let _ = CloseHandle(handle);
        }
        Ok(None)
    } else {
        Ok(Some(TrayMutex(handle)))
    }
}

#[cfg(not(windows))]
fn acquire_tray_mutex() -> Result<Option<()>, String> {
    Ok(Some(()))
}

#[cfg(windows)]
fn configure_hidden_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn configure_hidden_process(_command: &mut Command) {}

fn executable_directory() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "could not determine the executable directory".into())
}

fn initialize_files(directory: &Path) -> Result<(PathBuf, PathBuf), String> {
    let presets_dir = directory.join("presets");
    fs::create_dir_all(&presets_dir).map_err(|error| error.to_string())?;

    for (name, contents) in [
        ("flat.toml", include_str!("../../../presets/flat.toml")),
        (
            "bass_boost.toml",
            include_str!("../../../presets/bass_boost.toml"),
        ),
        (
            "vocal_clarity.toml",
            include_str!("../../../presets/vocal_clarity.toml"),
        ),
    ] {
        let path = presets_dir.join(name);
        if !path.exists() {
            fs::write(path, contents).map_err(|error| error.to_string())?;
        }
    }

    let settings_path = directory.join("engine.toml");
    if !settings_path.exists() {
        fs::write(
            &settings_path,
            "input_device = \"CABLE Output\"\noutput_device = \"\"\nlatency_ms = 10\nlast_preset = \"flat\"\n",
        )
        .map_err(|error| error.to_string())?;
    }
    Ok((presets_dir, settings_path))
}

fn load_settings(path: &Path) -> Result<EngineSettings, String> {
    let contents = fs::read_to_string(path).map_err(|error| error.to_string())?;
    EngineSettings::from_toml_str(&contents).map_err(|error| error.to_string())
}

fn exchange(command: &IpcCommand) -> Result<IpcResponse, String> {
    let mut stream = LocalSocketStream::connect(
        "leaneq"
            .to_ns_name::<GenericNamespaced>()
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    leaneq_proto::write_frame(&mut stream, command).map_err(|error| error.to_string())?;
    leaneq_proto::read_frame(&mut stream).map_err(|error| error.to_string())
}

fn ping_engine() -> bool {
    matches!(exchange(&IpcCommand::Ping), Ok(IpcResponse::Pong))
}

fn launch_daemon(executable: &Path, settings: &EngineSettings) -> Result<(), String> {
    let mut command = Command::new(executable);
    command
        .arg("daemon")
        .arg("--input")
        .arg(&settings.input_device)
        .arg("--output")
        .arg(&settings.output_device)
        .arg("--latency-ms")
        .arg(settings.latency_ms.to_string())
        .arg("--pipe")
        .arg("leaneq");
    if let Some(sample_rate) = settings.sample_rate {
        command.arg("--sample-rate").arg(sample_rate.to_string());
    }
    configure_hidden_process(&mut command);
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn ensure_engine(executable: &Path, settings: &EngineSettings) -> Result<(), String> {
    if !ping_engine() {
        launch_daemon(executable, settings)?;
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1500) {
        if ping_engine() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("the audio engine did not start within 1500 ms".into())
}

fn load_startup_preset(presets_dir: &Path, settings: &EngineSettings) -> Result<(), String> {
    let preset_path = presets_dir.join(format!("{}.toml", settings.last_preset));
    let config = fs::read_to_string(preset_path)
        .map_err(|error| error.to_string())
        .and_then(|text| EqConfig::from_toml_str(&text).map_err(|error| error.to_string()))?;
    match exchange(&IpcCommand::SetConfig(config))? {
        IpcResponse::Ok => Ok(()),
        IpcResponse::Error(error) => Err(error),
        _ => Err("the engine returned an unexpected response".into()),
    }
}

fn run_tray() -> Result<(), String> {
    let Some(_mutex) = acquire_tray_mutex()? else {
        return Ok(());
    };
    let directory = executable_directory()?;
    let (presets_dir, settings_path) = initialize_files(&directory)?;
    let settings = load_settings(&settings_path)?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    if ensure_engine(&executable, &settings).is_ok() {
        let _ = load_startup_preset(&presets_dir, &settings);
    }
    leaneq_tray::run("leaneq".to_string(), presets_dir, settings_path, executable)
}

fn run_daemon(args: &[OsString]) -> Result<(), String> {
    let executable_directory = executable_directory()?;
    let settings_path = executable_directory.join("engine.toml");
    let settings = if settings_path.is_file() {
        load_settings(&settings_path)?
    } else {
        EngineSettings::default()
    };
    let parser_args =
        std::iter::once(OsString::from("leaneq-daemon")).chain(args.iter().skip(1).cloned());
    let args = DaemonArgs::try_parse_from(parser_args).map_err(|error| error.to_string())?;

    leaneq_engine::run(leaneq_engine::EngineOptions::new(
        args.input.unwrap_or(settings.input_device),
        args.output.or({
            if settings.output_device.is_empty() {
                None
            } else {
                Some(settings.output_device)
            }
        }),
        args.latency_ms.unwrap_or(settings.latency_ms),
        args.sample_rate.or(settings.sample_rate),
        args.pipe.unwrap_or_else(|| "leaneq".to_string()),
    ))
}

fn run_cli(args: &[OsString]) -> Result<(), String> {
    attach_parent_console();
    let ui_args = std::iter::once(OsString::from("leaneq-ui")).chain(args.iter().cloned());
    leaneq_ui::run(ui_args)
}

fn run() -> Result<(), String> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.first().and_then(|arg| arg.to_str()) {
        None => run_tray(),
        Some("daemon") => run_daemon(&args),
        Some(_) => run_cli(&args),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("leaneq: {error}");
        std::process::exit(1);
    }
}
