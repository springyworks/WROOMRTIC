# RTIC Usage Report — WROOMRTIC

## Verdict: Full RTIC v2 async — monotonic timer, async tasks, cooperative multitasking.

![Async Task Architecture](rtic-async-architecture.drawio.svg)

---

## WROOMRTIC — RTIC v2 Async (Xtensa backend)

WROOMRTIC is a `#![no_std]`, `#![no_main]` bare-metal binary running on
ESP32-WROOM-32 (Xtensa LX6 @ 240 MHz). It uses the RTIC v2 framework with
a **custom Xtensa ESP32 backend** (from the local `rtic` repo) and a
**hand-written TIMG1 monotonic timer** for non-blocking async delays.

Two async tasks run cooperatively at the same priority, yielding to each other
through `Mono::delay().await`. The CPU sleeps (`waiti 0`) between interrupts —
zero busy-waits anywhere in the firmware.

---

## Architecture overview

### RTIC app macro + dispatchers

```rust
#[rtic::app(device = esp32, dispatchers = [FROM_CPU_INTR0, FROM_CPU_INTR1])]
mod app {
    esp32_timg_monotonic!(Mono);
    // ...
}
```

- `device = esp32` — PAC for interrupt vectors
- `dispatchers` — two software interrupts for RTIC's software task dispatch
- `esp32_timg_monotonic!(Mono)` — custom monotonic type backed by TIMG1 Timer 0

### Local resources — split across async tasks

```rust
#[local]
struct Local {
    button: Input<'static>,           // → gpio_handler (priority 2)
    led: Output<'static>,             // → led_task (async, priority 1)
    dac: Dac<'static, DAC1<'static>>, // → network_task (async, priority 1)
    adc: ...,                         // → network_task
    wifi_device: WifiDevice<'static>, // → network_task
    wifi_interface: Interface,        // → network_task
    wifi_sockets: SocketSet<'static>, // → network_task
    tcp_handles: [SocketHandle; 2],   // → network_task
    dhcp_handle: SocketHandle,        // → network_task
    dns_handle: SocketHandle,         // → network_task
}
```

Peripherals are **distributed across three tasks** — RTIC guarantees exclusive
ownership at compile time, no runtime locks needed.

### Cross-task communication via atomics

```rust
static TRAFFIC_COUNT: AtomicU32 = AtomicU32::new(0);
static REQUEST_COUNT: AtomicU32 = AtomicU32::new(0);
static LED_OFF_TICK: AtomicU32 = AtomicU32::new(0);
```

`network_task` writes `LED_OFF_TICK` and `TRAFFIC_COUNT`;
`led_task` reads them for blink timing and morse display.
Lock-free, wait-free, no priority inversion.

---

## Async RTIC code snippets

### Snippet 1 — Async task spawning from `#[init]`

```rust
#[init]
fn init(_: init::Context) -> (Shared, Local) {
    // ... hardware setup, WiFi AP, smoltcp ...
    Mono::start();  // start TIMG1 monotonic

    led_task::spawn().unwrap();
    network_task::spawn().unwrap();

    (Shared {}, Local { ... })
}
```

Both async tasks are spawned during init. RTIC's executor manages them —
they begin running once `init` returns and the system enters the dispatcher.

### Snippet 2 — `led_task`: async morse with non-blocking delays

```rust
#[task(local = [led], priority = 1)]
async fn led_task(cx: led_task::Context) {
    let led = cx.local.led;
    loop {
        // ... morse pattern loop ...
        for (ei, &units) in pattern.iter().enumerate() {
            if ei > 0 {
                led.set_low();
                Mono::delay(MORSE_UNIT_MS.millis()).await;   // ← yields here
            }
            led.set_high();
            Mono::delay((MORSE_UNIT_MS * units as u64).millis()).await;  // ← yields here
        }
        // ...
        Mono::delay(10u64.millis()).await;  // LED refresh rate — yields to network_task
    }
}
```

Every `Mono::delay().await` suspends the task and returns control to the RTIC
executor. During morse pauses, `network_task` runs — HTTP requests are served
**concurrently with morse blinking**.

### Snippet 3 — `network_task`: async polling loop

```rust
#[task(local = [dac, adc, adc_pin, wifi_device, wifi_interface,
                wifi_sockets, tcp_handles, dhcp_handle, dns_handle], priority = 1)]
async fn network_task(cx: network_task::Context) {
    loop {
        Mono::delay(5u64.millis()).await;  // ← yields for 5ms

        let now_ticks = Mono::now().ticks();
        let millis = (now_ticks / 1000) as i64;
        let timestamp = smoltcp::time::Instant::from_millis(millis);
        iface.poll(timestamp, device, sockets);

        handle_dhcp(sockets, dhcp_handle);
        handle_dns(sockets, dns_handle);

        for &tcp_handle in &tcp_handles {
            let (traffic, ping) = handle_tcp(...);
            if traffic {
                crate::TRAFFIC_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            if ping {
                crate::LED_OFF_TICK.fetch_max(now_ms + 150, Ordering::Relaxed);
            }
        }
    }
}
```

Reads monotonic time via `Mono::now()` for smoltcp timestamps.
Communicates LED blink requests to `led_task` through `LED_OFF_TICK` atomic.

### Snippet 4 — `#[idle]`: CPU sleep between interrupts

```rust
#[idle]
fn idle(_: idle::Context) -> ! {
    loop {
        unsafe { core::arch::asm!("waiti 0") };
    }
}
```

Idle does **nothing** — all work is in async tasks. The CPU sleeps until the
next interrupt (timer alarm, WiFi, GPIO). This is the hallmark of a fully
async RTIC application: idle is empty, tasks are async, delays are non-blocking.

