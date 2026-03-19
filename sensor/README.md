# Прошивки сенсоров

Встроенные прошивки для системы хронометража Laps.

## Структура

```
sensor/
├── common/          # Общие утилиты (no_std)
├── rssi/            # Логика RSSI сенсора
│   ├── src/lib.rs   # Реализация сенсора
│   └── rp2040/      # Прошивка для RP2040
└── Cargo.toml       # Workspace
```

## Требования

- [Rust](https://rustup.rs/)
- Таргет `thumbv6m-none-eabi`: `rustup target add thumbv6m-none-eabi`

## Сборка и прошивка RSSI для RP2040

### Автоматический способ

```bash
cd sensor/rssi/rp2040

# Подключи Pico в режиме BOOTSEL (зажми BOOTSEL, воткни USB, отпусти)
cargo run --release
```

### Ручной способ

Сборка:
```bash
cd sensor/rssi/rp2040
cargo build --release
```

Конвертация в UF2:
```bash
cargo install elf2uf2-rs
elf2uf2-rs \
  target/thumbv6m-none-eabi/release/sensor-rssi-rp2040 \
  firmware.uf2
```

Прошивка:
- Зажми **BOOTSEL**, подключи USB, отпусти BOOTSEL
- Скопируй `firmware.uf2` на диск `RPI-RP2`

## Проверка

```bash
# Linux
ls /dev/ttyACM*

# macOS
ls /dev/tty.usbmodem*
```

## Железо

- Raspberry Pi Pico / Pico W
- USB кабель (с передачей данных)
