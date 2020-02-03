use std::io;

const DATA_BITS: u64 = 254;
const TARGET_BITS: u64 = 256;

const BUFFER_BYTE_SIZE: usize = 1;
const BUFFER_BIT_SIZE: usize = BUFFER_BYTE_SIZE * 8;

#[derive(Debug)]
pub struct PadReader<R> {
    /// The source being padded.
    source: R,
    /// How much of the target already was `read` from, in bits.
    target_offset: u64,
    /// Currently read byte.
    buffer: [u8; BUFFER_BYTE_SIZE],
    /// How many bits are available to read in the buffer.
    buffer_avail: usize,
    /// Are we done reading?
    done: bool,
}

impl<R: io::Read> PadReader<R> {
    pub fn new(source: R) -> Self {
        PadReader {
            source,
            target_offset: 0,
            buffer: [0],
            buffer_avail: 0,
            done: false,
        }
    }

    fn read_bit(&mut self) -> bool {
        let val = test_bit(self.buffer[0], self.buffer_offset());
        self.buffer_avail -= 1;
        val
    }

    fn buffer_offset(&self) -> usize {
        BUFFER_BIT_SIZE - self.buffer_avail
    }

    fn read_u8_no_pad(&mut self, target: &mut [u8]) -> io::Result<usize> {
        if !self.fill_buffer()? {
            return Ok(0);
        }

        // copy all available bits
        target[0] = self.buffer[0].wrapping_shr(self.buffer_offset() as u32);
        let bits_read = self.buffer_avail;
        self.buffer_avail -= bits_read;
        self.target_offset += bits_read as u64;

        if bits_read == 8 as usize {
            return Ok(1);
        }

        // we read less than 8 bits, need to do another round
        if !self.fill_buffer()? {
            return Ok(1);
        }
        // copy missing bits
        let bits_to_read = 8 - bits_read;
        let source = self.buffer[0].wrapping_shr(self.buffer_offset() as u32);
        let source = source << bits_read;

        target[0] |= source;
        self.target_offset += bits_to_read as u64;
        self.buffer_avail -= bits_to_read;

        Ok(1)
    }

    /// Read 1 byte into the targets first element.
    /// Assumes that target is not empty.
    fn read_u8(&mut self, target: &mut [u8]) -> io::Result<usize> {
        let bit_pos = self.target_offset % TARGET_BITS;
        let bits_to_padding = if bit_pos < DATA_BITS {
            DATA_BITS as usize - bit_pos as usize
        } else {
            0
        };

        if bits_to_padding >= 8 {
            // No padding is needed this round.
            return self.read_u8_no_pad(target);
        }

        let mut byte = 0;
        for i in 0..8 {
            let bit = if self.target_offset % TARGET_BITS < DATA_BITS {
                if !self.fill_buffer()? {
                    if i > 0 {
                        return Ok(1);
                    } else {
                        return Ok(0);
                    }
                }

                // write data
                self.read_bit()
            } else {
                // write padding
                false
            };

            if bit {
                byte = set_bit(byte, i);
            } else {
                byte = clear_bit(byte, i);
            }

            self.target_offset += 1;
        }

        target[0] = byte;

        Ok(1)
    }

    /// Fill the inner buffer, only if necessary. Returns `true` if more data is available.
    fn fill_buffer(&mut self) -> io::Result<bool> {
        if self.buffer_avail > 0 {
            // Nothing to do, already some data available.
            return Ok(true);
        }

        let read = self.source.read(&mut self.buffer)?;
        self.buffer_avail = read * 8;

        Ok(read > 0)
    }
}

impl<R: io::Read> io::Read for PadReader<R> {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        if self.done || target.is_empty() {
            return Ok(0);
        }

        let mut read = 0;
        while read < target.len() {
            let current_read = self.read_u8(&mut target[read..])?;
            read += current_read;

            if current_read == 0 {
                debug_assert_eq!(self.buffer_avail, 0);
                self.done = true;
                break;
            }
        }

        Ok(read)
    }
}

fn set_bit(x: u8, bit: usize) -> u8 {
    x | (1 << bit)
}

fn clear_bit(x: u8, bit: usize) -> u8 {
    x & !(1 << bit)
}

