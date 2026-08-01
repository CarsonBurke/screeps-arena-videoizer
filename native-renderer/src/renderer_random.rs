use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RendererRandom {
    state: u32,
}

impl RendererRandom {
    pub const fn new(state: u32) -> Self {
        Self { state }
    }

    pub const fn state(self) -> u32 {
        self.state
    }

    pub fn next_f64(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6d2b_79f5);
        let mut value = (self.state ^ (self.state >> 15)).wrapping_mul(1 | self.state);
        value ^= value.wrapping_add((value ^ (value >> 7)).wrapping_mul(61 | value));
        f64::from(value ^ (value >> 14)) / 4_294_967_296.0
    }

    pub fn from_replay_state(state: Option<u32>) -> Result<Self> {
        state
            .map(Self::new)
            .ok_or_else(|| Error::Invalid("ReplayIR lacks the renderer PRNG checkpoint".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::RendererRandom;

    #[test]
    fn matches_the_javascript_mulberry32_renderer_stream() {
        let mut random = RendererRandom::new(123);
        let expected = [
            0.7872516233474016,
            0.1785435655619949,
            0.49531551403924823,
            0.23136196262203157,
        ];
        for expected in expected {
            assert_eq!(random.next_f64(), expected);
        }
        assert_eq!(random.state(), 3_031_296_079);
    }
}
