//! Serial/TCP framing for the MeshCore companion protocol.
//!
//! Each frame is a direction byte, a little-endian u16 payload length, then the payload.
//! `<` marks host to radio and `>` marks radio to host. The first payload byte is the command
//! or response code.

pub const TO_RADIO: u8 = b'<';
pub const FROM_RADIO: u8 = b'>';
/// Largest payload the radio firmware sends or accepts.
pub const MAX_PAYLOAD: usize = 172;

pub fn encode(dir: u8, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    let mut out = Vec::with_capacity(payload.len() + 3);
    out.push(dir);
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Reassembles frames from a byte stream. Bytes before a frame marker (debug text from the
/// firmware, or the tail of a frame cut off by a reconnect) are skipped, and a length that is
/// impossible makes the decoder resynchronise on the next marker.
pub struct Decoder {
    want: u8,
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new(want: u8) -> Decoder {
        Decoder { want, buf: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            // Drop everything before the next marker.
            match self.buf.iter().position(|&b| b == self.want) {
                None => {
                    self.buf.clear();
                    break;
                }
                Some(n) if n > 0 => {
                    self.buf.drain(..n);
                }
                _ => {}
            }
            if self.buf.len() < 3 {
                break;
            }
            let len = u16::from_le_bytes([self.buf[1], self.buf[2]]) as usize;
            if len == 0 || len > MAX_PAYLOAD {
                // Not a real frame start: skip this marker byte and look for another.
                self.buf.remove(0);
                continue;
            }
            if self.buf.len() < 3 + len {
                break;
            }
            let frame: Vec<u8> = self.buf[3..3 + len].to_vec();
            self.buf.drain(..3 + len);
            frames.push(frame);
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_split_across_reads() {
        let f = encode(FROM_RADIO, &[5, 1, 2, 3]);
        assert_eq!(f, vec![b'>', 4, 0, 5, 1, 2, 3]);
        let mut d = Decoder::new(FROM_RADIO);
        assert!(d.push(&f[..2]).is_empty());
        assert!(d.push(&f[2..5]).is_empty());
        assert_eq!(d.push(&f[5..]), vec![vec![5, 1, 2, 3]]);
    }

    #[test]
    fn two_frames_in_one_read_and_leading_noise() {
        let mut bytes = b"boot log\r\n".to_vec();
        bytes.extend(encode(FROM_RADIO, &[0]));
        bytes.extend(encode(FROM_RADIO, &[0x80, 9, 9]));
        let mut d = Decoder::new(FROM_RADIO);
        assert_eq!(d.push(&bytes), vec![vec![0], vec![0x80, 9, 9]]);
    }

    #[test]
    fn impossible_length_resyncs() {
        // A stray marker followed by a huge length, then a good frame.
        let mut bytes = vec![b'>', 0xff, 0xff];
        bytes.extend(encode(FROM_RADIO, &[1, 7]));
        let mut d = Decoder::new(FROM_RADIO);
        assert_eq!(d.push(&bytes), vec![vec![1, 7]]);
    }

    #[test]
    fn ignores_the_other_direction() {
        let mut d = Decoder::new(FROM_RADIO);
        assert!(d.push(&encode(TO_RADIO, &[4])).is_empty());
    }
}
