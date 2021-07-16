#![no_std]
#![feature(const_generics)]

#![cfg_attr(test, no_main)]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "test_main"]
#![test_runner(nanos_sdk::sdk_test_runner)]

#[cfg(all(not(target_os = "linux"), test))]
use nanos_sdk::exit_app;
#[cfg(all(not(target_os = "linux"), test))]
#[no_mangle]
extern "C" fn sample_main() {
    use nanos_sdk::exit_app;
    test_main();
    exit_app(0);
}

#[cfg(all(not(target_os = "linux"), test))]
use core::panic::PanicInfo;
#[cfg(all(not(target_os = "linux"), test))]
#[panic_handler]
fn handle_panic(_: &PanicInfo) -> ! {
    exit_app(0)
}


pub mod core_parsers;

pub mod forward_parser;

pub mod endianness;
