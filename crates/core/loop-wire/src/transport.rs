//! Transports carry whole messages between two peers.
//!
//! A [`Transport`] is a pair of channels of JSON texts, one message each.
//! Adapters turn a byte stream or a socket into one: [`lines`] for stdio and
//! Unix sockets (one message per line), [`memory`] for two peers in one
//! process. Dropping the outgoing side closes the underlying stream's output.

use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Longest message accepted from the peer. Longer ones are dropped so a
/// misbehaving peer cannot exhaust memory.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Most whole messages queued in either direction on a WebSocket.
pub const SOCKET_QUEUE_MESSAGES: usize = 128;

#[derive(Debug)]
struct QueuedMessage {
    text: String,
    _permit: OwnedSemaphorePermit,
}

/// Nonblocking message sender. Bounded sockets disconnect on overflow so a
/// slow Surface can reconnect and replay instead of exhausting the host.
#[derive(Debug, Clone)]
pub struct MessageSender(SenderKind);

#[derive(Debug, Clone)]
enum SenderKind {
    Unbounded(mpsc::UnboundedSender<String>),
    Bounded(
        mpsc::Sender<QueuedMessage>,
        Arc<Semaphore>,
        CancellationToken,
    ),
}

impl From<mpsc::UnboundedSender<String>> for MessageSender {
    fn from(sender: mpsc::UnboundedSender<String>) -> Self {
        Self(SenderKind::Unbounded(sender))
    }
}

impl MessageSender {
    /// Queue a message, or close an overloaded socket and return the unsent text.
    pub fn send(&self, text: String) -> Result<(), mpsc::error::SendError<String>> {
        let (sender, budget, closed) = match &self.0 {
            SenderKind::Unbounded(sender) => return sender.send(text),
            SenderKind::Bounded(sender, budget, closed) => (sender, budget, closed),
        };
        if closed.is_cancelled() {
            return Err(mpsc::error::SendError(text));
        }
        let permit = if text.len() <= MAX_MESSAGE_BYTES {
            budget
                .clone()
                .try_acquire_many_owned(text.len().max(1) as u32)
                .ok()
        } else {
            None
        };
        let Some(permit) = permit else {
            tracing::warn!("WebSocket queue byte limit reached; disconnecting client for replay");
            closed.cancel();
            return Err(mpsc::error::SendError(text));
        };
        sender
            .try_send(QueuedMessage {
                text,
                _permit: permit,
            })
            .map_err(|error| {
                tracing::warn!("WebSocket queue full or closed; disconnecting client for replay");
                closed.cancel();
                mpsc::error::SendError(error.into_inner().text)
            })
    }
}

/// Incoming whole messages from a transport.
#[derive(Debug)]
pub struct MessageReceiver(ReceiverKind);

#[derive(Debug)]
enum ReceiverKind {
    Unbounded(mpsc::UnboundedReceiver<String>),
    Bounded(mpsc::Receiver<QueuedMessage>),
}

impl From<mpsc::UnboundedReceiver<String>> for MessageReceiver {
    fn from(receiver: mpsc::UnboundedReceiver<String>) -> Self {
        Self(ReceiverKind::Unbounded(receiver))
    }
}

impl MessageReceiver {
    /// Receive an already queued message without waiting.
    pub fn try_recv(&mut self) -> Result<String, mpsc::error::TryRecvError> {
        match &mut self.0 {
            ReceiverKind::Unbounded(receiver) => receiver.try_recv(),
            ReceiverKind::Bounded(receiver) => receiver.try_recv().map(|message| message.text),
        }
    }

    /// Receive the next message in order, or `None` after the peer closes.
    pub async fn recv(&mut self) -> Option<String> {
        match &mut self.0 {
            ReceiverKind::Unbounded(receiver) => receiver.recv().await,
            ReceiverKind::Bounded(receiver) => receiver.recv().await.map(|message| message.text),
        }
    }
}

pub(crate) fn bounded(closed: CancellationToken) -> (MessageSender, MessageReceiver) {
    let (sender, receiver) = mpsc::channel(SOCKET_QUEUE_MESSAGES);
    (
        MessageSender(SenderKind::Bounded(
            sender,
            Arc::new(Semaphore::new(MAX_MESSAGE_BYTES)),
            closed,
        )),
        MessageReceiver(ReceiverKind::Bounded(receiver)),
    )
}

