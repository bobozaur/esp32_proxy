#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use embassy_executor::_export::StaticCell;
use embassy_executor::{Executor, Spawner};
use embassy_futures::select::{select, select3};
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Stack, StackResources};

use embassy_net::tcp::Error as TcpError;
use embassy_time::{Duration, Timer};
use embedded_io::asynch::Write;
use embedded_svc::wifi::{ClientConfiguration, Configuration, Wifi};
use esp32_hal::clock::{ClockControl, CpuClock};
use esp32_hal::Rng;
use esp32_hal::{embassy, peripherals::Peripherals, prelude::*, timer::TimerGroup, Rtc};
use esp_backtrace as _;
use esp_println::logger::init_logger;
use esp_wifi::wifi::{WifiController, WifiDevice, WifiEvent, WifiMode, WifiState};
use esp_wifi::{initialize, EspWifiInitFor};
use log::{debug, error, info};
use smoltcp::wire::DnsQueryType;

const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASS");

const BUFFER_SIZE: usize = 8192;
static mut RX_BUFFERS: [[u8; BUFFER_SIZE]; NUM_TUNNELS * 2] = [[0; BUFFER_SIZE]; NUM_TUNNELS * 2];
static mut TX_BUFFERS: [[u8; BUFFER_SIZE]; NUM_TUNNELS * 2] = [[0; BUFFER_SIZE]; NUM_TUNNELS * 2];
static mut APP_BUFFERS: [[u8; BUFFER_SIZE]; NUM_TUNNELS] = [[0; BUFFER_SIZE]; NUM_TUNNELS];

macro_rules! singleton {
    ($val:expr) => {{
        type T = impl Sized;
        static STATIC_CELL: StaticCell<T> = StaticCell::new();
        let (x,) = STATIC_CELL.init(($val,));
        x
    }};
}

static EXECUTOR: StaticCell<Executor> = StaticCell::new();

#[entry]
fn main() -> ! {
    init_logger(log::LevelFilter::Info);

    let peripherals = Peripherals::take();

    let system = peripherals.DPORT.split();
    let mut peripheral_clock_control = system.peripheral_clock_control;
    let clocks = ClockControl::configure(system.clock_control, CpuClock::Clock240MHz).freeze();

    let mut rtc = Rtc::new(peripherals.RTC_CNTL);
    rtc.rwdt.disable();

    let mut timer_group0 =
        TimerGroup::new(peripherals.TIMG0, &clocks, &mut peripheral_clock_control);
    let mut timer_group1 =
        TimerGroup::new(peripherals.TIMG1, &clocks, &mut peripheral_clock_control);

    timer_group0.wdt.disable();
    timer_group1.wdt.disable();

    embassy::init(&clocks, timer_group0.timer0);
    let mut rng = Rng::new(peripherals.RNG);
    let net_seed = rng.random().into();
    let init = initialize(
        EspWifiInitFor::Wifi,
        timer_group1.timer0,
        rng,
        system.radio_clock_control,
        &clocks,
    )
    .unwrap();

    let (wifi, _) = peripherals.RADIO.split();
    let (wifi_interface, controller) = esp_wifi::wifi::new_with_mode(&init, wifi, WifiMode::Sta);

    let config = Config::dhcpv4(Default::default());

    // Init network stack
    let stack = &*singleton!(Stack::new(
        wifi_interface,
        config,
        singleton!(StackResources::<{ NUM_TUNNELS * 2 + 2 }>::new()),
        net_seed
    ));

    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.spawn(connection(controller, stack, spawner)).ok();
        spawner.spawn(net_task(stack)).ok();
    });
}

