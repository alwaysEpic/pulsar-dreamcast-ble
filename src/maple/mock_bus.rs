use crate::maple::{MaplePacket, bus::BusStatus, traits::MapleBus};

pub struct MockMapleBus;

impl MapleBus for MockMapleBus {
    fn write(&mut self, packet: &MaplePacket, autostart_read: bool, timeout_us: u64) -> bool {
        let mut buffer = heapless::Vec::<u32, 258>::new();
        packet.encode(&mut buffer);
        for (i, word) in buffer.iter().enumerate() {
            defmt::info!("MockBus TX Word[{}] = {=u32:X}", i, word);
        }
        true
    }

    fn process_events(&mut self, _now_us: u64) -> BusStatus {
        BusStatus::Idle
    }
}
