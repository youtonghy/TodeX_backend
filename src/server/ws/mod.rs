//! `/v2/ws` socket plumbing below the business dispatcher: the transport
//! handshake, the frame codec (plaintext, transport v1, transport v2) and the
//! socket read/write loops. Dispatch code only ever sees JSON text.
pub(crate) mod codec;
pub(crate) mod socket;
