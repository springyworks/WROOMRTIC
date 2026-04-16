# RTIC Usage Report — WROOMRTIC

## Verdict: Real RTIC v2, with room to use more features.

---

## WROOMRTIC — RTIC v2 (Xtensa backend)

WROOMRTIC is a `#![no_std]`, `#![no_main]` bare-metal binary that uses the RTIC v2
framework with the Xtensa ESP32 backend (custom, from the local `rtic` repo).

### RTIC app macro

The entire application is structured inside `#[rtic::app(...)]`:

```rust
#[rtic::app(device = esp32, dispatchers = [FROM_CPU_INTR0, FROM_CPU_INTR1])]
mod app {
    // ...
}
```

- `device = esp32` — uses the `esp32` PAC for interrupt vector definitions
- `dispatchers = [FROM_CPU_INTR0, FROM_CPU_INTR1]` — two software interrupts
  reserved for RTIC's software task dispatching

### Shared and Local resources

```rust
#[shared]
struct Shared {}

#[local]
struct Local {
    button: Input<'static>,
    led: Output<'static>,
    dac: Dac<'static, DAC1<'static>>,
    adc: Adc<'static, ADC1<'static>, esp_hal::Blocking>,
    adc_pin: AdcPin<GPIO34<'static>, ADC1<'static>>,
    wifi_device: WifiDevice<'static>,
    wifi_interface: Interface,
    wifi_sockets: SocketSet<'static>,
    tcp_handle: SocketHandle,
    dhcp_handle: SocketHandle,
    dns_handle: SocketHandle,
}
```

`Shared` is empty — no cross-task data sharing is used.
`Local` holds all peripherals and network state, owned exclusively by the idle task.

### `#[init]` — hardware initialization

```rust
#[init]
fn init(_: init::Context) -> (Shared, Local) {
    // GPIO, ADC, DAC, WiFi AP, smoltcp stack, DHCP+DNS sockets
    // ...
    (Shared {}, Local { button, led, dac, adc, adc_pin, ... })
}
```

Does real work: configures GPIO2 (LED), GPIO0 (button interrupt), DAC1, ADC1,
WiFi AP (SSID "WROOMRTIC"), smoltcp interface, TCP/UDP socket setup.
Returns peripherals as Local resources with RTIC lifetime guarantees.

### `#[idle]` — the main polling loop

```rust
#[idle(local = [led, dac, adc, adc_pin, wifi_device, wifi_interface,
                wifi_sockets, tcp_handle, dhcp_handle, dns_handle])]
fn idle(cx: idle::Context) -> ! {
    loop {
        delay.delay_millis(5);
        millis += 5;
        let (traffic, ping) = poll_network(...);
        // LED blink logic
    }
}
```

This is where **all** real work happens: WiFi polling, HTTP serving, DHCP, DNS,
LED control. It runs as RTIC's idle task (lowest priority, never blocks higher
priority tasks).

### `#[task(binds = GPIO)]` — hardware interrupt handler

```rust
#[task(binds = GPIO, local = [button], priority = 2)]
fn gpio_handler(cx: gpio_handler::Context) {
    cx.local.button.clear_interrupt();
    println!("BOOT button pressed!");
}
```

This is a real RTIC hardware-bound task. When the BOOT button (GPIO0) triggers
a falling-edge interrupt, RTIC dispatches `gpio_handler` at priority 2,
preempting the idle loop. The `button` peripheral is safely owned via
`cx.local.button` — no mutex needed because RTIC guarantees exclusive access.

### Xtensa interrupt level workaround

```rust
let saved_ps: u32;
unsafe { core::arch::asm!("rsil {0}, 0", out(reg) saved_ps) };
// ... WiFi init (needs interrupts enabled) ...
unsafe { core::arch::asm!("wsr.ps {0}", "isync", in(reg) saved_ps) };
```

This manually saves/restores the Xtensa PS register to keep INTLEVEL=0 during
WiFi init, because RTIC startup runs with interrupts masked. This is a real
RTIC-specific constraint — the framework controls interrupt enable timing, and
esp-wifi requires interrupts enabled for its init sequence.

---

## What RTIC features are actually used

| Feature                  | Used? | Notes                                      |
|--------------------------|-------|--------------------------------------------|
| `#[rtic::app]` macro     | Yes   | Structures the entire application          |
| `#[init]`                | Yes   | All hardware setup                         |
| `#[idle]`                | Yes   | Main loop — all network + LED logic        |
| `#[shared]` resources    | No    | Shared is empty                            |
| `#[local]` resources     | Yes   | All peripherals owned by idle + gpio tasks |
| Hardware-bound task      | Yes   | GPIO interrupt for BOOT button             |
| Software tasks           | No    | Dispatchers declared but never used        |
| `spawn`                  | No    | No software tasks spawned                  |
| `lock` / priority ceiling| No    | No shared resources to lock                |
| Monotonic / async tasks  | No    | Uses busy-wait Delay instead               |

---

## Honest assessment: Real RTIC, but underused

**WROOMRTIC genuinely uses RTIC** — it's not window dressing. The app macro,
init/idle/task structure, local resource ownership, and the Xtensa interrupt
level management are all real RTIC patterns that wouldn't work without the
framework.

**But it only uses ~40% of what RTIC offers.** The heavy features — shared
resources with priority ceiling locks, software task spawning, async/await with
monotonics — are not used. Everything runs in `#[idle]` with a 5ms polling
loop, which is basically a bare-metal superloop wrapped in RTIC structure.

The GPIO interrupt handler is the only true preemptive task, and it just prints
a message.

### What would make it "full RTIC"

1. **Software tasks**: Move HTTP handling, DHCP, DNS into separate
   `#[task]` functions, spawned from idle when data is available
2. **Shared resources**: Put `wifi_sockets` in `Shared`, accessed from
   multiple tasks with `lock()`
3. **Monotonics**: Replace `delay.delay_millis(5)` busy-wait with
   `Mono::delay(5.millis()).await` using a timer monotonic
4. **Async tasks**: Make network polling an async task that yields
   between poll cycles instead of blocking idle


