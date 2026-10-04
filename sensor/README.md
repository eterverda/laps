# Прошивки сенсоров

Встроенные прошивки для системы хронометража Laps.

## Структура

```
sensor/
├── common/          # Общие утилиты (no_std, пока заготовка)
├── rssi/            # Логика RSSI сенсора (пока заготовка)
│   ├── src/lib.rs
│   └── rp2040/      # Прошивка для RP2040 (bring-up стенда: кнопка + WS2812)
│       ├── .cargo/  # config.toml (runner) и flash.sh (прошивка из BOOTSEL)
│       ├── build.rs # подкладывает memory.x линкеру
│       └── memory.x # карта памяти: boot2 в начале флеша (обязательно!)
└── Cargo.toml       # Workspace
```

## Требования

- [Rust](https://rustup.rs/)
- Таргет `thumbv6m-none-eabi`: `rustup target add thumbv6m-none-eabi`

## Сборка и прошивка RSSI для RP2040

```bash
cd sensor/rssi/rp2040
cargo build --release   # только собрать
cargo run --release     # собрать и прошить
```

Прошивка через runner `sh .cargo/flash.sh` (ждёт плату в BOOTSEL, монтирует
диск `RPI-RP2`, шьёт через `elf2uf2-rs`). Перед прошивкой: зажми **BOOTSEL**,
подключи USB, отпусти BOOTSEL. Запускать из `sensor/rssi/rp2040`.

Требования: `elf2uf2-rs` (`cargo install elf2uf2-rs`), `lsusb`, `lsblk`,
`findmnt`, `udisksctl`. Для доступа к USB без root — udev-правило:

```
SUBSYSTEM=="usb", ATTR{idVendor}=="2e8a", MODE="0660", GROUP="uucp"
```

(в `/etc/udev/rules.d/99-rp2040.rules`, затем `sudo udevadm control --reload`).

### Ручной способ (без runner)

Сборка и конвертация в UF2:

```bash
cd sensor/rssi/rp2040
cargo build --release
elf2uf2-rs \
  ../../target/thumbv6m-none-eabi/release/sensor-rssi-rp2040 \
  firmware.uf2
```

Прошивка:

- Зажми **BOOTSEL**, подключи USB, отпусти BOOTSEL
- Смонтируй диск (`udisksctl mount -b /dev/sdX1` по метке `RPI-RP2`) и скопируй
  `firmware.uf2` на него

## Проверка

Прошивка сэмпла не использует USB: признак успеха — отсутствие устройства
`2e8a:0003` после сброса. Поведение: нажатие кнопки зажигает очередной цвет
(red → blue → green → yellow), отпускание гасит.

## Железо

- Raspberry Pi Pico / Pico W
- USB кабель (с передачей данных)

### Пины (проверено на стенде)

| Сигнал           | Пин  | Примечание                           |
| ---------------- | ---- | ------------------------------------ |
| WS2812 (RGB LED) | GP23 | 1 диод, порядок цветов GRB, PIO0/SM0 |
| Кнопка USR       | GP24 | активный уровень низкий (к GND)      |
