use std::io::{Read, Write};

use leaneq_dsp::{FilterType, MAX_BANDS};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const MAX_FRAME_SIZE: u32 = 65_536;

#[derive(Debug)]
pub enum ProtoError {
    Io(std::io::Error),
    Serialize(String),
    Deserialize(String),
    Toml(String),
    Invalid(String),
    FrameTooLarge(u32),
    EmptyFrame,
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Serialize(error) => write!(f, "serialization error: {error}"),
            Self::Deserialize(error) => write!(f, "deserialization error: {error}"),
            Self::Toml(error) => write!(f, "TOML error: {error}"),
            Self::Invalid(error) => write!(f, "invalid configuration: {error}"),
            Self::FrameTooLarge(length) => write!(f, "frame is too large: {length} bytes"),
            Self::EmptyFrame => write!(f, "frame length is zero"),
        }
    }
}

impl std::error::Error for ProtoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ProtoError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BandConfig {
    pub enabled: bool,
    pub filter_type: FilterType,
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

impl Default for BandConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            filter_type: FilterType::Peaking,
            freq_hz: 1000.0,
            gain_db: 0.0,
            q: 1.41,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqConfig {
    pub preamp_db: f32,
    pub bypass: bool,
    pub bands: Vec<BandConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineSettings {
    pub input_device: String,
    pub output_device: String,
    pub latency_ms: u32,
    pub last_preset: String,
    pub sample_rate: Option<u32>,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            input_device: "CABLE Output".to_string(),
            output_device: String::new(),
            latency_ms: 10,
            last_preset: "flat".to_string(),
            sample_rate: None,
        }
    }
}

impl EngineSettings {
    pub fn from_toml_str(value: &str) -> Result<Self, ProtoError> {
        toml::from_str(value).map_err(|error| ProtoError::Toml(error.to_string()))
    }

    pub fn to_toml_string(&self) -> Result<String, ProtoError> {
        toml::to_string_pretty(self).map_err(|error| ProtoError::Toml(error.to_string()))
    }
}

impl Default for EqConfig {
    fn default() -> Self {
        const FREQUENCIES: [f32; 10] = [
            31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
        ];

        let mut bands = Vec::with_capacity(10);
        for freq_hz in FREQUENCIES {
            bands.push(BandConfig {
                enabled: true,
                filter_type: FilterType::Peaking,
                freq_hz,
                gain_db: 0.0,
                q: 1.41,
            });
        }

        Self {
            preamp_db: 0.0,
            bypass: false,
            bands,
        }
    }
}

impl EqConfig {
    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.bands.len() > MAX_BANDS {
            return Err(ProtoError::Invalid(format!(
                "band count {} exceeds maximum {}",
                self.bands.len(),
                MAX_BANDS
            )));
        }
        if !self.preamp_db.is_finite() {
            return Err(ProtoError::Invalid("preamp_db must be finite".into()));
        }

        for (index, band) in self.bands.iter().enumerate() {
            if !band.freq_hz.is_finite() {
                return Err(ProtoError::Invalid(format!(
                    "band {index}: freq_hz must be finite"
                )));
            }
            if band.freq_hz <= 0.0 {
                return Err(ProtoError::Invalid(format!(
                    "band {index}: freq_hz must be > 0"
                )));
            }
            if !band.gain_db.is_finite() {
                return Err(ProtoError::Invalid(format!(
                    "band {index}: gain_db must be finite"
                )));
            }
            if !band.q.is_finite() {
                return Err(ProtoError::Invalid(format!(
                    "band {index}: q must be finite"
                )));
            }
            if band.q <= 0.0 {
                return Err(ProtoError::Invalid(format!("band {index}: q must be > 0")));
            }
        }

