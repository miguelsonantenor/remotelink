//! Recording input injector for CI and unit tests (no OS APIs).

use remotelink_protocol::InputEvent;

use super::{InjectError, InputInjector};

/// Injector that records events instead of calling the OS.
///
/// Safe on all platforms; used by default on non-Windows and whenever
/// [`super::InjectorOpenMode::StubOnly`] is selected.
#[derive(Debug, Clone, Default)]
pub struct StubInjector {
    recorded: Vec<InputEvent>,
    screen_width: u32,
    screen_height: u32,
}

impl StubInjector {
    /// Create a stub with the given virtual screen size (for pixel mapping tests).
    pub fn new(screen_width: u32, screen_height: u32) -> Self {
        Self {
            recorded: Vec::new(),
            screen_width: screen_width.max(1),
            screen_height: screen_height.max(1),
        }
    }

    /// Events accepted so far (in order).
    pub fn recorded(&self) -> &[InputEvent] {
        &self.recorded
    }

    /// Drain and return all recorded events.
    pub fn take_recorded(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.recorded)
    }

    /// Number of recorded events.
    pub fn len(&self) -> usize {
        self.recorded.len()
    }

    /// Whether no events have been recorded.
    pub fn is_empty(&self) -> bool {
        self.recorded.is_empty()
    }

    /// Configured virtual screen width.
    pub fn screen_width(&self) -> u32 {
        self.screen_width
    }

    /// Configured virtual screen height.
    pub fn screen_height(&self) -> u32 {
        self.screen_height
    }

    /// Map normalized coordinates to pixels using the stub screen size.
    pub fn map_to_pixels(&self, x: f32, y: f32) -> (i32, i32) {
        let x = x.clamp(0.0, 1.0);
        let y = y.clamp(0.0, 1.0);
        let px = (x * (self.screen_width.saturating_sub(1) as f32)).round() as i32;
        let py = (y * (self.screen_height.saturating_sub(1) as f32)).round() as i32;
        (px, py)
    }
}

impl InputInjector for StubInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), InjectError> {
        self.recorded.push(event.clone());
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "stub"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_protocol::{InputPayload, KeyEvent, MouseMove};

    #[test]
    fn records_in_order() {
        let mut stub = StubInjector::new(100, 100);
        let a = InputEvent {
            client_ts_us: 1,
            seq: 1,
            payload: InputPayload::MouseMove(MouseMove {
                x: 0.0,
                y: 0.0,
                display_id: 0,
            }),
        };
        let b = InputEvent {
            client_ts_us: 2,
            seq: 2,
            payload: InputPayload::Key(KeyEvent {
                scancode: 0x1E,
                extended: false,
                pressed: true,
                modifiers: 0,
            }),
        };
        stub.inject(&a).unwrap();
        stub.inject(&b).unwrap();
        assert_eq!(stub.len(), 2);
        assert_eq!(stub.recorded()[0].seq, 1);
        assert_eq!(stub.recorded()[1].seq, 2);
        assert_eq!(stub.map_to_pixels(1.0, 1.0), (99, 99));
    }
}