### Snippet 5 — Custom TIMG1 monotonic backend (`mono.rs`)

```rust
impl TimerQueueBackend for Esp32TimgBackend {
    type Ticks = u64;

    fn now() -> u64 {
        unsafe {
            T0_UPDATE.write_volatile(1);       // latch counter
            let lo = T0_LO.read_volatile() as u64;
            let hi = T0_HI.read_volatile() as u64;
            (hi << 32) | lo
        }
    }

    fn set_compare(instant: u64) {
        unsafe {
            T0_ALARM_LO.write_volatile(instant as u32);
            T0_ALARM_HI.write_volatile((instant >> 32) as u32);
            let cfg = T0_CONFIG.read_volatile();
            T0_CONFIG.write_volatile(cfg | T0_CFG_ALARM_EN);
        }
    }

    fn clear_compare_flag() {
        unsafe { INT_CLR.write_volatile(1); }
    }
    // ...
}

unsafe extern "C" fn tg1_t0_handler() {
    TIMER_QUEUE.on_monotonic_interrupt();
}
```

This is a **from-scratch monotonic implementation** for the ESP32 Xtensa —
no upstream crate exists. TIMG1 Timer 0 runs at 1 MHz (80 MHz APB / divider 80).
The alarm interrupt is routed through esp-hal's peripheral interrupt system at
Priority2 (CPU int 19, Level 2), avoiding the Xtensa-internal CCOMPARE lines
which cannot be triggered by the DPORT interrupt matrix.

### Snippet 6 — Xtensa PS workaround for WiFi init under RTIC

```rust
// RTIC #[init] runs with PS.INTLEVEL > 0 (interrupts masked).
// esp-wifi requires INTLEVEL=0 for init, configure, and start.
let saved_ps: u32;
unsafe { core::arch::asm!("rsil {0}, 0", out(reg) saved_ps) };

let wifi_init = esp_wifi::init(timg0.timer0, rng).unwrap();
wifi_controller.set_configuration(&Configuration::AccessPoint(ap_config)).unwrap();
wifi_controller.start().unwrap();

unsafe { core::arch::asm!("wsr.ps {0}", "isync", in(reg) saved_ps) };
```

RTIC controls interrupt timing — `#[init]` runs with interrupts disabled.
This manually drops INTLEVEL to 0, lets esp-wifi run its init sequence, then
restores the original PS value before RTIC takes over.

### Snippet 7 — Hardware-bound task: GPIO interrupt

```rust
#[task(binds = GPIO, local = [button], priority = 2)]
fn gpio_handler(cx: gpio_handler::Context) {
    cx.local.button.clear_interrupt();
    println!("BOOT button pressed!");
}
```

Priority 2 — preempts both async tasks (priority 1). RTIC guarantees exclusive
ownership of `button` via `cx.local` — no mutex, no lock, compile-time safety.

---

## What RTIC features are used

| Feature                  | Used? | Notes                                            |
|--------------------------|-------|--------------------------------------------------|
| `#[rtic::app]` macro     | Yes   | Entire application structure                     |
| `#[init]`                | Yes   | Hardware setup + `Mono::start()` + `spawn`       |
| `#[idle]`                | Yes   | CPU sleep (`waiti 0`) — empty, all work in tasks |
| `#[shared]` resources    | No    | Cross-task via atomics instead                   |
| `#[local]` resources     | Yes   | Split across 3 tasks (gpio, led, network)        |
| Hardware-bound task      | Yes   | GPIO interrupt for BOOT button (priority 2)      |
| Software tasks (async)   | Yes   | `led_task` + `network_task` (both priority 1)    |
| `spawn`                  | Yes   | Both async tasks spawned from init               |
| Dispatchers              | Yes   | FROM_CPU_INTR0, FROM_CPU_INTR1 for task dispatch |
| Monotonic timer          | Yes   | Custom TIMG1 backend (1 MHz, `mono.rs`)          |
| `Mono::delay().await`    | Yes   | All timed waits — no busy-loops anywhere         |
| `Mono::now()`            | Yes   | smoltcp timestamps, morse timing, LED off-tick   |
| `lock` / priority ceiling| No    | Not needed — atomics for cross-task data          |

---

## Assessment: Full async RTIC v2

**WROOMRTIC now uses ~85% of RTIC v2's feature set.** The major async features
are all exercised:

- **Custom monotonic timer** — hand-written TIMG1 backend with proper
  peripheral clock enable, interrupt routing through esp-hal, and
  `TimerQueueBackend` implementation
- **Async software tasks** — two concurrent tasks (`led_task`, `network_task`)
  cooperatively multitask via `Mono::delay().await`
- **`spawn` from init** — both tasks launched at startup
- **Dispatchers** — RTIC uses FROM_CPU_INTR0/1 for async task context switching
- **Hardware interrupt** — GPIO button at priority 2 preempts async tasks
- **Idle = sleep** — CPU halts between interrupts, zero wasted cycles
- **Cross-task communication** — lock-free atomics for LED timing signals

The only unused RTIC feature is `#[shared]` resources with priority-ceiling
locks, which is intentionally avoided — atomics are sufficient and simpler
for the current data flow (unidirectional: network→LED).

### Key technical challenge solved

No upstream RTIC monotonic exists for ESP32 Xtensa. The `mono.rs` module is
a complete from-scratch implementation. A critical discovery during development:
**CPU interrupt 15 (CCOMPARE1) cannot be triggered by the DPORT interrupt
matrix** — it's an Xtensa-internal timer line. The fix was routing TG1_T0_LEVEL
through esp-hal's `interrupt::enable()` API to CPU int 19 (Level 2, external).


