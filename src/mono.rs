//! ESP32 TIMG1-based monotonic for RTIC v2.
//!
//! Uses Timer Group 1, Timer 0 as a 64-bit free-running counter at 1 MHz
//! (APB clock 80 MHz / divider 80). The alarm compare interrupt drives
//! the rtic-time timer queue for async delay/timeout support.
//!
//! The TG1_T0_LEVEL peripheral interrupt is routed through esp-hal's
//! interrupt system at Priority2 (CPU int 19, Level 2).

use core::arch::asm;
use rtic_time::timer_queue::{TimerQueue, TimerQueueBackend};

// ── TIMG1 register addresses (ESP32 TRM §18) ───────────────────────────
const TIMG1_BASE: u32 = 0x3FF6_0000;

// Timer 0 sub-registers (offsets from TIMG1_BASE)
const T0_CONFIG: *mut u32 = (TIMG1_BASE + 0x00) as *mut u32;
const T0_LO: *const u32 = (TIMG1_BASE + 0x04) as *const u32;
const T0_HI: *const u32 = (TIMG1_BASE + 0x08) as *const u32;
const T0_UPDATE: *mut u32 = (TIMG1_BASE + 0x0C) as *mut u32;
const T0_ALARM_LO: *mut u32 = (TIMG1_BASE + 0x10) as *mut u32;
const T0_ALARM_HI: *mut u32 = (TIMG1_BASE + 0x14) as *mut u32;
const T0_LOAD_LO: *mut u32 = (TIMG1_BASE + 0x18) as *mut u32;
const T0_LOAD_HI: *mut u32 = (TIMG1_BASE + 0x1C) as *mut u32;
const T0_LOAD: *mut u32 = (TIMG1_BASE + 0x20) as *mut u32;

// Interrupt registers (shared across all timers in group)
const INT_ENA: *mut u32 = (TIMG1_BASE + 0x98) as *mut u32;
const INT_CLR: *mut u32 = (TIMG1_BASE + 0xA4) as *mut u32;

// ── Timer config register bit positions ─────────────────────────────────
const T0_CFG_ALARM_EN: u32 = 1 << 10;
const T0_CFG_LEVEL_INT_EN: u32 = 1 << 11;
const T0_CFG_INCREASE: u32 = 1 << 30;
const T0_CFG_EN: u32 = 1 << 31;

/// Divider value: APB_CLK (80 MHz) / 80 = 1 MHz tick rate.
const DIVIDER: u16 = 80;

// ── Backend ─────────────────────────────────────────────────────────────

/// Timer queue backend for ESP32 TIMG1.
pub struct Esp32TimgBackend;

static TIMER_QUEUE: TimerQueue<Esp32TimgBackend> = TimerQueue::new();

impl Esp32TimgBackend {
    /// Initialise TIMG1 Timer 0 and start the monotonic.
    ///
    /// # Safety / requirements
    /// - Must be called exactly once, during `#[init]`.
    /// - TIMG1 must not be used by any other driver.
    pub fn start() {
        unsafe {
            // ── 0. Enable TIMG1 peripheral clock ───────────────────
            let perip_clk_en = 0x3FF0_00C0 as *mut u32;
            let clk = perip_clk_en.read_volatile();
            perip_clk_en.write_volatile(clk | (1 << 15));

            let perip_rst_en = 0x3FF0_00C4 as *mut u32;
            let rst = perip_rst_en.read_volatile();
            perip_rst_en.write_volatile(rst | (1 << 15));  // assert reset
            perip_rst_en.write_volatile(rst & !(1 << 15)); // de-assert reset

            // ── 1. Configure timer ─────────────────────────────────
            T0_LOAD_LO.write_volatile(0);
            T0_LOAD_HI.write_volatile(0);
            T0_LOAD.write_volatile(1);

            let cfg = T0_CFG_EN
                | T0_CFG_INCREASE
                | T0_CFG_LEVEL_INT_EN
                | ((DIVIDER as u32) << 13);
            T0_CONFIG.write_volatile(cfg);

            // Enable T0 interrupt in the timer group
            let ena = INT_ENA.read_volatile();
            INT_ENA.write_volatile(ena | 1);

            // ── 2. Route TG1_T0_LEVEL via esp-hal interrupt system ─
            // Bind our handler, then enable at Priority2 (CPU int 19, Level 2).
            // This routes through the DPORT interrupt matrix to an external
            // CPU interrupt line (not an internal CCOMPARE line).
            esp_hal::interrupt::bind_interrupt(
                esp_hal::peripherals::Interrupt::TG1_T0_LEVEL,
                tg1_t0_handler,
            );
            esp_hal::interrupt::enable(
                esp_hal::peripherals::Interrupt::TG1_T0_LEVEL,
                esp_hal::interrupt::Priority::Priority2,
            )
            .unwrap();
        }

        TIMER_QUEUE.initialize(Esp32TimgBackend);
    }
}

