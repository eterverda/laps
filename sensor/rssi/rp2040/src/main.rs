#![no_std]
#![no_main]

use cortex_m_rt::entry;
use panic_halt as _;

#[unsafe(link_section = ".boot2")]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_GENERIC_03H;

#[entry]
fn main() -> ! {
    loop {}
}
