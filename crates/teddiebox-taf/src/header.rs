//! The TAF header: a big-endian length prefix followed by a protobuf message
//! that fills the remainder of page 0.
//!
//! The message is padded to the page boundary by a zero-filled field inside
//! it, not by bytes after it. The normal unknown-field skip handles that
//! field.

use crate::varint::read_varint;
use crate::{TafError, MAX_CHAPTERS, PAGE_SIZE};
use heapless::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TonieHeader {
    /// Identifies the content; also used as the cache key against teddyCloud.
    pub audio_id: u32,
    /// Length in bytes of the Ogg stream following the header page.
    pub data_length: u32,
    /// Ogg page indices at which each chapter starts.
    ///
    /// **Indices are relative to the Ogg stream, not the file.** Chapter page
    /// 0 is the first Ogg page, at file page 1 (the header is file page 0). A
    /// single-chapter file has `[0]`, not `[1]`. Callers that seek must add
    /// one.
    pub chapter_pages: Vec<u32, MAX_CHAPTERS>,
}

impl TonieHeader {
    pub fn parse(page: &[u8; PAGE_SIZE]) -> Result<Self, TafError> {
        let proto_len = u32::from_be_bytes([page[0], page[1], page[2], page[3]]) as usize;
        if proto_len == 0 || proto_len > PAGE_SIZE - 4 {
            return Err(TafError::MalformedHeader);
        }
        let body = &page[4..4 + proto_len];

        let mut header = TonieHeader {
            audio_id: 0,
            data_length: 0,
            chapter_pages: Vec::new(),
        };

        let mut pos = 0usize;
        while pos < body.len() {
            let key = read_varint(body, &mut pos).ok_or(TafError::MalformedHeader)?;
            apply_field(&mut header, key >> 3, key & 0x07, body, &mut pos)?;
        }

        Ok(header)
    }
}

impl TonieHeader {
    /// The `audio_id` alone, from however much of the header page is in hand.
    ///
    /// For the freshness check, which needs nothing else from the header.
    /// The field sits a few dozen bytes in, right after the hash, so a short
    /// read of the file is enough, and the chapter list after it may be cut
    /// off. `None` when the bytes do not reach it or are not a header.
    pub fn audio_id_in(prefix: &[u8]) -> Option<u32> {
        let proto_len = u32::from_be_bytes(prefix.get(0..4)?.try_into().ok()?) as usize;
        if proto_len == 0 || proto_len > PAGE_SIZE - 4 {
            return None;
        }
        let body = &prefix[4..prefix.len().min(4 + proto_len)];
        let mut pos = 0usize;
        while pos < body.len() {
            let key = read_varint(body, &mut pos)?;
            match (key >> 3, key & 0x07) {
                (3, 0) => return u32::try_from(read_varint(body, &mut pos)?).ok(),
                (_, 0) => {
                    read_varint(body, &mut pos)?;
                }
                (_, 1) => pos = pos.checked_add(8)?,
                (_, 2) => {
                    let len = read_varint(body, &mut pos)? as usize;
                    pos = pos.checked_add(len)?;
                }
                (_, 5) => pos = pos.checked_add(4)?,
                _ => return None,
            }
        }
        None
    }
}

