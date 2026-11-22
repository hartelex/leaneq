use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, Host, SampleFormat, SampleRate, StreamConfig};
use interprocess::local_socket::traits::Listener;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToNsName};
use leaneq_dsp::{BiquadCoefficients, EqualizerChain, FilterType, MAX_BANDS};
use leaneq_proto::{read_frame, write_frame, EngineStatus, EqConfig, IpcCommand, IpcResponse};
use rtrb::RingBuffer;
use triple_buffer::triple_buffer;

#[derive(Debug, Clone)]
pub struct EngineOptions {
    input: String,
    output: Option<String>,
    latency_ms: u32,
    sample_rate: Option<u32>,
    pipe: String,
}

impl EngineOptions {
    pub fn new(
        input: String,
        output: Option<String>,
        latency_ms: u32,
        sample_rate: Option<u32>,
        pipe: String,
    ) -> Self {
        Self {
            input,
            output,
            latency_ms,
            sample_rate,
            pipe,
        }
    }
}

#[derive(Clone)]
struct DspSnapshot {
    coefficients: [BiquadCoefficients; MAX_BANDS],
    enabled: [bool; MAX_BANDS],
    preamp_linear: f32,
    bypass: bool,
}

impl DspSnapshot {
    fn from_config(config: &EqConfig, sample_rate: u32) -> Result<Self, String> {
        config.validate().map_err(|error| error.to_string())?;

        let mut coefficients = [BiquadCoefficients::IDENTITY; MAX_BANDS];
        let mut enabled = [false; MAX_BANDS];

        for (index, band) in config.bands.iter().take(MAX_BANDS).enumerate() {
            enabled[index] = band.enabled;
            if band.enabled && band.filter_type != FilterType::Bypass {
                coefficients[index] = leaneq_dsp::calculate_coefficients(
                    band.filter_type,
                    sample_rate as f32,
                    band.freq_hz,
                    band.gain_db,
                    band.q,
                );
            } else {
                coefficients[index] = BiquadCoefficients::IDENTITY;
            }
        }

        Ok(Self {
            coefficients,
            enabled,
            preamp_linear: EqualizerChain::preamp_linear(config.preamp_db),
            bypass: config.bypass,
        })
    }
}

struct AudioChain {
    chain: EqualizerChain,
}

impl AudioChain {
    fn new(initial: &DspSnapshot) -> Self {
        let mut chain = EqualizerChain {
            bypass: initial.bypass,
            preamp_db: 0.0,
            preamp_linear: initial.preamp_linear,
            ..EqualizerChain::default()
        };
        for index in 0..MAX_BANDS {
            chain.bands[index].enabled = initial.enabled[index];
            chain.bands[index].set_coefficients(initial.coefficients[index]);
        }
        Self { chain }
    }

    fn apply_snapshot(&mut self, snapshot: &DspSnapshot) {
        self.chain.bypass = snapshot.bypass;
        self.chain.preamp_linear = snapshot.preamp_linear;
        for index in 0..MAX_BANDS {
            let band = &mut self.chain.bands[index];
            band.enabled = snapshot.enabled[index];
            band.set_coefficients(snapshot.coefficients[index]);
        }
    }

    fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.chain.process(left, right)
    }
}

