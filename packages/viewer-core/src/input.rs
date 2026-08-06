//! Viewer → host input event emitter (DataChannel JSON).

use std::time::{SystemTime, UNIX_EPOCH};

use remotelink_net::DataMessage;
use remotelink_protocol::{
    encode_input, InputEvent, InputPayload, KeyEvent, MouseButton, MouseButtonKind, MouseMove,
    MouseWheel,
};

use crate::error::Result;

/// Label used on the PeerTransport DataChannel for input.
pub const INPUT_CHANNEL_LABEL: &str = "input";

/// Builds sequenced [`InputEvent`]s and encoded [`DataMessage`]s for the host.
#[derive(Debug, Clone)]
pub struct InputEmitter {
    next_seq: u32,
    /// When true, mouse-move messages are marked unordered (partial reliability hint).
    moves_unordered: bool,
    /// Events successfully emitted (encoded).
    emitted: u64,
}

impl Default for InputEmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl InputEmitter {
    /// Create an emitter starting at sequence 1.
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            moves_unordered: true,
            emitted: 0,
        }
    }

    /// Number of events encoded so far.
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Next sequence number that will be assigned.
    pub fn next_seq(&self) -> u32 {
        self.next_seq
    }

    /// Build a full [`InputEvent`] with the next sequence and current client timestamp.
    pub fn make_event(&mut self, payload: InputPayload) -> InputEvent {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        InputEvent {
            client_ts_us: client_now_us(),
            seq,
            payload,
        }
    }

    /// Encode an event as a DataChannel message on the input label.
    pub fn encode_message(&mut self, event: &InputEvent) -> Result<DataMessage> {
        let json = encode_input(event)?;
        self.emitted = self.emitted.saturating_add(1);
        let unordered = matches!(event.payload, InputPayload::MouseMove(_));
        Ok(DataMessage {
            label: INPUT_CHANNEL_LABEL.into(),
            data: json.into_bytes(),
            unordered: unordered && self.moves_unordered,
        })
    }

    /// Convenience: mouse move in normalized coordinates.
    pub fn mouse_move(&mut self, x: f32, y: f32) -> Result<(InputEvent, DataMessage)> {
        let event = self.make_event(InputPayload::MouseMove(MouseMove {
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            display_id: 0,
        }));
        let msg = self.encode_message(&event)?;
        Ok((event, msg))
    }

    /// Convenience: mouse button.
    pub fn mouse_button(
        &mut self,
        button: MouseButtonKind,
        pressed: bool,
        x: f32,
        y: f32,
    ) -> Result<(InputEvent, DataMessage)> {
        let event = self.make_event(InputPayload::MouseButton(MouseButton {
            button,
            pressed,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            display_id: 0,
        }));
        let msg = self.encode_message(&event)?;
        Ok((event, msg))
    }

    /// Convenience: mouse wheel.
    pub fn mouse_wheel(
        &mut self,
        delta_x: f32,
        delta_y: f32,
        precise: bool,
        x: f32,
        y: f32,
    ) -> Result<(InputEvent, DataMessage)> {
        let event = self.make_event(InputPayload::MouseWheel(MouseWheel {
            delta_x,
            delta_y,
            precise,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            display_id: 0,
        }));
        let msg = self.encode_message(&event)?;
        Ok((event, msg))
    }

    /// Convenience: key event.
    pub fn key(
        &mut self,
        scancode: u32,
        extended: bool,
        pressed: bool,
        modifiers: u32,
    ) -> Result<(InputEvent, DataMessage)> {
        let event = self.make_event(InputPayload::Key(KeyEvent {
            scancode,
            extended,
            pressed,
            modifiers,
        }));
        let msg = self.encode_message(&event)?;
        Ok((event, msg))
    }
}

fn client_now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_protocol::decode_input;

    #[test]
    fn sequences_increase() {
        let mut em = InputEmitter::new();
        let (e1, _) = em.mouse_move(0.5, 0.5).unwrap();
        let (e2, _) = em.mouse_move(0.6, 0.5).unwrap();
        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        assert_eq!(em.emitted(), 2);
    }

    #[test]
    fn message_roundtrips_protocol() {
        let mut em = InputEmitter::new();
        let (event, msg) = em
            .mouse_button(MouseButtonKind::Left, true, 0.1, 0.2)
            .unwrap();
        assert_eq!(msg.label, INPUT_CHANNEL_LABEL);
        let decoded = decode_input(std::str::from_utf8(&msg.data).unwrap()).unwrap();
        assert_eq!(decoded.seq, event.seq);
        assert!(matches!(
            decoded.payload,
            InputPayload::MouseButton(MouseButton {
                button: MouseButtonKind::Left,
                pressed: true,
                ..
            })
        ));
    }

    #[test]
    fn mouse_move_marked_unordered() {
        let mut em = InputEmitter::new();
        let (_, msg) = em.mouse_move(0.0, 0.0).unwrap();
        assert!(msg.unordered);
        let (_, msg2) = em.key(0x1C, false, true, 0).unwrap();
        assert!(!msg2.unordered);
    }
}
