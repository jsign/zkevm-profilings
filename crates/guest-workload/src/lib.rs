#![no_std]

use serde::{Deserialize, Serialize};

const MEMORY_WORDS: usize = 32;

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
pub fn initialize_memory(seed: u64, state: &[u64; 4]) -> [u64; MEMORY_WORDS] {
    let mut memory = [0_u64; MEMORY_WORDS];
    let mut value = seed ^ state[0];
    let mut index = 0;
    while index < memory.len() {
        value = value.wrapping_add(0x9e37_79b9_7f4a_7c15).rotate_left(17) ^ state[index & 3];
        value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        memory[index] = value ^ (value >> 29);
        index += 1;
    }
    memory
}

#[inline(never)]
pub fn arx_permutation(state: &mut [u64; 4], round: u64) {
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
pub fn arithmetic_kernel(state: &mut [u64; 4], round: u32) {
    let round = u64::from(round).wrapping_mul(0xd6e8_feb8_6659_fd93);
    arx_permutation(state, round);
    arx_permutation(state, round ^ 0xa076_1d64_78bd_642f);
}

#[inline(never)]
pub fn scatter_memory(memory: &mut [u64; MEMORY_WORDS], state: &[u64; 4], round: u32) {
    let round = u64::from(round).wrapping_mul(0xe703_7ed1_a0b4_28db);
    let mut step = 0;
    while step < 8 {
        let lane = step & 3;
        let index = (state[lane] ^ round ^ (step as u64).wrapping_mul(0x9e37_79b9)) as usize
            & (MEMORY_WORDS - 1);
        memory[index] = memory[index]
            .wrapping_add(state[(lane + 1) & 3] ^ round)
            .rotate_left(7 + step as u32);
        step += 1;
    }
}

#[inline(never)]
pub fn gather_memory(memory: &[u64; MEMORY_WORDS], state: &[u64; 4], round: u32) -> u64 {
    let mut cursor = (state[0] as usize ^ round as usize) & (MEMORY_WORDS - 1);
    let mut accumulator = state[3] ^ u64::from(round);
    let mut step = 0;
    while step < 8 {
        let word = memory[cursor];
        accumulator = accumulator
            .wrapping_add(word ^ state[step & 3])
            .rotate_left(11)
            .wrapping_mul(0x9ddf_ea08_eb38_2d69);
        cursor = (cursor + (accumulator as usize) + step + 1) & (MEMORY_WORDS - 1);
        step += 1;
    }
    accumulator
}

#[inline(never)]
pub fn memory_kernel(state: &mut [u64; 4], memory: &mut [u64; MEMORY_WORDS], round: u32) {
    scatter_memory(memory, state, round);
    let gathered = gather_memory(memory, state, round);
    let lane = round as usize & 3;
    state[lane] ^= gathered;
    state[(lane + 1) & 3] = state[(lane + 1) & 3].wrapping_add(gathered.rotate_left(23));
}

#[inline(never)]
pub fn branch_kernel(state: &mut [u64; 4], round: u32) {
    let mut value = state[round as usize & 3] ^ u64::from(round);
    let mut step = 0;
    while step < 12 {
        value = match value & 3 {
            0 => value.rotate_left(9).wrapping_add(state[1]),
            1 => value.wrapping_mul(3).wrapping_add(1) ^ state[2],
            2 => (value >> 1).wrapping_add(state[3]).rotate_left(27),
            _ => value.wrapping_sub(state[0]).rotate_right(13),
        };
        value ^= (step as u64).wrapping_mul(0xa076_1d64_78bd_642f);
        step += 1;
    }
    let lane = value as usize & 3;
    state[lane] = state[lane].wrapping_add(value);
    state[(lane + 2) & 3] ^= value.rotate_left(37);
}

#[inline(never)]
pub fn division_kernel(state: &mut [u64; 4], round: u32) {
    let mut value = state[0] ^ state[2].rotate_left(17) ^ u64::from(round);
    let mut step = 0;
    while step < 3 {
        // Keep this kernel on explicit 32-bit operands so it stays visible without
        // letting wide compiler-provided division helpers dominate a guest profile.
        let numerator = value as u32;
        let divisor = ((value >> 32) as u32) | 1;
        let quotient = numerator / divisor;
        let remainder = numerator % divisor;
        value = value.rotate_left(19)
            ^ (u64::from(quotient) << 32)
            ^ u64::from(remainder)
            ^ state[(step + 1) & 3];
        step += 1;
    }
    state[0] = state[0].wrapping_add(value);
    state[2] ^= value.rotate_right(21);
}

#[inline(never)]
pub fn execute_rounds(state: &mut [u64; 4], memory: &mut [u64; MEMORY_WORDS], rounds: u32) {
    let mut round = 0;
    while round < rounds {
        // The kernels have different per-call costs. This cadence gives each one a
        // useful flamegraph width while keeping the selection deterministic.
        match round & 7 {
            0 | 4 | 7 => arithmetic_kernel(state, round),
            1 | 5 => memory_kernel(state, memory, round),
            2 | 6 => branch_kernel(state, round),
            _ => division_kernel(state, round),
        }
        round += 1;
    }
}

#[inline(never)]
pub fn finalize(mut state: [u64; 4], memory: &[u64; MEMORY_WORDS]) -> Output {
    let mut checksum = 0x6a09_e667_f3bc_c909_u64;
    let mut index = 0;
    while index < memory.len() {
        let lane = index & 3;
        let word = memory[index] ^ state[(lane + 1) & 3].rotate_left(11 + lane as u32 * 7);
        state[lane] = state[lane]
            .wrapping_add(word)
            .rotate_left(17)
            .wrapping_mul(0x9ddf_ea08_eb38_2d69);
        checksum = checksum.wrapping_add(state[lane] ^ word).rotate_left(23);
        index += 1;
    }
    Output {
        state: [state[0], state[1], state[2]],
        checksum: checksum ^ state[3],
    }
}

#[inline(never)]
pub fn run(input: Input) -> Output {
    let mut state = initialize(input.seed);
    let mut memory = initialize_memory(input.seed, &state);
    execute_rounds(&mut state, &mut memory, input.rounds);
    finalize(state, &memory)
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
    fn every_kernel_changes_the_result() {
        let mut previous = run(Input {
            seed: 42,
            rounds: 0,
        });
        let mut rounds = 1;
        while rounds <= 8 {
            let output = run(Input { seed: 42, rounds });
            assert_ne!(output, previous);
            previous = output;
            rounds += 1;
        }
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
                    0x6889_4e4e_ff30_6471,
                    0xaf5d_1b0c_c821_738b,
                    0x74bd_957f_d5b0_c39d,
                ],
                checksum: 0x0155_56c9_8610_777b,
            }
        );
    }
}
