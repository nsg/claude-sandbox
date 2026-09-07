use std::io::{self, Read, Write};

use base64::Engine;
use sha1::{Digest, Sha1};

const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
pub const MAX_PAYLOAD_LEN: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Opcode {
    Continuation = 0,
    Text = 1,
    Binary = 2,
    Close = 8,
    Ping = 9,
    Pong = 10,
}

impl Opcode {
    fn from_u8(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::Continuation),
            1 => Ok(Self::Text),
            2 => Ok(Self::Binary),
            8 => Ok(Self::Close),
            9 => Ok(Self::Ping),
            10 => Ok(Self::Pong),
            _ => Err(invalid_data("reserved WebSocket opcode")),
        }
    }

    const fn is_control(self) -> bool {
        matches!(self, Self::Close | Self::Ping | Self::Pong)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Frame {
    pub fin: bool,
    pub opcode: Opcode,
    pub payload: Vec<u8>,
}

pub fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(WEBSOCKET_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

pub fn read_frame(reader: &mut impl Read) -> io::Result<Frame> {
    read_frame_inner(reader, true)
}

fn read_frame_inner(reader: &mut impl Read, require_mask: bool) -> io::Result<Frame> {
    let mut header = [0_u8; 2];
    reader.read_exact(&mut header)?;

    let fin = header[0] & 0x80 != 0;
    if header[0] & 0x70 != 0 {
        return Err(invalid_data("WebSocket RSV bits must be zero"));
    }
    let opcode = Opcode::from_u8(header[0] & 0x0f)?;

    let masked = header[1] & 0x80 != 0;
    if require_mask && !masked {
        return Err(invalid_data("client WebSocket frames must be masked"));
    }

    let short_len = header[1] & 0x7f;
    let payload_len = match short_len {
        126 => {
            let mut bytes = [0_u8; 2];
            reader.read_exact(&mut bytes)?;
            let len = u16::from_be_bytes(bytes) as u64;
            if len < 126 {
                return Err(invalid_data("non-canonical WebSocket payload length"));
            }
            len
        }
        127 => {
            let mut bytes = [0_u8; 8];
            reader.read_exact(&mut bytes)?;
            let len = u64::from_be_bytes(bytes);
            if len >> 63 != 0 {
                return Err(invalid_data("WebSocket payload length is too large"));
            }
            if len <= u16::MAX as u64 {
                return Err(invalid_data("non-canonical WebSocket payload length"));
            }
            len
        }
        len => u64::from(len),
    };

    if opcode.is_control() && (!fin || payload_len > 125) {
        return Err(invalid_data(
            "WebSocket control frames must be final and at most 125 bytes",
        ));
    }
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(invalid_data("WebSocket payload exceeds the size limit"));
    }

    let mask = if masked {
        let mut mask = [0_u8; 4];
        reader.read_exact(&mut mask)?;
        Some(mask)
    } else {
        None
    };

    let mut payload = vec![0_u8; payload_len as usize];
    reader.read_exact(&mut payload)?;
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % mask.len()];
        }
    }

    Ok(Frame {
        fin,
        opcode,
        payload,
    })
}

