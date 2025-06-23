use crate::maple::{MaplePacket, bus::BusStatus};

pub trait MapleBusInterface {
    fn write(&mut self, packet: &MaplePacket, autostart_read: bool, timeout_us: u64) -> bool;
    fn start_read(&mut self, timeout_us: u64) -> bool;
    fn process_events(&mut self, now_us: u64) -> BusStatus;
}

pub trait MapleBus {
    fn write(&mut self, packet: &MaplePacket, autostart_read: bool, timeout_us: u64) -> bool;
    fn process_events(&mut self, now_us: u64) -> BusStatus;
}
