use crate::maple::packet::MaplePacket;
use heapless::{String, Vec};

/// Number of u32 words for TX and RX buffer sizes (including framing + CRC)
const MAX_PACKET_WORDS: usize = 258;

#[derive(Debug)]
pub enum BusStatus {
    Idle,
    WriteInProgress,
    ReadInProgress,
    WriteComplete,
    ReadComplete(Vec<u32, MAX_PACKET_WORDS>),
    Error(String<64>),
}

pub struct MapleBus {
    // Pin configuration (GPIO index for pin A and optional direction pin)
    pin_a: u32,
    pin_b: u32,
    dir_pin: Option<u32>,
    dir_out_high: bool,

    // Buffers
    tx_buffer: Vec<u32, MAX_PACKET_WORDS>,
    rx_buffer: Vec<u32, MAX_PACKET_WORDS>,

    // Phase tracking
    state: BusPhase,
    expecting_response: bool,
    response_timeout_us: u64,
    proc_kill_time: u64,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum BusPhase {
    Idle,
    WriteInProgress,
    WaitingForReadStart,
    ReadInProgress,
    ReadComplete,
    WriteComplete,
    Failed,
}

impl MapleBus {
    pub fn new(pin_a: u32, dir_pin: Option<u32>, dir_out_high: bool) -> Self {
        Self {
            pin_a,
            pin_b: pin_a + 1,
            dir_pin,
            dir_out_high,
            tx_buffer: Vec::new(),
            rx_buffer: Vec::new(),
            state: BusPhase::Idle,
            expecting_response: false,
            response_timeout_us: 0,
            proc_kill_time: u64::MAX,
        }
    }

    pub fn write(
        &mut self,
        packet: &MaplePacket,
        autostart_read: bool,
        read_timeout_us: u64,
    ) -> bool {
        // Placeholder logic for now
        defmt::info!("Starting write to MapleBus");
        self.expecting_response = autostart_read;
        self.response_timeout_us = read_timeout_us;
        self.state = BusPhase::WriteInProgress;
        true
    }

    pub fn start_read(&mut self, timeout_us: u64) -> bool {
        defmt::info!("Starting read on MapleBus");
        self.response_timeout_us = timeout_us;
        self.state = BusPhase::WaitingForReadStart;
        true
    }

    pub fn process_events(&mut self, now_us: u64) -> BusStatus {
        match self.state {
            BusPhase::Idle => BusStatus::Idle,
            BusPhase::WriteInProgress => {
                // In real impl, we’d check DMA completion here
                self.state = BusPhase::WriteComplete;
                BusStatus::WriteComplete
            }
            BusPhase::WaitingForReadStart => {
                // Would start PIO input state machine here
                self.state = BusPhase::ReadInProgress;
                BusStatus::ReadInProgress
            }
            BusPhase::ReadInProgress => {
                if now_us > self.proc_kill_time {
                    self.state = BusPhase::Failed;
                    BusStatus::Error("Timeout".into())
                } else {
                    BusStatus::ReadInProgress
                }
            }
            BusPhase::ReadComplete => {
                self.state = BusPhase::Idle;
                BusStatus::ReadComplete(self.rx_buffer.clone())
            }
            BusPhase::WriteComplete => {
                if self.expecting_response {
                    self.state = BusPhase::WaitingForReadStart;
                    BusStatus::ReadInProgress
                } else {
                    self.state = BusPhase::Idle;
                    BusStatus::WriteComplete
                }
            }
            BusPhase::Failed => {
                self.state = BusPhase::Idle;
                BusStatus::Error("General failure".into())
            }
        }
    }
}