pub fn write_frame(writer: &mut impl Write, opcode: Opcode, payload: &[u8]) -> io::Result<()> {
    let payload_len = u64::try_from(payload.len())
        .map_err(|_| invalid_data("WebSocket payload length does not fit in u64"))?;
    if opcode.is_control() && payload_len > 125 {
        return Err(invalid_data(
            "WebSocket control frames must be at most 125 bytes",
        ));
    }
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(invalid_data("WebSocket payload exceeds the size limit"));
    }

    writer.write_all(&[0x80 | opcode as u8])?;
    match payload_len {
        0..=125 => writer.write_all(&[payload_len as u8])?,
        126..=65_535 => {
            writer.write_all(&[126])?;
            writer.write_all(&(payload_len as u16).to_be_bytes())?;
        }
        _ => {
            writer.write_all(&[127])?;
            writer.write_all(&payload_len.to_be_bytes())?;
        }
    }
    writer.write_all(payload)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, ErrorKind};

    use super::*;

    const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

    fn masked_frame(fin: bool, opcode: Opcode, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![(u8::from(fin) << 7) | opcode as u8];
        match payload.len() {
            0..=125 => frame.push(0x80 | payload.len() as u8),
            126..=65_535 => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            _ => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&MASK);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| *byte ^ MASK[index % MASK.len()]),
        );
        frame
    }

    #[test]
    fn accept_key_matches_rfc_vector() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn parses_masked_short_payload() {
        let bytes = masked_frame(true, Opcode::Text, b"Hello");
        assert_eq!(
            read_frame(&mut Cursor::new(bytes)).unwrap(),
            Frame {
                fin: true,
                opcode: Opcode::Text,
                payload: b"Hello".to_vec(),
            }
        );
    }

    #[test]
    fn parses_masked_u16_payload() {
        let payload = vec![0x5a; 126];
        let bytes = masked_frame(true, Opcode::Binary, &payload);
        assert_eq!(
            read_frame(&mut Cursor::new(bytes)).unwrap().payload,
            payload
        );
    }

    #[test]
    fn parses_masked_u64_payload() {
        let payload = vec![0xa5; 65_536];
        let bytes = masked_frame(true, Opcode::Binary, &payload);
        assert_eq!(
            read_frame(&mut Cursor::new(bytes)).unwrap().payload,
            payload
        );
    }

    #[test]
    fn rejects_unmasked_client_frame() {
        let error = read_frame(&mut Cursor::new([0x82, 0x01, 0x00])).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_rsv_bits() {
        let mut bytes = masked_frame(true, Opcode::Binary, b"data");
        bytes[0] |= 0x40;
        let error = read_frame(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_reserved_opcode() {
        let mut bytes = masked_frame(true, Opcode::Binary, b"data");
        bytes[0] = 0x83;
        let error = read_frame(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_non_canonical_and_excessive_lengths() {
        let non_canonical_u16 = [0x82, 0xfe, 0x00, 0x7d];
        assert_eq!(
            read_frame(&mut Cursor::new(non_canonical_u16))
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidData
        );

        let mut non_canonical_u64 = vec![0x82, 0xff];
        non_canonical_u64.extend_from_slice(&65_535_u64.to_be_bytes());
        assert_eq!(
            read_frame(&mut Cursor::new(non_canonical_u64))
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidData
        );

        let mut excessive = vec![0x82, 0xff];
        excessive.extend_from_slice(&(MAX_PAYLOAD_LEN + 1).to_be_bytes());
        assert_eq!(
            read_frame(&mut Cursor::new(excessive)).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_invalid_control_frames() {
        let fragmented_ping = masked_frame(false, Opcode::Ping, b"ping");
        assert_eq!(
            read_frame(&mut Cursor::new(fragmented_ping))
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidData
        );

        let oversized_payload = vec![0; 126];
        let oversized_ping = masked_frame(true, Opcode::Ping, &oversized_payload);
        assert_eq!(
            read_frame(&mut Cursor::new(oversized_ping))
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(
            write_frame(&mut Vec::new(), Opcode::Pong, &oversized_payload)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn parses_control_frames() {
        for (opcode, payload) in [
            (Opcode::Ping, b"ping".as_slice()),
            (Opcode::Pong, b"pong".as_slice()),
            (Opcode::Close, [0x03, 0xe8].as_slice()),
        ] {
            let bytes = masked_frame(true, opcode, payload);
            let frame = read_frame(&mut Cursor::new(bytes)).unwrap();
            assert!(frame.fin);
            assert_eq!(frame.opcode, opcode);
            assert_eq!(frame.payload, payload);
        }
    }

    #[test]
    fn writes_each_length_class_and_round_trips() {
        for payload in [vec![0x11; 125], vec![0x22; 126], vec![0x33; 65_536]] {
            let mut encoded = Vec::new();
            write_frame(&mut encoded, Opcode::Binary, &payload).unwrap();

            let frame = read_frame_inner(&mut Cursor::new(&encoded), false).unwrap();
            assert_eq!(
                frame,
                Frame {
                    fin: true,
                    opcode: Opcode::Binary,
                    payload,
                }
            );
            assert_eq!(encoded[1] & 0x80, 0, "server frames must be unmasked");
        }
    }

    #[test]
    fn write_uses_expected_length_encodings() {
        let mut short = Vec::new();
        write_frame(&mut short, Opcode::Binary, &[0; 125]).unwrap();
        assert_eq!(&short[..2], &[0x82, 125]);

        let mut u16_length = Vec::new();
        write_frame(&mut u16_length, Opcode::Binary, &[0; 126]).unwrap();
        assert_eq!(&u16_length[..4], &[0x82, 126, 0, 126]);

        let mut u64_length = Vec::new();
        write_frame(&mut u64_length, Opcode::Binary, &[0; 65_536]).unwrap();
        assert_eq!(&u64_length[..10], &[0x82, 127, 0, 0, 0, 0, 0, 1, 0, 0]);
    }

    #[test]
    fn reports_truncated_frames() {
        let error = read_frame(&mut Cursor::new([0x82])).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnexpectedEof);

        let bytes = masked_frame(true, Opcode::Binary, b"payload");
        let error = read_frame(&mut Cursor::new(&bytes[..bytes.len() - 1])).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
    }
}
