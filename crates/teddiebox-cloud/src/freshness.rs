//! The stock box's question "has this story changed?":
//! `POST /v1/freshness-check`.
//!
//! The box sends a figure's identifier with the `audio_id` from the TAF
//! header on its card, and the server answers with the identifiers whose
//! story differs from what it holds. teddyCloud compares the `audio_id` with
//! its own file's, so a story placed on the card by hand is checked like a
//! downloaded one: nothing but the file itself is needed.
//!
//! Both messages are protobuf, hand-encoded here because they are tiny:
//!
//! ```text
//! TonieFreshnessCheckRequest  { repeated TonieFCInfo tonie_infos = 1; }
//! TonieFCInfo                 { fixed64 uid = 1; fixed32 audio_id = 2; }
//! TonieFreshnessCheckResponse { repeated fixed64 tonie_marked = 1; ... }
//! ```
//!
//! The other response fields are box settings (volume caps, LED), which this
//! firmware does not take from the server.

use crate::request::SliceWriter;
use crate::stream::read_head;
use crate::CloudError;
use core::fmt::Write as _;
use embedded_io_async::{Read, Write};

/// Where the question goes. `/v3` exists too, but teddyCloud answers both
/// alike and the stock box sends `/v1`.
pub const FRESHNESS_PATH: &str = "/v1/freshness-check";

/// One figure's entry: tag, 14-byte message, 8-byte uid, 4-byte audio id.
const BODY_LEN: usize = 2 + 1 + 8 + 1 + 4;

/// What the server said about the story on the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fresh {
    /// The server's story differs from the card's.
    Changed,
    /// The server holds the same story, or knows nothing newer.
    Current,
    /// The server did not answer the question (any status but `200`).
    /// Nothing is contradicted, so the card's copy stays.
    Unstated,
}

/// The identifier as the stock box puts it on the wire.
///
/// `ruid` is the console's byte order: `0x59756F20500304E0` for the figure
/// the reader reports as `E0:04:03:50:20:6F:75:59`. teddyCloud prints the
/// `fixed64` as sixteen hex digits and reverses the byte pairs to get that
/// same `59756f20500304e0`, so on the wire (little-endian) the eight bytes
/// are the `ruid`'s big-endian bytes.
fn uid_bytes(ruid: u64) -> [u8; 8] {
    ruid.to_be_bytes()
}

/// The request body for one figure.
pub fn encode_body(ruid: u64, audio_id: u32) -> [u8; BODY_LEN] {
    let mut body = [0u8; BODY_LEN];
    body[0] = 0x0A; // field 1, length-delimited
    body[1] = (BODY_LEN - 2) as u8;
    body[2] = 0x09; // field 1, fixed64
    body[3..11].copy_from_slice(&uid_bytes(ruid));
    body[11] = 0x15; // field 2, fixed32
    body[12..16].copy_from_slice(&audio_id.to_le_bytes());
    body
}

/// Writes the whole request, head and body, into `out`.
pub fn build_freshness_check(
    server: &str,
    ruid: u64,
    audio_id: u32,
    auth: Option<&[u8; 32]>,
    out: &mut [u8],
) -> Result<usize, CloudError> {
    let mut buf = SliceWriter { out, used: 0 };
    write!(
        buf,
        "POST {FRESHNESS_PATH} HTTP/1.1\r\nHost: {server}\r\n\
         Content-Type: application/octet-stream\r\nContent-Length: {BODY_LEN}\r\n"
    )
    .map_err(|_| CloudError::RequestTooLong)?;
    if let Some(token) = auth {
        write!(buf, "Authorization: BD ").map_err(|_| CloudError::RequestTooLong)?;
        for byte in token {
            write!(buf, "{byte:02X}").map_err(|_| CloudError::RequestTooLong)?;
        }
        write!(buf, "\r\n").map_err(|_| CloudError::RequestTooLong)?;
    }
    write!(buf, "Connection: close\r\n\r\n").map_err(|_| CloudError::RequestTooLong)?;
    let used = buf.used;
    let end = used + BODY_LEN;
    if end > out.len() {
        return Err(CloudError::RequestTooLong);
    }
    out[used..end].copy_from_slice(&encode_body(ruid, audio_id));
    Ok(end)
}