/// One end of a connection: the messages that arrive, and a sender for the
/// messages to send.
#[derive(Debug)]
pub struct Transport {
    /// Messages from the peer, in order. Closes when the peer's side ends.
    pub incoming: MessageReceiver,
    /// Messages to the peer. Dropping it closes the output.
    pub outgoing: MessageSender,
}

/// Two connected ends in one process.
pub fn memory() -> (Transport, Transport) {
    let (a_tx, a_rx) = mpsc::unbounded_channel();
    let (b_tx, b_rx) = mpsc::unbounded_channel();
    (
        Transport {
            incoming: b_rx.into(),
            outgoing: a_tx.into(),
        },
        Transport {
            incoming: a_rx.into(),
            outgoing: b_tx.into(),
        },
    )
}

/// One message per line over a byte stream (stdio, a Unix socket). Blank
/// lines are skipped; lines over [`MAX_MESSAGE_BYTES`] are dropped.
pub fn lines<R, W>(reader: R, writer: W) -> Transport
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (incoming_tx, incoming) = mpsc::unbounded_channel();
    let (outgoing, mut to_write) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(mut message) = to_write.recv().await {
            message.push('\n');
            if writer.write_all(message.as_bytes()).await.is_err() || writer.flush().await.is_err()
            {
                break;
            }
        }
        // Closing the output tells the peer nothing more is coming.
        let _ = writer.shutdown().await;
    });
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        loop {
            line.clear();
            match read_line_capped(&mut reader, &mut line).await {
                Ok(0) => return,
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "connection read failed");
                    return;
                }
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            if !text.is_empty() && incoming_tx.send(text.to_owned()).is_err() {
                return;
            }
        }
    });
    Transport {
        incoming: incoming.into(),
        outgoing: outgoing.into(),
    }
}

/// Read one line, dropping lines over [`MAX_MESSAGE_BYTES`]. Returns the
/// bytes read including the newline; 0 at end of stream.
async fn read_line_capped<R>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
) -> std::io::Result<usize>
where
    R: AsyncRead + Unpin,
{
    let mut total = 0;
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        let (chunk, done) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (&available[..=i], true),
            None => (available, false),
        };
        let used = chunk.len();
        if !oversized && line.len() + used <= MAX_MESSAGE_BYTES {
            line.extend_from_slice(chunk);
        } else if !oversized {
            oversized = true;
            line.clear();
            tracing::warn!("dropping a message larger than {MAX_MESSAGE_BYTES} bytes");
        }
        reader.consume(used);
        total += used;
        if done {
            return Ok(total);
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn lines_split_and_join_messages_across_partial_writes() {
        let (ours, theirs) = duplex(64);
        let (read, write) = tokio::io::split(ours);
        let mut transport = lines(read, write);
        let (mut their_read, mut their_write) = tokio::io::split(theirs);

        let burst = "{\"a\":1}\n\n{\"b\":2}\n";
        let (first, second) = burst.split_at(5);
        their_write.write_all(first.as_bytes()).await.unwrap();
        their_write.write_all(second.as_bytes()).await.unwrap();
        assert_eq!(transport.incoming.recv().await.unwrap(), "{\"a\":1}");
        assert_eq!(transport.incoming.recv().await.unwrap(), "{\"b\":2}");

        transport.outgoing.send("{\"c\":3}".into()).unwrap();
        drop(transport.outgoing);
        let mut written = String::new();
        their_read.read_to_string(&mut written).await.unwrap();
        assert_eq!(
            written, "{\"c\":3}\n",
            "dropping the sender closes the output"
        );
    }

    #[tokio::test]
    async fn memory_ends_are_crossed() {
        let (mut a, mut b) = memory();
        a.outgoing.send("ping".into()).unwrap();
        assert_eq!(b.incoming.recv().await.unwrap(), "ping");
        b.outgoing.send("pong".into()).unwrap();
        assert_eq!(a.incoming.recv().await.unwrap(), "pong");
        drop(a);
        assert!(b.incoming.recv().await.is_none());
    }
}
