use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    SpotifyDark,
    NcmpcppBlue,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioQuality {
    Kbps128,
    Kbps320,
    Flac,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub theme: Theme,
    pub crossfade_enabled: bool,
    pub crossfade_duration_ms: u64,
    pub default_quality: AudioQuality,
    #[serde(default)]
    pub discord_rpc_enabled: bool,
    #[serde(default)]
    pub arl: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::SpotifyDark,
            crossfade_enabled: false,
            crossfade_duration_ms: 0,
            default_quality: AudioQuality::Kbps320,
            discord_rpc_enabled: false,
            arl: String::new(),
        }
    }
}