fn collect_devices(host: &Host) -> (Vec<String>, Vec<String>) {
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

fn find_input_device(host: &Host, needle: &str) -> Result<Device, String> {
    let needle = needle.to_lowercase();
    let devices = host.input_devices().map_err(|error| error.to_string())?;
    for device in devices {
        if let Ok(name) = device.name() {
            if name.to_lowercase().contains(&needle) {
                return Ok(device);
            }
        }
    }
    Err(format!("input device matching \"{needle}\" was not found"))
}

fn find_output_device(host: &Host, needle: &str) -> Result<Device, String> {
    let needle = needle.to_lowercase();
    let devices = host.output_devices().map_err(|error| error.to_string())?;
    for device in devices {
        if let Ok(name) = device.name() {
            if name.to_lowercase().contains(&needle) {
                return Ok(device);
            }
        }
    }
    Err(format!("output device matching \"{needle}\" was not found"))
}

fn stereo_f32_sample_rates(device: &Device, output: bool) -> Result<Vec<(u32, u32)>, String> {
    let ranges = if output {
        device
            .supported_output_configs()
            .map_err(|error| error.to_string())?
            .filter(|range| range.channels() == 2 && range.sample_format() == SampleFormat::F32)
            .map(|range| (range.min_sample_rate().0, range.max_sample_rate().0))
            .collect::<Vec<_>>()
    } else {
        device
            .supported_input_configs()
            .map_err(|error| error.to_string())?
            .filter(|range| range.channels() == 2 && range.sample_format() == SampleFormat::F32)
            .map(|range| (range.min_sample_rate().0, range.max_sample_rate().0))
            .collect::<Vec<_>>()
    };

    if ranges.is_empty() {
        return Err("selected device does not report stereo 32-bit floating-point support".into());
    }
    Ok(ranges)
}

fn supports_sample_rate(ranges: &[(u32, u32)], sample_rate: u32) -> bool {
    ranges
        .iter()
        .any(|(min_rate, max_rate)| (*min_rate..=*max_rate).contains(&sample_rate))
}

fn choose_sample_rate(
    input_ranges: &[(u32, u32)],
    output_ranges: &[(u32, u32)],
    preferred_rate: u32,
    requested_rate: Option<u32>,
) -> Result<u32, String> {
    if let Some(sample_rate) = requested_rate {
        if sample_rate > 0
            && supports_sample_rate(input_ranges, sample_rate)
            && supports_sample_rate(output_ranges, sample_rate)
        {
            return Ok(sample_rate);
        }
        return Err(format!(
            "requested sample rate {sample_rate} Hz is not supported by both devices as stereo f32"
        ));
    }

    if supports_sample_rate(input_ranges, preferred_rate)
        && supports_sample_rate(output_ranges, preferred_rate)
    {
        return Ok(preferred_rate);
    }

    let mut best_rate = None;
    let mut best_distance = u32::MAX;
    for (input_min, input_max) in input_ranges {
        for (output_min, output_max) in output_ranges {
            let min_rate = (*input_min).max(*output_min);
            let max_rate = (*input_max).min(*output_max);
            if min_rate <= max_rate {
                let candidate = preferred_rate.clamp(min_rate, max_rate);
                let distance = candidate.abs_diff(preferred_rate);
                if distance < best_distance {
                    best_rate = Some(candidate);
                    best_distance = distance;
                }
            }
        }
    }

    best_rate.ok_or_else(|| {
        "selected input and output devices have no shared stereo f32 sample rate".into()
    })
}

fn ipc_thread(
    pipe_name: String,
    shutdown: Arc<AtomicBool>,
    underruns: Arc<AtomicU64>,
    sample_rate: u32,
    input_name: String,
    output_name: String,
    mut triple_input: triple_buffer::Input<DspSnapshot>,
) {
    let ns_name = match pipe_name.to_ns_name::<GenericNamespaced>() {
        Ok(name) => name,
        Err(_) => return,
    };
    let listener = match ListenerOptions::new().name(ns_name).create_sync() {
        Ok(listener) => listener,
        Err(_) => return,
    };

    let mut config = EqConfig::default();

    while !shutdown.load(Ordering::Relaxed) {
        let mut stream = match listener.accept() {
            Ok(stream) => stream,
            Err(_) => continue,
        };

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let command = match read_frame::<_, IpcCommand>(&mut stream) {
                Ok(command) => command,
                Err(_) => break,
            };

            let response = match command {
                IpcCommand::Ping => IpcResponse::Pong,
                IpcCommand::ListDevices => {
                    let (inputs, outputs) = collect_devices(&cpal::default_host());
                    IpcResponse::Devices { inputs, outputs }
                }
                IpcCommand::GetStatus => IpcResponse::Status(EngineStatus {
                    sample_rate,
                    channels: 2,
                    input_device: input_name.clone(),
                    output_device: output_name.clone(),
                    underruns: underruns.load(Ordering::Relaxed),
                    config: config.clone(),
                }),
                IpcCommand::SetConfig(next) => match next.validate() {
                    Ok(()) => match DspSnapshot::from_config(&next, sample_rate) {
                        Ok(snapshot) => {
                            triple_input.write(snapshot);
                            config = next;
                            IpcResponse::Ok
                        }
                        Err(error) => IpcResponse::Error(error),
                    },
                    Err(error) => IpcResponse::Error(error.to_string()),
                },
                IpcCommand::UpdateBand { index, band } => {
                    if index >= config.bands.len() {
                        IpcResponse::Error(format!("band index {index} is out of range"))
                    } else {
                        let mut next = config.clone();
                        next.bands[index] = band;
                        match next.validate() {
                            Ok(()) => match DspSnapshot::from_config(&next, sample_rate) {
                                Ok(snapshot) => {
                                    triple_input.write(snapshot);
                                    config = next;
                                    IpcResponse::Ok
                                }
                                Err(error) => IpcResponse::Error(error),
                            },
                            Err(error) => IpcResponse::Error(error.to_string()),
                        }
                    }
                }
                IpcCommand::SetPreamp(db) => {
                    let mut next = config.clone();
                    next.preamp_db = db;
                    match next.validate() {
                        Ok(()) => match DspSnapshot::from_config(&next, sample_rate) {
                            Ok(snapshot) => {
                                triple_input.write(snapshot);
                                config = next;
                                IpcResponse::Ok
                            }
                            Err(error) => IpcResponse::Error(error),
                        },
                        Err(error) => IpcResponse::Error(error.to_string()),
                    }
                }
                IpcCommand::SetBypass(bypass) => {
                    let mut next = config.clone();
                    next.bypass = bypass;
                    match next.validate() {
                        Ok(()) => match DspSnapshot::from_config(&next, sample_rate) {
                            Ok(snapshot) => {
                                triple_input.write(snapshot);
                                config = next;
                                IpcResponse::Ok
                            }
                            Err(error) => IpcResponse::Error(error),
                        },
                        Err(error) => IpcResponse::Error(error.to_string()),
                    }
                }
                IpcCommand::Shutdown => {
                    shutdown.store(true, Ordering::SeqCst);
                    let _ = write_frame(&mut stream, &IpcResponse::Ok);
                    return;
                }
            };

            if write_frame(&mut stream, &response).is_err() {
                break;
            }
        }
    }
}

