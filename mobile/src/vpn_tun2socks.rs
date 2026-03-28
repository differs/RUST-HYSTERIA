use anyhow::Result;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tun2SocksUdpMode {
    Disabled,
    Tcp,
    Udp,
}

#[derive(Clone, Debug)]
pub struct Tun2SocksConfig {
    pub socks_host: String,
    pub socks_port: u16,
    pub udp_mode: Tun2SocksUdpMode,
    pub tunnel_name: String,
    pub mtu: u16,
    pub ipv4_addr: String,
    pub ipv6_addr: Option<String>,
    pub connect_timeout_ms: Option<u32>,
}

impl Tun2SocksConfig {
    pub fn render(&self) -> String {
        let udp_mode = match self.udp_mode {
            Tun2SocksUdpMode::Disabled => "off",
            Tun2SocksUdpMode::Tcp => "tcp",
            Tun2SocksUdpMode::Udp => "udp",
        };
        let ipv6 = self
            .ipv6_addr
            .as_deref()
            .map(|addr| format!("\n  ipv6: '{addr}'"))
            .unwrap_or_default();
        let connect_timeout = self
            .connect_timeout_ms
            .map(|value| format!("  connect-timeout: {value}\n"))
            .unwrap_or_default();

        format!(
            "tunnel:\n  name: {}\n  mtu: {}\n  multi-queue: false\n  ipv4: {}{}\n\nsocks5:\n  address: {}\n  port: {}\n  udp: '{}'\nmisc:\n{}  log-file: stderr\n  log-level: info\n",
            self.tunnel_name,
            self.mtu,
            self.ipv4_addr,
            ipv6,
            self.socks_host,
            self.socks_port,
            udp_mode,
            connect_timeout,
        )
    }
}

#[cfg(target_os = "android")]
mod imp {
    use super::Tun2SocksConfig;
    use anyhow::{Context, Result, anyhow, bail};
    use std::{
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        thread::{self, JoinHandle},
    };

    pub fn spawn(config: Tun2SocksConfig, tun_fd: i32) -> Result<JoinHandle<()>> {
        if tun_fd < 0 {
            bail!("Android VpnService did not provide a valid TUN fd");
        }

        let yaml = config.render();
        Ok(thread::Builder::new()
            .name("hy-mobile-tun2socks".to_string())
            .spawn(move || {
                let tun_fd = unsafe { OwnedFd::from_raw_fd(tun_fd) };
                let raw = tun_fd.as_raw_fd();
                if let Err(code) = tun2socks::main_from_str(&yaml, raw) {
                    eprintln!("tun2socks exited with error code {code}");
                }
                drop(tun_fd);
            })
            .context("failed to spawn tun2socks thread")?)
    }

    pub fn stop(handle: JoinHandle<()>) -> Result<()> {
        tun2socks::quit();
        handle
            .join()
            .map_err(|_| anyhow!("tun2socks thread panicked"))?;
        Ok(())
    }
}

#[cfg(not(target_os = "android"))]
mod imp {
    use super::Tun2SocksConfig;
    use anyhow::{Result, bail};
    use std::thread::JoinHandle;

    pub fn spawn(_config: Tun2SocksConfig, _tun_fd: i32) -> Result<JoinHandle<()>> {
        bail!("tun2socks is only available on Android builds")
    }

    pub fn stop(_handle: JoinHandle<()>) -> Result<()> {
        Ok(())
    }
}

pub type Tun2SocksHandle = std::thread::JoinHandle<()>;

pub fn spawn(config: Tun2SocksConfig, tun_fd: i32) -> Result<Tun2SocksHandle> {
    imp::spawn(config, tun_fd)
}

pub fn stop(handle: Tun2SocksHandle) -> Result<()> {
    imp::stop(handle)
}
