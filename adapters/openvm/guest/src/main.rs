#![no_main]
#![no_std]

use guest_workload::Input;
use openvm::io::{read, reveal_u64};

openvm::entry!(main);

pub fn main() {
    let input = read::<Input>();
    let output = guest_workload::run(input);
    let values = [
        output.state[0],
        output.state[1],
        output.state[2],
        output.checksum,
    ];
    for (value_index, value) in values.into_iter().enumerate() {
        reveal_u64(value, value_index);
    }
}
