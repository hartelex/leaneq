use std::ffi::OsString;
use std::fs;
use std::time::Instant;

use clap::{Parser, Subcommand};
use interprocess::local_socket::prelude::*;
use interprocess::local_socket::GenericNamespaced;
use leaneq_proto::{read_frame, write_frame, EqConfig, IpcCommand, IpcResponse};

#[derive(Debug, Parser)]
#[command(name = "leaneq-ui")]
#[command(about = "Synchronous terminal client for leaneq-engine")]
struct Cli {
    #[arg(long, global = true, default_value = "leaneq")]
    pipe: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Ping,
    Status,
    Band {
        index: usize,
        #[arg(long)]
        freq: Option<f32>,
        #[arg(long, allow_hyphen_values = true)]
        gain: Option<f32>,
        #[arg(long)]
        q: Option<f32>,
        #[arg(long)]
        r#type: Option<String>,
        #[arg(long)]
        enabled: Option<bool>,
    },
    Preamp {
        #[arg(allow_hyphen_values = true)]
        db: f32,
    },
    Bypass {
        state: String,
    },
    Load {
        path: std::path::PathBuf,
    },
    Save {
        path: std::path::PathBuf,
    },
    Stop,
}

fn connect(pipe_name: &str) -> Result<LocalSocketStream, String> {
    let name = pipe_name
        .to_ns_name::<GenericNamespaced>()
        .map_err(|_| "invalid IPC socket name".to_string())?;
    LocalSocketStream::connect(name).map_err(|_| {
        format!("Could not connect to the engine IPC socket \"{pipe_name}\". Start leaneq first.")
    })
}

fn exchange(pipe_name: &str, command: &IpcCommand) -> Result<IpcResponse, String> {
    let mut stream = connect(pipe_name)?;
    write_frame(&mut stream, command).map_err(|error| format!("IPC send failed: {error}"))?;
    read_frame(&mut stream).map_err(|error| format!("IPC receive failed: {error}"))
}

fn require_ok(response: IpcResponse) -> Result<(), String> {
    match response {
        IpcResponse::Ok => Ok(()),
        IpcResponse::Pong => Ok(()),
        IpcResponse::Status(_) => Err("received an unexpected engine status".into()),
        IpcResponse::Devices { .. } => Err("received an unexpected device list".into()),
        IpcResponse::Error(error) => Err(error),
    }
}

fn get_status(pipe_name: &str) -> Result<leaneq_proto::EngineStatus, String> {
    match exchange(pipe_name, &IpcCommand::GetStatus)? {
        IpcResponse::Status(status) => Ok(status),
        IpcResponse::Error(error) => Err(error),
        _ => Err("the engine returned an unexpected response".into()),
    }
}

fn parse_bypass(value: &str) -> Option<bool> {
    match value.to_lowercase().as_str() {
        "on" | "true" | "1" => Some(true),
        "off" | "false" | "0" => Some(false),
        _ => None,
    }
}

fn parse_filter_type(value: &str) -> Result<leaneq_dsp::FilterType, String> {
    match value.to_lowercase().as_str() {
        "peaking" => Ok(leaneq_dsp::FilterType::Peaking),
        "lowshelf" => Ok(leaneq_dsp::FilterType::LowShelf),
        "highshelf" => Ok(leaneq_dsp::FilterType::HighShelf),
        "lowpass" => Ok(leaneq_dsp::FilterType::LowPass),
        "highpass" => Ok(leaneq_dsp::FilterType::HighPass),
        _ => Err(format!(
            "Invalid filter type \"{value}\". Allowed: peaking, lowshelf, highshelf, lowpass, highpass"
        )),
    }
}

fn filter_type_name(filter_type: leaneq_dsp::FilterType) -> &'static str {
    match filter_type {
        leaneq_dsp::FilterType::Bypass => "Bypass",
        leaneq_dsp::FilterType::Peaking => "Peaking",
        leaneq_dsp::FilterType::LowShelf => "LowShelf",
        leaneq_dsp::FilterType::HighShelf => "HighShelf",
        leaneq_dsp::FilterType::LowPass => "LowPass",
        leaneq_dsp::FilterType::HighPass => "HighPass",
    }
}