fn read_varint(body: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *body.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn take<'a>(body: &'a [u8], pos: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = pos.checked_add(len)?;
    let bytes = body.get(*pos..end)?;
    *pos = end;
    Some(bytes)
}

/// Whether the response lists this figure among the changed ones.
///
/// Unknown fields are skipped by wire type, so a newer server that adds
/// fields still reads. `tonie_marked` is accepted packed or not.
pub fn is_marked(body: &[u8], ruid: u64) -> Result<bool, CloudError> {
    let wanted = uid_bytes(ruid);
    let mut pos = 0usize;
    let mut marked = false;
    while pos < body.len() {
        let key = read_varint(body, &mut pos).ok_or(CloudError::MalformedResponse)?;
        match (key >> 3, key & 0x07) {
            (1, 1) => {
                let uid = take(body, &mut pos, 8).ok_or(CloudError::MalformedResponse)?;
                marked |= uid == wanted;
            }
            (1, 2) => {
                let len = read_varint(body, &mut pos).ok_or(CloudError::MalformedResponse)?;
                let packed =
                    take(body, &mut pos, len as usize).ok_or(CloudError::MalformedResponse)?;
                if packed.len() % 8 != 0 {
                    return Err(CloudError::MalformedResponse);
                }
                marked |= packed.chunks_exact(8).any(|uid| uid == wanted);
            }
            (_, 0) => {
                read_varint(body, &mut pos).ok_or(CloudError::MalformedResponse)?;
            }
            (_, 1) => {
                take(body, &mut pos, 8).ok_or(CloudError::MalformedResponse)?;
            }
            (_, 2) => {
                let len = read_varint(body, &mut pos).ok_or(CloudError::MalformedResponse)?;
                take(body, &mut pos, len as usize).ok_or(CloudError::MalformedResponse)?;
            }
            (_, 5) => {
                take(body, &mut pos, 4).ok_or(CloudError::MalformedResponse)?;
            }
            _ => return Err(CloudError::MalformedResponse),
        }
    }
    Ok(marked)
}

