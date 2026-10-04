#![no_std]
#![no_main]

use cortex_m_rt::entry;
use embedded_hal::digital::InputPin;
use panic_halt as _;
use pio::pio_asm;
use rp2040_hal::{
    self as hal,
    clocks::Clock,
    pac,
    pio::{Buffers, PIOBuilder, PIOExt, PinDir, ShiftDirection},
    sio::Sio,
    watchdog::Watchdog,
};

#[unsafe(link_section = ".boot2")]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_GENERIC_03H;

const COLORS: [(u8, u8, u8); 4] = [
    (0xA6, 0x08, 0x08), // Red
    (0x08, 0x10, 0xA6), // Blue
    (0x21, 0x85, 0x21), // Green
    (0xAD, 0x73, 0x00), // Yellow
];

#[entry]
fn main() -> ! {
    let mut pac = pac::Peripherals::take().unwrap();

    let mut watchdog = Watchdog::new(pac.WATCHDOG);
    let sio = Sio::new(pac.SIO);

    let clocks = hal::clocks::init_clocks_and_plls(
        12_000_000u32,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let pins = hal::gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    // WS2812 на GP23 через PIO0/SM0.
    let ws_pin = pins.gpio23.into_function::<hal::gpio::FunctionPio0>();
    let (mut pio, sm0, _, _, _) = pac.PIO0.split(&mut pac.RESETS);
    let program = pio_asm!(
        ".side_set 1"
        "bitloop:"
        "    out x, 1        side 0 [2]"
        "    jmp !x, do_zero side 1 [1]"
        "    jmp bitloop     side 1 [4]"
        "do_zero:"
        "    nop             side 0 [4]"
    );
    let installed = pio.install(&program.program).unwrap();

    // SM clock = 800 kHz бит * 10 циклов = 8 МГц
    let periph_hz = clocks.peripheral_clock.freq().to_Hz();
    let bit_hz = 800_000u32 * 10;
    let int = (periph_hz / bit_hz) as u16;
    let frac = (((periph_hz % bit_hz) * 256) / bit_hz) as u8;

    let (mut sm, _, mut tx) = PIOBuilder::from_installed_program(installed)
        .buffers(Buffers::OnlyTx)
        .side_set_pin_base(ws_pin.id().num)
        .out_shift_direction(ShiftDirection::Left)
        .autopull(true)
        .pull_threshold(24)
        .clock_divisor_fixed_point(int, frac)
        .build(sm0);
    sm.set_pindirs([(ws_pin.id().num, PinDir::Output)]);
    sm.start();

    // Кнопка USR на GP24, активный уровень низкий.
    let mut button = pins.gpio24.into_pull_up_input();

    let mut color_idx = 0usize;
    let mut pressed = false;

    loop {
        if !pressed {
            if button.is_low().unwrap_or(false) {
                pressed = true;
                write_color(&mut tx, COLORS[color_idx]);
                color_idx = (color_idx + 1) % COLORS.len();
            }
        } else if button.is_high().unwrap_or(true) {
            pressed = false;
            write_color(&mut tx, (0, 0, 0));
        }
    }
}

/// GRB, старшим битом вперёд.
fn write_color(tx: &mut hal::pio::Tx<(pac::PIO0, hal::pio::SM0)>, (r, g, b): (u8, u8, u8)) {
    let word = (u32::from(g) << 24) | (u32::from(r) << 16) | (u32::from(b) << 8);
    while !tx.write(word) {
        cortex_m::asm::nop();
    }
}
