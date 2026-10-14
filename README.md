# ticros

A **Victoria-like HDD/SSD diagnostic tool** written in Rust.

`ticros` talks to storage devices via **ATA PASS-THROUGH (16)** over `SG_IO`.
It supports **S.M.A.R.T.**, **surface scanning** with configurable thresholds,
**remapping of bad sectors**, and falls back to `smartctl` when the kernel
blocks ATA PASS-THROUGH on certain controllers.

## Features

- **ATA IDENTIFY DEVICE** — model, serial, firmware, capacity, form factor, interface, NCQ, TRIM.
- **NVMe Identify Controller** — for `/dev/nvme*` devices.
- **S.M.A.R.T.** — direct via SG_IO, fallback to `smartctl -a -j`.
- **Surface scan** with `O_DIRECT` — Victoria thresholds, DOS-style map, CSV log.
- **Remap** of bad sectors via `SET FEATURES / REASSIGN_SECTORS`.
- **JSON reports** — `--report <FILE>`.
- **Pure Rust**, no runtime deps.

## Installation

    git clone https://github.com/tdogdanov/ticros.git
    cd ticros
    cargo build --release

The binary is at `target/release/ticros`.

### Requirements

- Linux kernel with `SG_IO` support.
- `smartmontools` (optional, for SMART fallback).
- **root** (`sudo`) — required for `SG_IO` and `O_DIRECT`.

## Usage

    ticros [OPTIONS] --device <DEVICE> <COMMAND>

### Commands

| Command | Description |
|---|---|
| `info` | IDENTIFY DEVICE (ATA) or Identify Controller (NVMe) |
| `smart [--full]` | S.M.A.R.T. attributes |
| `scan` | Surface scan |
| `remap` | Remap a single LBA (⚠ irreversible) |
| `remap-log` | Remap all bad LBAs from a scan log (⚠ irreversible) |
| `presets` | List threshold presets |

### Examples

    sudo ./target/release/ticros --device /dev/sda info
    sudo ./target/release/ticros --device /dev/sda smart --full
    sudo ./target/release/ticros --device /dev/sda scan --end-mb 100 --map
    sudo ./target/release/ticros --device /dev/sda smart --report smart.json

## Thresholds

| Latency (ms) | Class | Meaning |
|---|---|---|
| `< fast_ms` | Grey | Excellent |
| `< good_ms` | Green | Good |
| `< slow_ms` | Yellow | Slow |
| `< bad_ms` | Red | Bad (readable) |
| `>= bad_ms` or I/O error | Bad | Unreadable |

### Presets

| Name | fast | good | slow | bad |
|---|---|---|---|---|
| victoria (default) | 10 ms | 50 ms | 200 ms | 1000 ms |
| mhdd | 5 ms | 20 ms | 100 ms | 500 ms |
| fast | 20 ms | 100 ms | 500 ms | 2000 ms |
| strict | 5 ms | 15 ms | 50 ms | 200 ms |

## Supported devices

| Category | Status |
|---|---|
| SATA HDD / SSD | ✅ full |
| M.2 SATA SSD | ✅ full |
| NVMe M.2 SSD | ✅ info, smart |
| USB-attached | ⚠ best-effort |
| PATA (IDE) | ⚠ partial |

Tested with:
- ADATA SU650 (SATA SSD)
- WD Green M.2 SATA 120 GB / 240 GB
- WDC WD3200BEVT (SATA HDD, 5400 RPM)

## Testing

    cargo test --release    # 60 tests, all passing
    cargo clippy --release -- -D warnings    # 0 warnings
    cargo fmt --check    # clean

## License

MIT — see [LICENSE](LICENSE).

## Acknowledgements

Inspired by [Victoria](https://hdd.by/) by Dmitry Postnikov (XoT) and
[smartmontools](https://www.smartmontools.org/).