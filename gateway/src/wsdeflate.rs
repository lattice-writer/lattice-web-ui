//! RFC 7692 permessage-deflate on the browser's leg of a WebSocket tunnel.
//!
//! Lattice's browser host does not compress WebSocket messages (it expects a
//! browser on the same machine), yet everything the workspace does crosses its
//! bridge socket, whole PDFs included, base64-encoded inside JSON. When the
//! browser offers permessage-deflate, the gateway accepts it for itself, takes
//! the offer off the request it sends to Lattice, and transcodes frames in
//! between: it inflates what the browser compresses and hands Lattice plain
//! masked frames, and it deflates what Lattice sends before it goes back out.
//! The loopback leg to Lattice stays uncompressed.
//!
//! The relay streams: it never holds a whole message, only the frame headers
//! and control frames it has not seen the end of, plus up to one fragment of
//! output. A message larger than [`MAX_MESSAGE_BYTES`] (Lattice's own bridge
//! limit) closes the connection with 1009.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use hyper::header::{HeaderMap, HeaderValue, SEC_WEBSOCKET_EXTENSIONS};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// The largest message either side may send (Lattice's bridge accepts 256 MiB).
pub const MAX_MESSAGE_BYTES: u64 = 256 * 1024 * 1024;
/// Messages from Lattice smaller than this go out as they came: deflate's
/// framing would eat most of what it saves.
const MIN_COMPRESS_BYTES: u64 = 256;
/// The relay emits a fragment once this much transcoded output is waiting.
const FRAGMENT_BYTES: usize = 64 * 1024;
const READ_BYTES: usize = 64 * 1024;
/// The tail a sync flush leaves, which RFC 7692 drops from every message.
const SYNC_TAIL: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

pub const CLOSE_PROTOCOL: u16 = 1002;
pub const CLOSE_INVALID_DATA: u16 = 1007;
pub const CLOSE_TOO_BIG: u16 = 1009;
pub const CLOSE_INTERNAL: u16 = 1011;

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xa;

/// What the gateway agreed to with the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Agreed {
    /// The browser asked the gateway to compress each message on its own.
    pub server_no_context_takeover: bool,
    /// The browser asked for `server_max_window_bits=15`, which must be echoed.
    pub server_max_window_bits: bool,
}

impl Agreed {
    /// The `Sec-WebSocket-Extensions` value of the 101 sent to the browser.
    pub fn response(&self) -> HeaderValue {
        let mut value = String::from("permessage-deflate");
        if self.server_no_context_takeover {
            value.push_str("; server_no_context_takeover");
        }
        if self.server_max_window_bits {
            value.push_str("; server_max_window_bits=15");
        }
        HeaderValue::from_str(&value).expect("ASCII header value")
    }
}

/// A window size parameter's value: an integer 8 to 15 without leading zeros,
/// optionally quoted (RFC 7692 section 7.1.2).
fn window_bits(value: &str) -> Option<u8> {
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    if value.is_empty() || value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|bits| (8..=15).contains(bits))
}

/// One permessage-deflate offer's parameters, or None to decline the offer.
fn accept_offer<'a>(params: impl Iterator<Item = &'a str>) -> Option<Agreed> {
    let mut agreed = Agreed::default();
    let mut seen: Vec<String> = Vec::new();
    for param in params {
        let (name, value) = match param.split_once('=') {
            Some((name, value)) => (name.trim().to_ascii_lowercase(), Some(value.trim())),
            None => (param.trim().to_ascii_lowercase(), None),
        };
        if name.is_empty() {
            continue;
        }
        if seen.contains(&name) {
            return None;
        }
        seen.push(name.clone());
        match (name.as_str(), value) {
            ("server_no_context_takeover", None) => agreed.server_no_context_takeover = true,
            // A hint only: the gateway keeps the browser's window either way.
            ("client_no_context_takeover", None) => {}
            // The browser can limit its window; the gateway inflates any
            // window up to 15 bits, so it never asks it to.
            ("client_max_window_bits", None) => {}
            ("client_max_window_bits", Some(value)) => {
                window_bits(value)?;
            }
            // The deflate backend always uses a 32 KiB window, so the gateway
            // can only honour a limit of 15 bits and declines smaller ones.
            ("server_max_window_bits", Some(value)) => {
                if window_bits(value)? != 15 {
                    return None;
                }
                agreed.server_max_window_bits = true;
            }
            _ => return None,
        }
    }
    Some(agreed)
}

/// The first permessage-deflate offer in `headers` the gateway can accept.
pub fn negotiate(headers: &HeaderMap) -> Option<Agreed> {
    headers
        .get_all(SEC_WEBSOCKET_EXTENSIONS)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .find_map(|offer| {
            let mut parts = offer.split(';');
            let name = parts.next()?.trim();
            if !name.eq_ignore_ascii_case("permessage-deflate") {
                return None;
            }
            accept_offer(parts)
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    fin: bool,
    rsv1: bool,
    rsv23: bool,
    opcode: u8,
    mask: Option<[u8; 4]>,
    len: u64,
    /// Bytes of the header itself, mask key included.
    size: usize,
}

/// A frame header at the start of `buf`, or None until all of it has arrived.
fn parse_header(buf: &[u8]) -> Result<Option<Header>, u16> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let masked = buf[1] & 0x80 != 0;
    let (len, mut size) = match buf[1] & 0x7f {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as u64, 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let len = u64::from_be_bytes(buf[2..10].try_into().expect("eight bytes"));
            if len >> 63 != 0 {
                return Err(CLOSE_PROTOCOL);
            }
            (len, 10)
        }
        len => (len as u64, 2),
    };
    let mask = if masked {
        if buf.len() < size + 4 {
            return Ok(None);
        }
        let key = buf[size..size + 4].try_into().expect("four bytes");
        size += 4;
        Some(key)
    } else {
        None
    };
    Ok(Some(Header {
        fin: buf[0] & 0x80 != 0,
        rsv1: buf[0] & 0x40 != 0,
        rsv23: buf[0] & 0x30 != 0,
        opcode: buf[0] & 0x0f,
        mask,
        len,
        size,
    }))
}

