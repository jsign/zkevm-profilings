#![no_std]

use serde::{Deserialize, Serialize};

/// The sample workload uses this VM-independent input.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Input {
    pub seed: u64,
    pub rounds: u32,
}

/// Each adapter produces this public result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Output {
    pub state: [u64; 3],
    pub checksum: u64,
}

#[inline(never)]
pub fn initialize(seed: u64) -> [u64; 4] {
    let mut value = seed ^ 0x9e37_79b9_7f4a_7c15;
    let mut state = [0_u64; 4];
    let mut index = 0;
    while index < state.len() {
        value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = value;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        state[index] = mixed ^ (mixed >> 31);
        index += 1;
    }
    state
}

#[inline(never)]
pub fn mix(state: &mut [u64; 4], round: u32) {
    let round = u64::from(round).wrapping_mul(0xd6e8_feb8_6659_fd93);
    let a = state[0].wrapping_add(state[3]).rotate_left(17) ^ round;
    let b = state[1].wrapping_add(a).rotate_left(31) ^ state[0];
    let c = state[2].wrapping_add(b).rotate_left(43) ^ state[1];
    let d = state[3].wrapping_add(c).rotate_left(29) ^ state[2];
    state[0] = a.wrapping_mul(0xa24b_aed4_963e_e407);
    state[1] = b.wrapping_mul(0x9fb2_1c65_1e98_df25);
    state[2] = c.wrapping_mul(0xc13f_a9a9_02a6_328f);
    state[3] = d.wrapping_mul(0x91e1_0da5_c79e_7b1d);
}

#[inline(never)]
pub fn finalize(mut state: [u64; 4]) -> Output {
    let mut checksum = 0x6a09_e667_f3bc_c909_u64;
    let mut index = 0;
    while index < state.len() {
        state[index] ^= state[(index + 1) & 3].rotate_left((11 + index * 7) as u32);
        checksum = checksum
            .wrapping_add(state[index])
            .rotate_left(23)
            .wrapping_mul(0x9ddf_ea08_eb38_2d69);
        index += 1;
    }
    Output {
        state: [state[0], state[1], state[2]],
        checksum,
    }
}

#[inline(never)]
pub fn run(input: Input) -> Output {
    let mut state = initialize(input.seed);
    let mut round = 0;
    while round < input.rounds {
        mix(&mut state, round);
        round += 1;
    }
    finalize(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_is_deterministic() {
        let input = Input {
            seed: 42,
            rounds: 100,
        };
        assert_eq!(run(input), run(input));
    }

    #[test]
    fn rounds_change_the_result() {
        let one = run(Input {
            seed: 42,
            rounds: 1,
        });
        let two = run(Input {
            seed: 42,
            rounds: 2,
        });
        assert_ne!(one, two);
    }

    #[test]
    fn fixed_vector() {
        let output = run(Input {
            seed: 0x1234_5678_9abc_def0,
            rounds: 100,
        });
        assert_eq!(
            output,
            Output {
                state: [
                    0x2e4d_f266_4b93_7edd,
                    0xa8aa_4b8c_bbb4_3d3d,
                    0x7858_61cd_5932_fb38,
                ],
                checksum: 0xfbc1_e4f9_a377_9989,
            }
        );
    }
}