        Ok(())
    }

    pub fn from_toml_str(s: &str) -> Result<Self, ProtoError> {
        let config: Self =
            toml::from_str(s).map_err(|error| ProtoError::Toml(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_toml_string(&self) -> Result<String, ProtoError> {
        self.validate()
            .map_err(|error| ProtoError::Invalid(error.to_string()))?;
        toml::to_string_pretty(self).map_err(|error| ProtoError::Toml(error.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcCommand {
    Ping,
    GetStatus,
    ListDevices,
    SetConfig(EqConfig),
    UpdateBand { index: usize, band: BandConfig },
    SetPreamp(f32),
    SetBypass(bool),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineStatus {
    pub sample_rate: u32,
    pub channels: u16,
    pub input_device: String,
    pub output_device: String,
    pub underruns: u64,
    pub config: EqConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcResponse {
    Ok,
    Pong,
    Status(EngineStatus),
    Devices {
        inputs: Vec<String>,
        outputs: Vec<String>,
    },
    Error(String),
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, msg: &T) -> Result<(), ProtoError> {
    let payload =
        bincode::serialize(msg).map_err(|error| ProtoError::Serialize(error.to_string()))?;
    let length = u32::try_from(payload.len())
        .map_err(|_| ProtoError::Serialize("serialized frame exceeds u32 length".into()))?;
    if length == 0 || length > MAX_FRAME_SIZE {
        return Err(ProtoError::Serialize(format!(
            "serialized frame length {length} is outside the supported range"
        )));
    }

    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&payload)?;
    Ok(())
}

pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, ProtoError> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header)?;
    let length = u32::from_le_bytes(header);

    if length == 0 {
        return Err(ProtoError::EmptyFrame);
    }
    if length > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge(length));
    }

    let mut payload = vec![0u8; length as usize];
    reader.read_exact(&mut payload)?;
    bincode::deserialize(&payload).map_err(|error| ProtoError::Deserialize(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn valid_config() -> EqConfig {
        EqConfig::default()
    }

    #[test]
    fn frame_round_trip() {
        let command = IpcCommand::SetConfig(valid_config());
        let mut cursor = Cursor::new(Vec::new());

        write_frame(&mut cursor, &command).expect("frame should serialize");
        cursor.set_position(0);
        let decoded: IpcCommand = read_frame(&mut cursor).expect("frame should deserialize");

        assert_eq!(decoded, command);
    }

    #[test]
    fn device_listing_ipc_round_trip() {
        let command = IpcCommand::ListDevices;
        let mut command_frame = Cursor::new(Vec::new());
        write_frame(&mut command_frame, &command).expect("device command should serialize");
        command_frame.set_position(0);
        let decoded_command: IpcCommand =
            read_frame(&mut command_frame).expect("device command should deserialize");
        assert_eq!(decoded_command, command);

        let response = IpcResponse::Devices {
            inputs: vec!["Cable Input".to_string()],
            outputs: vec!["Speakers".to_string()],
        };
        let mut response_frame = Cursor::new(Vec::new());
        write_frame(&mut response_frame, &response).expect("device response should serialize");
        response_frame.set_position(0);
        let decoded_response: IpcResponse =
            read_frame(&mut response_frame).expect("device response should deserialize");
        assert_eq!(decoded_response, response);
    }

    #[test]
    fn oversized_frame_is_rejected_before_allocation() {
        let mut frame = Cursor::new((MAX_FRAME_SIZE + 1).to_le_bytes());
        let result = read_frame::<_, IpcCommand>(&mut frame);

        match result {
            Err(ProtoError::FrameTooLarge(length)) => {
                assert_eq!(length, MAX_FRAME_SIZE + 1);
            }
            _ => panic!("oversized frame should be rejected"),
        }
    }

    #[test]
    fn toml_round_trip_and_validation() {
        let config = valid_config();
        let toml_text = config.to_toml_string().expect("TOML should serialize");
        let decoded = EqConfig::from_toml_str(&toml_text).expect("TOML should deserialize");
        assert_eq!(decoded, config);

        let invalid_q = r#"
preamp_db = 0.0
bypass = false

[[bands]]
enabled = true
filter_type = "Peaking"
freq_hz = 1000.0
gain_db = 0.0
q = 0.0
"#;
        assert!(EqConfig::from_toml_str(invalid_q).is_err());

        let invalid_frequency = r#"
preamp_db = 0.0
bypass = false

[[bands]]
enabled = true
filter_type = "Peaking"
freq_hz = -100.0
gain_db = 0.0
q = 1.41
"#;
        assert!(EqConfig::from_toml_str(invalid_frequency).is_err());
    }

    #[test]
    fn rejects_too_many_bands_and_non_finite_values() {
        let mut config = valid_config();
        config.bands[0].q = f32::NAN;
        assert!(config.validate().is_err());

        config = valid_config();
        for _ in 0..MAX_BANDS {
            config.bands.push(BandConfig::default());
        }
        assert!(config.validate().is_err());
    }

    #[test]
    fn engine_settings_round_trip_without_sample_rate() {
        let settings = EngineSettings::default();
        let serialized = settings
            .to_toml_string()
            .expect("default engine settings should serialize");
        let decoded = EngineSettings::from_toml_str(&serialized)
            .expect("default engine settings should deserialize");

        assert_eq!(decoded, settings);
        assert_eq!(decoded.input_device, "CABLE Output");
        assert_eq!(decoded.last_preset, "flat");
    }
}