fn print_status(status: &leaneq_proto::EngineStatus) {
    println!("Input device         : {}", status.input_device);
    println!("Output device        : {}", status.output_device);
    println!(
        "Sample rate          : {} Hz | Channels: {}",
        status.sample_rate, status.channels
    );
    println!(
        "Global bypass        : {}",
        if status.config.bypass {
            "ACTIVE"
        } else {
            "DISABLED"
        }
    );
    println!("Preamp               : {:.2} dB", status.config.preamp_db);
    println!("Underruns            : {}", status.underruns);
    println!();
    println!(
        "{:<7} {:<8} {:<11} {:>12} {:>10} {:>8}",
        "Index", "State", "Type", "Frequency", "Gain dB", "Q"
    );
    println!(
        "{:-<7} {:-<8} {:-<11} {:-<12} {:-<10} {:-<8}",
        "", "", "", "", "", ""
    );
    for (index, band) in status.config.bands.iter().enumerate() {
        println!(
            "{:<7} {:<8} {:<11} {:>12.2} Hz {:>9.2} {:>8.2}",
            index,
            if band.enabled { "[ON]" } else { "[OFF]" },
            filter_type_name(band.filter_type),
            band.freq_hz,
            band.gain_db,
            band.q
        );
    }
}

pub fn run<I, T>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let should_exit_successfully = matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            );
            error
                .print()
                .map_err(|print_error| print_error.to_string())?;
            return if should_exit_successfully {
                Ok(())
            } else {
                Err("invalid command-line arguments".into())
            };
        }
    };

    match cli.command {
        Command::Ping => {
            let start = Instant::now();
            require_ok(exchange(&cli.pipe, &IpcCommand::Ping)?)?;
            let elapsed = start.elapsed();
            if elapsed.as_micros() < 1000 {
                println!("Pong - RTT: {} µs", elapsed.as_micros());
            } else {
                println!("Pong - RTT: {:.3} ms", elapsed.as_secs_f64() * 1000.0);
            }
        }
        Command::Status => {
            let status = get_status(&cli.pipe)?;
            print_status(&status);
        }
        Command::Band {
            index,
            freq,
            gain,
            q,
            r#type,
            enabled,
        } => {
            let status = get_status(&cli.pipe)?;
            if index >= status.config.bands.len() {
                return Err(format!(
                    "Band index {index} is out of range (available bands: 0..{})",
                    status.config.bands.len().saturating_sub(1)
                ));
            }

            let mut band = status.config.bands[index].clone();
            if let Some(freq) = freq {
                band.freq_hz = freq;
            }
            if let Some(gain) = gain {
                band.gain_db = gain;
            }
            if let Some(q) = q {
                band.q = q;
            }
            if let Some(type_name) = r#type {
                band.filter_type = parse_filter_type(&type_name)?;
            }
            if let Some(enabled) = enabled {
                band.enabled = enabled;
            }

            require_ok(exchange(
                &cli.pipe,
                &IpcCommand::UpdateBand { index, band },
            )?)?;
            println!("Zaktualizowano pasmo {index}");
        }
        Command::Preamp { db } => {
            require_ok(exchange(&cli.pipe, &IpcCommand::SetPreamp(db))?)?;
            println!("Preamp set to {db:.2} dB");
        }
        Command::Bypass { state } => {
            let bypass = parse_bypass(&state)
                .ok_or_else(|| "Bypass must be one of: on, true, 1, off, false, 0".to_string())?;
            require_ok(exchange(&cli.pipe, &IpcCommand::SetBypass(bypass))?)?;
            println!(
                "Global bypass: {}",
                if bypass { "ACTIVE" } else { "DISABLED" }
            );
        }
        Command::Load { path } => {
            let text = fs::read_to_string(&path)
                .map_err(|error| format!("Could not read preset {}: {error}", path.display()))?;
            let config = EqConfig::from_toml_str(&text)
                .map_err(|error| format!("Invalid preset: {error}"))?;
            require_ok(exchange(&cli.pipe, &IpcCommand::SetConfig(config))?)?;
            println!("Loaded preset: {}", path.display());
        }
        Command::Save { path } => {
            let status = get_status(&cli.pipe)?;
            let text = status
                .config
                .to_toml_string()
                .map_err(|error| format!("Could not serialize configuration: {error}"))?;
            fs::write(&path, text)
                .map_err(|error| format!("Could not save {}: {error}", path.display()))?;
            println!("Saved configuration: {}", path.display());
        }
        Command::Stop => {
            require_ok(exchange(&cli.pipe, &IpcCommand::Shutdown)?)?;
            println!("leaneq engine stopped");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bypass_states() {
        assert_eq!(parse_bypass("on"), Some(true));
        assert_eq!(parse_bypass("TRUE"), Some(true));
        assert_eq!(parse_bypass("1"), Some(true));
        assert_eq!(parse_bypass("off"), Some(false));
        assert_eq!(parse_bypass("0"), Some(false));
        assert_eq!(parse_bypass("maybe"), None);
    }

    #[test]
    fn parses_filter_types() {
        assert_eq!(
            parse_filter_type("lowshelf").ok(),
            Some(leaneq_dsp::FilterType::LowShelf)
        );
        assert!(parse_filter_type("invalid").is_err());
    }
}
