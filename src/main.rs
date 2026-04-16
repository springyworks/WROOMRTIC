#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![no_main]
#![no_std]
#![feature(asm_experimental_arch)]

extern crate alloc;

mod mono;

esp_bootloader_esp_idf::esp_app_desc!();

// Cross-task communication via atomics (no locking needed).
use core::sync::atomic::AtomicU32;
static TRAFFIC_COUNT: AtomicU32 = AtomicU32::new(0);
static REQUEST_COUNT: AtomicU32 = AtomicU32::new(0);
/// Monotonic tick at which the LED should turn off (set by network_task).
static LED_OFF_TICK: AtomicU32 = AtomicU32::new(0);

/// WROOMRTIC — bare-metal RTIC v2 async demo for ESP32-WROOM-32.
///
/// Demonstrates:
///  - WiFi AP mode (SSID: WROOMRTIC, open, 192.168.4.1)
///  - RTIC v2 async tasks with TIMG1 monotonic timer (no busy-waits)
///  - Multi-client HTTP (2 TCP sockets on port 80)
///  - Morse code heartbeat on GPIO2 (async, non-blocking)
///  - DAC/ADC loopback, CCOUNT audio signature analyzer
///  - Hardware GPIO interrupt (BOOT button on GPIO0)
#[rtic::app(device = esp32, dispatchers = [FROM_CPU_INTR0, FROM_CPU_INTR1])]
mod app {
    use alloc::format;
    use alloc::string::String;
    use crate::esp32_timg_monotonic;
    use esp_backtrace as _;
    use esp_hal::analog::adc::{Adc, AdcConfig, AdcPin, Attenuation};
    use esp_hal::analog::dac::Dac;
    use esp_hal::gpio::{Event, Input, InputConfig, Level, Output, OutputConfig, Pull};
    use esp_hal::peripherals::{ADC1, DAC1, GPIO34};
    use esp_hal::rng::Rng;
    use esp_hal::timer::timg::TimerGroup;
    use esp_println::println;
    use esp_wifi::wifi::{
        AccessPointConfiguration, AuthMethod, Configuration, WifiController, WifiDevice,
    };
    use fugit::ExtU64;
    use rtic_time::Monotonic;
    use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet, SocketStorage};
    use smoltcp::socket::{tcp, udp};
    use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint};
    use static_cell::StaticCell;

    esp32_timg_monotonic!(Mono);

    /// Number of concurrent TCP sockets for multi-client HTTP.
    const TCP_SOCKET_COUNT: usize = 2;

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
        tcp_handles: [SocketHandle; TCP_SOCKET_COUNT],
        dhcp_handle: SocketHandle,
        dns_handle: SocketHandle,
    }

    #[init]
    fn init(_: init::Context) -> (Shared, Local) {
        println!("WROOMRTIC init (async)");

        // Set up heap allocator — required by esp-wifi
        esp_alloc::heap_allocator!(size: 72 * 1024);

        let peripherals = esp_hal::init(esp_hal::Config::default());

        // GPIO2 = onboard blue LED
        let led = Output::new(peripherals.GPIO2, Level::Low, OutputConfig::default());

        // GPIO0 = BOOT button (active low, externally pulled up)
        let mut button = Input::new(
            peripherals.GPIO0,
            InputConfig::default().with_pull(Pull::Up),
        );
        button.listen(Event::FallingEdge);

        // DAC1 on GPIO25 — 8-bit output (0-255 -> 0-3.3V)
        let dac = Dac::new(peripherals.DAC1, peripherals.GPIO25);

        // ADC1 on GPIO34 — 12-bit input, 11dB attenuation (~0-2450mV)
        let mut adc1_config = AdcConfig::new();
        let adc_pin = adc1_config.enable_pin(peripherals.GPIO34, Attenuation::_11dB);
        let adc = Adc::new(peripherals.ADC1, adc1_config);

        // ---- WiFi AP Setup ----
        let timg0 = TimerGroup::new(peripherals.TIMG0);
        let rng = Rng::new(peripherals.RNG);

        // esp-wifi requires PS.INTLEVEL == 0 (interrupts enabled) for init,
        // wifi::new, configure, and start. RTIC startup may have INTLEVEL > 0.
        let saved_ps: u32;
        unsafe { core::arch::asm!("rsil {0}, 0", out(reg) saved_ps) };

        static WIFI_INIT: StaticCell<esp_wifi::EspWifiController<'static>> = StaticCell::new();
        let wifi_init = esp_wifi::init(timg0.timer0, rng).unwrap();
        let wifi_init = WIFI_INIT.init(wifi_init);

        let (mut wifi_controller, interfaces) =
            esp_wifi::wifi::new(wifi_init, peripherals.WIFI).unwrap();

        let ap_config = AccessPointConfiguration {
            ssid: String::from("WROOMRTIC"),
            channel: 1,
            auth_method: AuthMethod::None,
            ..Default::default()
        };
        wifi_controller
            .set_configuration(&Configuration::AccessPoint(ap_config))
            .unwrap();
        wifi_controller.start().unwrap();

        // Keep WifiController alive — dropping it stops WiFi
        static WIFI_CTRL: StaticCell<WifiController<'static>> = StaticCell::new();
        WIFI_CTRL.init(wifi_controller);

        // Restore PS.INTLEVEL — blocks task dispatch until RTIC post_init
        unsafe { core::arch::asm!("wsr.ps {0}", "isync", in(reg) saved_ps) };

        println!("[WIFI] AP started: SSID='WROOMRTIC', channel=1, open");
        println!("[WIFI] IP: 192.168.4.1 (DHCP + DNS captive portal)");

        // ---- Start TIMG1 monotonic timer ----
        Mono::start();
        println!("[MONO] TIMG1 monotonic started (1 MHz tick)");

        // ---- smoltcp network stack ----
        let mut wifi_device = interfaces.ap;
        let mac = wifi_device.mac_address();
        let config = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        let mut iface = Interface::new(config, &mut wifi_device, smoltcp::time::Instant::ZERO);
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::v4(192, 168, 4, 1), 24))
                .unwrap();
        });

        // Socket storage: 2 TCP + 1 DHCP + 1 DNS = 4, with 2 spare
        static SOCKET_STORAGE: StaticCell<[SocketStorage<'static>; 6]> = StaticCell::new();
        let storage: &'static mut [SocketStorage<'static>] =
            SOCKET_STORAGE.init([SocketStorage::EMPTY; 6]);
        let mut sockets = SocketSet::new(storage);

        // ---- Multi-client TCP sockets (port 80) ----
        static TCP0_RX: StaticCell<[u8; 2048]> = StaticCell::new();
        static TCP0_TX: StaticCell<[u8; 8192]> = StaticCell::new();
        static TCP1_RX: StaticCell<[u8; 1024]> = StaticCell::new();
        static TCP1_TX: StaticCell<[u8; 4096]> = StaticCell::new();

        let tcp0 = tcp::Socket::new(
            tcp::SocketBuffer::new(TCP0_RX.init([0; 2048]).as_mut_slice()),
            tcp::SocketBuffer::new(TCP0_TX.init([0; 8192]).as_mut_slice()),
        );
        let tcp1 = tcp::Socket::new(
            tcp::SocketBuffer::new(TCP1_RX.init([0; 1024]).as_mut_slice()),
            tcp::SocketBuffer::new(TCP1_TX.init([0; 4096]).as_mut_slice()),
        );

        let h0 = sockets.add(tcp0);
        let h1 = sockets.add(tcp1);
        sockets.get_mut::<tcp::Socket>(h0).listen(80).unwrap();
        sockets.get_mut::<tcp::Socket>(h1).listen(80).unwrap();
        let tcp_handles = [h0, h1];

        // DHCP server socket (UDP port 67)
        static DHCP_RX_META: StaticCell<[udp::PacketMetadata; 4]> = StaticCell::new();
        static DHCP_RX_DATA: StaticCell<[u8; 1024]> = StaticCell::new();
        static DHCP_TX_META: StaticCell<[udp::PacketMetadata; 4]> = StaticCell::new();
        static DHCP_TX_DATA: StaticCell<[u8; 1024]> = StaticCell::new();
        let dhcp_socket = udp::Socket::new(
            udp::PacketBuffer::new(
                DHCP_RX_META.init([udp::PacketMetadata::EMPTY; 4]).as_mut_slice(),
                DHCP_RX_DATA.init([0; 1024]).as_mut_slice(),
            ),
            udp::PacketBuffer::new(
                DHCP_TX_META.init([udp::PacketMetadata::EMPTY; 4]).as_mut_slice(),
                DHCP_TX_DATA.init([0; 1024]).as_mut_slice(),
            ),
        );
        let dhcp_handle = sockets.add(dhcp_socket);
        sockets
            .get_mut::<udp::Socket>(dhcp_handle)
            .bind(67)
            .unwrap();

        // DNS spoof socket (UDP port 53)
        static DNS_RX_META: StaticCell<[udp::PacketMetadata; 4]> = StaticCell::new();
        static DNS_RX_DATA: StaticCell<[u8; 1024]> = StaticCell::new();
        static DNS_TX_META: StaticCell<[udp::PacketMetadata; 4]> = StaticCell::new();
        static DNS_TX_DATA: StaticCell<[u8; 1024]> = StaticCell::new();
        let dns_socket = udp::Socket::new(
            udp::PacketBuffer::new(
                DNS_RX_META.init([udp::PacketMetadata::EMPTY; 4]).as_mut_slice(),
                DNS_RX_DATA.init([0; 1024]).as_mut_slice(),
            ),
            udp::PacketBuffer::new(
                DNS_TX_META.init([udp::PacketMetadata::EMPTY; 4]).as_mut_slice(),
                DNS_TX_DATA.init([0; 1024]).as_mut_slice(),
            ),
        );
        let dns_handle = sockets.add(dns_socket);
        sockets
            .get_mut::<udp::Socket>(dns_handle)
            .bind(53)
            .unwrap();

        println!();
        println!("=== WROOMRTIC Async Captive Portal ===");
        println!("  SSID: WROOMRTIC (open, no password)");
        println!("  IP:   192.168.4.1");
        println!("  DHCP: auto (192.168.4.100+)");
        println!("  DNS:  all queries -> 192.168.4.1");
        println!("  HTTP: captive portal ({} TCP sockets)", TCP_SOCKET_COUNT);
        println!("  Mono: TIMG1 1MHz async timer");
        println!("======================================");
        println!("LED : GPIO2  (blue, activity indicator)");
        println!("DAC : GPIO25 (DAC1, 8-bit, 0-3.3V output)");
        println!("ADC : GPIO34 (ADC1_CH6, 12-bit, 11dB ~0-2450mV)");
        println!(">> Wire GPIO25 --> GPIO34 for loopback test <<");
        println!();

        // Spawn async tasks
        led_task::spawn().unwrap();
        network_task::spawn().unwrap();

        (
            Shared {},
            Local {
                button,
                led,
                dac,
                adc,
                adc_pin,
                wifi_device,
                wifi_interface: iface,
                wifi_sockets: sockets,
                tcp_handles,
                dhcp_handle,
                dns_handle,
            },
        )
    }

    // =====================================================================
    // LED + Morse task (async, priority 1)
    // =====================================================================

    const MORSE_UNIT_MS: u64 = 150;

    fn morse_pattern(ch: char) -> &'static [u8] {
        match ch {
            '0' => &[4, 4, 4, 4, 4],
            '1' => &[1, 4, 4, 4, 4],
            '2' => &[1, 1, 4, 4, 4],
            '3' => &[1, 1, 1, 4, 4],
            '4' => &[1, 1, 1, 1, 4],
            '5' => &[1, 1, 1, 1, 1],
            '6' => &[4, 1, 1, 1, 1],
            '7' => &[4, 4, 1, 1, 1],
            '8' => &[4, 4, 4, 1, 1],
            '9' => &[4, 4, 4, 4, 1],
            _ => &[1],
        }
    }

    /// LED + Morse task — runs forever.
    ///
    /// Between morse sequences, manages the traffic/ping LED blink by
    /// reading the `LED_OFF_TICK` atomic set by `network_task`.
    /// During morse, the LED is under direct control of the morse pattern.
    ///
    /// Because this is an async task, every `Mono::delay().await` yields
    /// to the executor — allowing `network_task` to run during morse
    /// delays. This is the core async benefit: **morse no longer blocks
    /// network polling**.
    #[task(local = [led], priority = 1)]
    async fn led_task(cx: led_task::Context) {
        let led = cx.local.led;
        let mut next_morse_tick: u64 = 15_000_000; // 15s in µs ticks

        println!("[LED] async task started: morse every 15s, traffic blinks");

        loop {
            let now = Mono::now().ticks();

            if now >= next_morse_tick {
                next_morse_tick = now + 15_000_000;
                let tc = crate::TRAFFIC_COUNT.load(core::sync::atomic::Ordering::Relaxed);
                let display = tc % 100;
                let msg = format!("{:02}", display);
                println!("[MORSE] traffic={} (morse: {})", tc, msg);

                // ── Async morse ──
                for (ci, ch) in msg.chars().enumerate() {
                    if ci > 0 {
                        // Inter-character gap: 8 units
                        led.set_low();
                        Mono::delay((MORSE_UNIT_MS * 8).millis()).await;
                    }
                    let pattern = morse_pattern(ch);
                    for (ei, &units) in pattern.iter().enumerate() {
                        if ei > 0 {
                            led.set_low();
                            Mono::delay(MORSE_UNIT_MS.millis()).await;
                        }
                        led.set_high();
                        Mono::delay((MORSE_UNIT_MS * units as u64).millis()).await;
                    }
                    led.set_low();
                }
                continue; // re-check time after morse
            }

            // ── Traffic/ping LED blink ──
            let now_ms = (now / 1000) as u32;
            let off_ms = crate::LED_OFF_TICK.load(core::sync::atomic::Ordering::Relaxed);
            if now_ms < off_ms {
                led.set_high();
            } else {
                led.set_low();
            }

            // Poll every 10ms (LED refresh rate)
            Mono::delay(10u64.millis()).await;
        }
    }

    // =====================================================================
    // Network task (async, priority 1)
    // =====================================================================

    /// Network polling task — runs forever, yields every 5ms.
    ///
    /// Handles all smoltcp polling, HTTP shell, DHCP, DNS.
    /// Multi-client: iterates over all TCP socket handles.
    #[task(local = [dac, adc, adc_pin, wifi_device, wifi_interface, wifi_sockets, tcp_handles, dhcp_handle, dns_handle], priority = 1)]
    async fn network_task(cx: network_task::Context) {
        let dac = cx.local.dac;
        let adc = cx.local.adc;
        let adc_pin = cx.local.adc_pin;
        let device = cx.local.wifi_device;
        let iface = cx.local.wifi_interface;
        let sockets = cx.local.wifi_sockets;
        let tcp_handles = *cx.local.tcp_handles;
        let dhcp_handle = *cx.local.dhcp_handle;
        let dns_handle = *cx.local.dns_handle;

        println!("[NET] async task started: {} TCP sockets on :80", TCP_SOCKET_COUNT);

        loop {
            Mono::delay(5u64.millis()).await;

            let now_ticks = Mono::now().ticks();
            let millis = (now_ticks / 1000) as i64;
            let cycle = (millis / 1000) as u32;

            // Poll smoltcp
            let timestamp = smoltcp::time::Instant::from_millis(millis);
            iface.poll(timestamp, device, sockets);

            // ── DHCP server ──
            handle_dhcp(sockets, dhcp_handle);

            // ── DNS spoof ──
            handle_dns(sockets, dns_handle);

            // ── HTTP on all TCP sockets ──
            for &tcp_handle in &tcp_handles {
                let (traffic, ping) =
                    handle_tcp(sockets, tcp_handle, millis, cycle, dac, adc, adc_pin);

                if traffic {
                    crate::TRAFFIC_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                }
                let now_ms = (now_ticks / 1000) as u32;
                if ping {
                    let off = now_ms + 150; // 150ms blink
                    crate::LED_OFF_TICK.fetch_max(off, core::sync::atomic::Ordering::Relaxed);
                } else if traffic {
                    let off = now_ms + 50; // 50ms blink
                    crate::LED_OFF_TICK.fetch_max(off, core::sync::atomic::Ordering::Relaxed);
                }

                // Re-listen aggressively after connection closes
                let socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
                if !socket.is_active() {
                    socket.abort();
                    let _ = socket.listen(80);
                }
            }
        }
    }

    /// Handle one TCP socket: parse HTTP, dispatch to shell/portal/audio.
    /// Returns (had_traffic, had_ping).
    fn handle_tcp(
        sockets: &mut SocketSet<'static>,
        tcp_handle: SocketHandle,
        millis: i64,
        cycle: u32,
        dac: &mut Dac<'static, DAC1<'static>>,
        adc: &mut Adc<'static, ADC1<'static>, esp_hal::Blocking>,
        adc_pin: &mut AdcPin<GPIO34<'static>, ADC1<'static>>,
    ) -> (bool, bool) {
        let socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
        if !socket.may_recv() {
            return (false, false);
        }

        let mut buf = [0u8; 512];
        let size = match socket.recv_slice(&mut buf) {
            Ok(s) if s > 0 => s,
            _ => return (false, false),
        };

        let req = &buf[..size];
        let path = if req.starts_with(b"GET ") {
            let end = req[4..].iter().position(|&b| b == b' ').unwrap_or(size - 4);
            core::str::from_utf8(&req[4..4 + end]).unwrap_or("/")
        } else {
            "/"
        };

        let mut had_ping = false;
        let request_count = crate::REQUEST_COUNT.load(core::sync::atomic::Ordering::Relaxed);
        let traffic_count = crate::TRAFFIC_COUNT.load(core::sync::atomic::Ordering::Relaxed);

        // Captive-portal probes → 302 redirect
        if path.contains("/generate_204")
            || path.contains("/hotspot-detect")
            || path.contains("/ncsi.txt")
            || path.contains("/connecttest")
            || path.contains("/success.txt")
            || path.contains("/success.html")
        {
            println!("[HTTP] 302 portal redirect: {}", path);
            let r = "HTTP/1.1 302 Found\r\nLocation:http://192.168.4.1/\r\nConnection:close\r\nContent-Length:0\r\n\r\n";
            let _ = socket.send_slice(r.as_bytes());
        } else if path == "/ping" {
            had_ping = true;
            crate::REQUEST_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let r = format!(
                "HTTP/1.1 200 OK\r\nContent-Type:text/plain\r\nConnection:close\r\n\r\nPONG {}",
                millis
            );
            let _ = socket.send_slice(r.as_bytes());
        } else if path == "/audio" {
            let hdr = b"HTTP/1.1 200 OK\r\nContent-Type:application/octet-stream\r\nContent-Length:1024\r\nAccess-Control-Allow-Origin:*\r\nConnection:close\r\n\r\n";
            let _ = socket.send_slice(hdr);
            let mut audio = [0u8; 1024];
            generate_audio_buffer(&mut audio);
            let _ = socket.send_slice(&audio);
        } else if path.starts_with("/cmd") {
            crate::REQUEST_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let cmd_raw = path.split("c=").nth(1).unwrap_or("help");
            let cmd = url_decode(cmd_raw);
            println!("[SHELL] {}", cmd);
            let body = exec_cmd(&cmd, millis, cycle, request_count, traffic_count, dac, adc, adc_pin);
            let r = format!(
                "HTTP/1.1 200 OK\r\nContent-Type:text/plain\r\nAccess-Control-Allow-Origin:*\r\nConnection:close\r\n\r\n{}",
                body
            );
            let _ = socket.send_slice(r.as_bytes());
        } else {
            let html = TERMINAL_HTML.as_bytes();
            match socket.send_slice(html) {
                Ok(n) => println!("[HTTP] 200 shell page: {} ({}/{} bytes)", path, n, html.len()),
                Err(e) => println!("[HTTP] send error: {:?}", e),
            }
        }
        socket.close();

        (true, had_ping)
    }

    // =====================================================================
    // Idle — just sleep (waiti), all work is in async tasks
    // =====================================================================

    #[idle]
    fn idle(_: idle::Context) -> ! {
        println!("[IDLE] sleeping (waiti 0) — all work in async tasks");
        loop {
            unsafe { core::arch::asm!("waiti 0") };
        }
    }

    /// BOOT button GPIO interrupt handler
    #[task(binds = GPIO, local = [button], priority = 2)]
    fn gpio_handler(cx: gpio_handler::Context) {
        cx.local.button.clear_interrupt();
        println!("BOOT button pressed!");
    }

    // =====================================================================
    // Network helpers (unchanged logic, adapted signatures)
    // =====================================================================

    fn find_dhcp_option(options: &[u8], target: u8) -> Option<u8> {
        let mut i = 0;
        while i < options.len() {
            let opt = options[i];
            if opt == 255 {
                break;
            }
            if opt == 0 {
                i += 1;
                continue;
            }
            if i + 1 >= options.len() {
                break;
            }
            let len = options[i + 1] as usize;
            if opt == target && len >= 1 && i + 2 < options.len() {
                return Some(options[i + 2]);
            }
            i += 2 + len;
        }
        None
    }

    fn handle_dhcp(sockets: &mut SocketSet<'static>, dhcp_handle: SocketHandle) {
        let socket = sockets.get_mut::<udp::Socket>(dhcp_handle);
        if !socket.can_recv() {
            return;
        }

        let mut req = [0u8; 576];
        let (req_len, _meta) = match socket.recv_slice(&mut req) {
            Ok(v) => v,
            Err(_) => return,
        };
        if req_len < 244 {
            return;
        }
        if req[0] != 1 {
            return;
        }
        if req[236..240] != [99, 130, 83, 99] {
            return;
        }

        let msg_type = match find_dhcp_option(&req[240..req_len], 53) {
            Some(t) => t,
            None => return,
        };
        let reply_type: u8 = match msg_type {
            1 => 2,
            3 => 5,
            _ => return,
        };

        let assigned_last = 100u8.wrapping_add(req[33] % 50);

        let mut resp = [0u8; 300];
        resp[0] = 2;
        resp[1] = 1;
        resp[2] = 6;
        resp[4..8].copy_from_slice(&req[4..8]);
        resp[10..12].copy_from_slice(&req[10..12]);
        resp[16..20].copy_from_slice(&[192, 168, 4, assigned_last]);
        resp[20..24].copy_from_slice(&[192, 168, 4, 1]);
        resp[28..44].copy_from_slice(&req[28..44]);
        resp[236..240].copy_from_slice(&[99, 130, 83, 99]);

        let mut p = 240;
        resp[p] = 53; resp[p + 1] = 1; resp[p + 2] = reply_type; p += 3;
        resp[p] = 54; resp[p + 1] = 4;
        resp[p + 2..p + 6].copy_from_slice(&[192, 168, 4, 1]); p += 6;
        resp[p] = 1; resp[p + 1] = 4;
        resp[p + 2..p + 6].copy_from_slice(&[255, 255, 255, 0]); p += 6;
        resp[p] = 3; resp[p + 1] = 4;
        resp[p + 2..p + 6].copy_from_slice(&[192, 168, 4, 1]); p += 6;
        resp[p] = 6; resp[p + 1] = 4;
        resp[p + 2..p + 6].copy_from_slice(&[192, 168, 4, 1]); p += 6;
        resp[p] = 51; resp[p + 1] = 4;
        resp[p + 2..p + 6].copy_from_slice(&3600u32.to_be_bytes()); p += 6;
        resp[p] = 255; p += 1;

        let dest = IpEndpoint::new(IpAddress::v4(255, 255, 255, 255), 68);
        let _ = socket.send_slice(&resp[..p], dest);

        println!(
            "[DHCP] {} -> 192.168.4.{}",
            if reply_type == 2 { "OFFER" } else { "ACK" },
            assigned_last
        );
    }

    fn handle_dns(sockets: &mut SocketSet<'static>, dns_handle: SocketHandle) {
        let socket = sockets.get_mut::<udp::Socket>(dns_handle);
        if !socket.can_recv() {
            return;
        }

        let mut query = [0u8; 512];
        let (qlen, meta) = match socket.recv_slice(&mut query) {
            Ok(v) => v,
            Err(_) => return,
        };
        if qlen < 12 {
            return;
        }

        let mut pos = 12usize;
        while pos < qlen {
            let label_len = query[pos] as usize;
            if label_len == 0 {
                pos += 1;
                break;
            }
            pos += 1 + label_len;
        }
        if pos + 4 > qlen {
            return;
        }
        let qtype = u16::from_be_bytes([query[pos], query[pos + 1]]);
        pos += 4;
        let question_end = pos;

        let mut resp = [0u8; 512];
        resp[..question_end].copy_from_slice(&query[..question_end]);
        resp[2] = 0x85;
        resp[3] = 0x80;
        resp[8] = 0; resp[9] = 0;
        resp[10] = 0; resp[11] = 0;

        let mut rpos = question_end;

        if qtype == 1 {
            resp[6] = 0; resp[7] = 1;
            resp[rpos] = 0xC0; resp[rpos + 1] = 0x0C; rpos += 2;
            resp[rpos] = 0; resp[rpos + 1] = 1; rpos += 2;
            resp[rpos] = 0; resp[rpos + 1] = 1; rpos += 2;
            resp[rpos..rpos + 4].copy_from_slice(&60u32.to_be_bytes()); rpos += 4;
            resp[rpos] = 0; resp[rpos + 1] = 4; rpos += 2;
            resp[rpos..rpos + 4].copy_from_slice(&[192, 168, 4, 1]); rpos += 4;
        } else {
            resp[6] = 0; resp[7] = 0;
        }

        let _ = socket.send_slice(&resp[..rpos], meta.endpoint);
    }

    // =====================================================================
    // Audio / shell / HTML (unchanged)
    // =====================================================================

    fn generate_audio_buffer(buf: &mut [u8]) {
        let mut prev: u32;
        unsafe { core::arch::asm!("rsr.ccount {0}", out(reg) prev) };
        let mut phase: u32 = 0;
        for sample in buf.iter_mut() {
            let cc: u32;
            unsafe { core::arch::asm!("rsr.ccount {0}", out(reg) cc) };
            let delta = cc.wrapping_sub(prev);
            prev = cc;
            let mod_val = (delta & 0xFFF) as u32;
            let step = 1802u32.wrapping_add(mod_val);
            phase = phase.wrapping_add(step);
            let p = ((phase >> 8) & 0xFF) as u8;
            let tri = if p < 128 { p * 2 } else { (255 - p) * 2 };
            *sample = tri;
        }
    }

    fn url_decode(s: &str) -> String {
        let mut out = String::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                let hi = (b[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
                let lo = (b[i + 2] as char).to_digit(16).unwrap_or(0) as u8;
                out.push((hi * 16 + lo) as char);
                i += 3;
            } else if b[i] == b'+' {
                out.push(' ');
                i += 1;
            } else {
                out.push(b[i] as char);
                i += 1;
            }
        }
        out
    }

    fn exec_cmd(
        cmd: &str,
        millis: i64,
        cycle: u32,
        request_count: u32,
        traffic_count: u32,
        dac: &mut Dac<'static, DAC1<'static>>,
        adc: &mut Adc<'static, ADC1<'static>, esp_hal::Blocking>,
        adc_pin: &mut AdcPin<GPIO34<'static>, ADC1<'static>>,
    ) -> String {
        let mut words = cmd.split_whitespace();
        let verb = words.next().unwrap_or("");
        let arg1 = words.next().unwrap_or("");

        match verb {
            "help" => String::from(concat!(
                "Commands (all run on the ESP32 chip):\n",
                "  help          this help\n",
                "  whoami        device identity\n",
                "  uptime        uptime\n",
                "  free          heap stats\n",
                "  status        system status\n",
                "  explain       what is this device\n",
                "  uname -a      system info\n",
                "  ip addr       network info\n",
                "  neofetch      system info (pretty)\n",
                "  date          build date\n",
                "  dmesg         boot log\n",
                "  ping          connectivity check\n",
                "  traffic       traffic counter\n",
                "  dac <0-255>   set DAC output\n",
                "  adc           read ADC value\n",
                "  echo <text>   echo text back\n",
                "  info          hardware info\n",
                "  led           LED mode info\n",
                "  audio on/off  MCU audio signature analyzer\n",
                "  listen        audio info\n",
                "  screensaver   ASCII worms (any key stops)\n",
                "  clear         clear screen (local)",
            )),
            "whoami" => String::from("root@wroomrtic (bare-metal RTIC device)"),
            "status" => format!(
                "Uptime:    {}ms\nCycle:     #{}\nRequests:  {}\nTraffic:   {} events\nLED:       auto (async)\nWiFi:      AP WROOMRTIC\nTasks:     network_task + led_task (async)",
                millis, cycle, request_count, traffic_count
            ),
            "explain" => String::from(concat!(
                "WROOMRTIC — Bare-Metal Async Embedded WiFi Device\n",
                "──────────────────────────────────────────────────\n",
                "This is a standalone embedded device (ESP32-WROOM-32)\n",
                "running bare-metal Rust with RTIC v2 async tasks.\n",
                "\n",
                "RTIC v2 async architecture:\n",
                "  - TIMG1 monotonic timer (1 MHz, non-blocking delays)\n",
                "  - network_task: polls smoltcp every 5ms via Mono::delay\n",
                "  - led_task: morse + traffic blinks via Mono::delay\n",
                "  - idle: waiti 0 (CPU sleeps between interrupts)\n",
                "  - Multi-client: 2 TCP sockets on port 80\n",
                "\n",
                "Morse no longer blocks networking — async yield.\n",
                "Every command you type executes on the chip, not in JS.",
            )),
            "led" => String::from("LED is automatic (async): 150ms blink=ping, 50ms=traffic, morse every 15s"),
            "dac" => {
                if let Ok(val) = arg1.parse::<u8>() {
                    dac.write(val);
                    format!("DAC: {}/255 (~{}mV)", val, val as u32 * 3300 / 256)
                } else {
                    String::from("Usage: dac <0-255>")
                }
            },
            "adc" => {
                let raw = nb::block!(adc.read_oneshot(adc_pin)).unwrap_or(0);
                format!("ADC: raw={} (~{}mV)", raw, raw as u32 * 2450 / 4095)
            },
            "ping" => format!("PONG {}ms", millis),
            "uptime" => {
                let secs = millis / 1000;
                format!("up {}s ({}ms)", secs, millis)
            },
            "free" | "heap" => {
                String::from("Heap configured: 72 KB (esp-alloc)\nNote: no runtime heap stats in bare-metal")
            },
            "traffic" | "wificount" => {
                format!("Traffic events: {}\nHTTP requests:  {}", traffic_count, request_count)
            },
            "uname" => {
                if arg1 == "-a" || arg1 == "--all" {
                    let secs = millis / 1000;
                    format!(
                        "wroomrtic 0.1.0 xtensa-lx6 ESP32-WROOM-32 240MHz rtic-v2-async esp-hal-1.0.0-rc.0 up {}s",
                        secs
                    )
                } else {
                    String::from("wroomrtic")
                }
            },
            "ip" => {
                if arg1 == "addr" || arg1 == "a" {
                    String::from(concat!(
                        "1: wlan0: <BROADCAST,MULTICAST,UP> mtu 1500\n",
                        "    inet 192.168.4.1/24 brd 192.168.4.255\n",
                        "    mode: AP  ssid: WROOMRTIC  channel: 1",
                    ))
                } else {
                    String::from("Usage: ip addr")
                }
            },
            "neofetch" => {
                let secs = millis / 1000;
                format!(concat!(
                    "  root@wroomrtic\n",
                    "  ─────────────────\n",
                    "  OS:     wroomrtic 0.1.0 (bare-metal)\n",
                    "  Host:   ESP32-WROOM-32\n",
                    "  Kernel: RTIC v2 async (Xtensa backend)\n",
                    "  CPU:    Xtensa LX6 (2) @ 240MHz\n",
                    "  Memory: 520 KB SRAM\n",
                    "  Flash:  4 MB\n",
                    "  Heap:   72 KB (esp-alloc)\n",
                    "  Uptime: {}s\n",
                    "  WiFi:   802.11 b/g/n AP ({} events)\n",
                    "  HAL:    esp-hal 1.0.0-rc.0\n",
                    "  Mono:   TIMG1 1MHz async\n",
                    "  Tasks:  network + led (async)\n",
                    "  Lang:   Rust (no_std)"),
                    secs, traffic_count
                )
            },
            "date" => {
                let secs = millis / 1000;
                format!("wroomrtic v0.1.0\nUptime: {}s (no RTC — no real-time clock)", secs)
            },
            "dmesg" => {
                let secs = millis / 1000;
                format!(concat!(
                    "[  0.000] boot: ESP32-WROOM-32 (Xtensa LX6)\n",
                    "[  0.001] heap: 72KB esp-alloc\n",
                    "[  0.010] gpio: LED=GPIO2, BTN=GPIO0\n",
                    "[  0.020] dac: DAC1 on GPIO25\n",
                    "[  0.021] adc: ADC1 ch6 on GPIO34\n",
                    "[  0.050] mono: TIMG1 1MHz monotonic started\n",
                    "[  0.100] wifi: AP WROOMRTIC ch1 open\n",
                    "[  0.200] net: smoltcp stack 192.168.4.1/24\n",
                    "[  0.210] dhcp: server on :67\n",
                    "[  0.220] dns: spoof on :53\n",
                    "[  0.230] http: shell on :80 (2 TCP sockets)\n",
                    "[  0.240] rtic: async tasks spawned\n",
                    "[live] uptime: {}s, requests: {}, traffic: {}"),
                    secs, request_count, traffic_count
                )
            },
            "reboot" => {
                String::from("Reboot not implemented in bare-metal RTIC (lift battery to reset)")
            },
            "info" => String::from(concat!(
                "Chip:      ESP32-WROOM-32 (Xtensa LX6)\n",
                "Clock:     240MHz\n",
                "Framework: RTIC v2 async (Xtensa backend)\n",
                "HAL:       esp-hal 1.0.0-rc.0\n",
                "Monotonic: TIMG1 (1 MHz, non-blocking)\n",
                "WiFi:      AP mode, ch1, open\n",
                "Heap:      72KB (esp-alloc)\n",
                "OS:        none (bare-metal)",
            )),
            "echo" => {
                let rest = cmd.strip_prefix("echo").unwrap_or("").trim();
                String::from(rest)
            },
            "audio" => {
                match arg1 {
                    "on" => String::from("__AUDIO_ON__"),
                    "off" => String::from("__AUDIO_OFF__"),
                    _ => String::from("Usage: audio on | audio off"),
                }
            },
            "listen" => String::from(concat!(
                "Audio Signature Analyzer (FM)\n",
                "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
                "Toggle: tap SND or type 'audio on/off'\n",
                "\n",
                "A 220Hz carrier is FM-modulated by CCOUNT jitter.\n",
                "The Xtensa CCOUNT register ticks at 240MHz.\n",
                "The delta between successive reads varies with:\n",
                "  - cache hits vs misses\n",
                "  - WiFi DMA bus contention\n",
                "  - interrupt preemption\n",
                "  - memory access patterns\n",
                "\n",
                "These micro-timing variations modulate the carrier\n",
                "frequency, producing tonal shifts you can hear.\n",
                "Anomalous bus activity = audible frequency change.\n",
                "\n",
                "Volume: 3% (quiet by design).\n",
                "Multi-client: 2 TCP sockets, async RTIC tasks.\n",
                "Morse runs concurrently — no network blocking.",
            )),
            "screensaver" => String::from("__WORM__"),
            _ => format!("{}: unknown device command\nType 'help' for commands", verb),
        }
    }

    // ---- Terminal shell HTML ----
    const TERMINAL_HTML: &str = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type:text/html\r\n",
        "Cache-Control:no-store\r\n",
        "Connection:close\r\n\r\n",
        "<!DOCTYPE html><html><head><title>WROOMRTIC</title>",
        "<meta name=viewport content='width=device-width,initial-scale=1,interactive-widget=resizes-content'>",
        "<style>",
        "*{margin:0;padding:0;box-sizing:border-box}",
        "html,body{background:#111;color:#0f0;font:14px/1.4 monospace;",
        "height:100%;overflow:hidden}",
        "#wrap{display:flex;flex-direction:column;height:100%}",
        "#priv{padding:3px 8px;background:#0a1a0a;border-bottom:1px solid #030;",
        "font-size:11px;color:#080;text-align:center;flex-shrink:0}",
        "#net{padding:3px 8px;background:#0a0a00;border-bottom:1px solid #220;",
        "font-size:11px;display:flex;justify-content:space-between;align-items:center;flex-shrink:0}",
        "#net .lbl{color:#880}",
        "#net .val{color:#ff0}",
        ".arr{display:inline-block;margin:0 2px;transition:color .15s}",
        ".arr.active{color:#0f0!important}",
        "#s{padding:4px 8px;background:#000;border-bottom:1px solid #030;font-size:12px;flex-shrink:0}",
        "#t{flex:1;overflow-y:auto;padding:8px;white-space:pre-wrap;min-height:0}",
        "#b{display:flex;padding:4px;background:#000;border-top:1px solid #030;flex-shrink:0}",
        "#i{flex:1;background:#000;color:#0f0;border:0;outline:0;font:inherit;padding:0 4px}",
        "@keyframes pulse{0%,100%{opacity:1}50%{opacity:.2}}",
        ".blink{animation:pulse .8s infinite}",
        "</style></head><body><div id=wrap>",
        "<div id=priv>&#x1F512; This device is friendly &mdash; ",
        "no cookies, no tracking, no metadata collection. Your browser data stays yours.</div>",
        "<div id=net>",
        "<span><span class=lbl>NET </span>",
        "<span id=txA class=arr style='color:#333'>&#9650;</span>",
        "<span id=rxA class=arr style='color:#333'>&#9660;</span></span>",
        "<span><span class=lbl>TX:</span><span id=txC class=val>0</span></span>",
        "<span><span class=lbl>RX:</span><span id=rxC class=val>0</span></span>",
        "<span><span class=lbl>RTT:</span><span id=rtt class=val>--</span></span>",
        "<span id=nst style='color:#880'>idle</span>",
        "<span id=aBtn style='color:#f80;cursor:pointer;margin-left:8px;border:1px solid #f80;padding:0 6px;border-radius:3px;font-size:12px' onclick='toggleAudio()'>SND</span>",
        "</div>",
        "<div id=s><span id=dot style='color:#0f0'>&#9679;</span> ",
        "<span id=st style='color:#0f0'>CONNECTED</span></div>",
        "<div id=t></div>",
        "<div id=b><span style='color:#0a0'>wroom&gt;&nbsp;</span>",
        "<input id=i autofocus enterkeyhint=send></div>",
        "</div>",
        "<script>",
        "var t=document.getElementById('t'),i=document.getElementById('i');",
        "var wrap=document.getElementById('wrap');",
        "var stEl=document.getElementById('st'),dotEl=document.getElementById('dot');",
        "function fixH(){var h=window.visualViewport?window.visualViewport.height:window.innerHeight;",
        "wrap.style.height=h+'px';t.scrollTop=1e9}",
        "fixH();",
        "if(window.visualViewport){window.visualViewport.addEventListener('resize',fixH);",
        "window.visualViewport.addEventListener('scroll',fixH)}",
        "window.addEventListener('resize',fixH);",
        "i.addEventListener('focus',function(){setTimeout(fixH,300)});",
        "var txA=document.getElementById('txA'),rxA=document.getElementById('rxA');",
        "var txCEl=document.getElementById('txC'),rxCEl=document.getElementById('rxC');",
        "var rttEl=document.getElementById('rtt'),nstEl=document.getElementById('nst');",
        "var txCount=0,rxCount=0;",
        "function flash(el){el.classList.add('active');el.style.color='#0f0';",
        "setTimeout(function(){el.classList.remove('active');el.style.color='#333'},200)}",
        "function tfetch(url){txCount++;txCEl.textContent=txCount;flash(txA);",
        "nstEl.textContent='\\u25CF';nstEl.style.color='#0f0';",
        "return fetch(url).then(function(r){",
        "rxCount++;rxCEl.textContent=rxCount;flash(rxA);",
        "nstEl.textContent='idle';nstEl.style.color='#880';return r})",
        ".catch(function(e){nstEl.textContent='\\u2716';nstEl.style.color='#f00';throw e})}",
        "document.body.onclick=function(){i.focus()};",
        "function w(s,c){var d=document.createElement('div');",
        "d.textContent=s;if(c)d.style.color=c;t.appendChild(d);t.scrollTop=1e9}",
        "w('WROOMRTIC Shell v3.0 (async RTIC v2)','#0f0');",
        "w('ESP32-WROOM-32 | RTIC v2 async | TIMG1 monotonic');",
        "w('All commands run on the chip. Nothing runs in JavaScript.','#080');",
        "w('LED: 150ms=ping, 50ms=traffic, morse every 15s');",
        "w('Type help or explain for commands');w('');",
        "var ss=0,worms=[];",
        "function stopSS(){ss=0;worms=[];t.innerHTML='';w('Screensaver stopped.','#0a0')}",
        "function runSS(){if(!ss)return;",
        "var W=Math.floor(t.clientWidth/8.4),H=Math.floor((t.clientHeight-40)/19.6);",
        "if(W<2||H<2)return;",
        "if(!worms.length){t.innerHTML='';",
        "for(var n=0;n<5;n++)worms.push({x:Math.floor(Math.random()*W),",
        "y:Math.floor(Math.random()*H),",
        "c:['#0f0','#0a0','#0d0','#080','#0f0'][n],",
        "ch:'~@#*o'.charAt(n),trail:[]})}",
        "var g=[];for(var y=0;y<H;y++){g[y]=[];for(var x=0;x<W;x++)g[y][x]=' '}",
        "worms.forEach(function(wr){",
        "var dx=[1,-1,0,0],dy=[0,0,1,-1],r=Math.floor(Math.random()*4);",
        "wr.x=(wr.x+dx[r]+W)%W;wr.y=(wr.y+dy[r]+H)%H;",
        "wr.trail.push({x:wr.x,y:wr.y});",
        "if(wr.trail.length>20)wr.trail.shift();",
        "wr.trail.forEach(function(p){if(p.y<H&&p.x<W)g[p.y][p.x]=wr.ch})});",
        "t.innerHTML='';var html='';",
        "for(var y=0;y<H;y++)html+=g[y].join('')+'\\n';",
        "var pre=document.createElement('pre');",
        "pre.style.cssText='color:#0f0;margin:0;line-height:1.4';",
        "pre.textContent=html;t.appendChild(pre);",
        "setTimeout(runSS,150)}",
        "i.onkeydown=function(e){if(e.key!='Enter'&&e.keyCode!==13)return;e.preventDefault();",
        "var c=i.value.trim();if(!c)return;i.value='';",
        "if(ss){stopSS();return}",
        "w('> '+c,'#0a0');",
        "if(c=='clear'){t.innerHTML='';return}",
        "tfetch('/cmd?c='+encodeURIComponent(c))",
        ".then(function(r){return r.text()})",
        ".then(function(s){if(s=='__WORM__'){ss=1;worms=[];runSS();return}",
        "if(s=='__AUDIO_ON__'){if(!audioOn)toggleAudio();w('Audio signature analyzer: ON','#0f0');return}",
        "if(s=='__AUDIO_OFF__'){if(audioOn)toggleAudio();w('Audio signature analyzer: OFF','#f80');return}",
        "s.split('\\n').forEach(function(l){w(l)})})",
        ".catch(function(){w('ERR: disconnected','#f00')})};",
        "var m=0,linkOK=true;",
        "setInterval(function(){var t0=performance.now();",
        "tfetch('/ping')",
        ".then(function(r){return r.text()})",
        ".then(function(){var ms=Math.round(performance.now()-t0);",
        "rttEl.textContent=ms+'ms';m=0;",
        "if(!linkOK){linkOK=true;",
        "dotEl.style.color='#0f0';stEl.style.color='#0f0';",
        "stEl.textContent='CONNECTED';stEl.className='';",
        "w('--- LINK RESTORED ---','#0f0')}})",
        ".catch(function(){rttEl.textContent='--';m++;",
        "if(m>=2&&linkOK){linkOK=false;",
        "dotEl.style.color='#f00';stEl.style.color='#f00';",
        "stEl.textContent='LINK LOST';stEl.className='blink';",
        "w('');",
        "w('=============================','#f00');",
        "w('    !!!  LINK  LOST  !!!     ','#f00');",
        "w('  watchdog: no ping reply    ','#f00');",
        "w('=============================','#f00');",
        "w('')}})},2000);",
        "var actx=null,audioOn=false,aGain=null;",
        "function toggleAudio(){",
        "if(!actx){actx=new(window.AudioContext||window.webkitAudioContext)();",
        "aGain=actx.createGain();aGain.gain.value=0.03;aGain.connect(actx.destination)}",
        "audioOn=!audioOn;",
        "document.getElementById('aBtn').style.color=audioOn?'#0f0':'#f80';",
        "document.getElementById('aBtn').style.borderColor=audioOn?'#0f0':'#f80';",
        "if(audioOn)fetchAudio()}",
        "function fetchAudio(){if(!audioOn)return;",
        "fetch('/audio').then(function(r){return r.arrayBuffer()}).then(function(ab){",
        "var arr=new Uint8Array(ab);",
        "var buf=actx.createBuffer(1,arr.length,8000);",
        "var ch=buf.getChannelData(0);",
        "for(var j=0;j<arr.length;j++)ch[j]=(arr[j]-128)/128;",
        "var src=actx.createBufferSource();src.buffer=buf;src.connect(aGain);",
        "src.start();src.onended=function(){setTimeout(fetchAudio,10)}",
        "}).catch(function(){setTimeout(fetchAudio,500)})}",
        "</script></body></html>",
    );
}