/// Decodes one protobuf field into `header`, or skips it by wire type if the
/// field is unknown — a newer TAF with an extra field still reads.
fn apply_field(
    header: &mut TonieHeader,
    field: u64,
    wire: u64,
    body: &[u8],
    pos: &mut usize,
) -> Result<(), TafError> {
    match (field, wire) {
        // data_length
        (2, 0) => {
            let v = read_varint(body, pos).ok_or(TafError::MalformedHeader)?;
            header.data_length = u32::try_from(v).map_err(|_| TafError::MalformedHeader)?;
        }
        // audio_id
        (3, 0) => {
            let v = read_varint(body, pos).ok_or(TafError::MalformedHeader)?;
            header.audio_id = u32::try_from(v).map_err(|_| TafError::MalformedHeader)?;
        }
        // chapter_pages, unpacked
        (4, 0) => {
            let v = read_varint(body, pos).ok_or(TafError::MalformedHeader)?;
            let v = u32::try_from(v).map_err(|_| TafError::MalformedHeader)?;
            header
                .chapter_pages
                .push(v)
                .map_err(|_| TafError::TooManyChapters)?;
        }
        // chapter_pages, packed
        (4, 2) => {
            let len = read_varint(body, pos).ok_or(TafError::MalformedHeader)? as usize;
            let end = pos.checked_add(len).ok_or(TafError::MalformedHeader)?;
            if end > body.len() {
                return Err(TafError::MalformedHeader);
            }
            while *pos < end {
                let v = read_varint(body, pos).ok_or(TafError::MalformedHeader)?;
                let v = u32::try_from(v).map_err(|_| TafError::MalformedHeader)?;
                header
                    .chapter_pages
                    .push(v)
                    .map_err(|_| TafError::TooManyChapters)?;
            }
            // `read_varint` does not stop at `end`, so a varint that runs
            // past the packed field is only caught here.
            if *pos != end {
                return Err(TafError::MalformedHeader);
            }
        }
        // Unknown field: skip by wire type.
        (_, 0) => {
            read_varint(body, pos).ok_or(TafError::MalformedHeader)?;
        }
        (_, 2) => {
            let len = read_varint(body, pos).ok_or(TafError::MalformedHeader)? as usize;
            *pos = pos.checked_add(len).ok_or(TafError::MalformedHeader)?;
            if *pos > body.len() {
                return Err(TafError::MalformedHeader);
            }
        }
        // Fixed-width fields need a bounds check too. Otherwise stepping
        // past the end would just end the loop and accept a truncated
        // message.
        (_, 5) => {
            *pos = pos.checked_add(4).ok_or(TafError::MalformedHeader)?;
            if *pos > body.len() {
                return Err(TafError::MalformedHeader);
            }
        }
        (_, 1) => {
            *pos = pos.checked_add(8).ok_or(TafError::MalformedHeader)?;
            if *pos > body.len() {
                return Err(TafError::MalformedHeader);
            }
        }
        _ => return Err(TafError::MalformedHeader),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_pages::header_page;

    #[test]
    fn parses_audio_id_and_data_length() {
        // Given: field 2 (data_length) varint = 8192; field 3 (audio_id) varint = 0x1234
        let fields = [0x10, 0x80, 0x40, 0x18, 0xB4, 0x24];

        // When
        let h = TonieHeader::parse(&header_page(&fields)).unwrap();

        // Then
        assert_eq!(h.data_length, 8192);
        assert_eq!(h.audio_id, 0x1234);
    }

    #[test]
    fn parses_packed_chapter_pages() {
        // Given: field 4, wire type 2, payload length 4, values 1, 50, 120, 1
        let fields = [0x22, 0x04, 0x01, 0x32, 0x78, 0x01];

        // When
        let h = TonieHeader::parse(&header_page(&fields)).unwrap();

        // Then
        assert_eq!(h.chapter_pages.as_slice(), &[1, 50, 120, 1]);
    }

    #[test]
    fn parses_unpacked_chapter_pages() {
        // Given: field 4, wire type 0, repeated
        let fields = [0x20, 0x01, 0x20, 0x32];

        // When
        let h = TonieHeader::parse(&header_page(&fields)).unwrap();

        // Then
        assert_eq!(h.chapter_pages.as_slice(), &[1, 50]);
    }

    #[test]
    fn skips_unknown_fields() {
        // Given: field 1 (hash), wire type 2, 3 bytes, then field 3 (audio_id)
        let fields = [0x0A, 0x03, 0xAA, 0xBB, 0xCC, 0x18, 0x07];

        // When
        let h = TonieHeader::parse(&header_page(&fields)).unwrap();

        // Then
        assert_eq!(h.audio_id, 7);
    }

    /// A newer TAF may carry fields this parser does not know, in any of the
    /// four wire types. Each is placed last, so a fixed-width one ends exactly
    /// where the message does.
    #[test]
    fn skips_an_unknown_field_of_every_wire_type() {
        // Given: audio_id 7, then an unknown field 9 of each wire type
        let fields: [&[u8]; 4] = [
            &[0x18, 0x07, 0x48, 0x96, 0x01],             // varint 150
            &[0x18, 0x07, 0x49, 1, 2, 3, 4, 5, 6, 7, 8], // fixed64
            &[0x18, 0x07, 0x4A, 0x02, 0xAA, 0xBB],       // two length-delimited bytes
            &[0x18, 0x07, 0x4D, 1, 2, 3, 4],             // fixed32
        ];

        // When
        let audio_ids =
            fields.map(|fields| TonieHeader::parse(&header_page(fields)).map(|h| h.audio_id));

        // Then
        assert_eq!(audio_ids, [Ok(7); 4]);
    }

    #[test]
    fn rejects_length_prefix_larger_than_the_page() {
        // Given
        let mut page = [0xFFu8; PAGE_SIZE];
        page[0..4].copy_from_slice(&(PAGE_SIZE as u32).to_be_bytes());

        // When
        let parsed = TonieHeader::parse(&page);

        // Then
        assert_eq!(parsed, Err(TafError::MalformedHeader));
    }

    #[test]
    fn rejects_truncated_fixed32_field() {
        // Given: field 9, wire type 5 (fixed32): declares a 4-byte payload but only
        // 2 bytes remain in the body.
        let fields = [0x4D, 0x00, 0x00];

        // When
        let parsed = TonieHeader::parse(&header_page(&fields));

        // Then
        assert_eq!(parsed, Err(TafError::MalformedHeader));
    }

    #[test]
    fn rejects_truncated_fixed64_field() {
        // Given: field 9, wire type 1 (fixed64): declares an 8-byte payload but only
        // 2 bytes remain in the body.
        let fields = [0x49, 0x00, 0x00];

        // When
        let parsed = TonieHeader::parse(&header_page(&fields));

        // Then
        assert_eq!(parsed, Err(TafError::MalformedHeader));
    }

    #[test]
    fn rejects_a_data_length_that_overflows_u32() {
        // Given: field 2 (data_length), varint 4295024640 (> u32::MAX). `as u32`
        // would silently wrap this to 57344, and `data_length` decides how
        // much of the file is read as audio.
        let fields = [0x10, 0x80, 0xC0, 0x83, 0x80, 0x10];

        // When
        let parsed = TonieHeader::parse(&header_page(&fields));

        // Then
        assert_eq!(parsed, Err(TafError::MalformedHeader));
    }

    #[test]
    fn rejects_an_audio_id_that_overflows_u32() {
        // Given: field 3 (audio_id), varint 4294967338 (u32::MAX + 43).
        let fields = [0x18, 0xAA, 0x80, 0x80, 0x80, 0x10];

        // When
        let parsed = TonieHeader::parse(&header_page(&fields));

        // Then
        assert_eq!(parsed, Err(TafError::MalformedHeader));
    }

    #[test]
    fn rejects_a_chapter_page_that_overflows_u32() {
        // Given: field 4 (chapter_pages), unpacked, varint 4294967297 (u32::MAX + 2).
        let fields = [0x20, 0x81, 0x80, 0x80, 0x80, 0x10];

        // When
        let parsed = TonieHeader::parse(&header_page(&fields));

        // Then
        assert_eq!(parsed, Err(TafError::MalformedHeader));
    }

    #[test]
    fn rejects_packed_field_whose_final_varint_overruns_its_declared_length() {
        // Given: field 4, wire type 2, declared length 2, payload [0x80, 0x80]: both
        // bytes carry a continuation bit, so the varint is unterminated
        // within the declared length and only resolves by reading the extra
        // trailing byte that follows the field.
        let fields = [0x22, 0x02, 0x80, 0x80, 0x00];

        // When
        let parsed = TonieHeader::parse(&header_page(&fields));

        // Then
        assert_eq!(parsed, Err(TafError::MalformedHeader));
    }
}

#[cfg(test)]
mod audio_id_tests {
    use super::*;

    /// hash (field 1, 20 bytes), data_length (2) = 8192, audio_id (3) =
    /// 0x1AF0F5B0, then a chapter list (4) that may be cut off.
    fn fields() -> [u8; 40] {
        let mut f = [0u8; 40];
        f[0] = 0x0A;
        f[1] = 20;
        f[2..22].copy_from_slice(&[0xCD; 20]);
        f[22..25].copy_from_slice(&[0x10, 0x80, 0x40]); // 8192
        f[25..31].copy_from_slice(&[0x18, 0xB0, 0xEB, 0xC3, 0xD7, 0x01]); // 0x1AF0F5B0
        f[31..40].copy_from_slice(&[0x20, 0x00, 0x20, 0x10, 0x20, 0x20, 0x20, 0x30, 0x20]);
        f
    }

    fn prefix(cut: usize) -> heapless::Vec<u8, 64> {
        let f = fields();
        let mut out = heapless::Vec::new();
        out.extend_from_slice(&(f.len() as u32).to_be_bytes())
            .unwrap();
        out.extend_from_slice(&f[..cut.min(f.len())]).unwrap();
        out
    }

    #[test]
    fn the_audio_id_is_read_from_a_whole_header() {
        // Given
        let bytes = prefix(40);

        // When
        let id = TonieHeader::audio_id_in(&bytes);

        // Then
        assert_eq!(id, Some(0x1AF0_F5B0));
    }

    #[test]
    fn a_prefix_cut_in_the_chapter_list_still_gives_the_audio_id() {
        // Given: 31 bytes of fields reach the end of audio_id exactly
        let bytes = prefix(31);

        // When
        let id = TonieHeader::audio_id_in(&bytes);

        // Then
        assert_eq!(id, Some(0x1AF0_F5B0));
    }

    #[test]
    fn a_prefix_cut_before_the_audio_id_gives_none() {
        // Given
        let bytes = prefix(28);

        // When
        let id = TonieHeader::audio_id_in(&bytes);

        // Then
        assert_eq!(id, None);
    }

    #[test]
    fn bytes_that_are_not_a_header_give_none() {
        // Given
        let not_a_header = [0u8; 8];
        let too_short = [0u8; 3];

        // When / Then
        assert_eq!(TonieHeader::audio_id_in(&not_a_header), None);
        assert_eq!(TonieHeader::audio_id_in(&too_short), None);
    }
}