fn write_header(
    out: &mut Vec<u8>,
    fin: bool,
    rsv1: bool,
    opcode: u8,
    mask: Option<[u8; 4]>,
    len: u64,
) {
    out.push(u8::from(fin) << 7 | u8::from(rsv1) << 6 | opcode);
    let bit = if mask.is_some() { 0x80 } else { 0 };
    if len < 126 {
        out.push(bit | len as u8);
    } else if len <= u16::MAX as u64 {
        out.push(bit | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(bit | 127);
        out.extend_from_slice(&len.to_be_bytes());
    }
    if let Some(key) = mask {
        out.extend_from_slice(&key);
    }
}

/// XOR `data` with `key`, starting `offset` bytes into the frame's payload.
fn apply_mask(data: &mut [u8], key: [u8; 4], offset: u64) {
    let start = (offset % 4) as usize;
    for (i, byte) in data.iter_mut().enumerate() {
        *byte ^= key[(start + i) % 4];
    }
}

fn fresh_mask() -> [u8; 4] {
    let mut key = [0u8; 4];
    // A predictable key only weakens the cache-poisoning defence masking exists
    // for, which a loopback hop to Lattice does not need; never fail over it.
    let _ = getrandom::getrandom(&mut key);
    key
}

/// A whole frame: masked when the gateway sends it to Lattice.
fn frame(fin: bool, rsv1: bool, opcode: u8, masked: bool, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    let mask = masked.then(fresh_mask);
    write_header(&mut out, fin, rsv1, opcode, mask, payload.len() as u64);
    let start = out.len();
    out.extend_from_slice(payload);
    if let Some(key) = mask {
        apply_mask(&mut out[start..], key, 0);
    }
    out
}

/// A close frame with `code` and no reason.
pub fn close_frame(code: u16, masked: bool) -> Vec<u8> {
    frame(true, false, OP_CLOSE, masked, &code.to_be_bytes())
}

/// Who sent the frames a [`Relay`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The browser: masked frames, compressed messages carry RSV1.
    Client,
    /// Lattice: unmasked, uncompressed frames.
    Server,
}

enum Codec {
    Inflate(Decompress),
    Deflate(Compress),
}

struct Frame {
    fin: bool,
    mask: Option<[u8; 4]>,
    remaining: u64,
    /// Payload bytes already relayed, for the mask's phase.
    offset: u64,
}

struct Message {
    transcode: bool,
    opcode: u8,
    /// A fragment of the transcoded message has gone out.
    started: bool,
    /// Payload bytes so far: inflated ones for a message from the browser.
    size: u64,
    /// The message ended its deflate stream (BFINAL); the rest is ignored.
    ended: bool,
    out: Vec<u8>,
}

/// One direction of the tunnel: reads the frames `from` sends and writes what
/// the other side should get.
pub struct Relay {
    from: Side,
    codec: Codec,
    no_context_takeover: bool,
    max_message: u64,
    pending: Vec<u8>,
    frame: Option<Frame>,
    message: Option<Message>,
    scratch: Vec<u8>,
}

impl Relay {
    pub fn new(from: Side, agreed: Agreed) -> Self {
        Self::with_limit(from, agreed, MAX_MESSAGE_BYTES)
    }

    pub fn with_limit(from: Side, agreed: Agreed, max_message: u64) -> Self {
        let codec = match from {
            Side::Client => Codec::Inflate(Decompress::new(false)),
            Side::Server => Codec::Deflate(Compress::new(Compression::default(), false)),
        };
        Self {
            from,
            codec,
            no_context_takeover: from == Side::Server && agreed.server_no_context_takeover,
            max_message,
            pending: Vec::new(),
            frame: None,
            message: None,
            scratch: Vec::new(),
        }
    }