impl TimerQueueBackend for Esp32TimgBackend {
    type Ticks = u64;

    fn now() -> u64 {
        unsafe {
            // Latch current counter into LO/HI (write any value to UPDATE)
            T0_UPDATE.write_volatile(1);
            // Read latched value
            let lo = T0_LO.read_volatile() as u64;
            let hi = T0_HI.read_volatile() as u64;
            (hi << 32) | lo
        }
    }

    fn set_compare(instant: u64) {
        unsafe {
            T0_ALARM_LO.write_volatile(instant as u32);
            T0_ALARM_HI.write_volatile((instant >> 32) as u32);

            // Re-enable alarm (auto-clears on alarm event on ESP32)
            let cfg = T0_CONFIG.read_volatile();
            T0_CONFIG.write_volatile(cfg | T0_CFG_ALARM_EN);
        }
    }

    fn clear_compare_flag() {
        unsafe {
            // Clear T0 interrupt flag (bit 0)
            INT_CLR.write_volatile(1);
        }
    }

    fn pend_interrupt() {
        // Process the timer queue directly with interrupts masked,
        // matching the approach used by the ESP32-C3/C6 monotonics.
        unsafe {
            let old_ps: u32;
            asm!("rsr.ps {0}", out(reg) old_ps);
            // Raise INTLEVEL to 5 (mask all maskable interrupts)
            let new_ps = (old_ps & !0xF) | 5;
            asm!("wsr.ps {0}", "rsync", in(reg) new_ps);

            TIMER_QUEUE.on_monotonic_interrupt();

            asm!("wsr.ps {0}", "rsync", in(reg) old_ps);
        }
    }

    fn timer_queue() -> &'static TimerQueue<Self> {
        &TIMER_QUEUE
    }
}

// ── Interrupt handler ───────────────────────────────────────────────────

/// TG1_T0_LEVEL interrupt handler, bound via `esp_hal::interrupt::bind_interrupt`.
/// Dispatched by esp-hal's Level 2 handler through the peripheral interrupt table.
unsafe extern "C" fn tg1_t0_handler() {
    TIMER_QUEUE.on_monotonic_interrupt();
}

// ── Monotonic type macro ────────────────────────────────────────────────

/// Create an ESP32 TIMG1-based monotonic type.
///
/// ```ignore
/// esp32_timg_monotonic!(Mono);
/// // then in init:  Mono::start();
/// // in tasks:      Mono::delay(100.millis()).await;
/// ```
#[macro_export]
macro_rules! esp32_timg_monotonic {
    ($name:ident) => {
        /// A `Monotonic` based on ESP32 Timer Group 1, Timer 0 (1 MHz tick).
        pub struct $name;

        impl $name {
            /// Start the monotonic. Call once from `#[init]`.
            pub fn start() {
                $crate::mono::Esp32TimgBackend::start();
            }
        }

        impl rtic_time::monotonic::TimerQueueBasedMonotonic for $name {
            type Backend = $crate::mono::Esp32TimgBackend;
            type Instant = fugit::Instant<
                <Self::Backend as rtic_time::timer_queue::TimerQueueBackend>::Ticks,
                1,
                1_000_000,
            >;
            type Duration = fugit::Duration<
                <Self::Backend as rtic_time::timer_queue::TimerQueueBackend>::Ticks,
                1,
                1_000_000,
            >;
        }

        rtic_time::impl_embedded_hal_delay_fugit!($name);
        rtic_time::impl_embedded_hal_async_delay_fugit!($name);
    };
}
