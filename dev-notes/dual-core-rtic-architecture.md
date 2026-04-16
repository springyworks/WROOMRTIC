# Dual-Core RTIC Architecture — ESP32 Deep Analysis

## The Question

Can we run RTIC on **both** ESP32 cores, have them communicate via shared
memory, and use core 1 as a hardware-level logic analyzer for core 0?

## TL;DR

| Question | Answer |
|----------|--------|
| Two RTIC instances, one per core? | **Yes, architecturally possible — requires new backend work** |
| Shared memory communication? | **Yes, via lock-free structures in SRAM** |
| Core 1 as logic analyzer of core 0? | **Yes, using DBREAKA watchpoints + CCOMPARE sampling** |
| Advanced hardware routing? | **Yes, the DPORT interrupt matrix is fully programmable** |
| Difficulty? | **Medium-hard** — RTIC removed multi-core support in v0.6 (PR #355) |

---

## 1. How the ESP32 interrupt matrix works

The ESP32 has a **fully programmable interrupt routing matrix** via the DPORT
peripheral. This is the key to dual-core RTIC:

```
┌─────────────────────────────────────────────────────────────────┐
│                   DPORT Interrupt Matrix                        │
│                                                                 │
│   71 peripheral interrupt sources                               │
│          │                                                      │
│          ▼                                                      │
│   ┌─────────────────┐      ┌─────────────────┐                 │
│   │  PRO_*_MAP regs │      │  APP_*_MAP regs │                 │
│   │  (DPORT+0x104)  │      │  (DPORT+0x218)  │                 │
│   │                 │      │                 │                  │
│   │ source → CPU int│      │ source → CPU int│                 │
│   └────────┬────────┘      └────────┬────────┘                 │
│            │                        │                           │
│            ▼                        ▼                           │
│   ┌────────────────┐       ┌────────────────┐                  │
│   │  PRO_CPU       │       │  APP_CPU       │                  │
│   │  32 CPU ints   │       │  32 CPU ints   │                  │
│   │  levels 1-7    │       │  levels 1-7    │                  │
│   └────────────────┘       └────────────────┘                  │
└─────────────────────────────────────────────────────────────────┘
```

**Key insight**: Every peripheral interrupt source can be independently
routed to **either** core (or both!). The current RTIC backend uses only
`PRO_*_MAP` registers (core 0). Adding core 1 means using the `APP_*_MAP`
registers at DPORT offset `0x218`.

### Register addresses

| Register set | DPORT offset | Purpose |
|-------------|-------------|---------|
| `PRO_MAC_INTR_MAP` ... | `0x104` + N×4 | Route source N → PRO_CPU int line |
| `APP_MAC_INTR_MAP` ... | `0x218` + N×4 | Route source N → APP_CPU int line |
| `FROM_CPU_INTR0_REG` | `0xDC` | Trigger software interrupt 0 |
| `FROM_CPU_INTR1_REG` | `0xE0` | Trigger software interrupt 1 |
| `FROM_CPU_INTR2_REG` | `0xE4` | Trigger software interrupt 2 |
| `FROM_CPU_INTR3_REG` | `0xE8` | Trigger software interrupt 3 |

### FROM_CPU interrupts — cross-core signaling

The 4 `FROM_CPU_INTRx` sources are **global** — writing 1 to `FROM_CPU_INTR0_REG`
sets the source pending. Both cores see it if both have it mapped. But you
can route FROM_CPU_INTR0 to core 0 and FROM_CPU_INTR2 to core 1, giving
each RTIC instance its own software dispatchers.

---

## 2. The dual-RTIC architecture

### Option A: Two independent `#[rtic::app]` (ideal but requires linker work)

```
                    ┌─────────────────────────────────────────┐
                    │           Shared SRAM region            │
                    │     lock-free SPSC ring buffers         │
                    │     atomic flags / mailbox slots        │
                    │     (placed via linker at 0x3FFB_xxxx)  │
                    └────────────┬──────────────┬─────────────┘
                                 │              │
          ┌──────────────────────┘              └──────────────────────┐
          │                                                           │
┌─────────┴──────────────┐                          ┌─────────────────┴────────┐
│   CORE 0 — RTIC #1    │                          │   CORE 1 — RTIC #2      │
│                        │                          │                          │
│ dispatchers:           │                          │ dispatchers:             │
│  FROM_CPU_INTR0        │  ◄── cross-core IRQ ──►  │  FROM_CPU_INTR2          │
│  FROM_CPU_INTR1        │                          │  FROM_CPU_INTR3          │
│                        │                          │                          │
│ hardware tasks:        │                          │ hardware tasks:          │
│  • WiFi                │                          │  • CCOMPARE0 (timer ISR) │
│  • GPIO (button)       │                          │  • DBREAKA0 exception    │
│  • UART (future)       │                          │  • perf counter overflow │
│                        │                          │                          │
│ software tasks:        │                          │ software tasks:          │
│  • http_server         │                          │  • bus_monitor            │
│  • audio_gen           │                          │  • anomaly_detect         │
│  • morse_blink         │                          │  • sonify_trace           │
│                        │                          │                          │
│ idle: poll network     │                          │ idle: watchpoint scan    │
│                        │                          │                          │
│ interrupt matrix:      │                          │ interrupt matrix:         │
│  PRO_*_MAP registers   │                          │  APP_*_MAP registers     │
│  (DPORT+0x104)         │                          │  (DPORT+0x218)           │
└────────────────────────┘                          └──────────────────────────┘
```

**Why this is hard today**: RTIC v2 deliberately removed multi-core support
(PR #355 in CHANGELOG: "Multi-core support was removed to reduce overall
complexity"). Two `#[rtic::app]` macros would fight over `main`, `init`,
and global symbols. You'd need:
1. A custom linker script that gives each core its own stack and entry point
2. APP_CPU boot code (write entry address to `DPORT_APPCPU_CTRL_D_REG`, then
   set `DPORT_APPCPU_CTRL_B_REG` bit 0 to release reset)
3. Separate `#[rtic::app]` expansion that generates `app_cpu_main` instead of `main`

### Option B: Single RTIC on core 0 + bare loop on core 1 (practical NOW)

```rust
// In RTIC init, boot core 1 with a bare monitoring loop
#[init]
fn init(cx: init::Context) -> (Shared, Local) {
    // ... normal init ...

    // Boot APP_CPU (core 1)
    unsafe {
        let dport = 0x3FF0_0000 as *mut u32;
        // Set core 1 entry point
        let ctrl_d = dport.add(0x058 / 4); // DPORT_APPCPU_CTRL_D_REG
        ctrl_d.write_volatile(core1_entry as u32);
        // Release core 1 from reset
        let ctrl_b = dport.add(0x050 / 4); // DPORT_APPCPU_CTRL_B_REG
        let v = ctrl_b.read_volatile();
        ctrl_b.write_volatile(v | 1);  // APPCPU_CLKGATE_EN
        // Deassert reset
        let ctrl_a = dport.add(0x04C / 4); // DPORT_APPCPU_CTRL_A_REG
        ctrl_a.write_volatile(1);  // release reset
    }

    (Shared {}, Local { ... })
}

// Runs on core 1 — NOT RTIC, just a bare function
#[link_section = ".rwtext"]
unsafe extern "C" fn core1_entry() -> ! {
    // Set up core 1's own stack (in upper SRAM)
    // Arm watchpoints, enter monitoring loop
    loop {
        // Sample, analyze, write to shared ring buffer
    }
}
```

**This is the practical path**: core 0 runs full RTIC, core 1 runs a
dedicated monitoring loop. Communication via atomic variables and
lock-free ring buffers in shared SRAM.

### Option C: Extend the Xtensa RTIC backend for dual-core (future)

Add a `cores = 2` attribute to `#[rtic::app]` that:
1. Generates two `idle` functions (one per core)
2. Lets tasks specify `core = 0` or `core = 1`
3. Uses `PRO_*_MAP` for core 0 tasks, `APP_*_MAP` for core 1 tasks
4. Splits `FROM_CPU_INTR0/1` to core 0 dispatchers, `FROM_CPU_INTR2/3` to core 1
5. Generates core 1 boot code in `init`

This is the **right** long-term answer but significant RTIC macro work.

---

## 3. Shared memory communication

### The ESP32 memory map (shared regions)

```
0x3FF8_0000 ─ 0x3FFF_FFFF   Internal SRAM 2     (200 KB, data)
0x3FFB_0000 ─ 0x3FFD_FFFF   Internal SRAM 1     (192 KB, data)
0x3FFE_0000 ─ 0x3FFF_FFFF   Internal SRAM 2     (continued)
```

**All internal SRAM is equally accessible by both cores** with identical
timing. There is NO per-core memory on the ESP32 (unlike ESP32-S3 which
has RTC fast memory per core). This makes shared-memory trivial.

### Communication primitives

```rust
use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};

// Placed in a known SRAM region (linker section or fixed address)
#[link_section = ".dram1.shared"]
static SHARED_MAILBOX: AtomicU32 = AtomicU32::new(0);

#[link_section = ".dram1.shared"]
static CORE1_READY: AtomicBool = AtomicBool::new(false);

// Lock-free SPSC ring buffer for bus trace events
#[link_section = ".dram1.shared"]
static mut TRACE_RING: [u32; 1024] = [0; 1024];
#[link_section = ".dram1.shared"]
static RING_HEAD: AtomicU32 = AtomicU32::new(0);  // written by core 1
#[link_section = ".dram1.shared"]
static RING_TAIL: AtomicU32 = AtomicU32::new(0);  // read by core 0
```

### Cross-core interrupt notification

When core 1 detects an interesting event, it can poke core 0 via a FROM_CPU
interrupt:

```rust
// Core 1 writes:
unsafe {
    let dport = 0x3FF0_0000 as *mut u32;
    dport.add(0xDC / 4).write_volatile(1);  // trigger FROM_CPU_INTR0 on core 0
}

// Core 0 RTIC handles it as a normal hardware task:
#[task(binds = FROM_CPU_INTR0, priority = 3)]
fn cross_core_event(cx: cross_core_event::Context) {
    // Read shared ring buffer, process trace data
    // Clear the FROM_CPU_INTR0
    unsafe {
        let dport = 0x3FF0_0000 as *mut u32;
        dport.add(0xDC / 4).write_volatile(0);
    }
}
```

Wait — FROM_CPU_INTR0/1 are currently used as RTIC dispatchers. So core 1
would use FROM_CPU_INTR2 or INTR3 to signal core 0, routing them to PRO_CPU
via the interrupt matrix but NOT using them as RTIC dispatchers.

---

## 4. What core 1 can monitor of core 0

### Hardware mechanisms (per-core, independent)

| Mechanism | What it sees | From core 1? |
|-----------|-------------|--------------|
| **DBREAKA0/1** (data breakpoint) | Memory read/write to specific addresses | **Own core only** — each core has separate DBREAKA |
| **IBREAKA0/1** (instruction breakpoint) | Instruction fetch at specific PC | **Own core only** |
| **CCOUNT** | Cycle counter | **Own core only** — but can read other core's via IPC ISR |
| **Shared bus snooping** | All SRAM accesses from both cores | **Yes** — watch shared variables |
| **Peripheral registers** | Any MMIO read/write | **Yes** — memory-mapped, visible to both cores |
| **Interrupt status** | INTERRUPT register (pending ints) | **Own core only** but DPORT status regs are shared |

### The trick: core 1 watches SHARED resources

Core 1 can't directly observe core 0's register file or PC. But it **can**:

1. **Watch shared SRAM locations** — arm DBREAKA0 on core 1 to trigger when
   **core 1 itself** reads a location that core 0 writes to. Polling pattern:
   ```
   Core 1: read watchpoint_addr → if changed since last read → log event
   ```
   This is **not** a true bus sniffer but works for monitoring specific variables.

2. **Read DPORT status registers** — the interrupt pending/enabled status of
   both cores is in DPORT and readable by both:
   ```
   PRO_INTR_STATUS_0/1/2  (DPORT + 0x10C/110/114) — core 0 pending bits
   APP_INTR_STATUS_0/1/2  (DPORT + 0x220/224/228) — core 1 pending bits
   ```
   Core 1 can continuously sample core 0's interrupt status to build an
   interrupt timeline.

3. **Cross-core IPC ISR** — ESP-IDF's mechanism to halt core 0 momentarily
   and read its registers (PS, PC, CCOUNT, EXCCAUSE). In bare-metal, we can
   implement the same thing: core 1 triggers a level-5 (NMI-like) interrupt
   on core 0 that dumps registers to shared memory.

### The real logic analyzer: GPIO matrix + peripheral routing

The ESP32's GPIO matrix can route **internal signals to external pins**:

```
┌────────────────────────────────────────────────────────┐
│                    GPIO Matrix                          │
│                                                        │
│  256 peripheral signals ◄──► 40 GPIO pads              │
│                                                        │
│  Internal signal → GPIO_FUNCx_OUT_SEL → physical pin   │
│  Physical pin → GPIO_FUNCx_IN_SEL → internal signal    │
└────────────────────────────────────────────────────────┘
```

**You can route CPU debug signals to GPIO pins and observe them with a
logic analyzer!** The CPU's `TRAX` trace interface (if configured) outputs
compressed PC traces on GPIO pins.

Even without TRAX, you can create your own diagnostic signals:

```rust
// Toggle a GPIO on every interrupt entry/exit — visible on scope
#[task(binds = WIFI_MAC, priority = 2, local = [debug_pin])]
fn wifi_isr(cx: wifi_isr::Context) {
    cx.local.debug_pin.set_high();   // visible on logic analyzer
    // ... handle interrupt ...
    cx.local.debug_pin.set_low();
}
```

With **4-6 GPIO pins as debug outputs**, one per interrupt level, you get
a real-time interrupt timeline on any $10 logic analyzer (Saleae, sigrok).

---

## 5. Advanced hardware routing possibilities

### A. Interrupt-level activity monitor (GPIO-based)

Route interrupt level transitions to GPIOs:

```
GPIO 25 ─── HIGH when core 0 is at INTLEVEL ≥ 1 (any ISR active)
GPIO 26 ─── HIGH when core 0 is at INTLEVEL ≥ 2 (priority 2+ ISR)
GPIO 27 ─── HIGH when core 0 is at INTLEVEL ≥ 3 (priority 3+ ISR)
GPIO 32 ─── HIGH when core 1 watchpoint fires
GPIO 33 ─── PULSE on cross-core IPC event
```

Read these with core 1's PCNT (pulse counter) peripheral or RMT
(remote control transceiver) peripheral for hardware counting.

### B. PCNT as a hardware event counter

The PCNT peripheral counts edges on any GPIO. Use it to count interrupt
events **in hardware** with zero CPU overhead:

```rust
// Count WiFi interrupts per second using PCNT unit 0
// Wire GPIO25 (toggled in WiFi ISR) to PCNT input via GPIO matrix (internal)
pcnt.channel0.set_edge_signal(gpio25);
pcnt.channel0.set_ctrl_signal(always_high);
// PCNT runs independently — read count whenever
```

### C. RMT as a bus timing recorder

The RMT peripheral records high/low pulse durations. If you toggle a GPIO
at ISR entry/exit, RMT records exact durations of each ISR — like a logic
analyzer channel, but internal to the chip, readable by core 1.

### D. I2S parallel mode as a data capture channel

The ESP32's I2S peripheral in parallel (LCD) mode can capture 8/16-bit
parallel data at up to 20MHz — driven by an external FPGA outputting bus
states, or even looped-back internal signals.

---

## 6. Proposed implementation roadmap

### Phase 1: Option B — bare core 1 monitor (doable now)

```
Week 1:  Boot core 1 from RTIC init
         → verify with PRID register read → "core 1 alive" via shared flag
Week 2:  Core 1 CCOMPARE periodic sampling (10kHz)
         → dump CCOUNT deltas to shared ring buffer
         → core 0 reads ring buffer in /audio endpoint
Week 3:  Core 1 DBREAKA armed on WiFi DMA descriptor region
         → count accesses, expose as "bus activity" metric
Week 4:  GPIO debug outputs for interrupt levels
         → external logic analyzer visualization
```

### Phase 2: Cross-core RTIC integration (future)

```
• Extend xtensa_esp32 backend: APP_*_MAP registers for core 1 routing
• Add `core = 1` attribute to task declarations
• Split FROM_CPU dispatchers: 0/1 for core 0, 2/3 for core 1
• Generate core 1 boot code in init
• Shared resources with cross-core lock (PS.INTLEVEL not sufficient —
  need hardware spinlock or atomics)
```

### Phase 3: Hardware logic analyzer

```
• GPIO matrix routing of interrupt signals
• PCNT counting ISR events in hardware
• RMT recording ISR timing
• Full interrupt timeline via /api/trace HTTP endpoint
```

---

## 7. What the cross-core lock looks like

Xtensa has `S32C1I` (store-conditional) for atomic compare-and-swap:

```rust
/// Cross-core spinlock using Xtensa S32C1I (compare-and-swap)
/// Required because PS.INTLEVEL only masks interrupts on ONE core
#[repr(C)]
struct CrossCoreLock {
    lock: AtomicU32,  // 0 = free, 1 = held
}

impl CrossCoreLock {
    #[inline(always)]
    fn acquire(&self) {
        loop {
            // S32C1I: if mem[addr] == a2, then mem[addr] = a3
            let old = self.lock.compare_exchange(
                0, 1, Ordering::Acquire, Ordering::Relaxed
            );
            if old.is_ok() { break; }
            // Spin — but on Xtensa this is fine, no WFE/WFI needed
        }
    }

    #[inline(always)]
    fn release(&self) {
        self.lock.store(0, Ordering::Release);
    }
}
```

For RTIC's `#[shared]` resources accessed by tasks on different cores,
the lock would combine:
1. **PS.INTLEVEL** raise (mask local interrupts up to ceiling) — same as now
2. **CrossCoreLock acquire** (prevent other core from entering) — new

---

## 8. Summary: what's unique about ESP32 for this

| ESP32 advantage | Why it matters |
|----------------|---------------|
| **Fully programmable interrupt matrix** | Route any of 71 sources to any CPU int on either core |
| **4 FROM_CPU software interrupts** | Enough for 2 dispatchers per core |
| **Shared SRAM with no access penalty** | Both cores see same memory at same speed |
| **DBREAKA/DBREAKC per core** | Hardware watchpoints without halting debug |
| **CCOMPARE per core** | Independent hardware timer interrupts |
| **GPIO matrix (256 signals)** | Route internal signals to external pins |
| **PCNT peripheral** | Hardware event counting with zero CPU cost |
| **RMT peripheral** | Hardware timing capture for ISR durations |
| **S32C1I instruction** | Atomic CAS for cross-core synchronization |
| **Separate CCOUNT per core** | Independent timing, no observer effect |

The ESP32 was designed for dual-core SMP with FreeRTOS. We're repurposing
that infrastructure for AMP (asymmetric multiprocessing) with RTIC —
which is actually a better fit for the monitor-and-application split.
