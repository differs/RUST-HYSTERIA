use std::{fmt, path::PathBuf, time::Duration};

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

const APP_DESC: &str = "a powerful, lightning fast and censorship resistant proxy";
const APP_AUTHORS: &str = "Aperture Internet Laboratory <https://github.com/apernet>";
const APP_LOGO: &str = r#"
░█░█░█░█░█▀▀░▀█▀░█▀▀░█▀▄░▀█▀░█▀█░░░▀▀▄
░█▀█░░█░░▀▀█░░█░░█▀▀░█▀▄░░█░░█▀█░░░▄▀░
░▀░▀░░▀░░▀▀▀░░▀░░▀▀▀░▀░▀░▀▀▀░▀░▀░░░▀▀▀
"#;

#[derive(Debug, Clone)]
pub struct AppMetadata {
    pub version: String,
    pub build_date: String,
    pub build_type: String,
    pub toolchain: String,
    pub commit: String,
    pub platform: String,
    pub arch: String,
    pub library_versions: String,
}

impl AppMetadata {
    pub fn current() -> Self {
        Self {
            version: option_env!("HYSTERIA_RS_VERSION")
                .unwrap_or(env!("CARGO_PKG_VERSION"))
                .to_string(),
            build_date: option_env!("HYSTERIA_RS_BUILD_DATE")
                .unwrap_or("Unknown")
                .to_string(),
            build_type: option_env!("HYSTERIA_RS_BUILD_TYPE")
                .unwrap_or("Unknown")
                .to_string(),
            toolchain: option_env!("HYSTERIA_RS_TOOLCHAIN")
                .unwrap_or("Unknown")
                .to_string(),
            commit: option_env!("HYSTERIA_RS_COMMIT")
                .unwrap_or("Unknown")
                .to_string(),
            platform: option_env!("HYSTERIA_RS_PLATFORM")
                .unwrap_or(std::env::consts::OS)
                .to_string(),
            arch: option_env!("HYSTERIA_RS_ARCH")
                .unwrap_or(std::env::consts::ARCH)
                .to_string(),
            library_versions: option_env!("HYSTERIA_RS_LIBS")
                .unwrap_or("core=partial,extras=partial")
                .to_string(),
        }
    }

    pub fn about_long(&self) -> String {
        format!(
            "{APP_LOGO}\n{APP_DESC}\n{APP_AUTHORS}\n\nVersion:\t{}\nBuildDate:\t{}\nBuildType:\t{}\nToolchain:\t{}\nCommitHash:\t{}\nPlatform:\t{}\nArchitecture:\t{}\nLibraries:\t{}",
            self.version,
            self.build_date,
            self.build_type,
            self.toolchain,
            self.commit,
            self.platform,
            self.arch,
            self.library_versions,
        )
    }
}

#[derive(Debug, Clone, Parser)]
#[command(name = "hysteria", author = APP_AUTHORS, about = APP_DESC)]
pub struct Cli {
    #[arg(short = 'c', long = "config", global = true)]
    pub config: Option<PathBuf>,

    #[arg(
        short = 'l',
        long = "log-level",
        global = true,
        env = "HYSTERIA_LOG_LEVEL",
        default_value = "info"
    )]
    pub log_level: LogLevel,

    #[arg(
        short = 'f',
        long = "log-format",
        global = true,
        env = "HYSTERIA_LOG_FORMAT",
        default_value = "console"
    )]
    pub log_format: LogFormat,

    #[arg(long = "disable-update-check", global = true, env = "HYSTERIA_DISABLE_UPDATE_CHECK", action = ArgAction::SetTrue)]
    pub disable_update_check: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    Client(ClientArgs),
    Server(ServerArgs),
    Version,
    Ping(PingArgs),
    Share(ShareArgs),
    Speedtest(SpeedtestArgs),
    #[command(alias = "check-update")]
    Update,
}

#[derive(Debug, Clone, Args, Default)]
pub struct ClientArgs {
    #[arg(long)]
    pub qr: bool,
}

#[derive(Debug, Clone, Args, Default)]
pub struct ServerArgs {}

#[derive(Debug, Clone, Args)]
pub struct PingArgs {
    pub address: String,
    #[arg(long = "count", default_value_t = 4)]
    pub count: u32,
    #[arg(long = "interval", default_value = "1s", value_parser = parse_duration)]
    pub interval: Duration,
}

#[derive(Debug, Clone, Args, Default)]
pub struct ShareArgs {
    #[arg(long = "notext")]
    pub no_text: bool,
    #[arg(long)]
    pub qr: bool,
}

#[derive(Debug, Clone, Args)]
pub struct SpeedtestArgs {
    #[arg(long = "skip-download")]
    pub skip_download: bool,
    #[arg(long = "skip-upload")]
    pub skip_upload: bool,
    #[arg(long = "duration", default_value = "10s", value_parser = parse_duration)]
    pub duration: Duration,
    #[arg(long = "data-size")]
    pub data_size: Option<u32>,
    #[arg(long = "use-bytes")]
    pub use_bytes: bool,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, ValueEnum)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, ValueEnum)]
pub enum LogFormat {
    Console,
    Json,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Debug => "debug",
                Self::Info => "info",
                Self::Warn => "warn",
                Self::Error => "error",
            }
        )
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Console => "console",
                Self::Json => "json",
            }
        )
    }
}

fn parse_duration(input: &str) -> Result<Duration, String> {
    humantime::parse_duration(input).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_update_alias() {
        let cli = Cli::parse_from(["hysteria", "check-update"]);
        assert!(matches!(cli.command, Some(Command::Update)));
    }

    #[test]
    fn defaults_to_no_subcommand() {
        let cli = Cli::parse_from(["hysteria"]);
        assert!(cli.command.is_none());
    }
}
