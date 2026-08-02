//! The firmware's xorshift96 generator, ported verbatim from `SOUND.ino`.
//!
//! Used for the "random shift" setting, which flips the sign of the grain shift on roughly half of
//! all control ticks. Keeping the exact generator (and its exact seed) keeps the character of that
//! randomness identical to the hardware.

/// xorshift96 with the firmware's seed constants.
#[derive(Clone, Debug)]
pub struct Xorshift96 {
    x: u32,
    y: u32,
    z: u32,
}

impl Default for Xorshift96 {
    fn default() -> Self {
        // Note the transposed digits in `x`; this is what the firmware ships.
        Self {
            x: 132_456_789,
            y: 362_436_069,
            z: 521_288_629,
        }
    }
}

impl Xorshift96 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next_u32(&mut self) -> u32 {
        self.x ^= self.x << 16;
        self.x ^= self.x >> 5;
        self.x ^= self.x << 1;

        let t = self.x;
        self.x = self.y;
        self.y = self.z;
        self.z = t ^ self.x ^ self.y;

        self.z
    }

    /// `rand(maxval)` from the firmware: `((next() & 0xFFFF) * maxval) >> 16`.
    pub fn rand(&mut self, maxval: u32) -> u32 {
        ((self.next_u32() & 0xFFFF) * maxval) >> 16
    }

    /// The coin flip used by the random shift setting.
    pub fn coin_flip(&mut self) -> bool {
        self.rand(2) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rand_stays_in_range() {
        let mut rng = Xorshift96::new();
        for _ in 0..10_000 {
            assert!(rng.rand(2) < 2);
        }
    }

    #[test]
    fn coin_flip_is_roughly_fair() {
        let mut rng = Xorshift96::new();
        let heads = (0..10_000).filter(|_| rng.coin_flip()).count();
        assert!((4_500..5_500).contains(&heads), "heads = {heads}");
    }

    #[test]
    fn is_deterministic_from_the_firmware_seed() {
        let mut a = Xorshift96::new();
        let mut b = Xorshift96::new();
        for _ in 0..100 {
            assert_eq!(a.next_u32(), b.next_u32());
        }
    }
}
