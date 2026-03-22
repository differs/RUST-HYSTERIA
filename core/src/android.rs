use std::{
    os::fd::RawFd,
    sync::{Arc, OnceLock},
};

type SocketProtector = Arc<dyn Fn(RawFd) + Send + Sync + 'static>;

static SOCKET_PROTECTOR: OnceLock<SocketProtector> = OnceLock::new();

pub fn set_socket_protector(protector: SocketProtector) -> Result<(), SocketProtector> {
    SOCKET_PROTECTOR.set(protector)
}

pub(crate) fn maybe_protect_socket(fd: RawFd) {
    if let Some(protector) = SOCKET_PROTECTOR.get() {
        protector(fd);
    }
}
