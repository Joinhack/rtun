struct FecPacket<'a>(&'a [u8]);

impl<'a> FecPacket<'a> {
    fn seq_id(&self) -> u32 {
        // Assuming the first 4 bytes represent the sequence number in big-endian format
        let bytes: [u8; 4] = self.0[0..4].try_into().unwrap();
        u32::from_le_bytes(bytes)
    }

    fn flag(&self) -> u16 {
        let bytes: [u8; 2] = self.0[4..6].try_into().unwrap();
        u16::from_le_bytes(bytes)
    }

    fn data(&self) -> &[u8] {
        &self.0[6..]
    }
}
