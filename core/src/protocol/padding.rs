use rand::{
    Rng,
    distr::{Alphanumeric, SampleString},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaddingRange {
    pub min: usize,
    pub max: usize,
}

impl PaddingRange {
    pub const fn new(min: usize, max: usize) -> Self {
        Self { min, max }
    }

    pub fn sample(self) -> String {
        let mut rng = rand::rng();
        let len = rng.random_range(self.min..self.max);
        Alphanumeric.sample_string(&mut rng, len)
    }
}

pub const AUTH_REQUEST_PADDING: PaddingRange = PaddingRange::new(256, 2048);
pub const AUTH_RESPONSE_PADDING: PaddingRange = PaddingRange::new(256, 2048);
pub const TCP_REQUEST_PADDING: PaddingRange = PaddingRange::new(64, 512);
pub const TCP_RESPONSE_PADDING: PaddingRange = PaddingRange::new(128, 1024);

pub fn auth_request_padding() -> String {
    AUTH_REQUEST_PADDING.sample()
}

pub fn auth_response_padding() -> String {
    AUTH_RESPONSE_PADDING.sample()
}

pub fn tcp_request_padding() -> String {
    TCP_REQUEST_PADDING.sample()
}

pub fn tcp_response_padding() -> String {
    TCP_RESPONSE_PADDING.sample()
}
