#!/usr/bin/env bash
# Runner для cargo run: прошивает RP2040, который в BOOTSEL.
# Плата должна быть в режиме прошивки: зажать BOOTSEL -> подключить USB -> отпустить.
# Linux: ждём плату и монтируем диск сами. Прочие системы: только вызов elf2uf2-rs.
set -euo pipefail

ELF="$1"

wait_for() { # <описание> <команда-проверка...>
    local what="$1"; shift
    echo "Waiting for $what..." >&2
    for _ in $(seq 1 150); do
        if "$@" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    echo "Timed out waiting for $what" >&2
    return 1
}

if [ "$(uname -s)" = "Linux" ]; then
    # 1. Плата в BOOTSEL (до ~30 с).
    wait_for "RP2040 in BOOTSEL [Reboot while holding BOOT]" sh -c "lsusb | grep -q '2e8a:0003'"

    # 2. Блочное устройство с меткой RPI-RP2 (появляется чуть позже USB).
    wait_for "RPI-RP2 disk" sh -c "lsblk -ln -o LABEL | grep -qx 'RPI-RP2'"

    dev=$(lsblk -ln -o PATH,LABEL | awk '$2 == "RPI-RP2" { print $1; exit }')

    # 3. Монтируем, если ещё не смонтирован.
    if ! findmnt -rn --source "$dev" >/dev/null 2>&1; then
        echo "Mounting $dev" >&2
        udisksctl mount -b "$dev" >/dev/null
    fi
fi

echo "Flashing $ELF" >&2
elf2uf2-rs -d "$ELF"

if [ "$(uname -s)" = "Linux" ]; then
    # Отчёт: плата должна уйти из BOOTSEL в приложение.
    size=$(stat -c %s "$ELF")
    echo "Flashed $ELF ($size bytes)" >&2
    if wait_for "board to reboot into application" sh -c "! lsusb | grep -q '2e8a:0003'"; then
        echo "Done. Board rebooted into application." >&2
    else
        echo "Flashing finished, but the board is still in BOOTSEL." >&2
    fi
fi