/// Asks the server whether the story on the card is still its story.
///
/// `buf` must hold the response head and body together; a few hundred bytes
/// is plenty, the body is well under a hundred.
pub async fn check<T: Read + Write>(
    transport: &mut T,
    server: &str,
    ruid: u64,
    audio_id: u32,
    auth: Option<&[u8; 32]>,
    buf: &mut [u8],
) -> Result<Fresh, CloudError> {
    let mut request = [0u8; 512];
    let n = build_freshness_check(server, ruid, audio_id, auth, &mut request)?;
    transport
        .write_all(&request[..n])
        .await
        .map_err(|_| CloudError::Transport)?;
    transport.flush().await.map_err(|_| CloudError::Transport)?;

    let (head, body_at, received) = read_head(transport, buf).await?;
    if head.status != 200 {
        return Ok(Fresh::Unstated);
    }
    let length = head.content_length.ok_or(CloudError::LengthRequired)? as usize;
    let end = body_at
        .checked_add(length)
        .filter(|&end| end <= buf.len())
        .ok_or(CloudError::ResponseTooLong)?;
    // `received` may run past the body (a keep-alive peer, or garbage).
    let mut have = received.min(end);
    while have < end {
        let n = transport
            .read(&mut buf[have..end])
            .await
            .map_err(|_| CloudError::Transport)?;
        if n == 0 {
            return Err(CloudError::BodyTruncated);
        }
        have += n;
    }
    Ok(if is_marked(&buf[body_at..end], ruid)? {
        Fresh::Changed
    } else {
        Fresh::Current
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader reports `E0:04:03:50:20:6F:75:59`.
    const KERMIT: u64 = 0x59756F20500304E0;
    const AUDIO_ID: u32 = 0x1AF0_F5B0;

    fn request_text(auth: Option<&[u8; 32]>) -> (heapless::String<512>, [u8; BODY_LEN]) {
        let mut out = [0u8; 512];
        let n = build_freshness_check("nixos:443", KERMIT, AUDIO_ID, auth, &mut out).unwrap();
        let head = &out[..n - BODY_LEN];
        let mut body = [0u8; BODY_LEN];
        body.copy_from_slice(&out[n - BODY_LEN..n]);
        (
            heapless::String::try_from(core::str::from_utf8(head).unwrap()).unwrap(),
            body,
        )
    }

    #[test]
    fn the_body_is_the_stock_box_encoding() {
        // Given / When
        let body = encode_body(KERMIT, AUDIO_ID);

        // Then: field 1 message of 14 bytes, uid as teddyCloud reverses it
        // back into `59756f20500304e0`, audio_id little-endian.
        assert_eq!(
            body,
            [
                0x0A, 14, 0x09, 0x59, 0x75, 0x6F, 0x20, 0x50, 0x03, 0x04, 0xE0, 0x15, 0xB0, 0xF5,
                0xF0, 0x1A
            ]
        );
    }

    #[test]
    fn the_request_posts_the_body_with_its_length() {
        // Given / When
        let (text, body) = request_text(None);

        // Then
        assert!(
            text.starts_with("POST /v1/freshness-check HTTP/1.1\r\nHost: nixos:443\r\n"),
            "got: {text}"
        );
        assert!(text.contains("Content-Length: 16\r\n"), "got: {text}");
        assert!(
            text.contains("Content-Type: application/octet-stream\r\n"),
            "got: {text}"
        );
        assert!(text.ends_with("Connection: close\r\n\r\n"), "got: {text}");
        assert!(!text.contains("Authorization"), "got: {text}");
        assert_eq!(body, encode_body(KERMIT, AUDIO_ID));
    }

    #[test]
    fn the_token_goes_in_the_authorization_header() {
        // Given
        let token = [0xAB; 32];

        // When
        let (text, _) = request_text(Some(&token));

        // Then
        assert!(text.contains("Authorization: BD ABAB"), "got: {text}");
    }

    #[test]
    fn a_request_that_does_not_fit_is_refused() {
        // Given
        let mut out = [0u8; 100];

        // When
        let result = build_freshness_check("nixos:443", KERMIT, AUDIO_ID, None, &mut out);

        // Then
        assert_eq!(result, Err(CloudError::RequestTooLong));
    }

    fn marked_response(uids: &[u64]) -> heapless::Vec<u8, 128> {
        let mut body = heapless::Vec::new();
        for &uid in uids {
            body.push(0x09).unwrap();
            body.extend_from_slice(&uid_bytes(uid)).unwrap();
        }
        // field2 .. led: the settings teddyCloud always sends, as varints
        for field in 2u8..=8 {
            body.push(field << 3).unwrap();
            body.push(1).unwrap();
        }
        body
    }

    #[test]
    fn a_figure_in_the_marked_list_has_changed() {
        // Given
        let body = marked_response(&[0x1111_2222_3333_4444, KERMIT]);

        // When
        let marked = is_marked(&body, KERMIT).unwrap();

        // Then
        assert!(marked);
    }

    #[test]
    fn a_figure_not_in_the_list_is_current() {
        // Given
        let body = marked_response(&[0x1111_2222_3333_4444]);

        // When
        let marked = is_marked(&body, KERMIT).unwrap();

        // Then
        assert!(!marked);
    }

    #[test]
    fn an_empty_list_is_current() {
        // Given
        let body = marked_response(&[]);

        // When
        let marked = is_marked(&body, KERMIT).unwrap();

        // Then
        assert!(!marked);
    }

    #[test]
    fn a_packed_marked_list_reads_too() {
        // Given: field 1, length-delimited, two uids
        let mut body = heapless::Vec::<u8, 64>::new();
        body.push(0x0A).unwrap();
        body.push(16).unwrap();
        body.extend_from_slice(&uid_bytes(0x1111_2222_3333_4444))
            .unwrap();
        body.extend_from_slice(&uid_bytes(KERMIT)).unwrap();

        // When
        let marked = is_marked(&body, KERMIT).unwrap();

        // Then
        assert!(marked);
    }

    #[test]
    fn a_truncated_uid_is_malformed() {
        // Given
        let body = [0x09, 0x59, 0x75];

        // When
        let result = is_marked(&body, KERMIT);

        // Then
        assert_eq!(result, Err(CloudError::MalformedResponse));
    }

    /// A transport that answers with a fixed response and records nothing.
    struct Scripted<'a> {
        response: &'a [u8],
        at: usize,
        /// Bytes per read, to exercise a body that arrives in pieces.
        bite: usize,
    }

    impl embedded_io_async::ErrorType for Scripted<'_> {
        type Error = core::convert::Infallible;
    }

    impl Read for Scripted<'_> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            let left = &self.response[self.at..];
            let n = left.len().min(buf.len()).min(self.bite);
            buf[..n].copy_from_slice(&left[..n]);
            self.at += n;
            Ok(n)
        }
    }

    impl Write for Scripted<'_> {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            Ok(buf.len())
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn response(status: &str, body: &[u8]) -> heapless::Vec<u8, 256> {
        let mut out = heapless::Vec::new();
        let mut head = heapless::String::<128>::new();
        write!(
            head,
            "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        out.extend_from_slice(head.as_bytes()).unwrap();
        out.extend_from_slice(body).unwrap();
        out
    }

    #[test]
    fn a_200_naming_the_figure_means_changed() {
        // Given
        let wire = response("200 OK", &marked_response(&[KERMIT]));
        let mut transport = Scripted {
            response: &wire,
            at: 0,
            bite: 7,
        };
        let mut buf = [0u8; 512];

        // When
        let fresh = pollster::block_on(check(
            &mut transport,
            "nixos:443",
            KERMIT,
            AUDIO_ID,
            None,
            &mut buf,
        ))
        .unwrap();

        // Then
        assert_eq!(fresh, Fresh::Changed);
    }

    #[test]
    fn a_200_without_the_figure_means_current() {
        // Given
        let wire = response("200 OK", &marked_response(&[]));
        let mut transport = Scripted {
            response: &wire,
            at: 0,
            bite: 1024,
        };
        let mut buf = [0u8; 512];

        // When
        let fresh = pollster::block_on(check(
            &mut transport,
            "nixos:443",
            KERMIT,
            AUDIO_ID,
            None,
            &mut buf,
        ))
        .unwrap();

        // Then
        assert_eq!(fresh, Fresh::Current);
    }

    #[test]
    fn any_other_status_is_unstated() {
        // Given
        let wire = response("500 Internal Server Error", b"");
        let mut transport = Scripted {
            response: &wire,
            at: 0,
            bite: 1024,
        };
        let mut buf = [0u8; 512];

        // When
        let fresh = pollster::block_on(check(
            &mut transport,
            "nixos:443",
            KERMIT,
            AUDIO_ID,
            None,
            &mut buf,
        ))
        .unwrap();

        // Then
        assert_eq!(fresh, Fresh::Unstated);
    }

    #[test]
    fn a_body_cut_short_is_an_error() {
        // Given: the head promises the whole list, the peer closes five
        // bytes early
        let whole = response("200 OK", &marked_response(&[KERMIT]));
        let wire = &whole[..whole.len() - 5];
        let mut transport = Scripted {
            response: wire,
            at: 0,
            bite: 1024,
        };
        let mut buf = [0u8; 512];

        // When
        let result = pollster::block_on(check(
            &mut transport,
            "nixos:443",
            KERMIT,
            AUDIO_ID,
            None,
            &mut buf,
        ));

        // Then
        assert_eq!(result, Err(CloudError::BodyTruncated));
    }
}
