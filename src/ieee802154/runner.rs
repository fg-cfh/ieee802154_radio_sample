use core::array::from_fn;
use core::cell::RefCell;
use core::mem::MaybeUninit;
use core::task::{Context, Poll};

pub use embassy_net_driver as driver;
use embassy_net_driver::{Capabilities, LinkState};
use embassy_nrf::radio::tasks::Packet;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::waitqueue::WakerRegistration;
use embassy_sync::zerocopy_channel;

/// Channel state.
///
/// Holds a buffer of radio packets, for both TX and RX.
pub struct State<'d, Pkt: Packet, const N_RX: usize, const N_TX: usize> {
    rx: [Pkt; N_RX],
    tx: [Pkt; N_TX],
    inner: MaybeUninit<StateInner<'d, Pkt>>,
}

impl<'d, Pkt: Packet, const N_RX: usize, const N_TX: usize> State<'d, Pkt, N_RX, N_TX> {
    /// Create a new radio driver runner state.
    pub fn new() -> Self {
        Self {
            rx: from_fn(|_| Pkt::new()),
            tx: from_fn(|_| Pkt::new()),
            inner: MaybeUninit::uninit(),
        }
    }
}

struct StateInner<'d, Pkt: Packet> {
    rx: zerocopy_channel::Channel<'d, NoopRawMutex, Pkt>,
    tx: zerocopy_channel::Channel<'d, NoopRawMutex, Pkt>,
    shared: Mutex<NoopRawMutex, RefCell<Shared>>,
}

struct Shared {
    link_state: LinkState,
    waker: WakerRegistration,
    hardware_address: driver::HardwareAddress,
}

/// Channel runner.
///
/// Holds the shared state and the lower end of channels for inbound and outbound packets.
pub struct Runner<'d, Pkt: Packet> {
    tx_chan: zerocopy_channel::Receiver<'d, NoopRawMutex, Pkt>,
    rx_chan: zerocopy_channel::Sender<'d, NoopRawMutex, Pkt>,
    shared: &'d Mutex<NoopRawMutex, RefCell<Shared>>,
}

/// State runner.
///
/// Holds the shared state of the channel such as link state.
#[derive(Clone, Copy)]
pub struct StateRunner<'d> {
    shared: &'d Mutex<NoopRawMutex, RefCell<Shared>>,
}

/// RX runner.
///
/// Holds the lower end of the channel for passing inbound packets up the stack.
pub struct RxRunner<'d, Pkt: Packet> {
    rx_chan: zerocopy_channel::Sender<'d, NoopRawMutex, Pkt>,
}

/// TX runner.
///
/// Holds the lower end of the channel for passing outbound packets down the stack.
pub struct TxRunner<'d, Pkt: Packet> {
    tx_chan: zerocopy_channel::Receiver<'d, NoopRawMutex, Pkt>,
}

impl<'d, Pkt: Packet> Runner<'d, Pkt> {
    /// Split the runner into separate runners for controlling state, rx and tx.
    pub fn split(self) -> (StateRunner<'d>, RxRunner<'d, Pkt>, TxRunner<'d, Pkt>) {
        (
            StateRunner {
                shared: self.shared,
            },
            RxRunner {
                rx_chan: self.rx_chan,
            },
            TxRunner {
                tx_chan: self.tx_chan,
            },
        )
    }

    /// Create a state runner sharing the state channel.
    pub fn state_runner(&self) -> StateRunner<'d> {
        StateRunner {
            shared: self.shared,
        }
    }

    /// Set the link state.
    pub fn set_link_state(&mut self, state: LinkState) {
        self.shared.lock(|s| {
            let s = &mut *s.borrow_mut();
            s.link_state = state;
            s.waker.wake();
        });
    }

    /// Set the hardware address.
    pub fn set_hardware_address(&mut self, address: driver::HardwareAddress) {
        self.shared.lock(|s| {
            let s = &mut *s.borrow_mut();
            s.hardware_address = address;
            s.waker.wake();
        });
    }
}

