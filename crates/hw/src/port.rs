use crate::HwError;
use crate::pawnio::{LPC_IO_MODULE, PawnIoModule};
use crate::superio::Slot;

pub trait PortIo: Send {
    fn read(&mut self, port: u16) -> Result<u8, HwError>;
    fn write(&mut self, port: u16, value: u8) -> Result<(), HwError>;
    fn find_bars(&mut self) -> Result<(), HwError>;
}

#[derive(Debug)]
pub struct LpcIo {
    module: PawnIoModule,
}

impl LpcIo {
    pub fn open(slot: Slot) -> Result<Self, HwError> {
        let module = PawnIoModule::load(LPC_IO_MODULE)?;
        module.execute("ioctl_select_slot", &[slot.index()], 0)?;
        Ok(Self { module })
    }
}

impl PortIo for LpcIo {
    fn read(&mut self, port: u16) -> Result<u8, HwError> {
        let output = self
            .module
            .execute("ioctl_pio_inb", &[u64::from(port)], 1)?;
        Ok(output[0] as u8)
    }

    fn write(&mut self, port: u16, value: u8) -> Result<(), HwError> {
        self.module
            .execute("ioctl_pio_outb", &[u64::from(port), u64::from(value)], 0)
            .map(drop)
    }

    fn find_bars(&mut self) -> Result<(), HwError> {
        self.module.execute("ioctl_find_bars", &[], 0).map(drop)
    }
}
