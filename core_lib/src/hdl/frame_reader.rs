//! Reads length-prefixed frames off a transport in a task of its own.
//!
//! **Why this exists.** Both request handlers used to do:
//!
//! ```ignore
//! tokio::select! {
//!     i = self.receiver.recv() => { ... }
//!     h = stream_read_exact(&mut self.socket, &mut length_buf) => { ... }
//! }
//! ```
//!
//! `stream_read_exact` is not cancel-safe. When the channel branch wins while
//! part of a length prefix has already been read, that read future is dropped
//! and the consumed bytes are gone - the stream desyncs and the next frame
//! parses as garbage. `receiver` is `sender.subscribe()`, so the handler's *own*
//! progress broadcasts come back to it and fire that branch repeatedly during a
//! transfer, which is the intermittent "Missing required fields" seen mid-
//! handshake with reassembly provably clean.
//!
//! Reading in a dedicated task removes the cancellation entirely: nothing races
//! a partial read. The handler then selects over two channels, both of which
//! *are* cancel-safe.
//!
//! It also decouples reading from writing, which is what lets a long send answer
//! keepalives. The send loop writes chunk after chunk without reading, so over
//! BLE - where a 1.9 MB file takes ~95s at 20 KB/s - the peer saw no KeepAlive
//! response and closed the connection at its 30s timeout, capping outbound
//! transfers at whatever fits in 30s.
//!
//! This task does **framing only**. It never decrypts, so there is no shared
//! crypto state and no mutex: sequence numbers and keys stay entirely inside the
//! handler.

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

use crate::utils::stream_read_exact;

/// A frame as it came off the wire, still encrypted.
pub type RawFrame = Vec<u8>;

/// Spawn a reader over `reader`, returning the channel its frames arrive on.
///
/// The channel is bounded so a peer cannot make us buffer without limit: if the
/// handler is busy, the reader stops reading and the transport applies its own
/// backpressure. Four frames is enough to keep the handler fed without letting
/// a fast sender run far ahead.
pub fn spawn_frame_reader<R>(mut reader: R, sane_frame_length: usize) -> mpsc::Receiver<RawFrame>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<RawFrame>(4);

    tokio::spawn(async move {
        loop {
            let mut length_buf = [0u8; 4];
            if let Err(e) = stream_read_exact(&mut reader, &mut length_buf).await {
                // EOF is the normal end of a session; anything else is worth a
                // line, but neither is fatal here - dropping `tx` tells the
                // handler the transport is finished.
                trace!("frame reader: read ended: {e}");
                break;
            }

            let msg_length = u32::from_be_bytes(length_buf) as usize;
            if msg_length == 0 || msg_length > sane_frame_length {
                error!("frame reader: refusing insane frame length {msg_length}");
                break;
            }

            let mut frame = vec![0u8; msg_length];
            if let Err(e) = reader.read_exact(&mut frame).await {
                trace!("frame reader: incomplete frame: {e}");
                break;
            }

            if tx.send(frame).await.is_err() {
                // Handler is gone.
                break;
            }
        }
    });

    rx
}
