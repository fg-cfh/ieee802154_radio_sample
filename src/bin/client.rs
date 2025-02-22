#![no_std]
#![no_main]

use core::num::Wrapping;

use embassy_executor::Spawner;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Ieee802154Address, IpEndpoint};
use embassy_nrf::config::{Config, HfclkSource};
use embassy_nrf::rng::Rng;
use embassy_nrf::{bind_interrupts, peripherals, rng};
use embassy_time::Timer;
use ieee802154_radio_sample::{
    spawn_ieee802154, IEEE802154_CLIENT_MAC_ADDR, IEEE802154_SERVER_MAC_ADDR, IEEE802154_UDP_PORT,
};

bind_interrupts!(struct Irqs {
    RNG => rng::InterruptHandler<peripherals::RNG>;
});

unsafe extern "C" {
    pub fn SEGGER_SYSVIEW_Start();
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config: Config = Default::default();
    config.hfclk_source = HfclkSource::ExternalXtal;
    let p = embassy_nrf::init(config);
    let mut rng = Rng::new(p.RNG, Irqs);

    let ieee802154_stack =
        spawn_ieee802154(spawner, p.RADIO, &mut rng, IEEE802154_CLIENT_MAC_ADDR).await;
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

    let local_endpoint = IpEndpoint::new(
        ieee802154_stack
            .config_v6()
            .unwrap()
            .address
            .address()
            .into(),
        IEEE802154_UDP_PORT,
    );

    let server_ipv6_address = Ieee802154Address::from_bytes(&IEEE802154_SERVER_MAC_ADDR)
        .as_link_local_address()
        .unwrap();
    let remote_endpoint = IpEndpoint::new(server_ipv6_address.into(), IEEE802154_UDP_PORT);

    ieee802154_socket.bind(local_endpoint).unwrap();

    let mut payload = Wrapping(0_u32);
    loop {
        payload += 1;

        if let Err(e) = ieee802154_socket
            .send_to(&payload.0.to_be_bytes(), remote_endpoint)
            .await
        {
            log::warn!("send error: {:?}", e);
        };

        log::info!("heartbeat: {:?}", payload.0);
        Timer::after_secs(3).await;
    }
}