impl<'d> StateRunner<'d> {
    /// Set link state.
    pub fn set_link_state(&self, state: LinkState) {
        self.shared.lock(|s| {
            let s = &mut *s.borrow_mut();
            s.link_state = state;
            s.waker.wake();
        });
    }

    /// Set the hardware address.
    pub fn set_hardware_address(&self, address: driver::HardwareAddress) {
        self.shared.lock(|s| {
            let s = &mut *s.borrow_mut();
            s.hardware_address = address;
            s.waker.wake();
        });
    }
}

impl<'d, Pkt: Packet> RxRunner<'d, Pkt> {
    /// Wait until there is space for more inbound packets and return it.
    pub async fn rx_pkt(&mut self) -> &'d mut Pkt {
        self.rx_chan.send().await
    }

    /// Check whether a second RX buffer is available - if so - return a
    /// reference to it.
    pub fn try_next_rx_pkt(&mut self) -> Option<&'d mut Pkt> {
        self.rx_chan.try_get_free(1)
    }

    /// Check if there is space for more inbound packets right now and if so, return it as a packet.
    pub fn try_rx_pkt(&mut self) -> Option<&'d mut Pkt> {
        self.rx_chan.try_send()
    }

    /// Polling the inbound channel if there is space for packets.
    pub fn poll_rx_pkt(&mut self, cx: &mut Context) -> Poll<&'d mut Pkt> {
        self.rx_chan.poll_send(cx)
    }

    /// Mark packet of len bytes as pushed to the inbound channel.
    pub fn rx_done(&mut self) {
        self.rx_chan.send_done();
    }
}

impl<'d, Pkt: Packet> TxRunner<'d, Pkt> {
    /// Wait until an outbound packet is available and return it.
    ///
    /// NOTE: Mutability of the return value is only required to prove
    ///       that the packet is in RAM such that DMA operations can be
    ///       executed on it.
    pub async fn tx_pkt(&mut self) -> &'d mut Pkt {
        self.tx_chan.receive().await
    }

    /// Check whether a second TX buffer is already pending and - if so - return
    /// a reference to it.
    pub fn try_next_tx_pkt(&mut self) -> Option<&'d mut Pkt> {
        self.tx_chan.try_get(1)
    }

    /// Check if there is an outbound packet available right now.
    pub fn try_tx_pkt(&mut self) -> Option<&'d mut Pkt> {
        self.tx_chan.try_receive()
    }

    /// Polling the outbound channel if there is a packet available.
    pub fn poll_tx_pkt(&mut self, cx: &mut Context) -> Poll<&'d mut Pkt> {
        self.tx_chan.poll_receive(cx)
    }

    /// Mark outbound packet as sent.
    pub fn tx_done(&mut self) {
        self.tx_chan.receive_done();
    }
}