const NUM_TUNNELS: usize = 4;
#[embassy_executor::task(pool_size = 4)]
async fn task(stack: &'static Stack<WifiDevice<'static>>, idx: usize) {
    unsafe {
        loop {
            let mut src_socket = TcpSocket::new(stack, &mut RX_BUFFERS[idx], &mut TX_BUFFERS[idx]);
            let mut dest_socket = TcpSocket::new(
                stack,
                &mut RX_BUFFERS[idx + NUM_TUNNELS],
                &mut TX_BUFFERS[idx + NUM_TUNNELS],
            );

            info!("Socket listening...");

            if let Err(e) = src_socket.accept(8118).await {
                error!("listen error: {e:?}");
                continue;
            }

            info!("source connected!");

            let n = match src_socket.read(&mut APP_BUFFERS[idx]).await {
                Ok(n) => n,
                Err(e) => {
                    error!("error when expecting CONNECT request: {e:?}");
                    sock_cleanup(&mut src_socket).await;
                    continue;
                }
            };

            let req_str = match core::str::from_utf8(&APP_BUFFERS[idx][..n]) {
                Ok(s) => s,
                Err(e) => {
                    error!("error when parsing CONNECT request: {e:?}");
                    sock_cleanup(&mut src_socket).await;
                    continue;
                }
            };

            let (host, port) = match parse_connect(req_str) {
                Ok(v) => v,
                Err(e) => {
                    error!("{e}");
                    sock_cleanup(&mut src_socket).await;
                    continue;
                }
            };

            let dns_results = match stack.dns_query(host, DnsQueryType::A).await {
                Ok(v) => v,
                Err(e) => {
                    error!("could not resolve host to IP: {e:?}");
                    sock_cleanup(&mut src_socket).await;
                    continue;
                }
            };

            let mut connected = false;
            for addr in dns_results {
                info!("resolved host to address: {addr}");
                if dest_socket.connect((addr, port)).await.is_ok() {
                    connected = true;
                    break;
                }
            }

            if !connected {
                error!("could not connect to any resolved host IPs!");
                sock_cleanup(&mut src_socket).await;
                continue;
            }

            info!("destination connected!");

            let r = src_socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await;

            let r2 = src_socket.flush().await;
            let r = r.and(r2);
            if let Err(e) = r {
                error!("error replying to CONNECT request: {e:?}");
                sock_cleanup(&mut src_socket).await;
                sock_cleanup(&mut dest_socket).await;
                continue;
            }

            info!("CONNECT reply sent to source!");

            let buf1 = core::slice::from_raw_parts_mut(
                APP_BUFFERS[idx].as_mut_ptr(),
                APP_BUFFERS[idx].len(),
            );
            let buf2 = core::slice::from_raw_parts_mut(
                APP_BUFFERS[idx].as_mut_ptr(),
                APP_BUFFERS[idx].len(),
            );

            loop {
                let res = select3(
                    sock_read(&mut src_socket, buf1),
                    sock_read(&mut dest_socket, buf2),
                    Timer::after(Duration::from_secs(1)),
                )
                .await;

                let should_continue = match res {
                    embassy_futures::select::Either3::First(r) => match r {
                        Ok(0) => Ok(false),
                        Ok(n) => sock_write(&mut dest_socket, &APP_BUFFERS[idx][..n]).await,
                        Err(e) => Err(e),
                    },
                    embassy_futures::select::Either3::Second(r) => match r {
                        Ok(0) => Ok(false),
                        Ok(n) => sock_write(&mut src_socket, &APP_BUFFERS[idx][..n]).await,
                        Err(e) => Err(e),
                    },
                    embassy_futures::select::Either3::Third(_) => Ok(false),
                };

                let should_continue = match should_continue {
                    Ok(v) => v,
                    Err(e) => {
                        info!("error: {e:?}");
                        false
                    }
                };

                if !should_continue {
                    break;
                }
            }

            sock_cleanup(&mut src_socket).await;
            sock_cleanup(&mut dest_socket).await;
        }
    }
}

#[embassy_executor::task]
async fn connection(
    mut controller: WifiController<'static>,
    stack: &'static Stack<WifiDevice<'static>>,
    spawner: Spawner,
) {
    info!("start connection task");
    info!("Device capabilities: {:?}", controller.get_capabilities());

    if let WifiState::StaConnected = esp_wifi::wifi::get_wifi_state() {
        // wait until we're no longer connected
        controller.wait_for_event(WifiEvent::StaDisconnected).await;
        Timer::after(Duration::from_millis(5000)).await
    }

    if !matches!(controller.is_started(), Ok(true)) {
        let client_config = Configuration::Client(ClientConfiguration {
            ssid: SSID.into(),
            password: PASSWORD.into(),
            ..Default::default()
        });
        controller.set_configuration(&client_config).unwrap();
        info!("Starting wifi");
        controller.start().await.unwrap();
        info!("Wifi started!");
    }
    info!("About to connect...");

    loop {
        match controller.connect().await {
            Ok(_) => {
                info!("Wifi connected!");
                break;
            }
            Err(e) => {
                error!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }

    info!("Waiting to get IP address...");
    loop {
        if let (_, Some(config)) = (stack.is_link_up(), stack.config_v4()) {
            info!("Got IP: {}", config.address);
            break;
        }

        Timer::after(Duration::from_millis(100)).await;
    }

    for i in 0..NUM_TUNNELS {
        spawner.spawn(task(stack, i)).ok();
    }
}

/// Runs the network stack, processing events
#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static>>) {
    stack.run().await
}

fn parse_connect(req_str: &str) -> Result<(&str, u16), &str> {
    let mut req_iter = req_str.split_whitespace();
    let method = req_iter.next().ok_or("at least one iteration")?;

    if method != "CONNECT" {
        return Err("expected CONNECT request");
    }

    info!("CONNECT request received");

    let Some(host_parts) = req_iter.next() else {
        return Err("missing host from request");
    };

    let mut host_iter = host_parts.split(':');
    let host = host_iter.next().expect("at least one iteration");
    let port = host_iter
        .next()
        .and_then(|s| atoi::atoi::<u16>(s.as_bytes()))
        .unwrap_or(443);

    Ok((host, port))
}

async fn sock_read(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<usize, TcpError> {
    if !socket.may_recv() {
        return Ok(0);
    }

    socket.read(buf).await
}

async fn sock_write(socket: &mut TcpSocket<'_>, buf: &[u8]) -> Result<bool, TcpError> {
    if !socket.may_send() {
        return Ok(false);
    }

    debug!("writing {} bytes", buf.len());

    socket.write_all(buf).await?;
    socket.flush().await?;
    Ok(true)
}

async fn sock_cleanup(socket: &mut TcpSocket<'_>) {
    socket.close();
    socket.abort();
    select(Timer::after(Duration::from_millis(125)), socket.flush()).await;
}
