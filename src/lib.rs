#![no_std]
#![no_main]

use crate::ieee802154::scheduler::{
    BestEffortScheduler as Ieee802154Scheduler, Device as Ieee802154NetDevice, RadioDriver,
    RunnerState as Ieee802154State, TaskRadioOff,
};
use embassy_executor::Spawner;
use embassy_net::driver::HardwareAddress;
use embassy_net::{
    Config as NetConfig, Ieee802154Address, Ipv6Cidr, Runner as NetRunner, StaticConfigV6,
};
use embassy_net::{Stack, StackResources};
use embassy_nrf::peripherals::{RADIO, RNG};
use embassy_nrf::radio::ieee802154::instrument_driver;
use embassy_nrf::rng::Rng;
use embassy_nrf::{bind_interrupts, radio};
use heapless::Vec;
use ieee802154::scheduler;
use static_cell::StaticCell;

use panic_probe as _;
use systemview_target::SystemView;

pub mod ieee802154;

pub const IP_MTU: usize = 1514;
pub const N_RX: usize = 3;
pub const N_TX: usize = 3;

bind_interrupts!(struct Irqs {
    RADIO => radio::InterruptHandler<RADIO>;
});

#[embassy_executor::task]
async fn ieee802154_scheduler_task(
    ieee802154_scheduler: Ieee802154Scheduler<'static, 'static, RADIO, N_RX, N_TX>,
) -> ! {
    ieee802154_scheduler.run().await
}

#[embassy_executor::task]
async fn ieee802154_task(mut net_runner: NetRunner<'static, Ieee802154NetDevice<'static>>) -> ! {
    net_runner.run().await
}

pub const IEEE802154_SERVER_MAC_ADDR: [u8; 8] = [0x1a, 0x0b, 0x42, 0x42, 0x42, 0x42, 0x42, 0x01];
pub const IEEE802154_CLIENT_MAC_ADDR: [u8; 8] = [0x1a, 0x0b, 0x42, 0x42, 0x42, 0x42, 0x42, 0x02];
pub const IEEE802154_UDP_PORT: u16 = 5678;

pub async fn spawn_ieee802154(
    spawner: Spawner,
    radio: RADIO,
    rng: &mut Rng<'_, RNG>,
    mac_addr: [u8; 8],
) -> Stack<'static> {
    static SYSTEMVIEW: SystemView = SystemView::new();
    SYSTEMVIEW.init();
    log::set_logger(&SYSTEMVIEW).ok();
    log::set_max_level(log::LevelFilter::Info);
    instrument_driver();

    let hardware_address = HardwareAddress::Ieee802154(mac_addr);

    let radio_driver: RadioDriver<'static, 'static, RADIO, TaskRadioOff> =
        RadioDriver::new(radio, Irqs);

    static STATE: StaticCell<Ieee802154State<N_RX, N_TX>> = StaticCell::new();
    let (ieee802154_scheduler, net_device) = scheduler::new(
        radio_driver,
        STATE.init(Ieee802154State::new()),
        hardware_address,
    );

    spawner
        .spawn(ieee802154_scheduler_task(ieee802154_scheduler))
        .unwrap();

    let mut seed = [0; 8];
    rng.blocking_fill_bytes(&mut seed);
    let seed = u64::from_le_bytes(seed);

    let ipv6_address = Ieee802154Address::from_bytes(&mac_addr)
        .as_link_local_address()
        .unwrap();
    let ipv6_cidr = Ipv6Cidr::new(ipv6_address, 64);
    let net_config = NetConfig::ipv6_static(StaticConfigV6 {
        address: ipv6_cidr,
        gateway: None,
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

    spawner.spawn(ieee802154_task(runner)).unwrap();

    stack
}

rtos_trace::global_trace! {SystemView}

struct Application;
rtos_trace::global_application_callbacks! {Application}
impl rtos_trace::RtosTraceApplicationCallbacks for Application {
    fn system_description() {
        systemview_target::send_system_desc_app_name!("dot15d4 Radio Driver");
        systemview_target::send_system_desc_device!("nRF52840");
        systemview_target::send_system_desc_interrupt!(17, "RADIO");
    }

    fn sysclock() -> u32 {
        64_000_000
    }
}
