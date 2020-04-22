#![deny(warnings)]
#![allow(clippy::missing_safety_doc)]
#![no_std]
#![no_main]
#![cfg_attr(feature = "nightly", feature(asm))]
// Enable returning `!`
#![cfg_attr(feature = "nightly", feature(never_type))]
#![cfg_attr(feature = "nightly", feature(core_intrinsics))]

#[inline(never)]
#[panic_handler]
#[cfg(all(feature = "nightly", not(feature = "semihosting")))]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let gpiod = unsafe { &*pac::GPIOD::ptr() };
    gpiod.odr.modify(|_, w| w.odr6().high().odr12().high()); // FP_LED_1, FP_LED_3
    unsafe {
        core::intrinsics::abort();
    }
}

#[cfg(feature = "semihosting")]
extern crate panic_semihosting;

#[cfg(not(any(feature = "nightly", feature = "semihosting")))]
extern crate panic_halt;

#[macro_use]
extern crate log;

use nb;

// use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};
use cortex_m_rt::exception;
use cortex_m::asm;
use stm32h7xx_hal as hal;
use stm32h7xx_hal::{
    prelude::*,
    stm32 as pac,
};

use embedded_hal::{
    digital::v2::OutputPin,
};

/*
use core::fmt::Write;
use heapless::{consts::*, String, Vec};
use smoltcp as net;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json_core::{de::from_slice, ser::to_string};
*/

mod eth;

mod iir;
use iir::*;

mod eeprom;

#[cfg(not(feature = "semihosting"))]
fn init_log() {}

#[cfg(feature = "semihosting")]
fn init_log() {
    use cortex_m_log::log::{init as init_log, Logger};
    use cortex_m_log::printer::semihosting::{hio::HStdout, InterruptOk};
    use log::LevelFilter;
    static mut LOGGER: Option<Logger<InterruptOk<HStdout>>> = None;
    let logger = Logger {
        inner: InterruptOk::<_>::stdout().unwrap(),
        level: LevelFilter::Info,
    };
    let logger = unsafe { LOGGER.get_or_insert(logger) };

    init_log(logger).unwrap();
}

