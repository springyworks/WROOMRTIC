# WROOMRTIC

Bare-metal RTIC v2 demo for **ESP32-WROOM-32** (Xtensa, no_std).

## What it does

1. **WiFi AP + Captive Portal** — broadcasts SSID `WROOMRTIC` (open, channel 1), with DHCP, DNS spoofing, and captive portal redirect so phones auto-show the landing page
2. **Web Shell** — interactive terminal at `http://192.168.4.1/` with commands for LED, DAC, ADC, status, and an ASCII worms screensaver
3. **Morse heartbeat** — blinks a two-character status code on the blue LED (GPIO2) every ~5 seconds
4. **DAC→ADC loopback** — ramps DAC1 (GPIO25) through 11 voltage steps, reads each with ADC1 (GPIO34), prints results
5. **BOOT button** — hardware GPIO interrupt prints on press

WiFi polling is interleaved with the heartbeat loop (every ~10ms), so both run cooperatively in idle.

## Captive Portal

All DNS queries resolve to `192.168.4.1`. Captive portal probes from Android (`/generate_204`), iOS (`/hotspot-detect`, `/success.html`), and Windows (`/ncsi.txt`, `/connecttest`, `/success.txt`) are intercepted with 302 redirects to the landing page. Any other URL also serves the shell page — there is no internet, only WROOM.

## Web Shell Commands

| Command | Description |
|---------|-------------|
| `help` | Show all commands |
| `status` | Uptime, cycle count, heartbeat, WiFi |
| `led <on\|off>` | Control onboard blue LED |
| `dac <0-255>` | Set DAC output (GPIO25) |
| `adc` | Read ADC value (GPIO34) |
| `ping` | Connectivity check |
| `uptime` | Milliseconds since boot |
| `info` | Hardware info |
| `echo <text>` | Echo text back |
| `screensaver` | ASCII worms animation (any key stops) |
| `clear` | Clear screen (local) |

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
| DHCP | Auto (192.168.4.100+, MAC-based) |
| DNS | All queries → 192.168.4.1 |
| HTTP | Port 80 — captive portal + web shell |

### Connecting from a phone

1. Open WiFi settings, connect to **WROOMRTIC**
2. DHCP assigns an IP automatically (192.168.4.100+)
3. The captive portal popup should appear — tap to open the web shell
4. Or open any URL in a browser — all routes lead to the shell

### Connecting via adb

```bash
# Connect phone to WROOMRTIC WiFi (manual step on phone)
# Then from PC:
adb shell curl http://192.168.4.1/
```

### Architecture notes

- WiFi uses **esp-wifi 0.15.1** with the built-in preemptive scheduler (TIMG0 timer)
- TCP/IP stack: **smoltcp 0.12** (no_std, polled from idle loop)
- Built-in DHCP server assigns IPs deterministically from client MAC
- DNS spoof responds to all A-record queries with 192.168.4.1
- Captive portal intercepts OS connectivity checks with 302 redirects
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

**Not yet** — the current firmware is output-only (`esp-println`). Commands are sent via the web shell at `http://192.168.4.1/`. A serial shell (UART0 RX interrupt + ring buffer) is a natural next feature.

## Target

- **MCU:** ESP32 (Xtensa dual-core LX6), 240 MHz
- **Board:** ESP32-WROOM-32 devkit
- **Toolchain:** `esp` channel (Rust nightly + Xtensa LLVM)
- **Framework:** RTIC v2 with custom Xtensa ESP32 backend
- **HAL:** esp-hal 1.0.0-rc.0 (no_std, bare-metal)
