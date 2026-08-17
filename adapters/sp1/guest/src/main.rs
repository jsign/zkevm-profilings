#![no_main]

use guest_workload::Input;

sp1_zkvm::entrypoint!(main);

pub fn main() {
    let input = sp1_zkvm::io::read::<Input>();
    let output = guest_workload::run(input);
    sp1_zkvm::io::commit(&output);
}
