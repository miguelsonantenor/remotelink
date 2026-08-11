//! Control-only local IPC between host service and session agent.
//!
//! Wire format: **length-prefix JSON** (`u32` BE payload length + UTF-8 JSON body).
//! Body is a versioned [`message::ControlEnvelope`]:
//! `{"v":1,"message":{"method":"…","params":{…}}}`.
//!
//! On Windows production hosts this rides a **named pipe** with a restrictive
//! SDDL DACL (SYSTEM + Administrators + Owner) and `PIPE_REJECT_REMOTE_CLIENTS`.
//! For CI and non-Windows tests the same codec runs over TCP localhost (or any
//! byte stream). Design mentions protobuf as a long-term option; v1 uses
//! serde JSON for simplicity and inspectability.

pub mod codec;
pub mod message;
#[cfg(windows)]
pub mod pipe;
pub mod transport;

pub use codec::{
    decode_control, decode_frame, encode_control, encode_frame, read_control, read_frame,
    write_control, write_frame, CodecError, MAX_FRAME_PAYLOAD,
};
pub use message::*;
pub use transport::{
    connect_control, listen_control, ControlEndpoint, ControlListener, ControlStream,
};