pub fn run(args: EngineOptions) -> Result<(), String> {
    let host = cpal::default_host();

    if args.latency_ms < 2 || args.latency_ms > 100 {
        return Err("latency-ms must be in range 2..100".into());
    }

    let input_device = find_input_device(&host, &args.input)?;
    let input_name = input_device.name().map_err(|error| error.to_string())?;
    let output_device = match args.output.as_deref().filter(|name| !name.is_empty()) {
        Some(name) => find_output_device(&host, name)?,
        None => host
            .default_output_device()
            .ok_or_else(|| "default output device was not found".to_string())?,
    };
    let output_name = output_device.name().map_err(|error| error.to_string())?;

    let input_sample_rates = stereo_f32_sample_rates(&input_device, false)?;
    let output_sample_rates = stereo_f32_sample_rates(&output_device, true)?;

    let output_default = output_device
        .default_output_config()
        .map_err(|error| error.to_string())?;
    let sample_rate = choose_sample_rate(
        &input_sample_rates,
        &output_sample_rates,
        output_default.sample_rate().0,
        args.sample_rate,
    )?;

    let stream_config = StreamConfig {
        channels: 2,
        sample_rate: SampleRate(sample_rate),
        buffer_size: BufferSize::Default,
    };

    let target_latency_samples =
        ((sample_rate as u64 * args.latency_ms as u64 / 1000) * 2) as usize;
    let target_latency_samples = target_latency_samples.max(2);
    let mut ring_capacity = 8 * target_latency_samples;
    if ring_capacity % 2 != 0 {
        ring_capacity += 1;
    }

    let (producer, mut consumer) = RingBuffer::<f32>::new(ring_capacity);
    let initial_config = EqConfig::default();
    let initial_snapshot = DspSnapshot::from_config(&initial_config, sample_rate)
        .map_err(|error| error.to_string())?;
    let (triple_input, mut triple_output) = triple_buffer(&initial_snapshot);

    let underruns = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));

    let mut producer = producer;
    let underruns_input = Arc::clone(&underruns);
    let input_stream = input_device
        .build_input_stream(
            &stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let writable = producer.slots();
                let usable = data.len().min(writable);
                let usable = usable - usable % 2;
                let mut index = 0usize;
                while index < usable {
                    if producer.push(data[index]).is_err()
                        || producer.push(data[index + 1]).is_err()
                    {
                        break;
                    }
                    index += 2;
                }
            },
            move |_error| {
                underruns_input.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|error| format!("failed to open stereo f32 input at {sample_rate} Hz: {error}"))?;

    let underruns_output = Arc::clone(&underruns);
    let underruns_output_error = Arc::clone(&underruns_output);
    let mut audio_chain = AudioChain::new(&initial_snapshot);
    let output_stream = output_device
        .build_output_stream(
            &stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                if triple_output.updated() {
                    let snapshot = triple_output.read();
                    audio_chain.apply_snapshot(snapshot);
                }

                let pending = consumer.slots();
                if pending > target_latency_samples * 2 {
                    let excess = pending - target_latency_samples;
                    let excess = excess - excess % 2;
                    for _ in 0..excess {
                        let _ = consumer.pop();
                    }
                }

                let mut index = 0usize;
                while index + 1 < data.len() {
                    if consumer.slots() >= 2 {
                        if let (Ok(left), Ok(right)) = (consumer.pop(), consumer.pop()) {
                            let (left, right) = audio_chain.process(left, right);
                            data[index] = left;
                            data[index + 1] = right;
                        } else {
                            data[index] = 0.0;
                            data[index + 1] = 0.0;
                            underruns_output.fetch_add(1, Ordering::Relaxed);
                        }
                    } else {
                        data[index] = 0.0;
                        data[index + 1] = 0.0;
                        underruns_output.fetch_add(1, Ordering::Relaxed);
                    }
                    index += 2;
                }
            },
            move |_error| {
                underruns_output_error.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|error| {
            format!("failed to open stereo f32 output at {sample_rate} Hz: {error}")
        })?;

    input_stream.play().map_err(|error| error.to_string())?;
    output_stream.play().map_err(|error| error.to_string())?;

    let shutdown_ipc = Arc::clone(&shutdown);
    let underruns_ipc = Arc::clone(&underruns);
    let ipc_shutdown = Arc::clone(&shutdown);
    let ipc_thread = thread::spawn(move || {
        ipc_thread(
            args.pipe,
            shutdown_ipc,
            underruns_ipc,
            sample_rate,
            input_name,
            output_name,
            triple_input,
        );
    });

    let shutdown_ctrlc = Arc::clone(&shutdown);
    let _ = ctrlc::set_handler(move || {
        shutdown_ctrlc.store(true, Ordering::SeqCst);
    });

    while !ipc_shutdown.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(10));
    }

    let _ = input_stream.pause();
    let _ = output_stream.pause();
    drop(input_stream);
    drop(output_stream);
    let _ = ipc_thread.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applying_a_preset_snapshot_changes_the_live_audio() {
        let initial_config = EqConfig::default();
        let initial_snapshot =
            DspSnapshot::from_config(&initial_config, 48_000).expect("default config is valid");

        let mut preset = EqConfig::default();
        preset.bands[0] = leaneq_proto::BandConfig {
            enabled: true,
            filter_type: FilterType::LowShelf,
            freq_hz: 100.0,
            gain_db: 8.0,
            q: 0.707,
        };
        let preset_snapshot =
            DspSnapshot::from_config(&preset, 48_000).expect("test preset is valid");

        let mut chain = AudioChain::new(&initial_snapshot);
        chain.apply_snapshot(&preset_snapshot);

        let mut input_energy = 0.0;
        let mut output_energy = 0.0;
        for sample_index in 0..48_000 {
            let input = (core::f32::consts::TAU * 60.0 * sample_index as f32 / 48_000.0).sin();
            let (output, _) = chain.process(input, input);
            if sample_index >= 4_800 {
                input_energy += input * input;
                output_energy += output * output;
            }
        }

        assert!(
            output_energy > input_energy * 2.0,
            "preset should audibly boost its target range"
        );
    }

    #[test]
    fn chooses_a_shared_rate_when_the_preferred_rate_is_unsupported() {
        let input_rates = [(48_000, 96_000)];
        let output_rates = [(44_100, 48_000)];

        assert_eq!(
            choose_sample_rate(&input_rates, &output_rates, 44_100, None),
            Ok(48_000)
        );
    }

    #[test]
    fn rejects_an_explicit_rate_not_supported_by_both_devices() {
        let input_rates = [(48_000, 96_000)];
        let output_rates = [(44_100, 48_000)];

        assert!(choose_sample_rate(&input_rates, &output_rates, 48_000, Some(44_100)).is_err());
    }
}
