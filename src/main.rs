#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![no_main]
#![no_std]
#![feature(asm_experimental_arch)]

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

/// WROOMRTIC — bare-metal RTIC demo for ESP32-WROOM-32.
///
/// Demonstrates:
///  - WiFi AP mode (SSID: WROOMRTIC, open, 192.168.4.1)
///  - HTTP status page on port 80
///  - Morse code 2-char status heartbeat on GPIO2 (blue LED), ~5 sec cycle
///  - DAC output on GPIO25 (DAC1, 8-bit) → ADC input on GPIO34 (ADC1_CH6, 12-bit)
///  - Wire GPIO25 to GPIO34 to verify loopback
///  - Hardware GPIO interrupt (BOOT button on GPIO0)
#[rtic::app(device = esp32, dispatchers = [FROM_CPU_INTR0, FROM_CPU_INTR1])]
mod app {
    use alloc::format;
    use alloc::string::String;
    use esp_backtrace as _;
    use esp_hal::analog::adc::{Adc, AdcConfig, AdcPin, Attenuation};
    use esp_hal::analog::dac::Dac;
    use esp_hal::delay::Delay;
    use esp_hal::gpio::{Event, Input, InputConfig, Level, Output, OutputConfig, Pull};
    use esp_hal::peripherals::{ADC1, DAC1, GPIO34};
    use esp_hal::rng::Rng;
    use esp_hal::timer::timg::TimerGroup;
    use esp_println::println;
    use esp_wifi::wifi::{
        AccessPointConfiguration, AuthMethod, Configuration, WifiController, WifiDevice,
    };
    use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet, SocketStorage};
    use smoltcp::socket::{tcp, udp};
    use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint};
    use static_cell::StaticCell;

    // Morse Farnsworth timing (user spec):
    //   dit  = 1 unit  = 150 ms
    //   dah  = 4 units = 600 ms
    //   element gap = 1 unit  = 150 ms
    //   char gap    = 2 units = 300 ms
    const DIT: u32 = 150;
    const DAH: u32 = DIT * 4;
    const ELEMENT_GAP: u32 = DIT;
    const CHAR_GAP: u32 = DIT * 2;

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

    #[init]
    fn init(_: init::Context) -> (Shared, Local) {
        println!("WROOMRTIC init");

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
        // Save PS and keep INTLEVEL=0 through all WiFi setup, then restore
        // before spawning tasks (so RTIC post_init controls enable timing).
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

        // Socket storage (static lifetime for SocketSet<'static>)
        static SOCKET_STORAGE: StaticCell<[SocketStorage<'static>; 5]> = StaticCell::new();
        let storage: &'static mut [SocketStorage<'static>] =
            SOCKET_STORAGE.init([SocketStorage::EMPTY; 5]);
        let mut sockets = SocketSet::new(storage);

        // TCP socket for HTTP server — static buffers
        static TCP_RX: StaticCell<[u8; 2048]> = StaticCell::new();
        static TCP_TX: StaticCell<[u8; 4096]> = StaticCell::new();
        let tcp_rx: &'static mut [u8] = TCP_RX.init([0; 2048]);
        let tcp_tx: &'static mut [u8] = TCP_TX.init([0; 4096]);
        let tcp_socket = tcp::Socket::new(
            tcp::SocketBuffer::new(tcp_rx),
            tcp::SocketBuffer::new(tcp_tx),
        );
        let tcp_handle = sockets.add(tcp_socket);

        // Start listening on port 80
        let socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
        socket.listen(80).unwrap();

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
        println!("=== WROOMRTIC Captive Portal ===");
        println!("  SSID: WROOMRTIC (open, no password)");
        println!("  IP:   192.168.4.1");
        println!("  DHCP: auto (192.168.4.100+)");
        println!("  DNS:  all queries -> 192.168.4.1");
        println!("  HTTP: captive portal landing page");
        println!("=================================");
        println!("LED : GPIO2  (blue, Morse heartbeat)");
        println!("DAC : GPIO25 (DAC1, 8-bit, 0-3.3V output)");
        println!("ADC : GPIO34 (ADC1_CH6, 12-bit, 11dB ~0-2450mV)");
        println!(">> Wire GPIO25 --> GPIO34 for loopback test <<");
        println!();

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
                tcp_handle,
                dhcp_handle,
                dns_handle,
            },
        )
    }

    // ---- Network helpers (called from idle) ----

    /// Find a DHCP option by code; return its first data byte.
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

    /// Minimal DHCP server — assigns IPs from 192.168.4.100+ based on client MAC.
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
        } // not BOOTREQUEST
        if req[236..240] != [99, 130, 83, 99] {
            return;
        } // bad magic cookie

        let msg_type = match find_dhcp_option(&req[240..req_len], 53) {
            Some(t) => t,
            None => return,
        };
        let reply_type: u8 = match msg_type {
            1 => 2, // DISCOVER -> OFFER
            3 => 5, // REQUEST -> ACK
            _ => return,
        };

        // Deterministic IP from client MAC last byte: 192.168.4.(100 + mac[5] % 50)
        let assigned_last = 100u8.wrapping_add(req[33] % 50);

        // Build DHCP reply (BOOTREPLY)
        let mut resp = [0u8; 300];
        resp[0] = 2; // op: BOOTREPLY
        resp[1] = 1; // htype: Ethernet
        resp[2] = 6; // hlen
        resp[4..8].copy_from_slice(&req[4..8]); // xid
        resp[10..12].copy_from_slice(&req[10..12]); // flags
        resp[16..20].copy_from_slice(&[192, 168, 4, assigned_last]); // yiaddr
        resp[20..24].copy_from_slice(&[192, 168, 4, 1]); // siaddr (server)
        resp[28..44].copy_from_slice(&req[28..44]); // chaddr
        resp[236..240].copy_from_slice(&[99, 130, 83, 99]); // magic cookie

        let mut p = 240;
        // Option 53: DHCP Message Type
        resp[p] = 53;
        resp[p + 1] = 1;
        resp[p + 2] = reply_type;
        p += 3;
        // Option 54: Server Identifier
        resp[p] = 54;
        resp[p + 1] = 4;
        resp[p + 2..p + 6].copy_from_slice(&[192, 168, 4, 1]);
        p += 6;
        // Option 1: Subnet Mask
        resp[p] = 1;
        resp[p + 1] = 4;
        resp[p + 2..p + 6].copy_from_slice(&[255, 255, 255, 0]);
        p += 6;
        // Option 3: Router (gateway)
        resp[p] = 3;
        resp[p + 1] = 4;
        resp[p + 2..p + 6].copy_from_slice(&[192, 168, 4, 1]);
        p += 6;
        // Option 6: DNS server (us — for captive portal spoofing)
        resp[p] = 6;
        resp[p + 1] = 4;
        resp[p + 2..p + 6].copy_from_slice(&[192, 168, 4, 1]);
        p += 6;
        // Option 51: Lease Time (3600 = 1 hour)
        resp[p] = 51;
        resp[p + 1] = 4;
        resp[p + 2..p + 6].copy_from_slice(&3600u32.to_be_bytes());
        p += 6;
        // Option 255: End
        resp[p] = 255;
        p += 1;

        // Always broadcast reply (client has no IP yet)
        let dest = IpEndpoint::new(IpAddress::v4(255, 255, 255, 255), 68);
        let _ = socket.send_slice(&resp[..p], dest);

        println!(
            "[DHCP] {} -> 192.168.4.{}",
            if reply_type == 2 { "OFFER" } else { "ACK" },
            assigned_last
        );
    }

    /// DNS spoof — respond to ALL A-record queries with 192.168.4.1.
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

        // Walk past question name (sequence of length-prefixed labels ending with 0)
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
        pos += 4; // skip QTYPE + QCLASS
        let question_end = pos;

        // Build response
        let mut resp = [0u8; 512];
        resp[..question_end].copy_from_slice(&query[..question_end]);

        // Set response flags: QR=1, AA=1, RD=1, RA=1
        resp[2] = 0x85;
        resp[3] = 0x80;
        // NSCOUNT=0, ARCOUNT=0
        resp[8] = 0;
        resp[9] = 0;
        resp[10] = 0;
        resp[11] = 0;

        let mut rpos = question_end;

        // Only answer A-record queries (type 1) with our IP
        if qtype == 1 {
            resp[6] = 0;
            resp[7] = 1; // ANCOUNT = 1
            // Name: pointer to offset 12
            resp[rpos] = 0xC0;
            resp[rpos + 1] = 0x0C;
            rpos += 2;
            // Type A
            resp[rpos] = 0;
            resp[rpos + 1] = 1;
            rpos += 2;
            // Class IN
            resp[rpos] = 0;
            resp[rpos + 1] = 1;
            rpos += 2;
            // TTL 60s
            resp[rpos..rpos + 4].copy_from_slice(&60u32.to_be_bytes());
            rpos += 4;
            // RDLENGTH 4
            resp[rpos] = 0;
            resp[rpos + 1] = 4;
            rpos += 2;
            // RDATA: 192.168.4.1
            resp[rpos..rpos + 4].copy_from_slice(&[192, 168, 4, 1]);
            rpos += 4;
        } else {
            resp[6] = 0;
            resp[7] = 0; // ANCOUNT = 0 for non-A queries
        }

        let _ = socket.send_slice(&resp[..rpos], meta.endpoint);
    }

    // ---- Terminal shell HTML (served at GET /) ----
    const TERMINAL_HTML: &str = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type:text/html\r\n",
        "Cache-Control:no-store\r\n",
        "Connection:close\r\n\r\n",
        "<!DOCTYPE html><html><head><title>WROOMRTIC</title>",
        "<meta name=viewport content='width=device-width,initial-scale=1'>",
        "<style>",
        "*{margin:0;padding:0;box-sizing:border-box}",
        "body{background:#111;color:#0f0;font:14px/1.4 monospace;height:100vh;",
        "display:flex;flex-direction:column}",
        "#t{flex:1;overflow-y:auto;padding:8px 8px 40px;white-space:pre-wrap}",
        "#b{position:fixed;bottom:0;left:0;right:0;display:flex;padding:4px;",
        "background:#000;border-top:1px solid #030;z-index:10}",
        "#i{flex:1;background:#000;color:#0f0;border:0;outline:0;font:inherit;padding:0 4px}",
        "</style></head><body>",
        "<div id=t></div>",
        "<div id=b><span style='color:#0a0'>wroom&gt;&nbsp;</span>",
        "<input id=i autofocus></div>",
        "<script>",
        "var t=document.getElementById('t'),i=document.getElementById('i');",
        "document.body.onclick=function(){i.focus()};",
        "function w(s,c){var d=document.createElement('div');",
        "d.textContent=s;if(c)d.style.color=c;t.appendChild(d);t.scrollTop=1e9}",
        "w('WROOMRTIC Shell v1.0','#0f0');",
        "w('ESP32-WROOM-32 | RTIC v2 | bare-metal');",
        "w('Type help for commands');w('');",
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
        "i.onkeydown=function(e){if(e.key!='Enter')return;",
        "var c=i.value.trim();if(!c)return;i.value='';",
        "if(ss){stopSS();return}",
        "w('> '+c,'#0a0');",
        "if(c=='clear'){t.innerHTML='';return}",
        "fetch('/cmd?c='+encodeURIComponent(c))",
        ".then(function(r){return r.text()})",
        ".then(function(s){if(s=='__WORM__'){ss=1;worms=[];runSS();return}",
        "s.split('\\n').forEach(function(l){w(l)})})",
        ".catch(function(){w('ERR: disconnected','#f00')})};",
        "var m=0;setInterval(function(){fetch('/ping')",
        ".then(function(){m=0})",
        ".catch(function(){if(++m>2)w('[LINK DOWN]','#f00')})},3000);",
        "</script></body></html>",
    );

    /// Decode %XX and + in URL query values.
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

    /// Execute a shell command and return the response text.
    fn exec_cmd(
        cmd: &str,
        millis: i64,
        cycle: u32,
        led: &mut Output<'static>,
        dac: &mut Dac<'static, DAC1<'static>>,
        adc: &mut Adc<'static, ADC1<'static>, esp_hal::Blocking>,
        adc_pin: &mut AdcPin<GPIO34<'static>, ADC1<'static>>,
    ) -> String {
        let mut words = cmd.split_whitespace();
        let verb = words.next().unwrap_or("");
        let arg1 = words.next().unwrap_or("");

        match verb {
            "help" => String::from(concat!(
                "Commands:\n",
                "  help          show this help\n",
                "  status        system status\n",
                "  led <on|off>  control LED\n",
                "  dac <0-255>   set DAC output\n",
                "  adc           read ADC value\n",
                "  ping          connectivity check\n",
                "  uptime        ms since boot\n",
                "  info          hardware info\n",
                "  echo <text>   echo text back\n",
                "  screensaver   ASCII worms (any key stops)\n",
                "  clear         clear screen (local)",
            )),
            "status" => format!(
                "Uptime:    {}ms\nCycle:     #{}\nHeartbeat: OK\nWiFi:      AP WROOMRTIC",
                millis, cycle
            ),
            "led" => match arg1 {
                "on" => { led.set_high(); String::from("LED: ON") },
                "off" => { led.set_low(); String::from("LED: OFF") },
                _ => String::from("Usage: led <on|off>"),
            },
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
            "uptime" => format!("{}ms", millis),
            "info" => String::from(concat!(
                "Chip:      ESP32-WROOM-32 (Xtensa LX6)\n",
                "Clock:     240MHz\n",
                "Framework: RTIC v2 (Xtensa backend)\n",
                "HAL:       esp-hal 1.0.0-rc.0\n",
                "WiFi:      AP mode, ch1, open\n",
                "Heap:      72KB (esp-alloc)",
            )),
            "echo" => {
                let rest = cmd.strip_prefix("echo").unwrap_or("").trim();
                String::from(rest)
            },
            "screensaver" => String::from("__WORM__"),
            _ => format!("Unknown: {}\nType 'help' for commands", verb),
        }
    }

    /// Poll smoltcp + handle HTTP shell, DHCP, and DNS.
    fn poll_network(
        millis: &mut i64,
        device: &mut WifiDevice<'static>,
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
        tcp_handle: SocketHandle,
        dhcp_handle: SocketHandle,
        dns_handle: SocketHandle,
        led: &mut Output<'static>,
        dac: &mut Dac<'static, DAC1<'static>>,
        adc: &mut Adc<'static, ADC1<'static>, esp_hal::Blocking>,
        adc_pin: &mut AdcPin<GPIO34<'static>, ADC1<'static>>,
        cycle: u32,
    ) {
        let timestamp = smoltcp::time::Instant::from_millis(*millis);
        iface.poll(timestamp, device, sockets);

        // ---- HTTP shell + captive portal (TCP port 80) ----
        let socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
        if socket.may_recv() {
            let mut buf = [0u8; 512];
            if let Ok(size) = socket.recv_slice(&mut buf) {
                if size > 0 {
                    let req = &buf[..size];

                    // Extract path from "GET /path HTTP/1.1"
                    let path = if req.starts_with(b"GET ") {
                        let end = req[4..].iter().position(|&b| b == b' ')
                            .unwrap_or(size - 4);
                        core::str::from_utf8(&req[4..4 + end]).unwrap_or("/")
                    } else {
                        "/"
                    };

                    // Intercept ALL captive-portal probes with 302 → landing page.
                    // This forces Android, iOS, and Windows to show "Sign in to network".
                    if path.contains("/generate_204")
                        || path.contains("/hotspot-detect")
                        || path.contains("/ncsi.txt")
                        || path.contains("/connecttest")
                        || path.contains("/success.txt")
                        || path.contains("/success.html") {
                        println!("[HTTP] 302 portal redirect: {}", path);
                        let r = "HTTP/1.1 302 Found\r\nLocation:http://192.168.4.1/\r\nConnection:close\r\nContent-Length:0\r\n\r\n";
                        let _ = socket.send_slice(r.as_bytes());
                    } else if path == "/ping" {
                        let r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type:text/plain\r\nConnection:close\r\n\r\nPONG {}",
                            millis
                        );
                        let _ = socket.send_slice(r.as_bytes());
                    } else if path.starts_with("/cmd") {
                        // /cmd?c=led%20on → extract and decode command
                        let cmd_raw = path.split("c=").nth(1).unwrap_or("help");
                        let cmd = url_decode(cmd_raw);
                        println!("[SHELL] {}", cmd);
                        let body = exec_cmd(&cmd, *millis, cycle, led, dac, adc, adc_pin);
                        let r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type:text/plain\r\nAccess-Control-Allow-Origin:*\r\nConnection:close\r\n\r\n{}",
                            body
                        );
                        let _ = socket.send_slice(r.as_bytes());
                    } else {
                        // Serve terminal shell page for any unknown path
                        println!("[HTTP] 200 shell page: {}", path);
                        let _ = socket.send_slice(TERMINAL_HTML.as_bytes());
                    }
                    socket.close();
                }
            }
        }
        // Re-listen aggressively: is_active() is false for Closed, Listen,
        // and TIME_WAIT states. This avoids the 30+ second TIME_WAIT delay
        // that blocks subsequent HTTP requests (ping, cmd, page loads).
        if !socket.is_active() {
            socket.abort();
            let _ = socket.listen(80);
        }

        // ---- DHCP server (UDP port 67) ----
        handle_dhcp(sockets, dhcp_handle);

        // ---- DNS spoof (UDP port 53) ----
        handle_dns(sockets, dns_handle);
    }

    /// Blocking delay that polls WiFi every 10ms.
    fn wifi_delay(
        delay: &Delay,
        ms: u32,
        millis: &mut i64,
        device: &mut WifiDevice<'static>,
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
        tcp_handle: SocketHandle,
        dhcp_handle: SocketHandle,
        dns_handle: SocketHandle,
        led: &mut Output<'static>,
        dac: &mut Dac<'static, DAC1<'static>>,
        adc: &mut Adc<'static, ADC1<'static>, esp_hal::Blocking>,
        adc_pin: &mut AdcPin<GPIO34<'static>, ADC1<'static>>,
        cycle: u32,
    ) {
        let chunks = ms / 10;
        let remainder = ms % 10;
        for _ in 0..chunks {
            delay.delay_millis(10);
            *millis += 10;
            poll_network(millis, device, iface, sockets, tcp_handle, dhcp_handle, dns_handle, led, dac, adc, adc_pin, cycle);
        }
        if remainder > 0 {
            delay.delay_millis(remainder);
            *millis += remainder as i64;
        }
    }

    // ---- Morse helpers ----

    /// Morse patterns: false = dit, true = dah.
    fn morse_pattern(ch: char) -> &'static [bool] {
        match ch {
            'A' => &[false, true],
            'D' => &[true, false, false],
            'E' => &[false],
            'G' => &[true, true, false],
            'H' => &[false, false, false, false],
            'I' => &[false, false],
            'K' => &[true, false, true],
            'L' => &[false, true, false, false],
            'N' => &[true, false],
            'O' => &[true, true, true],
            'P' => &[false, true, true, false],
            'R' => &[false, true, false],
            'T' => &[true],
            'U' => &[false, false, true],
            'X' => &[true, false, false, true],
            _ => &[],
        }
    }

    /// Morse pattern as ASCII string for display.
    fn morse_display(ch: char) -> &'static str {
        match ch {
            'A' => ".-",   'D' => "-..", 'E' => ".",    'G' => "--.",
            'H' => "....", 'I' => "..",  'K' => "-.-",  'L' => ".-..",
            'N' => "-.",   'O' => "---", 'P' => ".--.", 'R' => ".-.",
            'T' => "-",    'U' => "..-", 'X' => "-..-",
            _ => "?",
        }
    }

    /// Idle loop: interleaves Morse heartbeat + DAC/ADC with WiFi polling.
    /// WiFi + shell commands polled every ~10ms during all delays.
    #[idle(local = [led, dac, adc, adc_pin, wifi_device, wifi_interface, wifi_sockets, tcp_handle, dhcp_handle, dns_handle])]
    fn idle(cx: idle::Context) -> ! {
        let led = cx.local.led;
        let dac = cx.local.dac;
        let adc = cx.local.adc;
        let pin = cx.local.adc_pin;
        let device = cx.local.wifi_device;
        let iface = cx.local.wifi_interface;
        let sockets = cx.local.wifi_sockets;
        let tcp_handle = *cx.local.tcp_handle;
        let dhcp_handle = *cx.local.dhcp_handle;
        let dns_handle = *cx.local.dns_handle;
        let delay = Delay::new();
        let mut millis: i64 = 0;
        let mut cycle: u32 = 0;

        // Macro to reduce wifi_delay call verbosity
        macro_rules! wdelay {
            ($ms:expr) => {
                wifi_delay(&delay, $ms, &mut millis, device, iface, sockets,
                    tcp_handle, dhcp_handle, dns_handle, led, dac, adc, pin, cycle)
            };
        }

        let status: &[u8; 2] = b"OK";
        let ch1 = status[0] as char;
        let ch2 = status[1] as char;

        println!(
            "[HEARTBEAT] status='{}{}' morse: {} {}  |  Shell on :80",
            ch1, ch2, morse_display(ch1), morse_display(ch2)
        );

        loop {
            cycle += 1;

            // ---- Phase 1: Morse heartbeat ----
            println!("[MORSE] #{} '{}{}'", cycle, ch1, ch2);
            for ch in [ch1, ch2] {
                for (i, &is_dah) in morse_pattern(ch).iter().enumerate() {
                    let dur = if is_dah { DAH } else { DIT };
                    led.set_high();
                    wdelay!(dur);
                    led.set_low();
                    wdelay!(ELEMENT_GAP);
                    if i + 1 == morse_pattern(ch).len() {
                        wdelay!(CHAR_GAP.saturating_sub(ELEMENT_GAP));
                    }
                }
            }

            // ---- Phase 2: DAC->ADC loopback ramp ----
            println!("[DAC->ADC] loopback ramp (GPIO25 -> GPIO34):");
            let steps: [u8; 11] = [0, 25, 51, 76, 102, 127, 153, 178, 204, 229, 255];
            for &dac_val in &steps {
                dac.write(dac_val);
                wdelay!(50);
                let adc_raw = nb::block!(adc.read_oneshot(pin)).unwrap_or(0);
                let expect_mv = dac_val as u32 * 3300 / 256;
                let meas_mv = adc_raw as u32 * 2450 / 4095;
                let sat = if expect_mv > 2450 { " SAT" } else { "" };
                println!(
                    "  DAC={:3}/255 ({:4}mV) -> ADC={:4} ({:4}mV){}",
                    dac_val, expect_mv, adc_raw, meas_mv, sat
                );
            }
            dac.write(0);
            println!("[HEARTBEAT] cycle #{} done", cycle);
            println!();

            // Pad to ~5 sec total cycle
            wdelay!(200);
        }
    }

    /// BOOT button GPIO interrupt handler
    #[task(binds = GPIO, local = [button], priority = 2)]
    fn gpio_handler(cx: gpio_handler::Context) {
        cx.local.button.clear_interrupt();
        println!("BOOT button pressed!");
    }
}
