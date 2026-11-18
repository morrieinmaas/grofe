//! The options forwarding follows, from this machine's config.

/// `remote-upload-paste`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadMode {
    Ask,
    On,
    Off,
}

impl UploadMode {
    /// The option's value; `None` for another word.
    pub fn parse(s: &str) -> Option<UploadMode> {
        match s {
            "ask" => Some(UploadMode::Ask),
            "on" => Some(UploadMode::On),
            "off" => Some(UploadMode::Off),
            _ => None,
        }
    }
}

/// `remote-forward-ports`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardMode {
    Ask,
    Auto,
    Off,
}

impl ForwardMode {
    /// The option's value; `None` for another word.
    pub fn parse(s: &str) -> Option<ForwardMode> {
        match s {
            "ask" => Some(ForwardMode::Ask),
            "auto" => Some(ForwardMode::Auto),
            "off" => Some(ForwardMode::Off),
            _ => None,
        }
    }
}

/// The options forwarding follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub upload_paste: UploadMode,
    /// `remote-upload-max`, in bytes.
    pub upload_max: u64,
    pub forward_ports: ForwardMode,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            upload_paste: UploadMode::Ask,
            upload_max: 100 << 20,
            forward_ports: ForwardMode::Ask,
        }
    }
}

impl Settings {
    /// The settings `get` gives the options as: this machine's config, so a
    /// remote cannot grant itself uploads or local ports. A default for one
    /// it does not give or that does not parse.
    pub fn from_options(get: impl Fn(&str) -> Option<String>) -> Settings {
        let default = Settings::default();
        Settings {
            upload_paste: get("remote-upload-paste")
                .and_then(|v| UploadMode::parse(v.trim()))
                .unwrap_or(default.upload_paste),
            upload_max: get("remote-upload-max")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map_or(default.upload_max, |mib| mib << 20),
            forward_ports: get("remote-forward-ports")
                .and_then(|v| ForwardMode::parse(v.trim()))
                .unwrap_or(default.forward_ports),
        }
    }
}
