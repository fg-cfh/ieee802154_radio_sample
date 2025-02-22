use self::runner::{RxRunner, StateRunner, TxRunner};

use super::runner::Device as RunnerDevice;
pub use super::runner::{self, driver, Runner, State};
use embassy_futures::select::{select, Either};
use embassy_net::driver::LinkState;
use embassy_nrf::radio::ieee802154::NrfPkt;
pub use embassy_nrf::radio::tasks::TaskRadioOff;
use embassy_nrf::radio::tasks::{
    CompletedRadioTransition, ExternalRadioTransition, RadioOffState, RadioTaskError,
    RadioTransitionResult, RxError, RxResult, RxState, SelfRadioTransition, TxState,
};
use embassy_nrf::radio::tasks::{TaskRx, TaskTx, Timestamp::BestEffort};
use embassy_nrf::radio::Instance;

// TODO: Needs to be configurable.
pub use embassy_nrf::radio::tasks::RadioDriver;
pub type Pkt = NrfPkt;
pub type Device<'d> = RunnerDevice<'d, Pkt>;
pub type RunnerState<'d, const N_RX: usize, const N_TX: usize> = State<'d, Pkt, N_RX, N_TX>;

enum SchedPkt<'pkt> {
    Rx(&'pkt mut Pkt),
    Tx(&'pkt mut Pkt),
}

pub struct BestEffortScheduler<'state, 'task, P: Instance, const N_RX: usize, const N_TX: usize> {
    state: StateRunner<'state>,
    rx_chan: RxRunner<'state, Pkt>,
    tx_chan: TxRunner<'state, Pkt>,
    driver: Option<RadioDriver<'state, 'task, P, TaskRadioOff>>,
}

// We use this runtime state to prove that the radio can only be in three
// different states when looping. This allows us to implement the scheduler as
// an event loop while still reaping all benefits of a behaviorally typed radio
// driver.
pub enum SchedulerState<'radio, 'task, P: Instance> {
    // We are currently sending a packet.
    TxPkt(RadioDriver<'radio, 'task, P, TaskTx<'task, Pkt>>),
    // There is no TX packet pending and we have enough RX capacity to receive
    // further incoming packets.
    RxPkt(RadioDriver<'radio, 'task, P, TaskRx<'task, Pkt>>),
    // We ran out of RX capacity and no TX packet is pending.
    NoPkt(RadioDriver<'radio, 'task, P, TaskRadioOff>),
}

impl<'state, 'task, P: Instance, const N_RX: usize, const N_TX: usize>
    BestEffortScheduler<'state, 'task, P, N_RX, N_TX>
{
    pub fn new(
        runner: Runner<'state, Pkt>,
        driver: RadioDriver<'state, 'task, P, TaskRadioOff>,
    ) -> Self {
        let (state, rx_chan, tx_chan) = runner.split();
        Self {
            state,
            rx_chan,
            tx_chan,
            driver: Some(driver),
        }
    }

    pub async fn run(mut self) -> ! {
        self.state.set_link_state(LinkState::Up);

        let mut sched_state = SchedulerState::NoPkt(self.driver.take().expect("already running"));
        let mut next_pkt = Some(self.get_pkt().await);
        loop {
            (sched_state, next_pkt) = match next_pkt {
                Some(SchedPkt::Rx(rx_pkt)) => {
                    match sched_state {
                        SchedulerState::RxPkt(mut rx_driver) => {
                            // If we're already in Rx state then only schedule
                            // another Rx packet once we're sure that the
                            // current Rx task will actually handle a packet.
                            // Otherwise Tx takes precedence. See the comment in
                            // the API for back-to-back RX scheduling.
                            match select(rx_driver.frame_started(), self.tx_chan.tx_pkt()).await {
                                Either::Second(tx_pkt) => {
                                    (SchedulerState::RxPkt(rx_driver), Some(SchedPkt::Tx(tx_pkt)))
                                }
                                _ => (
                                    self.schedule_rx_pkt(SchedulerState::RxPkt(rx_driver), rx_pkt)
                                        .await
                                        .unwrap_or_else(|prev_state| prev_state),
                                    self.try_get_next_pkt_in_rx(),
                                ),
                            }
                        }
                        _ => (
                            self.schedule_rx_pkt(sched_state, rx_pkt)
                                .await
                                .unwrap_or_else(|prev_state| prev_state),
                            self.try_get_next_pkt_in_rx(),
                        ),
                    }
                }
                Some(SchedPkt::Tx(tx_pkt)) => (
                    self.schedule_tx_pkt(sched_state, tx_pkt).await,
                    self.try_get_next_pkt_in_tx(),
                ),
                None => {
                    let sched_off = self.switch_radio_off(sched_state).await;
                    let pkt = Some(self.get_pkt().await);
                    (sched_off, pkt)
                }
            };
        }
    }

    // Set the radio up to listen for incoming packets and return a successful
    // result with the updated scheduler state or - if this fails - return the
    // previous scheduler state unchanged in the error-part of the result.
    async fn schedule_rx_pkt<'this_task, 'next_task>(
        &mut self,
        sched_state: SchedulerState<'state, 'this_task, P>,
        rx_pkt: &'next_task mut Pkt,
    ) -> Result<SchedulerState<'state, 'next_task, P>, SchedulerState<'state, 'this_task, P>>
    where
        'state: 'next_task,
    {
        let rx_task = TaskRx {
            start: BestEffort,
            packet: rx_pkt,
        };
        let rx_driver = match sched_state {
            SchedulerState::NoPkt(off_driver) => {
                match off_driver.schedule_rx(rx_task).entry().await {
                    CompletedRadioTransition::Entered(RadioTransitionResult {
                        this_state: rx_driver,
                        ..
                    }) => rx_driver,
                    // TODO: Implement error handling if (and only if) some
                    //       actual driver requires it.
                    _ => todo!(),
                }
            }
            SchedulerState::TxPkt(tx_driver) => {
                match tx_driver.schedule_rx(rx_task).entry().await {
                    CompletedRadioTransition::Entered(RadioTransitionResult {
                        this_state: rx_driver,
                        ..
                    }) => {
                        self.tx_chan.tx_done();
                        rx_driver
                    }
                    // TODO: Implement error handling if (and only if) some
                    //       actual driver requires it.
                    _ => todo!(),
                }
            }
            SchedulerState::RxPkt(rx_driver) => {
                match rx_driver.schedule_rx(rx_task).task_complete().await {
                    CompletedRadioTransition::Entered(RadioTransitionResult {
                        prev_task_result,
                        this_state: rx_driver,
                        ..
                    }) => {
                        match prev_task_result {
                            RxResult::Packet => self.rx_chan.rx_done(),
                            RxResult::RxWindowEnded => {
                                // Coming from an RX scheduler state run to
                                // completion, the RX result must have been a
                                // reception error or a packet.
                                // TODO: If we want to prove this no-panic
                                // guarantee then we need a separate radio
                                // driver state representing "waiting for a
                                // packet" as opposed to "currently receiving a
                                // packet".
                                unreachable!()
                            }
                        }
                        rx_driver
                    }
                    CompletedRadioTransition::NotScheduled(rx_driver, rx_error, _) => {
                        if let RadioTaskError::TaskError(RxError::CrcError) = rx_error {
                            log::warn!("CRC error")
                        } else {
                            // TODO: Implement error handling if (and only if) some
                            //       actual driver requires it.
                            todo!()
                        }
                        return Err(SchedulerState::RxPkt(rx_driver));
                    }
                    // TODO: Implement error handling if (and only if) some
                    //       actual driver requires it.
                    _ => todo!(),
                }
            }
        };
        Ok(SchedulerState::RxPkt(rx_driver))
    }

    async fn schedule_tx_pkt<'this_task, 'next_task>(
        &mut self,
        sched_state: SchedulerState<'state, 'this_task, P>,
        tx_pkt: &'next_task mut NrfPkt,
    ) -> SchedulerState<'state, 'next_task, P>
    where
        'state: 'next_task,
    {
        let tx_task = TaskTx {
            at: BestEffort,
            packet: tx_pkt,
            cca: true,
        };
        match sched_state {
            SchedulerState::NoPkt(off_driver) => {
                match off_driver.schedule_tx(tx_task).entry().await {
                    CompletedRadioTransition::Entered(RadioTransitionResult {
                        this_state: tx_driver,
                        ..
                    }) => SchedulerState::TxPkt(tx_driver),
                    CompletedRadioTransition::Fallback(
                        RadioTransitionResult {
                            this_state: off_driver,
                            ..
                        },
                        _,
                    ) => SchedulerState::NoPkt(off_driver), // TODO: back off (e.g. via CSMA/CA)
                    // TODO: Implement error handling if (and only if) some
                    //       actual driver requires it.
                    _ => todo!(),
                }
            }
            SchedulerState::RxPkt(rx_driver) => {
                match rx_driver.schedule_tx(tx_task).entry().await {
                    CompletedRadioTransition::Entered(RadioTransitionResult {
                        prev_task_result: rx_result,
                        this_state: tx_driver,
                        ..
                    }) => {
                        if let RxResult::Packet = rx_result {
                            self.rx_chan.rx_done();
                        }
                        SchedulerState::TxPkt(tx_driver)
                    }
                    CompletedRadioTransition::Fallback(
                        RadioTransitionResult {
                            this_state: off_driver,
                            ..
                        },
                        _,
                    ) => SchedulerState::NoPkt(off_driver), // TODO: back off (e.g. via CSMA/CA)
                    // TODO: Implement error handling if (and only if) some
                    //       actual driver requires it.
                    _ => todo!(),
                }
            }
            SchedulerState::TxPkt(tx_driver) => {
                match tx_driver.schedule_tx(tx_task).task_complete().await {
                    CompletedRadioTransition::Entered(RadioTransitionResult {
                        this_state: tx_driver,
                        ..
                    }) => {
                        self.tx_chan.tx_done();
                        SchedulerState::TxPkt(tx_driver)
                    }
                    CompletedRadioTransition::Fallback(
                        RadioTransitionResult {
                            this_state: off_driver,
                            ..
                        },
                        _,
                    ) => SchedulerState::NoPkt(off_driver), // TODO: back off (e.g. via CSMA/CA)
                    // TODO: Implement error handling if (and only if) some
                    //       actual driver requires it.
                    _ => todo!(),
                }
            }
        }
    }

    async fn switch_radio_off<'this_task, 'next_task>(
        &mut self,
        sched_state: SchedulerState<'state, 'this_task, P>,
    ) -> SchedulerState<'state, 'next_task, P>
    where
        'state: 'next_task,
    {
        let off_task = TaskRadioOff { at: BestEffort };
        let off_driver = match sched_state {
            SchedulerState::NoPkt(_) => {
                // TODO: To prove this no-panic guarantee we need to rewrite the
                // runner loop to distinguish between the off and rx/tx states.
                unreachable!()
            }
            SchedulerState::TxPkt(tx_driver) => {
                match tx_driver.schedule_off(off_task).entry().await {
                    CompletedRadioTransition::Entered(RadioTransitionResult {
                        this_state: off_driver,
                        ..
                    }) => {
                        self.tx_chan.tx_done();
                        off_driver
                    }
                    // TODO: Implement error handling if (and only if) some
                    //       actual driver requires it.
                    _ => todo!(),
                }
            }
            SchedulerState::RxPkt(rx_driver) => {
                match rx_driver.schedule_off(off_task).entry().await {
                    CompletedRadioTransition::Entered(RadioTransitionResult {
                        prev_task_result: rx_result,
                        this_state: off_driver,
                        ..
                    }) => {
                        if let RxResult::Packet = rx_result {
                            self.rx_chan.rx_done();
                        }
                        off_driver
                    }
                    // TODO: Implement error handling if (and only if) some
                    //       actual driver requires it.
                    _ => todo!(),
                }
            }
        };
        SchedulerState::NoPkt(off_driver)
    }

    async fn get_pkt<'pkt>(&mut self) -> SchedPkt<'pkt>
    where
        'state: 'pkt,
    {
        // Fast path: Whenever an outgoing packet is available - send it first.
        let mut maybe_tx_pkt = self.tx_chan.try_tx_pkt();
        if maybe_tx_pkt.is_some() {
            SchedPkt::Tx(maybe_tx_pkt.take().unwrap())
        } else {
            match select(self.rx_chan.rx_pkt(), self.tx_chan.tx_pkt()).await {
                Either::First(pkt) => SchedPkt::Rx(pkt),
                Either::Second(pkt) => SchedPkt::Tx(pkt),
            }
        }
    }

    // Called when the scheduler is in RX state: Either return a newly scheduled
    // TX packet or the next available RX packet for back-to-back scheduling.
    fn try_get_next_pkt_in_rx<'pkt>(&mut self) -> Option<SchedPkt<'pkt>>
    where
        'state: 'pkt,
    {
        // Prefer scheduling pending TX packets over continuously receiving RX
        // packets.
        let maybe_tx_pkt = self.tx_chan.try_tx_pkt();
        if maybe_tx_pkt.is_some() {
            return Some(SchedPkt::Tx(maybe_tx_pkt.unwrap()));
        }

        let maybe_rx_pkt = self.rx_chan.try_next_rx_pkt();
        maybe_rx_pkt.map(|pkt| SchedPkt::Rx(pkt))
    }

    // Called when the scheduler is in TX state: Either return the next TX
    // packet if available or the next available RX packet to return to RX mode.
    fn try_get_next_pkt_in_tx<'pkt>(&mut self) -> Option<SchedPkt<'pkt>>
    where
        'state: 'pkt,
    {
        // Prefer scheduling pending TX packets over continuously receiving RX
        // packets.
        let maybe_tx_pkt = self.tx_chan.try_next_tx_pkt();
        if maybe_tx_pkt.is_some() {
            return Some(SchedPkt::Tx(maybe_tx_pkt.unwrap()));
        }

        let maybe_rx_pkt = self.rx_chan.try_rx_pkt();
        maybe_rx_pkt.map(|pkt| SchedPkt::Rx(pkt))
    }
}

pub fn new<'d, P: Instance, const N_RX: usize, const N_TX: usize>(
    driver: RadioDriver<'d, 'd, P, TaskRadioOff>,
    state: &'d mut State<'d, Pkt, N_RX, N_TX>,
    hardware_address: driver::HardwareAddress,
) -> (BestEffortScheduler<'d, 'd, P, N_RX, N_TX>, Device<'d>) {
    let (runner, device) = runner::new(state, hardware_address);
    let scheduler = BestEffortScheduler::new(runner, driver);
    (scheduler, device)
}