fn test_bit(x: u8, bit: usize) -> bool {
    x & (1 << bit) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::io::Read;

    #[test]
    fn test_test_bit() {
        assert_eq!(test_bit(0b0000_0000, 0), false);
        assert_eq!(test_bit(0b0000_0001, 0), true);
        assert_eq!(test_bit(0b0000_0001, 1), false);
    }

    #[test]
    fn test_simple_short() {
        // Source is shorter than 1 padding cycle.
        let source = vec![3u8; 30];
        let mut reader = PadReader::new(io::Cursor::new(&source));
        let mut target = Vec::new();
        reader.read_to_end(&mut target).unwrap();
        assert_eq!(&source[..], &target[..]);
    }

    #[test]
    fn test_simple_single() {
        let data = vec![255u8; 32];
        let mut padded = Vec::new();
        let mut reader = PadReader::new(io::Cursor::new(&data));
        reader.read_to_end(&mut padded).unwrap();

        assert_eq!(&padded[0..31], &data[0..31]);
        assert_eq!(padded[31], 0b0011_1111);
        assert_eq!(padded[32], 0b0000_0011);
        assert_eq!(padded.len(), 33);

        assert_eq!(padded.into_boxed_slice(), bit_vec_padding(data));
    }

    #[test]
    fn test_simple_long() {
        let data = vec![255u8; 151];
        let mut padded = Vec::new();
        let mut reader = PadReader::new(io::Cursor::new(&data));
        reader.read_to_end(&mut padded).unwrap();

        assert_eq!(&padded[0..31], &data[0..31]);
        assert_eq!(padded[31], 0b0011_1111);
        assert_eq!(padded[32], 0b1111_1111);
        assert_eq!(&padded[33..63], vec![255u8; 30].as_slice());
        assert_eq!(padded[63], 0b0011_1111);

        assert_eq!(padded.into_boxed_slice(), bit_vec_padding(data));
    }

    #[test]
    fn test_chained_byte_source() {
        let random_bytes: Vec<u8> = (0..127).map(|_| rand::random::<u8>()).collect();

        // read 127 bytes from a non-chained source
        let output_x = {
            let input_x = io::Cursor::new(random_bytes.clone());

            let mut reader = PadReader::new(input_x);
            let mut buf_x = Vec::new();
            reader.read_to_end(&mut buf_x).expect("could not seek");
            buf_x
        };

        // read 127 bytes from a 32-byte buffer and then a 95-byte buffer
        let output_y = {
            let input_y =
                io::Cursor::new(random_bytes.iter().take(32).cloned().collect::<Vec<u8>>()).chain(
                    io::Cursor::new(random_bytes.iter().skip(32).cloned().collect::<Vec<u8>>()),
                );

            let mut reader = PadReader::new(input_y);
            let mut buf_y = Vec::new();
            reader.read_to_end(&mut buf_y).expect("could not seek");

            buf_y
        };

        assert_eq!(output_x, output_y, "should have written same bytes");
        assert_eq!(output_x.into_boxed_slice(), bit_vec_padding(random_bytes));
    }

    #[test]
    fn test_full() {
        let data = vec![255u8; 127];

        let mut buf = Vec::new();
        let mut reader = PadReader::new(io::Cursor::new(&data));
        reader.read_to_end(&mut buf).unwrap();

        assert_eq!(buf.clone().into_boxed_slice(), bit_vec_padding(data));
        validate_fr32(&buf);
    }

    // Simple (and slow) padder implementation using `BitVec`.
    // It is technically not quite right to use `BitVec` to test this, since at
    // the moment that function still uses
    // it for some corner cases, but since largely this implementation
    // has been replaced it seems reasonable.
    fn bit_vec_padding(raw_data: Vec<u8>) -> Box<[u8]> {
        use bitvec::{BitVec, LittleEndian};
        use itertools::Itertools;

        let mut padded_data: BitVec<LittleEndian, u8> = BitVec::new();
        let raw_data: BitVec<LittleEndian, u8> = BitVec::from(raw_data);

        for data_unit in raw_data.into_iter().chunks(DATA_BITS as usize).into_iter() {
            padded_data.extend(data_unit.into_iter());

            // To avoid reconverting the iterator, we deduce if we need the padding
            // by the length of `padded_data`: a full data unit would not leave the
            // padded layout aligned (it would leave it unaligned by just `pad_bits()`).
            if padded_data.len() % 8 != 0 {
                for _ in 0..(TARGET_BITS - DATA_BITS) {
                    padded_data.push(false);
                }
            }
        }

        padded_data.into_boxed_slice()
    }

    fn validate_fr32(bytes: &[u8]) {
        let chunks = (bytes.len() as f64 / 32 as f64).ceil() as usize;
        for (i, chunk) in bytes.chunks(32).enumerate() {
            let _ = storage_proofs::fr32::bytes_into_fr::<paired::bls12_381::Bls12>(chunk).expect(
                &format!(
                    "chunk {}/{} cannot be converted to valid Fr: {:?}",
                    i + 1,
                    chunks,
                    chunk
                ),
            );
        }
    }

    // raw data stream of increasing values and specific
    // outliers (0xFF, 9), check the content of the raw data encoded (with
    // different alignments) in the padded layouts.
    #[test]
    fn test_exotic() {
        let mut source = vec![
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 0xff, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
            16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 0xff, 9, 9,
        ];
        source.extend(vec![9, 0xff]);

        let mut buf = Vec::new();
        let mut reader = PadReader::new(io::Cursor::new(&source));
        reader.read_to_end(&mut buf).unwrap();

        for i in 0..31 {
            assert_eq!(buf[i], i as u8 + 1);
        }
        assert_eq!(buf[31], 63); // Six least significant bits of 0xff
        assert_eq!(buf[32], (1 << 2) | 0b11); // 7
        for i in 33..63 {
            assert_eq!(buf[i], (i as u8 - 31) << 2);
        }
        assert_eq!(buf[63], (0x0f << 2)); // 4-bits of ones, half of 0xff, shifted by two, followed by two bits of 0-padding.
        assert_eq!(buf[64], 0x0f | 9 << 4); // The last half of 0xff, 'followed' by 9.
        assert_eq!(buf[65], 9 << 4); // A shifted 9.
        assert_eq!(buf[66], 9 << 4); // Another.
        assert_eq!(buf[67], 0xf0); // The final 0xff is split into two bytes. Here is the first half.
        assert_eq!(buf[68], 0x0f); // And here is the second.

        assert_eq!(buf.into_boxed_slice(), bit_vec_padding(source));
    }
}