    /// Relay `input`, appending what the other side should receive to `out`.
    /// An error is the close code to fail the connection with.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), u16> {
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend_from_slice(input);
        let used = self.run(&pending, out)?;
        pending.drain(..used);
        self.pending = pending;
        Ok(())
    }

    fn run(&mut self, buf: &[u8], out: &mut Vec<u8>) -> Result<usize, u16> {
        let mut pos = 0;
        loop {
            if let Some(frame) = self.frame.as_mut() {
                let take = frame.remaining.min((buf.len() - pos) as u64) as usize;
                if take == 0 && frame.remaining > 0 {
                    return Ok(pos);
                }
                let chunk = &buf[pos..pos + take];
                pos += take;
                let (mask, offset) = (frame.mask, frame.offset);
                frame.remaining -= take as u64;
                frame.offset += take as u64;
                let frame_done = frame.remaining == 0;
                let message_done = frame_done && frame.fin;
                if frame_done {
                    self.frame = None;
                }
                self.payload(chunk, mask, offset, message_done, out)?;
                continue;
            }
            let Some(header) = parse_header(&buf[pos..])? else {
                return Ok(pos);
            };
            self.check(&header)?;
            if header.opcode & 0x8 != 0 {
                let end = header.size + header.len as usize;
                if buf.len() - pos < end {
                    return Ok(pos);
                }
                out.extend_from_slice(&buf[pos..pos + end]);
                pos += end;
                continue;
            }
            if header.opcode != OP_CONTINUATION {
                let transcode = match self.from {
                    Side::Client => header.rsv1,
                    Side::Server => !(header.fin && header.len < MIN_COMPRESS_BYTES),
                };
                self.message = Some(Message {
                    transcode,
                    opcode: header.opcode,
                    started: false,
                    size: 0,
                    ended: false,
                    out: Vec::new(),
                });
            }
            let message = self.message.as_mut().expect("checked above");
            // A transcoded browser message counts what it inflates to.
            if !(message.transcode && self.from == Side::Client) {
                message.size += header.len;
                if message.size > self.max_message {
                    return Err(CLOSE_TOO_BIG);
                }
            }
            if !message.transcode {
                out.extend_from_slice(&buf[pos..pos + header.size]);
            }
            pos += header.size;
            self.frame = Some(Frame {
                fin: header.fin,
                mask: header.mask,
                remaining: header.len,
                offset: 0,
            });
        }
    }

    fn check(&self, header: &Header) -> Result<(), u16> {
        let masked_ok = match self.from {
            Side::Client => header.mask.is_some(),
            Side::Server => header.mask.is_none(),
        };
        if !masked_ok || header.rsv23 {
            return Err(CLOSE_PROTOCOL);
        }
        match header.opcode {
            OP_CLOSE | OP_PING | OP_PONG => {
                if !header.fin || header.len > 125 || header.rsv1 {
                    return Err(CLOSE_PROTOCOL);
                }
            }
            OP_TEXT | OP_BINARY => {
                if self.message.is_some() || (header.rsv1 && self.from == Side::Server) {
                    return Err(CLOSE_PROTOCOL);
                }
            }
            OP_CONTINUATION => {
                if self.message.is_none() || header.rsv1 {
                    return Err(CLOSE_PROTOCOL);
                }
            }
            _ => return Err(CLOSE_PROTOCOL),
        }
        Ok(())
    }

    fn payload(
        &mut self,
        chunk: &[u8],
        mask: Option<[u8; 4]>,
        offset: u64,
        message_done: bool,
        out: &mut Vec<u8>,
    ) -> Result<(), u16> {
        let transcode = self.message.as_ref().expect("in a message").transcode;
        if !transcode {
            out.extend_from_slice(chunk);
        } else {
            match self.from {
                Side::Client => {
                    let mut data = std::mem::take(&mut self.scratch);
                    data.clear();
                    data.extend_from_slice(chunk);
                    if let Some(key) = mask {
                        apply_mask(&mut data, key, offset);
                    }
                    let result = self.inflate(&data, out);
                    self.scratch = data;
                    result?;
                    if message_done {
                        self.inflate(&SYNC_TAIL, out)?;
                    }
                }
                Side::Server => {
                    self.deflate(chunk, FlushCompress::None, out)?;
                    if message_done {
                        self.deflate(&[], FlushCompress::Sync, out)?;
                        let message = self.message.as_mut().expect("in a message");
                        if message.out.ends_with(&SYNC_TAIL) {
                            message.out.truncate(message.out.len() - SYNC_TAIL.len());
                        }
                    }
                }
            }
            if message_done {
                self.emit(true, out);
                if let Codec::Inflate(inflate) = &mut self.codec {
                    if self.message.as_ref().is_some_and(|m| m.ended) {
                        inflate.reset(false);
                    }
                }
                if self.no_context_takeover {
                    if let Codec::Deflate(deflate) = &mut self.codec {
                        deflate.reset();
                    }
                }
            }
        }
        if message_done {
            self.message = None;
        }
        Ok(())
    }

    fn inflate(&mut self, mut input: &[u8], out: &mut Vec<u8>) -> Result<(), u16> {
        let Codec::Inflate(inflate) = &mut self.codec else {
            unreachable!("a browser relay inflates")
        };
        let message = self.message.as_mut().expect("in a message");
        // Until the input is used up and the inflater has room to spare, which
        // means it holds no more output.
        while !message.ended {
            message.out.reserve(FRAGMENT_BYTES);
            let (before_in, before_out) = (inflate.total_in(), inflate.total_out());
            let status = inflate
                .decompress_vec(input, &mut message.out, FlushDecompress::None)
                .map_err(|_| CLOSE_INVALID_DATA)?;
            let used = (inflate.total_in() - before_in) as usize;
            let made = inflate.total_out() - before_out;
            input = &input[used..];
            message.size += made;
            if message.size > self.max_message {
                return Err(CLOSE_TOO_BIG);
            }
            let full = message.out.len() == message.out.capacity();
            message.ended = status == Status::StreamEnd;
            if message.out.len() >= FRAGMENT_BYTES {
                Self::emit_message(self.from, message, false, out);
            }
            if !full && (input.is_empty() || (used == 0 && made == 0)) {
                break;
            }
        }
        Ok(())
    }

    fn deflate(
        &mut self,
        mut input: &[u8],
        flush: FlushCompress,
        out: &mut Vec<u8>,
    ) -> Result<(), u16> {
        let Codec::Deflate(deflate) = &mut self.codec else {
            unreachable!("a Lattice relay deflates")
        };
        let message = self.message.as_mut().expect("in a message");
        let finishing = flush != FlushCompress::None;
        loop {
            message.out.reserve(FRAGMENT_BYTES);
            let before_in = deflate.total_in();
            deflate
                .compress_vec(input, &mut message.out, flush)
                .map_err(|_| CLOSE_INTERNAL)?;
            input = &input[(deflate.total_in() - before_in) as usize..];
            let full = message.out.len() == message.out.capacity();
            // While finishing, keep the output whole so the sync tail can be cut.
            if !finishing && message.out.len() >= FRAGMENT_BYTES {
                Self::emit_message(self.from, message, false, out);
            }
            if input.is_empty() && !full {
                return Ok(());
            }
        }
    }

    fn emit(&mut self, fin: bool, out: &mut Vec<u8>) {
        let message = self.message.as_mut().expect("in a message");
        Self::emit_message(self.from, message, fin, out);
    }

    /// Send `message`'s waiting output as one fragment.
    fn emit_message(from: Side, message: &mut Message, fin: bool, out: &mut Vec<u8>) {
        let opcode = if message.started {
            OP_CONTINUATION
        } else {
            message.opcode
        };
        // Toward Lattice: plain and masked. Toward the browser: compressed.
        let (rsv1, masked) = match from {
            Side::Client => (false, true),
            Side::Server => (!message.started, false),
        };
        out.extend_from_slice(&frame(fin, rsv1, opcode, masked, &message.out));
        message.out.clear();
        message.started = true;
    }
}

