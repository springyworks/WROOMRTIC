# WROOMRTIC

Bare-metal RTIC v2 demo for **ESP32-WROOM-32** (Xtensa, no_std).

## What it does

1. **WiFi AP** — broadcasts SSID `WROOMRTIC` (open, channel 1), serves an HTTP status page on `http://192.168.4.1/`
2. **Morse heartbeat** — blinks a two-character status code on the blue LED (GPIO2) every ~5 seconds
3. **DAC→ADC loopback** — ramps DAC1 (GPIO25) through 11 voltage steps, reads each with ADC1 (GPIO34), prints results
4. **BOOT button** — hardware GPIO interrupt prints on press

WiFi polling is interleaved with the heartbeat loop (every ~10ms), so both run cooperatively in idle.

## Pin assignments

| Function | GPIO | Notes |
|----------|------|-------|
| Blue LED (Morse) | **GPIO2** | Onboard, single color |
| DAC output | **GPIO25** | DAC1, 8-bit (0–255 → 0–3.3V) |
| ADC input | **GPIO34** | ADC1_CH6, 12-bit, 11dB (~0–2450mV), input-only |
| BOOT button | **GPIO0** | Active low, external pull-up |

**Loopback test:** wire **GPIO25 → GPIO34** with a jumper. ADC saturates above DAC≈189 (~2450mV).

## Two-char Morse heartbeat status codes

| Code | Morse | Meaning |
|------|-------|---------|
| `OK` | `--- -.-` | System running normally |
| `HI` | `.... ..` | Hello / initializing |
| `ER` | `. .-.` | Error detected |
| `GO` | `--. ---` | Ready to go |
| `LO` | `.-.. ---` | Low resource / warning |
| `UP` | `..- .--.` | Up / recovered |
| `RX` | `.-. -..-` | Receiving data |
| `TX` | `- -..-` | Transmitting data |
| `AD` | `.- -..` | ADC active |
| `NO` | `-. ---` | Fault / stopped |

Morse Farnsworth timing: dit=150ms, dah=600ms, element gap=150ms, char gap=300ms.

Change the status code in `src/main.rs` at `let status: &[u8; 2] = b"OK";`.

## Build & flash

```bash
# Build
export PATH="$HOME/.espressif/tools/xtensa-esp-elf/esp-14.2.0_20241119/xtensa-esp-elf/bin:$PATH"
cargo build --release --features xtensa-esp32-backend

# Flash + monitor
espflash flash --monitor --chip esp32 target/xtensa-esp32-none-elf/release/wroomrtic
```

## Serial monitor

### Do we have one?

Yes — `espflash flash --monitor` enters serial monitor mode after flashing. The WROOM's CP2102/CP210x USB-UART bridge appears as `/dev/ttyUSB0` (115200 baud). The firmware uses `esp-println` which writes to UART0 — the same UART connected to USB.

## WiFi AP

The firmware starts a WiFi access point on boot:

| Setting | Value |
|---------|-------|
| SSID | **WROOMRTIC** |
| Security | Open (no password) |
| Channel | 1 |
| IP address | **192.168.4.1** |
| HTTP | Port 80 — status page |

### Connecting from a phone

1. Open WiFi settings, connect to **WROOMRTIC**
2. Set a **static IP** (no DHCP server):
   - IP: `192.168.4.2`
   - Subnet: `255.255.255.0`
   - Gateway: `192.168.4.1`
3. Open a browser to `http://192.168.4.1/`

### Connecting via adb

```bash
# Connect phone to WROOMRTIC WiFi (manual step on phone)
# Then from PC:
adb shell curl http://192.168.4.1/
```

### Architecture notes

- WiFi uses **esp-wifi 0.15.1** with the built-in preemptive scheduler (TIMG0 timer)
- TCP/IP stack: **smoltcp 0.12** (no_std, polled from idle loop)
- No DHCP server — clients must set a static IP in 192.168.4.0/24
- The `EspWifiController` and `WifiController` are stored in `StaticCell`s for `'static` lifetime
- During RTIC `init`, PS.INTLEVEL is temporarily lowered to 0 for esp-wifi (which requires interrupts enabled), then restored before RTIC post-init

### Standalone monitor (no re-flash)

```bash
# espflash monitor (just monitor, no flash)
espflash monitor --chip esp32

# OR use picocom
picocom -b 115200 /dev/ttyUSB0

# OR use screen
screen /dev/ttyUSB0 115200

# OR use minicom
minicom -D /dev/ttyUSB0 -b 115200
```

Press `Ctrl+]` (espflash/picocom) or `Ctrl+A K` (screen) to exit.

### Can we send commands over serial?

**Not yet** — the current firmware is output-only (`esp-println`). There is no command parser or UART RX handler. However the hardware fully supports it:

- UART0 TX+RX are both wired through the CP210x to USB
- `esp-hal` provides `Uart::new()` with RX interrupt support
- An RTIC hardware task can bind to `UART0` interrupts for RX

To add a serial shell, you would:

1. Create a `Uart` peripheral in `init()` (instead of relying on `esp-println`'s implicit UART)
2. Add a `#[task(binds = UART0, ...)]` hardware task for RX interrupts
3. Buffer incoming bytes in a ring buffer, parse commands on `\r` or `\n`
4. Dispatch commands from the shell task (e.g., change Morse status code, trigger ADC read, set DAC value)

This is a natural next feature — an interactive serial console that can change the heartbeat status code live, read/write DAC, and trigger ADC samples on demand.

## Target

- **MCU:** ESP32 (Xtensa dual-core LX6), 240 MHz
- **Board:** ESP32-WROOM-32 devkit
- **Toolchain:** `esp` channel (Rust nightly + Xtensa LLVM)
- **Framework:** RTIC v2 with custom Xtensa ESP32 backend
- **HAL:** esp-hal 1.0.0-rc.0 (no_std, bare-metal)
