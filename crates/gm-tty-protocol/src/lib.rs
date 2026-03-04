use std::fmt;

#[derive(Debug)]
pub enum ProtocolError {
    InsufficientData,
    UnknownFrameType(u8),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsufficientData => write!(f, "insufficient data"),
            Self::UnknownFrameType(t) => write!(f, "unknown frame type: 0x{t:02x}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameType {
    Data = 0x01,
    Resize = 0x02,
    Heartbeat = 0x03,
    HeartbeatAck = 0x04,
    Close = 0x05,
}

impl TryFrom<u8> for FrameType {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::Data),
            0x02 => Ok(Self::Resize),
            0x03 => Ok(Self::Heartbeat),
            0x04 => Ok(Self::HeartbeatAck),
            0x05 => Ok(Self::Close),
            _ => Err(ProtocolError::UnknownFrameType(value)),
        }
    }
}

/// 帧格式: [Type: 1B] [Length: 4B BE] [Payload: NB]
#[derive(Debug, Clone)]
pub struct Frame {
    pub frame_type: FrameType,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(frame_type: FrameType, payload: Vec<u8>) -> Self {
        Self { frame_type, payload }
    }

    pub fn new_data(data: &[u8]) -> Self {
        Self::new(FrameType::Data, data.to_vec())
    }

    pub fn new_resize(cols: u16, rows: u16) -> Self {
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&cols.to_be_bytes());
        payload.extend_from_slice(&rows.to_be_bytes());
        Self::new(FrameType::Resize, payload)
    }

    pub fn heartbeat() -> Self {
        Self::new(FrameType::Heartbeat, Vec::new())
    }

    pub fn heartbeat_ack() -> Self {
        Self::new(FrameType::HeartbeatAck, Vec::new())
    }

    pub fn close() -> Self {
        Self::new(FrameType::Close, Vec::new())
    }

    pub fn encode(&self) -> Vec<u8> {
        let len = self.payload.len() as u32;
        let mut buf = Vec::with_capacity(5 + self.payload.len());
        buf.push(self.frame_type as u8);
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < 5 {
            return Err(ProtocolError::InsufficientData);
        }
        let frame_type = FrameType::try_from(data[0])?;
        let len = u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize;
        if data.len() < 5 + len {
            return Err(ProtocolError::InsufficientData);
        }
        Ok(Frame {
            frame_type,
            payload: data[5..5 + len].to_vec(),
        })
    }

    /// 解析 Resize 帧的 (cols, rows)
    pub fn parse_resize(&self) -> Option<(u16, u16)> {
        if self.frame_type != FrameType::Resize || self.payload.len() < 4 {
            return None;
        }
        let cols = u16::from_be_bytes([self.payload[0], self.payload[1]]);
        let rows = u16::from_be_bytes([self.payload[2], self.payload[3]]);
        Some((cols, rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_data() {
        let data = b"hello world";
        let frame = Frame::new_data(data);
        let encoded = frame.encode();
        let decoded = Frame::decode(&encoded).unwrap();
        assert_eq!(decoded.frame_type, FrameType::Data);
        assert_eq!(decoded.payload, data);
    }

    #[test]
    fn test_encode_decode_resize() {
        let frame = Frame::new_resize(120, 40);
        let encoded = frame.encode();
        let decoded = Frame::decode(&encoded).unwrap();
        assert_eq!(decoded.frame_type, FrameType::Resize);
        let (cols, rows) = decoded.parse_resize().unwrap();
        assert_eq!(cols, 120);
        assert_eq!(rows, 40);
    }

    #[test]
    fn test_insufficient_data() {
        assert!(matches!(
            Frame::decode(&[0x01, 0x00]),
            Err(ProtocolError::InsufficientData)
        ));
    }

    #[test]
    fn test_unknown_frame_type() {
        let data = [0xFF, 0x00, 0x00, 0x00, 0x00];
        assert!(matches!(
            Frame::decode(&data),
            Err(ProtocolError::UnknownFrameType(0xFF))
        ));
    }
}
