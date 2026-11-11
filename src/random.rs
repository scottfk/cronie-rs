//! glibc's `random()` and `srandom()` (the default TYPE_3 additive feedback
//! generator, 31 words of state).
//!
//! cronie draws from it for `~` ranges in crontabs, `RANDOM_DELAY`, and
//! the daemon's random scale factor. `cronnext` never seeds it, so it gets
//! glibc's built-in sequence for seed 1. The port has to produce the same
//! numbers.

use std::cell::RefCell;

#[derive(Debug, Clone)]
struct State {
    r: [i32; 31],
    /// Index of the front pointer (`fptr`); the rear pointer is 3 behind,
    /// as in glibc's SEP_3.
    f: usize,
}

impl State {
    /// glibc `__srandom_r`.
    fn seeded(seed: u32) -> State {
        let mut r = [0i32; 31];
        let mut word: i32 = if seed == 0 { 1 } else { seed as i32 };
        r[0] = word;
        for item in r.iter_mut().skip(1) {
            // word = 16807 * word % 2147483647, without overflow (Schrage).
            let hi = word / 127_773;
            let lo = word % 127_773;
            word = 16807 * lo - 2836 * hi;
            if word < 0 {
                word += 2_147_483_647;
            }
            *item = word;
        }
        let mut s = State { r, f: 3 };
        for _ in 0..310 {
            s.next();
        }
        s
    }

    /// glibc `__random_r`.
    fn next(&mut self) -> i32 {
        let rear = (self.f + 31 - 3) % 31;
        let val = self.r[self.f].wrapping_add(self.r[rear]);
        self.r[self.f] = val;
        self.f = (self.f + 1) % 31;
        ((val as u32) >> 1) as i32
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::seeded(1));
}

/// `random()`.
pub fn random() -> i64 {
    i64::from(STATE.with(|s| s.borrow_mut().next()))
}

/// `srandom(seed)`.
pub fn srandom(seed: u32) {
    STATE.with(|s| *s.borrow_mut() = State::seeded(seed));
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" {
        fn random() -> libc::c_long;
        fn srandom(seed: libc::c_uint);
    }

    #[test]
    fn matches_glibc() {
        for seed in [1u32, 0, 42, 0xdead_beef] {
            // SAFETY: plain libc calls; this test owns the glibc state.
            unsafe { srandom(seed) };
            let mut ours = State::seeded(seed);
            for _ in 0..2000 {
                // SAFETY: as above.
                let theirs = unsafe { random() } as i32;
                assert_eq!(ours.next(), theirs, "seed {seed}");
            }
        }
    }
}