/// Relay frames in one direction until `src` ends, or a frame the other
/// direction injects (its close after an error) has been written.
async fn pump<R, W>(
    src: &mut R,
    dst: &mut W,
    mut relay: Relay,
    mut inject: mpsc::Receiver<Vec<u8>>,
    other: mpsc::Sender<Vec<u8>>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; READ_BYTES];
    let mut out = Vec::new();
    let mut inject_open = true;
    loop {
        tokio::select! {
            read = src.read(&mut buf) => {
                let n = match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                out.clear();
                let result = relay.feed(&buf[..n], &mut out);
                if !out.is_empty() && dst.write_all(&out).await.is_err() {
                    break;
                }
                if let Err(code) = result {
                    // Frames toward Lattice are masked, frames toward the browser are not.
                    let _ = dst.write_all(&close_frame(code, relay.from == Side::Client)).await;
                    let _ = other.send(close_frame(code, relay.from == Side::Server)).await;
                    break;
                }
            }
            injected = inject.recv(), if inject_open => match injected {
                Some(frame) => {
                    let _ = dst.write_all(&frame).await;
                    break;
                }
                None => inject_open = false,
            },
        }
    }
    let _ = dst.shutdown().await;
}

/// Tunnel a WebSocket whose browser leg negotiated `agreed`, with `server`
/// speaking plain WebSocket to Lattice.
pub async fn relay<C, S>(client: C, server: S, agreed: Agreed)
where
    C: AsyncRead + AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut server_read, mut server_write) = tokio::io::split(server);
    let (to_client, to_client_rx) = mpsc::channel(1);
    let (to_server, to_server_rx) = mpsc::channel(1);
    tokio::join!(
        pump(
            &mut client_read,
            &mut server_write,
            Relay::new(Side::Client, agreed),
            to_server_rx,
            to_client,
        ),
        pump(
            &mut server_read,
            &mut client_write,
            Relay::new(Side::Server, agreed),
            to_client_rx,
            to_server,
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(value: &str) -> Option<Agreed> {
        let mut headers = HeaderMap::new();
        headers.insert(
            SEC_WEBSOCKET_EXTENSIONS,
            HeaderValue::from_str(value).unwrap(),
        );
        negotiate(&headers)
    }

    #[test]
    fn accepts_what_browsers_offer() {
        // Chrome, Firefox and Safari all send this.
        let agreed = offer("permessage-deflate; client_max_window_bits").unwrap();
        assert_eq!(agreed, Agreed::default());
        assert_eq!(agreed.response(), "permessage-deflate");
        let agreed =
            offer("permessage-deflate; server_no_context_takeover; client_max_window_bits=10")
                .unwrap();
        assert_eq!(
            agreed.response(),
            "permessage-deflate; server_no_context_takeover"
        );
        let agreed = offer("PerMessage-Deflate; server_max_window_bits=\"15\"").unwrap();
        assert_eq!(
            agreed.response(),
            "permessage-deflate; server_max_window_bits=15"
        );
        assert!(offer("permessage-deflate; client_no_context_takeover").is_some());
    }

    #[test]
    fn declines_parameters_it_cannot_honour() {
        // A smaller server window: decline that offer, take the fallback.
        assert_eq!(offer("permessage-deflate; server_max_window_bits=10"), None);
        let agreed = offer("permessage-deflate; server_max_window_bits=10, permessage-deflate; server_no_context_takeover").unwrap();
        assert!(agreed.server_no_context_takeover);
        for bad in [
            "permessage-deflate; client_max_window_bits=7",
            "permessage-deflate; client_max_window_bits=016",
            "permessage-deflate; client_max_window_bits=abc",
            "permessage-deflate; server_max_window_bits",
            "permessage-deflate; server_no_context_takeover=1",
            "permessage-deflate; server_no_context_takeover; server_no_context_takeover",
            "permessage-deflate; mystery",
            "x-webkit-deflate-frame",
            "",
        ] {
            assert_eq!(offer(bad), None, "{bad}");
        }
        assert!(negotiate(&HeaderMap::new()).is_none());
    }

    #[test]
    fn header_lengths_round_trip() {
        for len in [0u64, 1, 125, 126, 65535, 65536, 1 << 32] {
            for mask in [None, Some([1, 2, 3, 4])] {
                let mut out = Vec::new();
                write_header(&mut out, true, true, OP_BINARY, mask, len);
                let header = parse_header(&out).unwrap().unwrap();
                assert_eq!(
                    (header.len, header.mask, header.size),
                    (len, mask, out.len())
                );
                assert!(header.fin && header.rsv1 && !header.rsv23);
                // Short reads wait for the rest of the header.
                assert_eq!(parse_header(&out[..out.len() - 1]).unwrap(), None);
            }
        }
        let size = |len| {
            let mut out = Vec::new();
            write_header(&mut out, true, false, OP_TEXT, None, len);
            out.len()
        };
        assert_eq!(
            (size(125), size(126), size(65535), size(65536)),
            (2, 4, 4, 10)
        );
        let mut huge = vec![0x82, 127];
        huge.extend_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(parse_header(&huge), Err(CLOSE_PROTOCOL));
    }

    /// A browser's frame: masked with `key`.
    fn client_frame(fin: bool, rsv1: bool, opcode: u8, key: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_header(&mut out, fin, rsv1, opcode, Some(key), payload.len() as u64);
        let start = out.len();
        out.extend_from_slice(payload);
        apply_mask(&mut out[start..], key, 0);
        out
    }

    /// Frames as (fin, rsv1, opcode, unmasked payload), checking each frame's
    /// mask is present exactly when `masked`.
    fn frames(mut bytes: &[u8], masked: bool) -> Vec<(bool, bool, u8, Vec<u8>)> {
        let mut found = Vec::new();
        while !bytes.is_empty() {
            let header = parse_header(bytes).unwrap().expect("a whole frame");
            assert_eq!(header.mask.is_some(), masked, "mask bit");
            let end = header.size + header.len as usize;
            let mut payload = bytes[header.size..end].to_vec();
            if let Some(key) = header.mask {
                apply_mask(&mut payload, key, 0);
            }
            found.push((header.fin, header.rsv1, header.opcode, payload));
            bytes = &bytes[end..];
        }
        found
    }

    /// Whole messages as (opcode, payload), with control frames listed in place.
    fn messages(bytes: &[u8], masked: bool) -> Vec<(u8, Vec<u8>)> {
        let mut found = Vec::new();
        let mut open: Option<(u8, Vec<u8>)> = None;
        for (fin, rsv1, opcode, payload) in frames(bytes, masked) {
            assert!(!rsv1, "plain frames toward Lattice");
            if opcode & 0x8 != 0 {
                found.push((opcode, payload));
                continue;
            }
            let entry = open.get_or_insert((opcode, Vec::new()));
            entry.1.extend_from_slice(&payload);
            if fin {
                found.push(open.take().unwrap());
            }
        }
        assert!(open.is_none(), "every message ends");
        found
    }

    /// What a browser does with frames from the gateway: inflate RSV1 messages.
    struct Browser(Decompress);

    impl Browser {
        fn new() -> Self {
            Browser(Decompress::new(false))
        }

        fn read(&mut self, bytes: &[u8]) -> Vec<(u8, bool, Vec<u8>)> {
            let mut found = Vec::new();
            let mut open: Option<(u8, bool, Vec<u8>)> = None;
            for (fin, rsv1, opcode, payload) in frames(bytes, false) {
                if opcode & 0x8 != 0 {
                    found.push((opcode, false, payload));
                    continue;
                }
                let entry = open.get_or_insert((opcode, rsv1, Vec::new()));
                entry.2.extend_from_slice(&payload);
                if fin {
                    let (opcode, compressed, mut data) = open.take().unwrap();
                    if compressed {
                        data.extend_from_slice(&SYNC_TAIL);
                        let mut plain = Vec::with_capacity(data.len() * 20 + 1024);
                        loop {
                            plain.reserve(1 << 20);
                            let before = self.0.total_in();
                            self.0
                                .decompress_vec(&data, &mut plain, FlushDecompress::Sync)
                                .unwrap();
                            data.drain(..(self.0.total_in() - before) as usize);
                            if data.is_empty() && plain.len() < plain.capacity() {
                                break;
                            }
                        }
                        found.push((opcode, true, plain));
                    } else {
                        found.push((opcode, false, data));
                    }
                }
            }
            found
        }
    }

    /// What a browser sends: each message deflated with a sync flush, tail cut.
    struct Sender(Compress);

    impl Sender {
        fn new() -> Self {
            Sender(Compress::new(Compression::default(), false))
        }

        fn compress(&mut self, data: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(data.len() + 1024);
            let mut input = data;
            loop {
                out.reserve(1 << 16);
                let before = self.0.total_in();
                self.0
                    .compress_vec(input, &mut out, FlushCompress::Sync)
                    .unwrap();
                input = &input[(self.0.total_in() - before) as usize..];
                if input.is_empty() && out.len() < out.capacity() {
                    break;
                }
            }
            assert!(out.ends_with(&SYNC_TAIL));
            out.truncate(out.len() - 4);
            out
        }
    }

    fn feed_all(relay: &mut Relay, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        relay.feed(bytes, &mut out).unwrap();
        out
    }

    /// The same input, a byte at a time.
    fn feed_bytewise(relay: &mut Relay, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for byte in bytes {
            relay.feed(std::slice::from_ref(byte), &mut out).unwrap();
        }
        out
    }

    const KEY: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

    #[test]
    fn inflates_rfc_7692_hello_examples() {
        let mut relay = Relay::new(Side::Client, Agreed::default());
        // 7.2.3.1: "Hello" in one compressed block, then 7.2.3.2: the same
        // message again, using the first one's sliding window.
        let mut input = client_frame(
            true,
            true,
            OP_TEXT,
            KEY,
            &[0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00],
        );
        input.extend(client_frame(
            true,
            true,
            OP_TEXT,
            KEY,
            &[0xf2, 0x00, 0x11, 0x00, 0x00],
        ));
        // 7.2.3.3: a block with no compression.
        input.extend(client_frame(
            true,
            true,
            OP_TEXT,
            KEY,
            &[
                0x00, 0x05, 0x00, 0xfa, 0xff, 0x48, 0x65, 0x6c, 0x6c, 0x6f, 0x00,
            ],
        ));
        // 7.2.3.1 again, fragmented: RSV1 only on the first frame.
        input.extend(client_frame(false, true, OP_TEXT, KEY, &[0xf2, 0x48, 0xcd]));
        input.extend(client_frame(
            true,
            false,
            OP_CONTINUATION,
            KEY,
            &[0xc9, 0xc9, 0x07, 0x00],
        ));
        let out = feed_all(&mut relay, &input);
        let hello = (OP_TEXT, b"Hello".to_vec());
        assert_eq!(
            messages(&out, true),
            vec![hello.clone(), hello.clone(), hello.clone(), hello.clone()]
        );
        // Byte by byte, from a fresh window, gives the same messages.
        let mut relay = Relay::new(Side::Client, Agreed::default());
        assert_eq!(messages(&feed_bytewise(&mut relay, &input), true).len(), 4);
    }

    #[test]
    fn inflates_a_message_that_ends_its_deflate_stream() {
        // 7.2.3.4: a final block (BFINAL set); the next message starts afresh.
        let mut relay = Relay::new(Side::Client, Agreed::default());
        let mut input = client_frame(
            true,
            true,
            OP_TEXT,
            KEY,
            &[0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x00],
        );
        input.extend(client_frame(
            true,
            true,
            OP_TEXT,
            KEY,
            &[0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00],
        ));
        let hello = (OP_TEXT, b"Hello".to_vec());
        assert_eq!(
            messages(&feed_all(&mut relay, &input), true),
            vec![hello.clone(), hello]
        );
    }

    fn sample(len: usize) -> Vec<u8> {
        let mut text = Vec::with_capacity(len);
        let mut i = 0u32;
        while text.len() < len {
            text.extend_from_slice(
                format!(
                    "{{\"id\":{i},\"method\":\"read_file\",\"path\":\"chapters/{}.tex\"}}",
                    i % 37
                )
                .as_bytes(),
            );
            i += 1;
        }
        text.truncate(len);
        text
    }

    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    #[test]
    fn browser_messages_reach_lattice_plain_and_masked() {
        let mut browser = Sender::new();
        let mut relay = Relay::new(Side::Client, Agreed::default());
        let bodies = [
            sample(40),
            sample(300),
            sample(70_000),
            noise(200_000),
            sample(3 * 1024 * 1024),
        ];
        let mut input = Vec::new();
        for body in &bodies {
            input.extend(client_frame(
                true,
                true,
                OP_BINARY,
                KEY,
                &browser.compress(body),
            ));
        }
        let out = feed_all(&mut relay, &input);
        let got = messages(&out, true);
        assert_eq!(got.len(), bodies.len());
        for (got, body) in got.iter().zip(&bodies) {
            assert_eq!((got.0, got.1.len()), (OP_BINARY, body.len()));
            assert!(got.1 == *body);
        }
        // Large messages arrive as fragments, so the relay holds one at a time.
        assert!(frames(&out, true)
            .iter()
            .all(|f| f.3.len() <= 2 * FRAGMENT_BYTES));
    }

    #[test]
    fn uncompressed_browser_frames_pass_unchanged() {
        let mut relay = Relay::new(Side::Client, Agreed::default());
        let mut input = client_frame(true, false, OP_TEXT, KEY, &sample(300));
        input.extend(client_frame(true, false, OP_BINARY, KEY, &noise(70_000)));
        input.extend(client_frame(true, false, OP_PING, KEY, b"hi"));
        assert_eq!(feed_bytewise(&mut relay, &input), input);
    }

    #[test]
    fn lattice_messages_reach_the_browser_compressed() {
        let bodies = [
            sample(40),
            sample(300),
            sample(70_000),
            noise(200_000),
            sample(3 * 1024 * 1024),
            Vec::new(),
        ];
        for agreed in [
            Agreed::default(),
            Agreed {
                server_no_context_takeover: true,
                ..Agreed::default()
            },
        ] {
            let mut relay = Relay::new(Side::Server, agreed);
            let mut input = Vec::new();
            for body in &bodies {
                input.extend(frame(true, false, OP_TEXT, false, body));
            }
            let out = feed_all(&mut relay, &input);
            let got = if agreed.server_no_context_takeover {
                // Each message inflates on its own.
                let mut got = Vec::new();
                let mut rest: &[u8] = &out;
                while !rest.is_empty() {
                    let mut end = 0;
                    loop {
                        let header = parse_header(&rest[end..]).unwrap().unwrap();
                        end += header.size + header.len as usize;
                        if header.fin {
                            break;
                        }
                    }
                    got.extend(Browser::new().read(&rest[..end]));
                    rest = &rest[end..];
                }
                got
            } else {
                Browser::new().read(&out)
            };
            assert_eq!(got.len(), bodies.len());
            for ((opcode, compressed, data), body) in got.iter().zip(&bodies) {
                assert_eq!(*opcode, OP_TEXT);
                assert_eq!(
                    *compressed,
                    body.len() as u64 >= MIN_COMPRESS_BYTES,
                    "len {}",
                    body.len()
                );
                assert!(data == body);
            }
            assert!(
                out.len() < input.len() / 5,
                "{} of {}",
                out.len(),
                input.len()
            );
        }
    }

    #[test]
    fn small_lattice_messages_and_control_frames_pass_unchanged() {
        let mut relay = Relay::new(Side::Server, Agreed::default());
        let mut input = frame(true, false, OP_TEXT, false, &sample(255));
        input.extend(frame(true, false, OP_PING, false, b"are you there"));
        input.extend(frame(true, false, OP_PONG, false, b""));
        input.extend(close_frame(1000, false));
        assert_eq!(feed_bytewise(&mut relay, &input), input);
    }

    #[test]
    fn control_frames_interleave_with_fragments() {
        // From the browser: a compressed message in three fragments, a ping
        // and a pong between them.
        let body = sample(150_000);
        let compressed = Sender::new().compress(&body);
        let (a, rest) = compressed.split_at(compressed.len() / 3);
        let (b, c) = rest.split_at(rest.len() / 2);
        let mut input = client_frame(false, true, OP_TEXT, KEY, a);
        input.extend(client_frame(true, false, OP_PING, KEY, b"1"));
        input.extend(client_frame(false, false, OP_CONTINUATION, [9, 8, 7, 6], b));
        input.extend(client_frame(true, false, OP_PONG, KEY, b"2"));
        input.extend(client_frame(true, false, OP_CONTINUATION, [1, 1, 1, 1], c));
        for bytewise in [false, true] {
            let mut relay = Relay::new(Side::Client, Agreed::default());
            let out = if bytewise {
                feed_bytewise(&mut relay, &input)
            } else {
                feed_all(&mut relay, &input)
            };
            let got = messages(&out, true);
            assert_eq!(
                got.iter().map(|m| m.0).collect::<Vec<_>>(),
                vec![OP_PING, OP_PONG, OP_TEXT]
            );
            assert_eq!(
                (got[0].1.as_slice(), got[1].1.as_slice()),
                (&b"1"[..], &b"2"[..])
            );
            assert!(got[2].1 == body);
        }

        // From Lattice: a fragmented message with a ping between its parts.
        let mut input = frame(false, false, OP_BINARY, false, &body[..1000]);
        input.extend(frame(true, false, OP_PING, false, b"p"));
        input.extend(frame(true, false, OP_CONTINUATION, false, &body[1000..]));
        let mut relay = Relay::new(Side::Server, Agreed::default());
        let got = Browser::new().read(&feed_bytewise(&mut relay, &input));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (OP_PING, false, b"p".to_vec()));
        assert_eq!((got[1].0, got[1].1), (OP_BINARY, true));
        assert!(got[1].2 == body);
    }

    #[test]
    fn refuses_malformed_frames() {
        let fail = |from: Side, bytes: Vec<u8>| {
            Relay::new(from, Agreed::default())
                .feed(&bytes, &mut Vec::new())
                .unwrap_err()
        };
        // The browser must mask; Lattice must not, nor compress.
        assert_eq!(
            fail(Side::Client, frame(true, false, OP_TEXT, false, b"x")),
            CLOSE_PROTOCOL
        );
        assert_eq!(
            fail(Side::Server, frame(true, false, OP_TEXT, true, b"x")),
            CLOSE_PROTOCOL
        );
        assert_eq!(
            fail(Side::Server, frame(true, true, OP_TEXT, false, b"x")),
            CLOSE_PROTOCOL
        );
        // RSV1 on a continuation or a control frame; RSV2; a stray continuation.
        let mut bytes = client_frame(false, true, OP_TEXT, KEY, &[0xf2]);
        bytes.extend(client_frame(true, true, OP_CONTINUATION, KEY, &[0x48]));
        assert_eq!(fail(Side::Client, bytes), CLOSE_PROTOCOL);
        assert_eq!(
            fail(Side::Client, client_frame(true, true, OP_PING, KEY, b"")),
            CLOSE_PROTOCOL
        );
        let mut rsv2 = client_frame(true, false, OP_TEXT, KEY, b"x");
        rsv2[0] |= 0x20;
        assert_eq!(fail(Side::Client, rsv2), CLOSE_PROTOCOL);
        assert_eq!(
            fail(
                Side::Client,
                client_frame(true, false, OP_CONTINUATION, KEY, b"x")
            ),
            CLOSE_PROTOCOL
        );
        // A new message before the last one ended; a fragmented ping; opcode 3.
        let mut bytes = client_frame(false, false, OP_TEXT, KEY, b"x");
        bytes.extend(client_frame(true, false, OP_TEXT, KEY, b"y"));
        assert_eq!(fail(Side::Client, bytes), CLOSE_PROTOCOL);
        assert_eq!(
            fail(Side::Client, client_frame(false, false, OP_PING, KEY, b"")),
            CLOSE_PROTOCOL
        );
        assert_eq!(
            fail(Side::Client, client_frame(true, false, 3, KEY, b"")),
            CLOSE_PROTOCOL
        );
        // Compressed data that is not deflate.
        assert_eq!(
            fail(
                Side::Client,
                client_frame(true, true, OP_TEXT, KEY, &[0xff; 16])
            ),
            CLOSE_INVALID_DATA
        );
    }

    #[test]
    fn oversize_messages_close_with_1009() {
        let limit = 64 * 1024;
        let fail = |from: Side, bytes: &[u8]| {
            let mut relay = Relay::with_limit(from, Agreed::default(), limit);
            let mut out = Vec::new();
            let result = relay.feed(bytes, &mut out);
            (result, out.len())
        };
        // A header announcing too much is refused before its payload arrives.
        let mut header = Vec::new();
        write_header(&mut header, true, false, OP_BINARY, Some(KEY), limit + 1);
        assert_eq!(fail(Side::Client, &header).0, Err(CLOSE_TOO_BIG));
        let big = frame(true, false, OP_BINARY, false, &noise(limit as usize + 1));
        assert_eq!(fail(Side::Server, &big).0, Err(CLOSE_TOO_BIG));
        // Fragments add up.
        let mut parts = frame(
            false,
            false,
            OP_BINARY,
            false,
            &noise(limit as usize / 2 + 1),
        );
        parts.extend(frame(
            true,
            false,
            OP_CONTINUATION,
            false,
            &noise(limit as usize / 2 + 1),
        ));
        assert_eq!(fail(Side::Server, &parts).0, Err(CLOSE_TOO_BIG));
        // A small compressed frame that inflates past the limit: refused
        // without inflating all 64 MiB of it.
        let bomb = Sender::new().compress(&vec![0u8; 64 * 1024 * 1024]);
        assert!(bomb.len() < 128 * 1024);
        let (result, out) = fail(
            Side::Client,
            &client_frame(true, true, OP_BINARY, KEY, &bomb),
        );
        assert_eq!(result, Err(CLOSE_TOO_BIG));
        assert!(out <= limit as usize + 2 * FRAGMENT_BYTES);
        // Exactly the limit passes.
        let fits = Sender::new().compress(&vec![7u8; limit as usize]);
        let mut relay = Relay::with_limit(Side::Client, Agreed::default(), limit);
        let out = feed_all(&mut relay, &client_frame(true, true, OP_BINARY, KEY, &fits));
        assert_eq!(messages(&out, true)[0].1.len(), limit as usize);
    }

    /// True once `bytes` holds whole frames ending a data message.
    fn whole_message(mut bytes: &[u8]) -> bool {
        let mut ended = false;
        while !bytes.is_empty() {
            let Ok(Some(header)) = parse_header(bytes) else {
                return false;
            };
            let end = header.size + header.len as usize;
            if bytes.len() < end {
                return false;
            }
            ended = header.fin && header.opcode & 0x8 == 0;
            bytes = &bytes[end..];
        }
        ended
    }

    #[tokio::test]
    async fn relays_both_ways_and_fails_a_bad_frame() {
        let (browser, client_end) = tokio::io::duplex(1 << 16);
        let (server_end, lattice) = tokio::io::duplex(1 << 16);
        let task = tokio::spawn(relay(client_end, server_end, Agreed::default()));
        let (mut browser_read, mut browser_write) = tokio::io::split(browser);
        let (mut lattice_read, mut lattice_write) = tokio::io::split(lattice);

        let body = sample(500_000);
        let mut sender = Sender::new();
        let message = client_frame(true, true, OP_TEXT, KEY, &sender.compress(&body));
        let writer = tokio::spawn(async move {
            browser_write.write_all(&message).await.unwrap();
            browser_write
        });
        let mut seen = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        while !whole_message(&seen) {
            let n = lattice_read.read(&mut buf).await.unwrap();
            seen.extend_from_slice(&buf[..n]);
        }
        let got = messages(&seen, true);
        assert_eq!(got.len(), 1);
        assert!(got[0].1 == body);
        let mut browser_write = writer.await.unwrap();

        let reply = frame(true, false, OP_TEXT, false, &body);
        let writer = tokio::spawn(async move {
            lattice_write.write_all(&reply).await.unwrap();
            lattice_write
        });
        let mut seen = Vec::new();
        while !whole_message(&seen) {
            let n = browser_read.read(&mut buf).await.unwrap();
            seen.extend_from_slice(&buf[..n]);
        }
        let lattice_write = writer.await.unwrap();
        let got = Browser::new().read(&seen);
        assert_eq!(got.len(), 1);
        assert!(got[0].1 && got[0].2 == body);
        assert!(seen.len() < body.len() / 10);

        // An unmasked frame from the browser fails the connection: both
        // sides get a close with 1002, then the tunnel ends.
        browser_write
            .write_all(&frame(true, false, OP_TEXT, false, b"x"))
            .await
            .unwrap();
        let mut tail = Vec::new();
        browser_read.read_to_end(&mut tail).await.unwrap();
        assert_eq!(
            frames(&tail, false),
            vec![(true, false, OP_CLOSE, CLOSE_PROTOCOL.to_be_bytes().to_vec())]
        );
        let mut tail = Vec::new();
        lattice_read.read_to_end(&mut tail).await.unwrap();
        assert_eq!(
            frames(&tail, true),
            vec![(true, false, OP_CLOSE, CLOSE_PROTOCOL.to_be_bytes().to_vec())]
        );
        drop(browser_write);
        drop(lattice_write);
        task.await.unwrap();
    }
}
