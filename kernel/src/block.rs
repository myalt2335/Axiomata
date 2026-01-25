pub trait BlockDeviceError {
    fn as_str(&self) -> &'static str;
}

pub trait BlockDevice {
    type Error: BlockDeviceError;

    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<(), Self::Error>;
    fn write_block(&self, block: u64, buf: &[u8]) -> Result<(), Self::Error>;
}