// Pull in build information (from `built` crate)
mod build_info {
    #![allow(dead_code)]
    // include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

const SCALE: f32 = ((1 << 15) - 1) as f32;

// static ETHERNET_PENDING: AtomicBool = AtomicBool::new(true);

/*
const TCP_RX_BUFFER_SIZE: usize = 8192;
const TCP_TX_BUFFER_SIZE: usize = 8192;

macro_rules! create_socket {
    ($set:ident, $rx_storage:ident, $tx_storage:ident, $target:ident) => {
        let mut $rx_storage = [0; TCP_RX_BUFFER_SIZE];
        let mut $tx_storage = [0; TCP_TX_BUFFER_SIZE];
        let tcp_rx_buffer =
            net::socket::TcpSocketBuffer::new(&mut $rx_storage[..]);
        let tcp_tx_buffer =
            net::socket::TcpSocketBuffer::new(&mut $tx_storage[..]);
        let tcp_socket =
            net::socket::TcpSocket::new(tcp_rx_buffer, tcp_tx_buffer);
        let $target = $set.add(tcp_socket);
    };
}
*/

#[rtfm::app(device = stm32h7xx_hal::stm32, peripherals = true)]
const APP: () = {
    struct Resources {
        adc1: hal::spi::Spi<hal::stm32::SPI2>,
        dac1: hal::spi::Spi<hal::stm32::SPI4>,

        adc2: hal::spi::Spi<hal::stm32::SPI3>,
        dac2: hal::spi::Spi<hal::stm32::SPI5>,

        _eeprom_i2c: hal::i2c::I2c<hal::stm32::I2C2>,

        dbg_pin: hal::gpio::gpioc::PC6<hal::gpio::Output<hal::gpio::PushPull>>,
        dac_pin: hal::gpio::gpiob::PB15<hal::gpio::Output<hal::gpio::PushPull>>,
        timer: hal::timer::Timer<hal::stm32::TIM2>,

        // TODO: Add in pounder hardware resources.

        //ethernet_periph:
        //    (pac::ETHERNET_MAC, pac::ETHERNET_DMA, pac::ETHERNET_MTL),
        #[init([[0.; 5]; 2])]
        iir_state: [IIRState; 2],
        #[init([IIR { ba: [1., 0., 0., 0., 0.], y_offset: 0., y_min: -SCALE - 1., y_max: SCALE }; 2])]
        iir_ch: [IIR; 2],
        //#[link_section = ".sram3.eth"]
        //#[init(eth::Device::new())]
        //ethernet: eth::Device,
    }

    #[init]
    fn init(c: init::Context) -> init::LateResources {
        let dp = c.device;
        let mut cp = cortex_m::Peripherals::take().unwrap();

        let pwr = dp.PWR.constrain();
        let vos = pwr.freeze();

        let rcc = dp.RCC.constrain();
        let mut clocks = rcc
            //TODO: Re-enable HSE for Stabilizer platform.
//            .use_hse(8.mhz())
            .sysclk(400.mhz())
            .hclk(200.mhz())
            .per_ck(100.mhz())
            .pll2_p_ck(100.mhz())
            .pll2_q_ck(100.mhz())
            .freeze(vos, &dp.SYSCFG);

        clocks.rb.rsr.write(|w| w.rmvf().set_bit());

        clocks.rb.d2ccip1r.modify(|_, w| w.spi123sel().pll2_p().spi45sel().pll2_q());

        let gpioa = dp.GPIOA.split(&mut clocks.ahb4);
        let gpiob = dp.GPIOB.split(&mut clocks.ahb4);
        let gpioc = dp.GPIOC.split(&mut clocks.ahb4);
        let gpiod = dp.GPIOD.split(&mut clocks.ahb4);
        let gpioe = dp.GPIOE.split(&mut clocks.ahb4);
        let gpiof = dp.GPIOF.split(&mut clocks.ahb4);
        let gpiog = dp.GPIOG.split(&mut clocks.ahb4);

        // Configure the SPI interfaces to the ADCs and DACs.
        let adc1_spi = {
            let spi_miso = gpiob.pb14.into_alternate_af5().set_speed(hal::gpio::Speed::VeryHigh);
            let spi_sck = gpiob.pb10.into_alternate_af5().set_speed(hal::gpio::Speed::VeryHigh);
            let _spi_nss = gpiob.pb9.into_alternate_af5();

            let config = hal::spi::Config::new(hal::spi::Mode{
                    polarity: hal::spi::Polarity::IdleHigh,
                    phase: hal::spi::Phase::CaptureOnSecondTransition,
                })
                .communication_mode(hal::spi::CommunicationMode::Receiver)
                .manage_cs()
                .cs_delay(220e-9)
                .frame_size(16);

            let mut spi = dp.SPI2.spi(
                    (spi_sck, spi_miso, hal::spi::NoMosi),
                    config,
                    50.mhz(),
                    &clocks);

            spi.listen(hal::spi::Event::Rxp);

            spi
        };

        let adc2_spi = {
            let spi_miso = gpiob.pb4.into_alternate_af6().set_speed(hal::gpio::Speed::VeryHigh);
            let spi_sck = gpioc.pc10.into_alternate_af6().set_speed(hal::gpio::Speed::VeryHigh);
            let _spi_nss = gpioa.pa15.into_alternate_af6();


            let config = hal::spi::Config::new(hal::spi::Mode{
                    polarity: hal::spi::Polarity::IdleHigh,
                    phase: hal::spi::Phase::CaptureOnSecondTransition,
                })
                .communication_mode(hal::spi::CommunicationMode::Receiver)
                .manage_cs()
                .frame_size(16)
                .cs_delay(220e-9);

            let spi = dp.SPI3.spi(
                    (spi_sck, spi_miso, hal::spi::NoMosi),
                    config,
                    50.mhz(),
                    &clocks);

            spi
        };

        let dac1_spi = {
            let spi_miso = gpioe.pe5.into_alternate_af5();
            let spi_sck = gpioe.pe2.into_alternate_af5();
            let _spi_nss = gpioe.pe4.into_alternate_af5();

            let config = hal::spi::Config::new(hal::spi::Mode{
                    polarity: hal::spi::Polarity::IdleHigh,
                    phase: hal::spi::Phase::CaptureOnSecondTransition,
                })
                .communication_mode(hal::spi::CommunicationMode::Transmitter)
                .manage_cs()
                .frame_size(16)
                .swap_mosi_miso();

            dp.SPI4.spi((spi_sck, spi_miso, hal::spi::NoMosi), config, 25.mhz(), &clocks)
        };

        let dac2_spi = {
            let spi_miso = gpiof.pf8.into_alternate_af5();
            let spi_sck = gpiof.pf7.into_alternate_af5();
            let _spi_nss = gpiof.pf6.into_alternate_af5();

            let config = hal::spi::Config::new(hal::spi::Mode{
                    polarity: hal::spi::Polarity::IdleHigh,
                    phase: hal::spi::Phase::CaptureOnSecondTransition,
                })
                .communication_mode(hal::spi::CommunicationMode::Transmitter)
                .manage_cs()
                .frame_size(16)
                .swap_mosi_miso();

            dp.SPI5.spi((spi_sck, spi_miso, hal::spi::NoMosi), config, 25.mhz(), &clocks)
        };

        // Instantiate the QUADSPI pins and peripheral interface.

        // TODO: Place these into a pins structure that is provided to the QSPI constructor.
        let _qspi_clk = gpiob.pb2.into_alternate_af9();
        let _qspi_ncs = gpioc.pc11.into_alternate_af9();
        let _qspi_io0 = gpioe.pe7.into_alternate_af10();
        let _qspi_io1 = gpioe.pe8.into_alternate_af10();
        let _qspi_io2 = gpioe.pe9.into_alternate_af10();
        let _qspi_io3 = gpioe.pe10.into_alternate_af10();

        let mut fp_led_0 = gpiod.pd5.into_push_pull_output();
        let mut fp_led_1 = gpiod.pd6.into_push_pull_output();
        let mut fp_led_2 = gpiod.pd12.into_push_pull_output();
        let mut fp_led_3 = gpiog.pg4.into_push_pull_output();

        fp_led_0.set_low().unwrap();
        fp_led_1.set_low().unwrap();
        fp_led_2.set_low().unwrap();
        fp_led_3.set_low().unwrap();

        let _i2c1 = {
            let sda = gpiob.pb7.into_alternate_af4().set_open_drain();
            let scl = gpiob.pb8.into_alternate_af4().set_open_drain();
            dp.I2C1.i2c((scl, sda), 100.khz(), &clocks)
        };

        let i2c2 = {
            let sda = gpiof.pf0.into_alternate_af4().set_open_drain();
            let scl = gpiof.pf1.into_alternate_af4().set_open_drain();
            dp.I2C2.i2c((scl, sda), 100.khz(), &clocks)
        };

        // Configure ethernet pins.

        // Reset the PHY before configuring pins.
        let mut eth_phy_nrst = gpioe.pe3.into_push_pull_output();
        eth_phy_nrst.set_high().unwrap();
        eth_phy_nrst.set_low().unwrap();
        eth_phy_nrst.set_high().unwrap();
        let _rmii_ref_clk = gpioa.pa1.into_alternate_af11().set_speed(hal::gpio::Speed::VeryHigh);
        let _rmii_mdio = gpioa.pa2.into_alternate_af11().set_speed(hal::gpio::Speed::VeryHigh);
        let _rmii_mdc = gpioc.pc1.into_alternate_af11().set_speed(hal::gpio::Speed::VeryHigh);
        let _rmii_crs_dv = gpioa.pa7.into_alternate_af11().set_speed(hal::gpio::Speed::VeryHigh);
        let _rmii_rxd0 = gpioc.pc4.into_alternate_af11().set_speed(hal::gpio::Speed::VeryHigh);
        let _rmii_rxd1 = gpioc.pc5.into_alternate_af11().set_speed(hal::gpio::Speed::VeryHigh);
        let _rmii_tx_en = gpiob.pb11.into_alternate_af11().set_speed(hal::gpio::Speed::VeryHigh);
        let _rmii_txd0 = gpiob.pb12.into_alternate_af11().set_speed(hal::gpio::Speed::VeryHigh);
        let _rmii_txd1 = gpiog.pg14.into_alternate_af11().set_speed(hal::gpio::Speed::VeryHigh);

        // TODO: Configure the ethernet controller
        // Enable the ethernet peripheral.
        //clocks.apb4.enr().modify(|_, w| w.syscfgen().set_bit());
        //clocks.ahb1.enr().modify(|_, w| {
        //    w.eth1macen().set_bit()
        //        .eth1txen().set_bit()
        //        .eth1rxen().set_bit()
        //});

        //dp.SYSCFG.pmcr.modify(|_, w| unsafe { w.epis().bits(0b100) }); // RMII

        cp.SCB.enable_icache();

        init_log();
        // info!("Version {} {}", build_info::PKG_VERSION, build_info::GIT_VERSION.unwrap());
        // info!("Built on {}", build_info::BUILT_TIME_UTC);
        // info!("{} {}", build_info::RUSTC_VERSION, build_info::TARGET);

        let mut debug_pin = gpioc.pc6.into_push_pull_output();
        debug_pin.set_low().unwrap();

        let mut dac_pin = gpiob.pb15.into_push_pull_output();
        dac_pin.set_low().unwrap();

        // Configure timer 2 to trigger conversions for the ADC
        let mut timer2 = dp.TIM2.timer(500.khz(), &mut clocks);
        timer2.listen(hal::timer::Event::TimeOut);

        init::LateResources {
            adc1: adc1_spi,
            dac1: dac1_spi,
            adc2: adc2_spi,
            dac2: dac2_spi,

            dbg_pin: debug_pin,
            dac_pin: dac_pin,
            timer: timer2,

            _eeprom_i2c: i2c2,
//            ethernet_periph: (
//                dp.ETHERNET_MAC,
//                dp.ETHERNET_DMA,
//                dp.ETHERNET_MTL,
//            ),
        }
    }

    #[task(binds = TIM2, resources = [dbg_pin, timer, adc1, adc2])]
    fn tim2(mut c: tim2::Context) {
        c.resources.timer.clear_uif_bit();
        c.resources.dbg_pin.set_high().unwrap();

        // Start a SPI transaction on ADC0 and ADC1
        c.resources.adc1.lock(|adc| adc.spi.cr1.modify(|_, w| w.cstart().set_bit()));
        c.resources.adc2.lock(|adc| adc.spi.cr1.modify(|_, w| w.cstart().set_bit()));

        c.resources.dbg_pin.set_low().unwrap();
    }

    #[task(binds = SPI2, resources = [adc1, dac1, adc2, dac2, iir_state, iir_ch, dac_pin], priority = 2)]
    fn adc_spi(c: adc_spi::Context) {
        #[cfg(feature = "bkpt")]
        cortex_m::asm::bkpt();

        c.resources.dac_pin.set_high().unwrap();

        let output_ch1 = {
            let a: u16 = c.resources.adc1.read().unwrap();
            let x0 = f32::from(a as i16);
            let y0 = c.resources.iir_ch[0].update(&mut c.resources.iir_state[0], x0);
            y0 as i16 as u16 ^ 0x8000
        };
        c.resources.adc1.spi.ifcr.write(|w| w.eotc().set_bit());

        let output_ch2 = {
            let a: u16 = nb::block!(c.resources.adc2.read()).unwrap();
            let x0 = f32::from(a as i16);
            let y0 = c.resources.iir_ch[1].update(&mut c.resources.iir_state[1], x0);
            y0 as i16 as u16 ^ 0x8000
        };
        c.resources.adc2.spi.ifcr.write(|w| w.eotc().set_bit());

        c.resources.dac1.send(output_ch1).unwrap();
        c.resources.dac2.send(output_ch2).unwrap();

        c.resources.dac_pin.set_low().unwrap();
        #[cfg(feature = "bkpt")]
        cortex_m::asm::bkpt();
    }

    #[idle]
    fn idle(_c: idle::Context) -> ! {
        // TODO Implement and poll ethernet interface.
        loop {
            asm::nop();
        }
    }

    /*
    #[idle(resources = [ethernet, ethernet_periph, iir_state, iir_ch, i2c])]
    fn idle(c: idle::Context) -> ! {
        let (MAC, DMA, MTL) = c.resources.ethernet_periph;

        let hardware_addr = match eeprom::read_eui48(c.resources.i2c) {
            Err(_) => {
                info!("Could not read EEPROM, using default MAC address");
                net::wire::EthernetAddress([0x10, 0xE2, 0xD5, 0x00, 0x03, 0x00])
            }
            Ok(raw_mac) => net::wire::EthernetAddress(raw_mac),
        };
        info!("MAC: {}", hardware_addr);

        unsafe { c.resources.ethernet.init(hardware_addr, MAC, DMA, MTL) };
        let mut neighbor_cache_storage = [None; 8];
        let neighbor_cache =
            net::iface::NeighborCache::new(&mut neighbor_cache_storage[..]);
        let local_addr = net::wire::IpAddress::v4(10, 0, 16, 99);
        let mut ip_addrs = [net::wire::IpCidr::new(local_addr, 24)];
        let mut iface =
            net::iface::EthernetInterfaceBuilder::new(c.resources.ethernet)
                .ethernet_addr(hardware_addr)
                .neighbor_cache(neighbor_cache)
                .ip_addrs(&mut ip_addrs[..])
                .finalize();
        let mut socket_set_entries: [_; 8] = Default::default();
        let mut sockets =
            net::socket::SocketSet::new(&mut socket_set_entries[..]);
        create_socket!(sockets, tcp_rx_storage0, tcp_tx_storage0, tcp_handle0);
        create_socket!(sockets, tcp_rx_storage0, tcp_tx_storage0, tcp_handle1);

        // unsafe { eth::enable_interrupt(DMA); }
        let mut time = 0u32;
        let mut next_ms = Instant::now();
        next_ms += 400_000.cycles();
        let mut server = Server::new();
        let mut iir_state: resources::iir_state = c.resources.iir_state;
        let mut iir_ch: resources::iir_ch = c.resources.iir_ch;
        loop {
            // if ETHERNET_PENDING.swap(false, Ordering::Relaxed) { }
            let tick = Instant::now() > next_ms;
            if tick {
                next_ms += 400_000.cycles();
                time += 1;
            }
            {
                let socket =
                    &mut *sockets.get::<net::socket::TcpSocket>(tcp_handle0);
                if socket.state() == net::socket::TcpState::CloseWait {
                    socket.close();
                } else if !(socket.is_open() || socket.is_listening()) {
                    socket
                        .listen(1234)
                        .unwrap_or_else(|e| warn!("TCP listen error: {:?}", e));
                } else if tick && socket.can_send() {
                    let s = iir_state.lock(|iir_state| Status {
                        t: time,
                        x0: iir_state[0][0],
                        y0: iir_state[0][2],
                        x1: iir_state[1][0],
                        y1: iir_state[1][2],
                    });
                    json_reply(socket, &s);
                }
            }
            {
                let socket =
                    &mut *sockets.get::<net::socket::TcpSocket>(tcp_handle1);
                if socket.state() == net::socket::TcpState::CloseWait {
                    socket.close();
                } else if !(socket.is_open() || socket.is_listening()) {
                    socket
                        .listen(1235)
                        .unwrap_or_else(|e| warn!("TCP listen error: {:?}", e));
                } else {
                    server.poll(socket, |req: &Request| {
                        if req.channel < 2 {
                            iir_ch.lock(|iir_ch| {
                                iir_ch[req.channel as usize] = req.iir
                            });
                        }
                    });
                }
            }

            if !match iface.poll(
                &mut sockets,
                net::time::Instant::from_millis(time as i64),
            ) {
                Ok(changed) => changed,
                Err(net::Error::Unrecognized) => true,
                Err(e) => {
                    info!("iface poll error: {:?}", e);
                    true
                }
            } {
                // cortex_m::asm::wfi();
            }
        }
    }
    */

    /*
    #[task(binds = ETH, resources = [ethernet_periph], priority = 1)]
    fn eth(c: eth::Context) {
        let dma = &c.resources.ethernet_periph.1;
        ETHERNET_PENDING.store(true, Ordering::Relaxed);
        unsafe { eth::interrupt_handler(dma) }
    }
    */

    extern "C" {
        // hw interrupt handlers for RTFM to use for scheduling tasks
        // one per priority
        fn DCMI();
        fn JPEG();
        fn SDMMC();
    }
};

/*
#[derive(Deserialize, Serialize)]
struct Request {
    channel: u8,
    iir: IIR,
}

#[derive(Serialize)]
struct Response<'a> {
    code: i32,
    message: &'a str,
}

#[derive(Serialize)]
struct Status {
    t: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

fn json_reply<T: Serialize>(socket: &mut net::socket::TcpSocket, msg: &T) {
    let mut u: String<U128> = to_string(msg).unwrap();
    u.push('\n').unwrap();
    socket.write_str(&u).unwrap();
}

struct Server {
    data: Vec<u8, U256>,
    discard: bool,
}

impl Server {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            discard: false,
        }
    }

    fn poll<T, F, R>(
        &mut self,
        socket: &mut net::socket::TcpSocket,
        f: F,
    ) -> Option<R>
    where
        T: DeserializeOwned,
        F: FnOnce(&T) -> R,
    {
        while socket.can_recv() {
            let found = socket
                .recv(|buf| {
                    let (len, found) =
                        match buf.iter().position(|&c| c as char == '\n') {
                            Some(end) => (end + 1, true),
                            None => (buf.len(), false),
                        };
                    if self.data.len() + len >= self.data.capacity() {
                        self.discard = true;
                        self.data.clear();
                    } else if !self.discard && len > 0 {
                        self.data.extend_from_slice(&buf[..len]).unwrap();
                    }
                    (len, found)
                })
                .unwrap();
            if found {
                if self.discard {
                    self.discard = false;
                    json_reply(
                        socket,
                        &Response {
                            code: 520,
                            message: "command buffer overflow",
                        },
                    );
                    self.data.clear();
                } else {
                    let r = from_slice::<T>(&self.data[..self.data.len() - 1]);
                    self.data.clear();
                    match r {
                        Ok(res) => {
                            let r = f(&res);
                            json_reply(
                                socket,
                                &Response {
                                    code: 200,
                                    message: "ok",
                                },
                            );
                            return Some(r);
                        }
                        Err(err) => {
                            warn!("parse error {:?}", err);
                            json_reply(
                                socket,
                                &Response {
                                    code: 550,
                                    message: "parse error",
                                },
                            );
                        }
                    }
                }
            }
        }
        None
    }
}
*/

#[exception]
fn HardFault(ef: &cortex_m_rt::ExceptionFrame) -> ! {
    panic!("HardFault at {:#?}", ef);
}

#[exception]
fn DefaultHandler(irqn: i16) {
    panic!("Unhandled exception (IRQn = {})", irqn);
}
