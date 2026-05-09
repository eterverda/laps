# Laps Host Agent Instructions

## Build

Always use `make` for building:

```bash
make build
```

This produces the binary at `build/bin/laps`.

## Other make targets

- `make install-symlink` — create `build/bin/laps-cli` symlink
- `make app-bundle` — build macOS `.app` bundle at `build/Laps.app`
- `make clean` — remove all build artifacts
- `make all` — alias for `make build install-symlink`
