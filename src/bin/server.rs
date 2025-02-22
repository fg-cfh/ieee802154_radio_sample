#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Config as NetConfig, Runner as NetRunner};
use embassy_net::{Ipv4Address, Ipv4Cidr, Stack, StackResources};
use embassy_nrf::config::{Config, HfclkSource};
use embassy_nrf::peripherals::{RNG, USBD};
use embassy_nrf::rng::Rng;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::{bind_interrupts, peripherals, rng, usb};
use embassy_usb::class::cdc_ncm::embassy_net::{
    Device as UsbNetDevice, Runner as CdcNcmRunner, State as NetState,
};
use embassy_usb::class::cdc_ncm::{CdcNcmClass, State as UsbState};
use embassy_usb::{Builder as UsbDeviceBuilder, Config as UsbConfig, UsbDevice};
use embedded_io_async::Write;
use heapless::Vec;
use ieee802154_radio_sample::{
    spawn_ieee802154, IEEE802154_SERVER_MAC_ADDR, IEEE802154_UDP_PORT, IP_MTU,
};
use static_cell::StaticCell;

// The apparent MAC address of the USB device.
const DEVICE_MAC_ADDR: [u8; 6] = [0x5a, 0x96, 0x4c, 0x7f, 0xf2, 0x3e];
// The apparent MAC address of the interface mounted on the host side.
const HOST_MAC_ADDR: [u8; 6] = [0x1a, 0xf3, 0x00, 0x9e, 0x0a, 0x1c];

const ETH_TCP_PORT: u16 = 1234;

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
    RNG => rng::InterruptHandler<peripherals::RNG>;
});

type NrfUsbDriver = UsbDriver<'static, peripherals::USBD, HardwareVbusDetect>;

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, NrfUsbDriver>) -> ! {
    device.run().await
}

#[embassy_executor::task]
async fn usb_ncm_task(cdc_ncm_runner: CdcNcmRunner<'static, NrfUsbDriver, IP_MTU>) -> ! {
    cdc_ncm_runner.run().await
}

#[embassy_executor::task]
async fn ethernet_task(mut net_runner: NetRunner<'static, UsbNetDevice<'static, IP_MTU>>) -> ! {
    net_runner.run().await
}

async fn spawn_ethernet(spawner: Spawner, usbd: USBD, rng: &mut Rng<'_, RNG>) -> Stack<'static> {
    // USB HAL driver
    let usb_driver = UsbDriver::new(usbd, Irqs, HardwareVbusDetect::new(Irqs));

    // USB configuration
    let mut usb_config = UsbConfig::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some("Code for Humans");
    usb_config.product = Some("IEEE 802.15.4 Radio Sample");
    usb_config.serial_number = Some("123456789");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;

    // USB device builder.
    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 128]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();
    let mut usb_device_builder = UsbDeviceBuilder::new(
        usb_driver,
        usb_config,
        &mut CONFIG_DESC.init([0; 256])[..],
        &mut BOS_DESC.init([0; 256])[..],
        &mut MSOS_DESC.init([0; 128])[..],
        &mut CONTROL_BUF.init([0; 128])[..],
    );

    // CDC NCM Class
    static USB_STATE: StaticCell<UsbState> = StaticCell::new();
    let cdc_ncm_class = CdcNcmClass::new(
        &mut usb_device_builder,
        USB_STATE.init(UsbState::new()),
        HOST_MAC_ADDR,
        64,
    );

    let usb_device = usb_device_builder.build();
    spawner.spawn(usb_task(usb_device)).unwrap();

    static NET_STATE: StaticCell<NetState<IP_MTU, 4, 4>> = StaticCell::new();
    let (runner, net_device) = cdc_ncm_class
        .into_embassy_net_device::<IP_MTU, 4, 4>(NET_STATE.init(NetState::new()), DEVICE_MAC_ADDR);
    spawner.spawn(usb_ncm_task(runner)).unwrap();

    let mut seed = [0; 8];
    rng.blocking_fill_bytes(&mut seed);
    let seed = u64::from_le_bytes(seed);

    let net_config = NetConfig::ipv4_static(embassy_net::StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Address::new(192, 168, 27, 2), 24),
        gateway: Some(Ipv4Address::new(192, 168, 27, 1)),
        dns_servers: Vec::new(),
    });

    // Init network stack
    static RESOURCES: StaticCell<StackResources<1>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        net_config,
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(ethernet_task(runner)).unwrap();

    stack
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config: Config = Default::default();
    config.hfclk_source = HfclkSource::ExternalXtal;
    let p = embassy_nrf::init(config);
    let mut rng = Rng::new(p.RNG, Irqs);

    let ethernet_stack = spawn_ethernet(spawner, p.USBD, &mut rng).await;
    let mut eth_rx_buffer = [0; 4096];
    let mut eth_tx_buffer = [0; 4096];
    let mut eth_socket = TcpSocket::new(ethernet_stack, &mut eth_rx_buffer, &mut eth_tx_buffer);

    let ieee802154_stack =
        spawn_ieee802154(spawner, p.RADIO, &mut rng, IEEE802154_SERVER_MAC_ADDR).await;
    let mut ieee802154_rx_meta = [PacketMetadata::EMPTY; 16];
    let mut ieee802154_rx_buffer = [0; 4096];
    let mut ieee802154_tx_meta = [PacketMetadata::EMPTY; 16];
    let mut ieee802154_tx_buffer = [0; 4096];
    let mut ieee802154_socket = UdpSocket::new(
        ieee802154_stack,
        &mut ieee802154_rx_meta,
        &mut ieee802154_rx_buffer,
        &mut ieee802154_tx_meta,
        &mut ieee802154_tx_buffer,
    );
    ieee802154_socket.bind(IEEE802154_UDP_PORT).ok();

    let mut pkt_buf = [0; 4096];

    loop {
        log::info!("Listening on TCP:{}...", ETH_TCP_PORT);
        if let Err(e) = eth_socket.accept(ETH_TCP_PORT).await {
            log::warn!("accept error: {:?}", e);
            continue;
        }

        log::info!(
            "Received connection from {:?}",
            eth_socket.remote_endpoint()
        );

        loop {
            let pkt_len = match ieee802154_socket.recv_from(&mut pkt_buf).await {
                Ok((pkt_len, rx_meta)) => {
                    log::info!("recv pkt: {:?}", rx_meta);
                    pkt_len
                }
                Err(e) => {
                    log::warn!("recv error: {:?}", e);
                    continue;
                }
            };

            match eth_socket.write_all(&pkt_buf[..pkt_len]).await {
                Ok(()) => {}
                Err(e) => {
                    log::warn!("write error: {:?}", e);
                    break;
                }
            };
        }
    }
}
