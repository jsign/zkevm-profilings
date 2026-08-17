#![no_main]

use guest_workload::Input;

ziskos::entrypoint!(main);

fn main() {
    let input = ziskos::io::read::<Input>();
    let output = guest_workload::run(input);
    ziskos::io::commit(&output);
}