/// Create a channel.
///
/// Returns a pair of handles for interfacing with the peripheral and the networking stack.
///
/// The runner is interfacing with the peripheral at the lower part of the stack.
/// The device is interfacing with the networking stack on the layer above.
pub fn new<'d, Pkt: Packet, const N_RX: usize, const N_TX: usize>(
    state: &'d mut State<'d, Pkt, N_RX, N_TX>,
    hardware_address: driver::HardwareAddress,
) -> (Runner<'d, Pkt>, Device<'d, Pkt>) {
    let mut caps = Capabilities::default();
    caps.max_transmission_unit = Pkt::CAPACITY;

    // safety: this is a self-referential struct, however:
    // - it can't move while the `'d` borrow is active.
    // - when the borrow ends, the dangling references inside the MaybeUninit will never be used again.
    let state_uninit: *mut MaybeUninit<StateInner<'d, Pkt>> =
        (&mut state.inner as *mut MaybeUninit<StateInner<'d, Pkt>>).cast();
    let state = unsafe { &mut *state_uninit }.write(StateInner {
        rx: zerocopy_channel::Channel::new(&mut state.rx[..]),
        tx: zerocopy_channel::Channel::new(&mut state.tx[..]),
        shared: Mutex::new(RefCell::new(Shared {
            link_state: LinkState::Down,
            hardware_address,
            waker: WakerRegistration::new(),
        })),
    });

    let (rx_sender, rx_receiver) = state.rx.split();
    let (tx_sender, tx_receiver) = state.tx.split();

    (
        Runner {
            tx_chan: tx_receiver,
            rx_chan: rx_sender,
            shared: &state.shared,
        },
        Device {
            caps,
            rx: rx_receiver,
            tx: tx_sender,
            shared: &state.shared,
        },
    )
}

/// Channel device.
///
/// Holds the shared state and upper end of channels for inbound and outbound packets.
pub struct Device<'d, Pkt: Packet> {
    rx: zerocopy_channel::Receiver<'d, NoopRawMutex, Pkt>,
    tx: zerocopy_channel::Sender<'d, NoopRawMutex, Pkt>,
    shared: &'d Mutex<NoopRawMutex, RefCell<Shared>>,
    caps: Capabilities,
}

impl<'d, Pkt: Packet> embassy_net_driver::Driver for Device<'d, Pkt> {
    type RxToken<'a>
        = RxToken<'a, Pkt>
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a, Pkt>
    where
        Self: 'a;

    /// Check if a received package is available and return it as soon as a corresponding TX token can be obtained.
    fn receive(&mut self, cx: &mut Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.rx.poll_receive(cx).is_ready() && self.tx.poll_send(cx).is_ready() {
            Some((
                RxToken {
                    rx: self.rx.borrow(),
                },
                TxToken {
                    tx: self.tx.borrow(),
                },
            ))
        } else {
            None
        }
    }

    /// Construct a transmit token.
    fn transmit(&mut self, cx: &mut Context) -> Option<Self::TxToken<'_>> {
        if self.tx.poll_send(cx).is_ready() {
            Some(TxToken {
                tx: self.tx.borrow(),
            })
        } else {
            None
        }
    }

    /// Get a description of device capabilities.
    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn hardware_address(&self) -> driver::HardwareAddress {
        self.shared.lock(|s| s.borrow().hardware_address)
    }

    fn link_state(&mut self, cx: &mut Context) -> LinkState {
        self.shared.lock(|s| {
            let s = &mut *s.borrow_mut();
            s.waker.register(cx.waker());
            s.link_state
        })
    }
}

/// An rx token.
///
/// Holds inbound receive channel and interfaces with embassy-net-driver.
pub struct RxToken<'a, Pkt: Packet> {
    rx: zerocopy_channel::Receiver<'a, NoopRawMutex, Pkt>,
}

impl<'a, Pkt: Packet> embassy_net_driver::RxToken for RxToken<'a, Pkt> {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // NOTE(unwrap): we checked the queue wasn't full when creating the token.
        let pkt = self.rx.try_receive().unwrap();

        // We cannot use the read-only ZerocopyBuffer::payload(), see
        // https://github.com/embassy-rs/embassy/issues/3670.
        let len = pkt.len();
        let r = f(&mut pkt.payload_buffer()[..len]);

        self.rx.receive_done();
        r
    }
}

/// A tx token.
///
/// Holds outbound transmit channel and interfaces with embassy-net-driver.
pub struct TxToken<'a, Pkt: Packet> {
    tx: zerocopy_channel::Sender<'a, NoopRawMutex, Pkt>,
}

impl<'a, Pkt: Packet> embassy_net_driver::TxToken for TxToken<'a, Pkt> {
    fn consume<R, F>(mut self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // NOTE(unwrap): we checked the queue wasn't full when creating the token.
        let pkt = self.tx.try_send().unwrap();
        let r = f(&mut pkt.payload_buffer()[..len]);
        pkt.truncate(len);
        self.tx.send_done();
        r
    }
}
